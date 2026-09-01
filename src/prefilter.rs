//! Request prefilter (adapter).
//!
//! A [`Prefilter`] is an adapter the proxy consults **before** any slot
//! acquisition, KV-cache restore/save, or backend dispatch: it inspects
//! the parsed chat-completion request ([`PrefilterRequest`]) and returns
//! a [`PrefilterDecision`] —
//! - `Accept`: run the normal pipeline (coalescing, slot acquire,
//!   backend call, save/meta);
//! - `Reject { status, message }`: answer the client immediately with a
//!   JSON error. The request never reaches the llama-server backend —
//!   no slot is acquired, no cache is restored or saved, no meta file is
//!   written, and (because the check runs before coalescing) the request
//!   never leads or joins a coalesced group.
//!
//! Built-in adapter: [`KeywordPrefilter`] — rejects requests whose
//! message contents contain any blocked keyword (plain substring match,
//! case-insensitive by default). Enabled via `PREFILTER_BLOCKLIST` /
//! `--prefilter-blocklist` (see the `config` module); when the list is
//! empty the prefilter is disabled and there is no per-request cost.
//!
//! Custom adapters implement the [`Prefilter`] trait and are wired into
//! `AppState::prefilter` (programmatic use, e.g. rate limiting, model
//! allow-lists, or tests):
//!
//! ```ignore
//! #[async_trait::async_trait]
//! impl Prefilter for MyFilter {
//!     fn name(&self) -> &str { "my_filter" }
//!     async fn check(&self, req: &PrefilterRequest) -> PrefilterDecision {
//!         if req.n_words > 1000 {
//!             return PrefilterDecision::Reject {
//!                 status: 413,
//!                 message: "prompt too long".to_string(),
//!             };
//!         }
//!         PrefilterDecision::Accept
//!     }
//! }
//! ```

use async_trait::async_trait;
use serde_json::Value;

/// Snapshot of everything the proxy knows about a request at prefilter
/// time: after body parsing and cache-key computation, before slot
/// acquisition or backend dispatch.
#[derive(Debug, Clone)]
pub struct PrefilterRequest {
    /// The full parsed request body (a JSON object).
    pub body: Value,
    /// The `messages` array (`[]` when the body has none).
    pub messages: Value,
    /// The model the client asked for (the proxy model id when absent).
    pub model: String,
    /// The backend model id used in the cache key.
    pub backend_model_id: String,
    /// `true` when the client asked for a streamed response.
    pub stream: bool,
    /// The request's KV cache key (sha256 hex).
    pub key: String,
    /// Total word count of the concatenated message contents.
    pub n_words: usize,
    /// `true` when `n_words > BIG_THRESHOLD_WORDS` (cache-eligible).
    pub is_big: bool,
}

/// Outcome of a prefilter check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrefilterDecision {
    /// Forward the request to the normal pipeline.
    Accept,
    /// Reject the request: the client receives a JSON error with this
    /// status code and message; the request never reaches the backend.
    Reject { status: u16, message: String },
}

/// Adapter trait for accept/reject checks that run before any
/// slot/backend work. Implementations must be `Send + Sync` — they are
/// shared across all requests behind an `Arc`.
#[async_trait]
pub trait Prefilter: Send + Sync {
    /// Short identifier used in log lines.
    fn name(&self) -> &str;

    /// Inspect the request: return [`PrefilterDecision::Accept`] to
    /// forward it, or [`PrefilterDecision::Reject`] to answer the client
    /// directly.
    async fn check(&self, req: &PrefilterRequest) -> PrefilterDecision;
}

/// Flatten all message contents into one searchable text.
///
/// Handles both plain-string `content` and OpenAI content-part arrays
/// (`[{"type": "text", "text": "..."}]`); parts without a text payload
/// (images, tool calls, ...) are skipped, as are messages without
/// `content`. Pieces are joined with newlines.
pub fn message_text(messages: &Value) -> String {
    let mut out = String::new();
    for msg in messages.as_array().into_iter().flatten() {
        let Some(content) = msg.get("content") else {
            continue;
        };
        match content {
            Value::String(s) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(s);
            }
            Value::Array(parts) => {
                for part in parts {
                    let Some(text) = (match part {
                        Value::String(s) => Some(s.as_str()),
                        Value::Object(o) => o.get("text").and_then(Value::as_str),
                        _ => None,
                    }) else {
                        continue;
                    };
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
            // Non-string, non-array content: nothing textual to match.
            _ => {}
        }
    }
    out
}

/// Built-in adapter: rejects a request when the combined message text
/// contains any blocked keyword (plain substring match).
///
/// - `case_insensitive = true` (default): keywords and text are matched
///   lowercased; `false`: exact case.
/// - Empty/whitespace keywords are dropped at construction.
/// - Rejections are `400` with a message naming the first matching
///   keyword.
#[derive(Debug, Clone)]
pub struct KeywordPrefilter {
    /// Keywords, already trimmed (and lowercased when `case_insensitive`).
    keywords: Vec<String>,
    case_insensitive: bool,
}

impl KeywordPrefilter {
    /// Build from an iterator of keyword strings.
    pub fn new(
        keywords: impl IntoIterator<Item = impl Into<String>>,
        case_insensitive: bool,
    ) -> Self {
        let keywords = keywords
            .into_iter()
            .map(Into::into)
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .map(|k| {
                if case_insensitive {
                    k.to_lowercase()
                } else {
                    k
                }
            })
            .collect();
        Self {
            keywords,
            case_insensitive,
        }
    }

    /// Parse a comma-separated keyword list (the `PREFILTER_BLOCKLIST`
    /// form); whitespace around entries is trimmed, empty entries are
    /// dropped. `None` when nothing usable remains (prefilter disabled).
    pub fn from_comma_list(raw: &str, case_insensitive: bool) -> Option<Self> {
        let keywords: Vec<String> = raw
            .split(',')
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect();
        if keywords.is_empty() {
            None
        } else {
            Some(Self::new(keywords, case_insensitive))
        }
    }

    /// The first blocked keyword contained in `text` (after
    /// normalization), if any.
    fn first_match(&self, text: &str) -> Option<&str> {
        if self.case_insensitive {
            let hay = text.to_lowercase();
            self.keywords
                .iter()
                .find(|k| hay.contains(k.as_str()))
                .map(|k| k.as_str())
        } else {
            self.keywords
                .iter()
                .find(|k| text.contains(k.as_str()))
                .map(|k| k.as_str())
        }
    }

    /// The configured keywords (already normalized).
    pub fn keywords(&self) -> &[String] {
        &self.keywords
    }
}

#[async_trait]
impl Prefilter for KeywordPrefilter {
    fn name(&self) -> &str {
        "keyword"
    }

    async fn check(&self, req: &PrefilterRequest) -> PrefilterDecision {
        let text = message_text(&req.messages);
        match self.first_match(&text) {
            Some(kw) => PrefilterDecision::Reject {
                status: 400,
                message: format!("request blocked by keyword prefilter: {kw:?}"),
            },
            None => PrefilterDecision::Accept,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

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

    #[tokio::test]
    async fn keyword_rejects_on_match() {
        let pf = KeywordPrefilter::new(["forbidden", "banned"], true);
        let req = preq(
            json!([{ "role": "user", "content": "this is forbidden content" }]),
            5,
        );
        assert_eq!(
            pf.check(&req).await,
            PrefilterDecision::Reject {
                status: 400,
                message: "request blocked by keyword prefilter: \"forbidden\"".to_string()
            }
        );
    }

    #[tokio::test]
    async fn keyword_accepts_clean_request() {
        let pf = KeywordPrefilter::new(["forbidden"], true);
        let req = preq(json!([{ "role": "user", "content": "hello world" }]), 2);
        assert_eq!(pf.check(&req).await, PrefilterDecision::Accept);
    }

    #[tokio::test]
    async fn keyword_case_insensitive_by_default() {
        let pf = KeywordPrefilter::new(["ForBIDDEN"], true);
        let req = preq(
            json!([{ "role": "user", "content": "say FORBIDDEN please" }]),
            3,
        );
        assert!(matches!(
            pf.check(&req).await,
            PrefilterDecision::Reject { .. }
        ));
        // Case-sensitive mode: same text, no match.
        let strict = KeywordPrefilter::new(["ForBIDDEN"], false);
        assert_eq!(strict.check(&req).await, PrefilterDecision::Accept);
        let req_exact = preq(json!([{ "role": "user", "content": "ForBIDDEN" }]), 1);
        assert!(matches!(
            strict.check(&req_exact).await,
            PrefilterDecision::Reject { .. }
        ));
    }

    #[tokio::test]
    async fn keyword_scans_all_messages() {
        let pf = KeywordPrefilter::new(["banned"], true);
        let req = preq(
            json!([
                { "role": "system", "content": "be nice" },
                { "role": "user", "content": "hello" },
                { "role": "assistant", "content": "hi" },
                { "role": "user", "content": "now the banned word" },
            ]),
            7,
        );
        assert!(matches!(
            pf.check(&req).await,
            PrefilterDecision::Reject { .. }
        ));
    }

    #[tokio::test]
    async fn keyword_inspects_content_part_arrays() {
        let pf = KeywordPrefilter::new(["forbidden"], true);
        let req = preq(
            json!([{
                "role": "user",
                "content": [
                    { "type": "text", "text": "part one" },
                    { "type": "image_url", "image_url": "http://x" },
                    { "type": "text", "text": "part two FORBIDDEN" }
                ]
            }]),
            5,
        );
        assert!(matches!(
            pf.check(&req).await,
            PrefilterDecision::Reject { .. }
        ));
    }

    #[tokio::test]
    async fn keyword_empty_messages_accepts() {
        let pf = KeywordPrefilter::new(["forbidden"], true);
        assert_eq!(
            pf.check(&preq(json!([]), 0)).await,
            PrefilterDecision::Accept
        );
        // A message without content is skipped.
        assert_eq!(
            pf.check(&preq(json!([{ "role": "user" }]), 0)).await,
            PrefilterDecision::Accept
        );
    }

    #[test]
    fn message_text_flattens_content() {
        let msgs = json!([
            { "role": "user", "content": "hello" },
            { "role": "assistant" },
            { "role": "user", "content": [
                { "type": "text", "text": "world" },
                "raw part",
                { "type": "image_url", "image_url": "x" }
            ]}
        ]);
        assert_eq!(message_text(&msgs), "hello\nworld\nraw part");
        assert_eq!(message_text(&json!([])), "");
        // Non-array messages value (defensive): nothing to flatten.
        assert_eq!(message_text(&json!("oops")), "");
    }

    #[test]
    fn from_comma_list_trims_and_drops_empty() {
        let pf = KeywordPrefilter::from_comma_list("a, b ,c,,  ", true).expect("some");
        assert_eq!(
            pf.keywords(),
            &["a".to_string(), "b".to_string(), "c".to_string()]
        );
        // Whitespace-only / empty list -> disabled.
        assert!(KeywordPrefilter::from_comma_list("", true).is_none());
        assert!(KeywordPrefilter::from_comma_list("  , ,", true).is_none());
    }

    #[tokio::test]
    async fn trait_object_dispatch() {
        // The adapter is used behind `Arc<dyn Prefilter>` in AppState.
        let pf: Arc<dyn Prefilter> = Arc::new(KeywordPrefilter::new(["x"], true));
        assert_eq!(pf.name(), "keyword");
        let req = preq(json!([{ "role": "user", "content": "x" }]), 1);
        assert!(matches!(
            pf.check(&req).await,
            PrefilterDecision::Reject { .. }
        ));
    }
}
