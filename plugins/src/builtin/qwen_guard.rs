//! `qwen_guard` plugin — a content-safety prefilter backed by a
//! **Qwen3Guard-Gen** model (Alibaba's safety moderation series built on
//! Qwen3).
//!
//! Qwen3Guard-Gen is a generative classifier: given a prompt, it answers
//!
//! ```text
//! Safety: Unsafe
//! Categories: Violent
//! ```
//!
//! with one of three severity levels — `Safe`, `Controversial`, `Unsafe` —
//! and zero or more harm categories. The plugin runs the guard model as a
//! separate llama-server (or any OpenAI-compatible endpoint) speaking
//! `/v1/chat/completions`, classifies each request's combined message
//! text as a *prompt* (Qwen3Guard's prompt-moderation mode: the flattened
//! text is sent as a single `user` message), and applies the configured
//! allow/reject policy.
//!
//! Params:
//! - `url` (required): base URL of the server hosting the Qwen3Guard-Gen
//!   model (e.g. `http://127.0.0.1:8010`); the plugin POSTs to
//!   `{url}/v1/chat/completions`.
//! - `api_key` (optional): sent as `Authorization: Bearer <key>`.
//! - `model` (optional): model id in the chat-completions request.
//! - `timeout_secs` (default `30`): per-check timeout.
//! - `max_input_chars` (default `0` = no truncation): cap on the
//!   flattened text sent to the guard (guard models have small contexts).
//! - `reject_levels` (default `["Unsafe"]`): severity levels that reject
//!   the request outright.
//! - `reject_categories` (default `[]`): categories that reject the
//!   request when present in the verdict (independent of the level).
//! - `on_error` (default `"accept"`): `"accept"` or `"reject"` — what to
//!   do when the guard call fails (transport/HTTP/timeout).
//! - `on_unparseable` (default `"accept"`): `"accept"` or `"reject"` —
//!   what to do when the guard responds but its output cannot be parsed
//!   into a safety level / categories.
//! - `cache_max` (default `0` = off): bounded LRU of verdicts keyed by
//!   the request's KV cache key — repeated same-key prompts skip the
//!   guard call.
//!
//! The nine Qwen3Guard categories (names are case-insensitive in config
//! and output; `PII` also matches "Personally Identifiable Information"):
//! `Violent`, `Non-violent Illegal Acts`, `Sexual Content or Sexual
//! Acts`, `PII`, `Suicide & Self-Harm`, `Unethical Acts`, `Politically
//! Sensitive Topics`, `Copyright Violation`, `Jailbreak` (input only).
//!
//! Rejections are `400` with a message naming the level/categories that
//! tripped; a rejected request never reaches the chat backend — no slot
//! is acquired, no KV cache is restored or saved, no meta file is
//! written, and the request never leads or joins a coalescing group.

use crate::api::{Prefilter, PrefilterDecision, PrefilterRequest};
use crate::builtin::keyword::message_text;
use crate::registry::{PluginError, PluginFactory};
use async_trait::async_trait;
use serde::Deserialize;
use serde::de::Error as _;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Qwen3Guard vocabulary
// ---------------------------------------------------------------------------

/// Qwen3Guard's three-tier severity levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyLevel {
    /// Generally safe across most scenarios.
    Safe,
    /// Harmfulness may be context-dependent or disputed.
    Controversial,
    /// Generally considered harmful across most scenarios.
    Unsafe,
}

impl SafetyLevel {
    /// The canonical output name (as printed by the model).
    pub fn name(self) -> &'static str {
        match self {
            Self::Safe => "Safe",
            Self::Controversial => "Controversial",
            Self::Unsafe => "Unsafe",
        }
    }

    /// Case-insensitive parse of a level name (`None` when unknown).
    pub fn from_name(raw: &str) -> Option<Self> {
        Some(match raw.trim().to_lowercase().as_str() {
            "safe" => Self::Safe,
            "controversial" => Self::Controversial,
            "unsafe" => Self::Unsafe,
            _ => return None,
        })
    }
}

/// The nine Qwen3Guard harm categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardCategory {
    /// Detailed instructions/depictions of violence, weapons.
    Violent,
    /// Guidance for non-violent illegal activity (hacking, drugs, theft).
    NonViolentIllegalActs,
    /// Sexual imagery, references or descriptions (incl. illegal/unethical).
    SexualContent,
    /// Unauthorized sharing of sensitive personal identifying information
    /// (also spelled out as "Personally Identifiable Information").
    Pii,
    /// Advocacy or methods for self-harm / suicide.
    SuicideSelfHarm,
    /// Bias, discrimination, hate speech, harassment, misinformation, etc.
    UnethicalActs,
    /// False information about government, history or public figures.
    PoliticallySensitive,
    /// Unauthorized use of copyrighted material.
    CopyrightViolation,
    /// Attempts to override the model's system prompt (input only).
    Jailbreak,
}

impl GuardCategory {
    /// All categories, in the model's canonical order.
    pub const ALL: [Self; 9] = [
        Self::Violent,
        Self::NonViolentIllegalActs,
        Self::SexualContent,
        Self::Pii,
        Self::SuicideSelfHarm,
        Self::UnethicalActs,
        Self::PoliticallySensitive,
        Self::CopyrightViolation,
        Self::Jailbreak,
    ];

    /// The canonical category name (as printed by the model).
    pub fn name(self) -> &'static str {
        match self {
            Self::Violent => "Violent",
            Self::NonViolentIllegalActs => "Non-violent Illegal Acts",
            Self::SexualContent => "Sexual Content or Sexual Acts",
            Self::Pii => "PII",
            Self::SuicideSelfHarm => "Suicide & Self-Harm",
            Self::UnethicalActs => "Unethical Acts",
            Self::PoliticallySensitive => "Politically Sensitive Topics",
            Self::CopyrightViolation => "Copyright Violation",
            Self::Jailbreak => "Jailbreak",
        }
    }

    /// Case-insensitive parse of a category name (aliases included:
    /// `PII` / "Personally Identifiable Information"); `None` when unknown.
    pub fn from_name(raw: &str) -> Option<Self> {
        Some(match raw.trim().to_lowercase().as_str() {
            "violent" => Self::Violent,
            "non-violent illegal acts" | "nonviolent illegal acts" => Self::NonViolentIllegalActs,
            "sexual content or sexual acts" => Self::SexualContent,
            "pii" | "personally identifiable information" => Self::Pii,
            "suicide & self-harm" | "suicide & self harm" | "suicide and self-harm" => {
                Self::SuicideSelfHarm
            }
            "unethical acts" => Self::UnethicalActs,
            "politically sensitive topics" | "politically sensitive" => Self::PoliticallySensitive,
            "copyright violation" => Self::CopyrightViolation,
            "jailbreak" => Self::Jailbreak,
            _ => return None,
        })
    }
}

/// A parsed Qwen3Guard verdict: the severity level and the harm
/// categories named in the model's output (`level = None` / empty
/// `categories` when the output did not contain them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardVerdict {
    pub level: Option<SafetyLevel>,
    pub categories: Vec<GuardCategory>,
}

/// Parse a Qwen3Guard-Gen completion into a [`GuardVerdict`].
///
/// Recognizes `Safety: <level>` and `Categories: <comma list>` lines in
/// the output (case-insensitively); `None` in the category list is
/// dropped, unknown names are ignored, duplicates are removed (order
/// preserved). Output with neither line yields an empty verdict.
pub fn parse_guard_output(content: &str) -> GuardVerdict {
    let lower = content.to_lowercase();

    let level = lower
        .find("safety:")
        .and_then(|i| {
            lower[i + "safety:".len()..]
                .trim_start()
                .split(|c: char| !c.is_alphabetic())
                .next()
        })
        .and_then(SafetyLevel::from_name);

    let mut categories: Vec<GuardCategory> = Vec::new();
    if let Some(i) = lower.find("categories:") {
        let line = lower[i + "categories:".len()..]
            .split('\n')
            .next()
            .unwrap_or("");
        for part in line.split(',') {
            let part = part.trim();
            if part.is_empty() || part == "none" {
                continue;
            }
            if let Some(cat) = GuardCategory::from_name(part)
                && !categories.contains(&cat)
            {
                categories.push(cat);
            }
        }
    }

    GuardVerdict { level, categories }
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// What to do when the guard cannot produce a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailAction {
    /// Fail open: let the request through.
    Accept,
    /// Fail closed: reject the request.
    Reject,
}

/// The allow/reject policy: which severity levels and which categories
/// reject a request.
#[derive(Debug, Clone)]
pub struct GuardPolicy {
    /// Verdicts at one of these levels reject the request outright.
    pub reject_levels: Vec<SafetyLevel>,
    /// Verdicts naming any of these categories reject the request
    /// (independent of the level).
    pub reject_categories: Vec<GuardCategory>,
    /// Action when the guard call itself fails.
    pub on_error: FailAction,
    /// Action when the guard's output cannot be parsed.
    pub on_unparseable: FailAction,
}

/// Apply the policy to a verdict: level rule first, then category rule,
/// then accept.
pub fn decide(verdict: &GuardVerdict, policy: &GuardPolicy) -> PrefilterDecision {
    if let Some(level) = verdict.level
        && policy.reject_levels.contains(&level)
    {
        let cats = if verdict.categories.is_empty() {
            "None".to_string()
        } else {
            verdict
                .categories
                .iter()
                .map(|c| c.name())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return PrefilterDecision::Reject {
            status: 400,
            message: format!(
                "request blocked by qwen_guard: Safety={}, Categories: {cats}",
                level.name()
            ),
        };
    }
    let hits: Vec<&str> = verdict
        .categories
        .iter()
        .filter(|c| policy.reject_categories.contains(c))
        .map(|c| c.name())
        .collect();
    if !hits.is_empty() {
        return PrefilterDecision::Reject {
            status: 400,
            message: format!(
                "request blocked by qwen_guard: category {} not allowed",
                hits.join(", ")
            ),
        };
    }
    PrefilterDecision::Accept
}

/// Truncate `text` to at most `max` chars (no-op when `max == 0` or the
/// text already fits).
fn truncate_chars(text: &str, max: usize) -> &str {
    if max == 0 || text.chars().count() <= max {
        text
    } else {
        text.chars().take(max).collect::<String>().leak()
    }
}

/// Bounded LRU of guard verdicts keyed by request KV cache key (a
/// repeated same-key prompt reuses its verdict instead of re-running the
/// guard model).
#[derive(Debug)]
struct VerdictCache {
    cap: usize,
    order: VecDeque<String>,
    map: std::collections::HashMap<String, PrefilterDecision>,
}

impl VerdictCache {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            order: VecDeque::with_capacity(cap),
            map: std::collections::HashMap::with_capacity(cap),
        }
    }

    fn get(&mut self, key: &str) -> Option<PrefilterDecision> {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            // mark most-recent: move to the back (eviction pops the front)
            self.order.remove(pos);
            self.order.push_back(key.to_string());
            self.map.get(key).cloned()
        } else {
            None
        }
    }

    fn put(&mut self, key: &str, decision: &PrefilterDecision) {
        if self.map.len() < self.cap {
            self.order.push_back(key.to_string());
        } else if let Some(old) = self.order.pop_front() {
            self.map.remove(&old);
            self.order.push_back(key.to_string());
        }
        self.map.insert(key.to_string(), decision.clone());
    }
}

// ---------------------------------------------------------------------------
// Prefilter adapter
// ---------------------------------------------------------------------------

/// Built-in adapter: sends the request's combined message text to a
/// Qwen3Guard-Gen server and applies the [`GuardPolicy`].
///
/// - Empty requests (no message text at all) are accepted without a
///   guard call.
/// - The guard call is `POST {url}/v1/chat/completions` with the
///   flattened text as a single `user` message (Qwen3Guard's
///   prompt-moderation mode), `max_tokens: 64`, `temperature: 0`.
/// - A failing guard call follows `policy.on_error`; an unparseable
///   successful response follows `policy.on_unparseable` (both default
///   to fail open).
/// - With `cache_max > 0`, verdicts are memoized in a bounded LRU keyed
///   by the request's KV cache key.
#[derive(Debug)]
pub struct QwenGuardPrefilter {
    client: reqwest::Client,
    chat_url: String,
    api_key: Option<String>,
    model: Option<String>,
    max_input_chars: usize,
    policy: GuardPolicy,
    cache: Option<Mutex<VerdictCache>>,
}

impl QwenGuardPrefilter {
    /// Build the adapter. `base_url` is the guard server's base URL
    /// (the plugin POSTs `{base_url}/v1/chat/completions`).
    pub fn new(
        base_url: &str,
        timeout: Duration,
        api_key: Option<&str>,
        model: Option<&str>,
        max_input_chars: usize,
        policy: GuardPolicy,
        cache_max: usize,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .pool_max_idle_per_host(20)
            .build()?;
        let chat_url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));
        tracing::info!(
            "client_init url={chat_url} api_key={} model={:?} reject_levels={:?} reject_categories={:?} on_error={:?} on_unparseable={:?} cache_max={cache_max}",
            api_key.is_some(),
            model,
            policy.reject_levels,
            policy.reject_categories,
            policy.on_error,
            policy.on_unparseable,
        );
        Ok(Self {
            client,
            chat_url,
            api_key: api_key.map(str::to_string),
            model: model.map(str::to_string),
            max_input_chars,
            policy,
            cache: (cache_max > 0).then(|| Mutex::new(VerdictCache::new(cache_max))),
        })
    }

    /// The configured policy.
    pub fn policy(&self) -> &GuardPolicy {
        &self.policy
    }

    fn lock_cache(&self) -> Option<MutexGuard<'_, VerdictCache>> {
        self.cache
            .as_ref()
            .map(|c| c.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    /// Applies the configured `Authorization` header (no-op when unset).
    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => req.header(reqwest::header::AUTHORIZATION, format!("Bearer {key}")),
            None => req,
        }
    }

    /// Wrap a failed / unparseable guard call into a decision per the
    /// policy's fail-open / fail-closed settings.
    fn finish(&self, outcome: Result<(), String>) -> PrefilterDecision {
        match outcome {
            Ok(()) => match self.policy.on_unparseable {
                FailAction::Accept => PrefilterDecision::Accept,
                FailAction::Reject => PrefilterDecision::Reject {
                    status: 400,
                    message: "content check failed: guard output could not be parsed".to_string(),
                },
            },
            Err(err) => match self.policy.on_error {
                FailAction::Accept => PrefilterDecision::Accept,
                FailAction::Reject => PrefilterDecision::Reject {
                    status: 503,
                    message: format!("content check unavailable: {err}"),
                },
            },
        }
    }
}

#[async_trait]
impl Prefilter for QwenGuardPrefilter {
    fn name(&self) -> &str {
        "qwen_guard"
    }

    async fn check(&self, req: &PrefilterRequest) -> PrefilterDecision {
        let text = message_text(&req.messages);
        // Nothing textual to moderate.
        if text.is_empty() {
            return PrefilterDecision::Accept;
        }

        // Repeated same-key prompts reuse their verdict.
        if let Some(decision) = self.lock_cache().and_then(|mut c| c.get(&req.key)) {
            return decision;
        }

        let input = truncate_chars(&text, self.max_input_chars);
        let mut body = json!({
            "messages": [{ "role": "user", "content": input }],
            "max_tokens": 64,
            "temperature": 0,
            "stream": false,
        });
        if let Some(model) = &self.model {
            body["model"] = json!(model);
        }

        let content = match self
            .apply_auth(self.client.post(&self.chat_url).json(&body))
            .send()
            .await
        {
            Err(err) => {
                tracing::warn!("qwen_guard: check failed: {err}");
                return self.finish(Err(err.to_string()));
            }
            Ok(resp) => {
                let status = resp.status();
                let v = match resp.json::<Value>().await {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::warn!("qwen_guard: response not JSON: {err}");
                        return self.finish(Err(err.to_string()));
                    }
                };
                if !status.is_success() {
                    // The guard's server answered with an HTTP error
                    // (llama-server does this while the model is loading:
                    // `503 {"error":{"message":"Loading model"}}`). That is
                    // a *failed check* — apply the `on_error` policy, not
                    // `on_unparseable`: a fail-closed deployment must not
                    // fail open just because the error body happened to be
                    // valid JSON.
                    let body = v.to_string();
                    let body: String = body.chars().take(200).collect();
                    tracing::warn!("qwen_guard: guard returned HTTP {status}: {body}");
                    return self.finish(Err(format!("guard returned HTTP {status}: {body}")));
                }
                v.get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }
        };

        let verdict = match content {
            Some(ref text) => parse_guard_output(text),
            None => {
                tracing::warn!("qwen_guard: response has no choices[0].message.content");
                return self.finish(Ok(()));
            }
        };
        if verdict.level.is_none() && verdict.categories.is_empty() {
            tracing::warn!(
                "qwen_guard: could not parse a Safety level / Categories from guard output: {content:?}"
            );
            return self.finish(Ok(()));
        }

        let decision = decide(&verdict, &self.policy);
        if let Some(mut c) = self.lock_cache() {
            c.put(&req.key, &decision);
        }
        decision
    }
}

// ---------------------------------------------------------------------------
// Plugin factory (configurable, dynamically loadable)
// ---------------------------------------------------------------------------

const NAME: &str = "qwen_guard";

#[derive(Debug, Deserialize)]
struct GuardParams {
    url: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
    #[serde(default)]
    max_input_chars: usize,
    #[serde(default = "default_reject_levels", deserialize_with = "string_list")]
    reject_levels: Vec<String>,
    #[serde(default, deserialize_with = "string_list")]
    reject_categories: Vec<String>,
    #[serde(default = "default_accept", deserialize_with = "fail_action")]
    on_error: FailAction,
    #[serde(default = "default_accept", deserialize_with = "fail_action")]
    on_unparseable: FailAction,
    #[serde(default)]
    cache_max: usize,
}

fn default_timeout_secs() -> u64 {
    30
}

fn default_accept() -> FailAction {
    FailAction::Accept
}

fn default_reject_levels() -> Vec<String> {
    vec!["Unsafe".to_string()]
}

/// Accepts either a JSON array of strings or a comma-separated string
/// (the same forms as the `keyword` plugin's `keywords`).
fn string_list<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let v = Value::deserialize(d)?;
    match v {
        Value::Array(items) => items
            .into_iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| D::Error::custom("list items must be strings"))
            })
            .collect(),
        Value::String(s) => Ok(s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()),
        other => Err(D::Error::custom(format!(
            "must be an array of strings or a comma-separated string, got {other:?}"
        ))),
    }
}

fn fail_action<'de, D: serde::Deserializer<'de>>(d: D) -> Result<FailAction, D::Error> {
    let v = Value::deserialize(d)?;
    match v.as_str() {
        Some("accept" | "allow") => Ok(FailAction::Accept),
        Some("reject" | "block") => Ok(FailAction::Reject),
        other => Err(D::Error::custom(format!(
            "must be \"accept\" or \"reject\", got {other:?}"
        ))),
    }
}

/// Factory for the `qwen_guard` plugin (registered by
/// [`crate::builtin::register`]).
pub struct QwenGuardPluginFactory;

impl PluginFactory for QwenGuardPluginFactory {
    fn create_prefilter(&self, params: &Value) -> Result<Option<Arc<dyn Prefilter>>, PluginError> {
        let bad = |message: String| PluginError::BadParams {
            name: NAME.to_string(),
            message,
        };
        let p: GuardParams = GuardParams::deserialize(params).map_err(|e| bad(e.to_string()))?;
        if p.url.trim().is_empty() {
            return Err(bad("`url` must be a non-empty base URL".to_string()));
        }
        let reject_levels: Vec<SafetyLevel> = p
            .reject_levels
            .iter()
            .map(|l| {
                SafetyLevel::from_name(l).ok_or_else(|| {
                    bad(format!(
                        "unknown level {l:?}; expected one of: Safe, Controversial, Unsafe"
                    ))
                })
            })
            .collect::<Result<_, _>>()?;
        let reject_categories: Vec<GuardCategory> = p
            .reject_categories
            .iter()
            .map(|c| {
                GuardCategory::from_name(c).ok_or_else(|| {
                    bad(format!(
                        "unknown category {c:?}; expected one of: {}",
                        GuardCategory::ALL
                            .iter()
                            .map(|c| c.name())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })
            })
            .collect::<Result<_, _>>()?;
        let policy = GuardPolicy {
            reject_levels,
            reject_categories,
            on_error: p.on_error,
            on_unparseable: p.on_unparseable,
        };
        let pf = QwenGuardPrefilter::new(
            &p.url,
            Duration::from_secs(p.timeout_secs),
            p.api_key.as_deref(),
            p.model.as_deref(),
            p.max_input_chars,
            policy,
            p.cache_max,
        )
        .map_err(|e| bad(format!("failed to build HTTP client: {e}")))?;
        Ok(Some(Arc::new(pf)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn preq(messages: Value, n_words: usize) -> PrefilterRequest {
        PrefilterRequest {
            body: json!({ "messages": messages }),
            messages,
            model: "llama.cpp".to_string(),
            backend_model_id: "backend-model".to_string(),
            stream: false,
            key: "k".to_string(),
            n_words,
            is_big: n_words > 500,
        }
    }

    fn policy(
        reject_levels: Vec<SafetyLevel>,
        reject_categories: Vec<GuardCategory>,
    ) -> GuardPolicy {
        GuardPolicy {
            reject_levels,
            reject_categories,
            on_error: FailAction::Accept,
            on_unparseable: FailAction::Accept,
        }
    }

    #[test]
    fn parses_model_card_outputs() {
        // Prompt moderation output (Qwen3Guard-Gen model card).
        let v = parse_guard_output("Safety: Unsafe\nCategories: Violent");
        assert_eq!(v.level, Some(SafetyLevel::Unsafe));
        assert_eq!(v.categories, vec![GuardCategory::Violent]);

        // Response moderation output carries a Refusal: line too.
        let v = parse_guard_output("Safety: Safe\nCategories: None\nRefusal: Yes");
        assert_eq!(v.level, Some(SafetyLevel::Safe));
        assert_eq!(v.categories, Vec::<GuardCategory>::new());

        // Multi-category line, unknown entries ignored, duplicates dropped.
        let v = parse_guard_output(
            "Safety: Controversial\nCategories: PII, Violent, Violent, Made-up Stuff",
        );
        assert_eq!(v.level, Some(SafetyLevel::Controversial));
        assert_eq!(
            v.categories,
            vec![GuardCategory::Pii, GuardCategory::Violent]
        );
    }

    #[test]
    fn parse_is_case_insensitive_and_tolerates_whitespace() {
        let v =
            parse_guard_output("  safety:   UNsafe \n  categories:   non-violent illegal acts  ");
        assert_eq!(v.level, Some(SafetyLevel::Unsafe));
        assert_eq!(v.categories, vec![GuardCategory::NonViolentIllegalActs]);
    }

    #[test]
    fn parse_category_aliases() {
        assert_eq!(
            GuardCategory::from_name("personally identifiable information"),
            Some(GuardCategory::Pii)
        );
        assert_eq!(
            GuardCategory::from_name("suicide and self-harm"),
            Some(GuardCategory::SuicideSelfHarm)
        );
        assert_eq!(GuardCategory::from_name("politics"), None);
        assert_eq!(SafetyLevel::from_name("maybe"), None);
        assert_eq!(parse_guard_output("no guard output at all").level, None);
        assert!(parse_guard_output("").categories.is_empty());
    }

    #[test]
    fn decide_level_rule_first() {
        let p = policy(vec![SafetyLevel::Unsafe], Vec::new());
        let d = decide(
            &GuardVerdict {
                level: Some(SafetyLevel::Unsafe),
                categories: vec![GuardCategory::Violent],
            },
            &p,
        );
        match d {
            PrefilterDecision::Reject { status, message } => {
                assert_eq!(status, 400);
                assert!(message.contains("Safety=Unsafe"), "{message}");
                assert!(message.contains("Violent"), "{message}");
            }
            other => panic!("expected reject, got {other:?}"),
        }

        // Unsafe level is rejected even when categories are allowed.
        let d = decide(
            &GuardVerdict {
                level: Some(SafetyLevel::Unsafe),
                categories: vec![],
            },
            &p,
        );
        assert!(matches!(d, PrefilterDecision::Reject { .. }));
    }

    #[test]
    fn decide_category_rule_independent_of_level() {
        let p = policy(
            Vec::new(),
            vec![GuardCategory::Jailbreak, GuardCategory::Pii],
        );
        // Controversial + PII -> rejected by category.
        let d = decide(
            &GuardVerdict {
                level: Some(SafetyLevel::Controversial),
                categories: vec![GuardCategory::Pii],
            },
            &p,
        );
        assert!(
            matches!(d, PrefilterDecision::Reject { status: 400, ref message }
            if message.contains("PII"))
        );
        // Controversial without a rejected category -> accepted.
        let d = decide(
            &GuardVerdict {
                level: Some(SafetyLevel::Controversial),
                categories: vec![GuardCategory::Violent],
            },
            &p,
        );
        assert_eq!(d, PrefilterDecision::Accept);
        // No level, categories only.
        let d = decide(
            &GuardVerdict {
                level: None,
                categories: vec![GuardCategory::Jailbreak],
            },
            &p,
        );
        assert!(matches!(d, PrefilterDecision::Reject { .. }));
    }

    #[test]
    fn decide_accepts_safe() {
        let p = policy(
            vec![SafetyLevel::Unsafe, SafetyLevel::Controversial],
            GuardCategory::ALL.to_vec(),
        );
        let d = decide(
            &GuardVerdict {
                level: Some(SafetyLevel::Safe),
                categories: vec![],
            },
            &p,
        );
        assert_eq!(d, PrefilterDecision::Accept);
    }

    #[test]
    fn verdict_cache_is_bounded_lru() {
        let mut c = VerdictCache::new(2);
        let a = PrefilterDecision::Accept;
        let r = PrefilterDecision::Reject {
            status: 400,
            message: "x".to_string(),
        };
        c.put("k1", &a);
        c.put("k2", &r);
        assert_eq!(c.get("k1"), Some(a.clone()));
        // k1 becomes most recent; k2 is now the oldest.
        c.put("k3", &a); // evicts k2
        assert_eq!(c.get("k2"), None);
        assert_eq!(c.get("k1"), Some(a.clone()));
        assert_eq!(c.get("k3"), Some(a));
    }

    // -- HTTP round-trip against a one-shot mock guard server ---------------

    /// A one-shot TCP server answering the first request with a canned
    /// OpenAI chat-completions body (or dropping the connection when
    /// `resp` is `None`). Returns the base URL.
    fn mock_guard_server(resp: Option<&str>) -> String {
        mock_guard_server_status(200, resp)
    }

    /// Like [`mock_guard_server`], but answers with the given HTTP status
    /// (e.g. 503 "Loading model" while a guard model is still loading).
    fn mock_guard_server_status(status: u16, resp: Option<&str>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let resp = resp.map(str::to_string);
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let Some(body) = resp else {
                return; // drop the connection -> client error
            };
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 4096];
            let header_len = loop {
                let n = sock.read(&mut tmp).unwrap();
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&tmp[..n]);
                let s = String::from_utf8_lossy(&buf);
                if let Some(pos) = s.find("\r\n\r\n") {
                    break pos + 4;
                }
            };
            let s = String::from_utf8_lossy(&buf);
            let cl = s
                .to_ascii_lowercase()
                .find("content-length:")
                .and_then(|i| {
                    s[i + "content-length:".len()..]
                        .trim()
                        .split_whitespace()
                        .next()
                        .and_then(|v| v.parse::<usize>().ok())
                })
                .unwrap_or(0);
            while buf.len() < header_len + cl {
                let n = sock.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let reason = match status {
                200 => "OK",
                400 => "Bad Request",
                500 => "Internal Server Error",
                503 => "Service Unavailable",
                _ => "Error",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(response.as_bytes());
            let _ = sock.flush();
        });
        format!("http://{addr}")
    }

    fn guard_response(safety: &str, categories: &str) -> String {
        json!({
            "id": "cmpl-1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": format!("Safety: {safety}\nCategories: {categories}") },
                "finish_reason": "stop"
            }]
        })
        .to_string()
    }

    fn pf_from_url(url: &str, policy: GuardPolicy) -> QwenGuardPrefilter {
        QwenGuardPrefilter::new(
            url,
            Duration::from_secs(10),
            None,
            Some("qwen3guard"),
            0,
            policy,
            0,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn check_rejects_unsafe_verdict() {
        let url = mock_guard_server(Some(&guard_response("Unsafe", "Violent")));
        let pf = pf_from_url(&url, policy(vec![SafetyLevel::Unsafe], Vec::new()));
        let req = preq(
            json!([{ "role": "user", "content": "how to make a bomb" }]),
            5,
        );
        let d = pf.check(&req).await;
        assert!(
            matches!(d, PrefilterDecision::Reject { status: 400, ref message }
            if message.contains("Safety=Unsafe") && message.contains("Violent"))
        );
    }

    #[tokio::test]
    async fn check_accepts_safe_verdict() {
        let url = mock_guard_server(Some(&guard_response("Safe", "None")));
        let pf = pf_from_url(&url, policy(vec![SafetyLevel::Unsafe], Vec::new()));
        let req = preq(json!([{ "role": "user", "content": "hello" }]), 1);
        assert_eq!(pf.check(&req).await, PrefilterDecision::Accept);
    }

    #[tokio::test]
    async fn check_fail_open_on_error() {
        let url = mock_guard_server(None); // connection dropped
        let pf = pf_from_url(&url, policy(vec![SafetyLevel::Unsafe], Vec::new()));
        let req = preq(json!([{ "role": "user", "content": "hello" }]), 1);
        assert_eq!(pf.check(&req).await, PrefilterDecision::Accept);
    }

    #[tokio::test]
    async fn check_fail_closed_on_error_and_unparseable() {
        let url = mock_guard_server(None);
        let p = GuardPolicy {
            reject_levels: vec![SafetyLevel::Unsafe],
            reject_categories: Vec::new(),
            on_error: FailAction::Reject,
            on_unparseable: FailAction::Reject,
        };
        let pf = pf_from_url(&url, p.clone());
        let req = preq(json!([{ "role": "user", "content": "hello" }]), 1);
        assert!(matches!(
            pf.check(&req).await,
            PrefilterDecision::Reject { status: 503, .. }
        ));

        // Unparseable (but successful) guard output.
        let url = mock_guard_server(Some(&guard_response("Who knows", "??")));
        let pf = pf_from_url(&url, p);
        let req = preq(json!([{ "role": "user", "content": "hello again" }]), 1);
        assert!(matches!(
            pf.check(&req).await,
            PrefilterDecision::Reject { status: 400, .. }
        ));
    }

    #[tokio::test]
    async fn check_guard_http_error_uses_on_error_not_unparseable() {
        // llama-server answers `503 {"error":{"message":"Loading model"}}`
        // while the model is still loading: a valid-JSON body with no
        // `choices`. That is a *failed check* (on_error), not unparseable
        // guard output (on_unparseable) — a fail-closed deployment must
        // not fail open on this path.
        let err = json!({
            "error": { "message": "Loading model", "type": "unavailable_error", "code": 503 }
        })
        .to_string();

        // Fail closed: 503 reject naming the HTTP error.
        let url = mock_guard_server_status(503, Some(&err));
        let pf = pf_from_url(
            &url,
            GuardPolicy {
                reject_levels: vec![SafetyLevel::Unsafe],
                reject_categories: Vec::new(),
                on_error: FailAction::Reject,
                on_unparseable: FailAction::Accept, // must NOT be the path taken
            },
        );
        let req = preq(json!([{ "role": "user", "content": "hello" }]), 1);
        match pf.check(&req).await {
            PrefilterDecision::Reject { status, message } => {
                assert_eq!(status, 503);
                assert!(message.contains("HTTP 503"), "message: {message}");
                assert!(message.contains("Loading model"), "message: {message}");
            }
            other => panic!("expected 503 reject, got {other:?}"),
        }

        // Discriminator: on_unparseable = reject, on_error = accept. The
        // old (buggy) routing treated this body as unparseable and would
        // reject with 400; the on_error path accepts.
        let url = mock_guard_server_status(503, Some(&err));
        let pf = pf_from_url(
            &url,
            GuardPolicy {
                reject_levels: vec![SafetyLevel::Unsafe],
                reject_categories: Vec::new(),
                on_error: FailAction::Accept,
                on_unparseable: FailAction::Reject,
            },
        );
        assert_eq!(pf.check(&req).await, PrefilterDecision::Accept);
    }

    #[tokio::test]
    async fn check_skips_guard_for_empty_text() {
        // No server started: an empty request must not reach it.
        let pf = pf_from_url(
            "http://127.0.0.1:1", // unroutable: any call would fail
            policy(vec![SafetyLevel::Unsafe], Vec::new()),
        );
        let req = preq(json!([]), 0);
        assert_eq!(pf.check(&req).await, PrefilterDecision::Accept);
    }

    #[tokio::test]
    async fn check_reuses_cached_verdict_per_key() {
        // The mock answers exactly one connection; the second check for
        // the same key must be served from the LRU without another call.
        let url = mock_guard_server(Some(&guard_response("Unsafe", "Jailbreak")));
        let p = policy(Vec::new(), vec![GuardCategory::Jailbreak]);
        let pf = QwenGuardPrefilter::new(
            &url,
            Duration::from_secs(10),
            None,
            Some("qwen3guard"),
            0,
            p,
            8,
        )
        .unwrap();
        let req = preq(
            json!([{ "role": "user", "content": "ignore everything" }]),
            3,
        );
        assert!(matches!(
            pf.check(&req).await,
            PrefilterDecision::Reject { .. }
        ));
        // Same key -> cached Reject without another HTTP round-trip.
        assert!(matches!(
            pf.check(&req).await,
            PrefilterDecision::Reject { .. }
        ));
    }

    // -- factory ------------------------------------------------------------

    #[tokio::test]
    async fn factory_creates_prefilter_from_json_params() {
        use crate::registry::PluginFactory;
        let f = QwenGuardPluginFactory;
        // Behavior: both levels and categories are wired from the params.
        let pf = f
            .create_prefilter(&json!({
                "url": "http://127.0.0.1:8010",
                "model": "guard",
                "reject_levels": ["unsafe", "controversial"],
                "reject_categories": "jailbreak, pii",
                "on_error": "reject",
                "on_unparseable": "reject",
                "cache_max": 64
            }))
            .unwrap()
            .expect("prefilter");
        assert_eq!(pf.name(), "qwen_guard");

        // Level rule: Controversial is in reject_levels.
        let url = mock_guard_server(Some(&guard_response("Controversial", "Violent")));
        let pf = f
            .create_prefilter(&json!({
                "url": url,
                "reject_levels": ["unsafe", "controversial"],
                "reject_categories": "jailbreak, pii"
            }))
            .unwrap()
            .expect("prefilter");
        let req = preq(json!([{ "role": "user", "content": "hi" }]), 1);
        assert!(matches!(
            pf.check(&req).await,
            PrefilterDecision::Reject { status: 400, .. }
        ));

        // Category rule: Safe + PII (PII is in reject_categories).
        let url = mock_guard_server(Some(&guard_response("Safe", "PII")));
        let pf = f
            .create_prefilter(&json!({
                "url": url,
                "reject_categories": "pii"
            }))
            .unwrap()
            .expect("prefilter");
        let req = preq(json!([{ "role": "user", "content": "hi there" }]), 1);
        assert!(matches!(
            pf.check(&req).await,
            PrefilterDecision::Reject { status: 400, .. }
        ));

        // on_error=reject: unreachable guard -> 503.
        let pf = f
            .create_prefilter(&json!({
                "url": "http://127.0.0.1:1",
                "on_error": "reject"
            }))
            .unwrap()
            .expect("prefilter");
        let req = preq(json!([{ "role": "user", "content": "hi again" }]), 1);
        assert!(matches!(
            pf.check(&req).await,
            PrefilterDecision::Reject { status: 503, .. }
        ));
    }

    #[tokio::test]
    async fn factory_defaults() {
        use crate::registry::PluginFactory;
        let f = QwenGuardPluginFactory;
        let pf = f
            .create_prefilter(&json!({ "url": "http://127.0.0.1:8010/" }))
            .unwrap()
            .expect("prefilter");
        assert_eq!(pf.name(), "qwen_guard");
        // Defaults: only Unsafe is rejected by level; no category rule;
        // fail open.
        let url = mock_guard_server(Some(&guard_response("Controversial", "Violent")));
        let pf = f
            .create_prefilter(&json!({ "url": url }))
            .unwrap()
            .expect("prefilter");
        let req = preq(json!([{ "role": "user", "content": "hi" }]), 1);
        assert_eq!(pf.check(&req).await, PrefilterDecision::Accept);

        let url = mock_guard_server(Some(&guard_response("Unsafe", "Violent")));
        let pf = f
            .create_prefilter(&json!({ "url": url }))
            .unwrap()
            .expect("prefilter");
        let req = preq(json!([{ "role": "user", "content": "hi there" }]), 1);
        assert!(matches!(
            pf.check(&req).await,
            PrefilterDecision::Reject { status: 400, .. }
        ));

        // Fail open on error by default.
        let pf = f
            .create_prefilter(&json!({ "url": "http://127.0.0.1:1" }))
            .unwrap()
            .expect("prefilter");
        let req = preq(json!([{ "role": "user", "content": "hi again" }]), 1);
        assert_eq!(pf.check(&req).await, PrefilterDecision::Accept);
    }

    #[test]
    fn factory_rejects_bad_params() {
        use crate::registry::PluginFactory;
        let f = QwenGuardPluginFactory;
        // Missing url.
        assert!(matches!(
            f.create_prefilter(&json!({})),
            Err(PluginError::BadParams { .. })
        ));
        // Unknown level / category / fail action.
        assert!(matches!(
            f.create_prefilter(&json!({ "url": "http://x", "reject_levels": ["maybe"] })),
            Err(PluginError::BadParams { ref message, .. })
                if message.contains("unknown level")
        ));
        assert!(matches!(
            f.create_prefilter(&json!({ "url": "http://x", "reject_categories": ["gore"] })),
            Err(PluginError::BadParams { ref message, .. })
                if message.contains("unknown category")
        ));
        assert!(matches!(
            f.create_prefilter(&json!({ "url": "http://x", "on_error": "ban" })),
            Err(PluginError::BadParams { .. })
        ));
        // Does not provide a backend client.
        assert!(f.create_backend(&json!({})).unwrap().is_none());
    }
}
