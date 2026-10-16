//! `keyword` plugin — rejects requests whose message contents contain a
//! blocked keyword (plain substring match, case-insensitive by default).
//!
//! Params:
//! - `keywords` (required): array of keyword strings, or a
//!   comma-separated string (`"a, b"`).
//! - `case_insensitive` (default `true`).
//!
//! Rejections are `400` with a message naming the first matching keyword.
//! An empty keyword list yields no plugin (a config entry is only
//! synthesized by `lpcache` when a non-empty list is configured).

use crate::api::{Prefilter, PrefilterDecision, PrefilterRequest};
use crate::registry::{PluginError, PluginFactory};
use async_trait::async_trait;
use serde::Deserialize;
use serde::de::Error as _;
use serde_json::Value;
use std::sync::Arc;

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

// ---------------------------------------------------------------------------
// Plugin factory (configurable, dynamically loadable)
// ---------------------------------------------------------------------------

const NAME: &str = "keyword";

#[derive(Debug, Deserialize)]
struct KeywordParams {
    #[serde(default, deserialize_with = "keywords_from_any")]
    keywords: Vec<String>,
    #[serde(default = "default_true")]
    case_insensitive: bool,
}

fn default_true() -> bool {
    true
}

/// `keywords` accepts either a JSON array of strings or a
/// comma-separated string.
fn keywords_from_any<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let v = Value::deserialize(d)?;
    match v {
        Value::Array(items) => items
            .into_iter()
            .map(|i| {
                i.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| D::Error::custom("`keywords` array items must be strings"))
            })
            .collect(),
        Value::String(s) => Ok(s
            .split(',')
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect()),
        other => Err(D::Error::custom(format!(
            "`keywords` must be an array of strings or a comma-separated string, got {other:?}"
        ))),
    }
}

/// Factory for the `keyword` plugin (registered by
/// [`crate::builtin::register`]).
pub struct KeywordPluginFactory;

impl PluginFactory for KeywordPluginFactory {
    fn create_prefilter(&self, params: &Value) -> Result<Option<Arc<dyn Prefilter>>, PluginError> {
        let KeywordParams {
            keywords,
            case_insensitive,
        } = KeywordParams::deserialize(params).map_err(|e| PluginError::BadParams {
            name: NAME.to_string(),
            message: e.to_string(),
        })?;
        let Some(pf) = KeywordPrefilter::from_comma_list(&keywords.join(","), case_insensitive)
        else {
            return Err(PluginError::BadParams {
                name: NAME.to_string(),
                message: "`keywords` must be a non-empty list".to_string(),
            });
        };
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

    #[tokio::test]
    async fn factory_creates_prefilter_from_json_params() {
        use crate::registry::PluginFactory;
        let f = KeywordPluginFactory;
        let pf = f
            .create_prefilter(&json!({
                "keywords": ["a", "b"],
                "case_insensitive": false
            }))
            .unwrap()
            .expect("prefilter");
        assert_eq!(pf.name(), "keyword");
        // Comma-string form and default case_insensitive=true.
        let pf = f
            .create_prefilter(&json!({ "keywords": "a, b" }))
            .unwrap()
            .expect("prefilter");
        assert!(matches!(
            pf.check(&preq(json!([{ "role": "user", "content": "A here" }]), 1))
                .await,
            PrefilterDecision::Reject { .. }
        ));
    }

    #[test]
    fn factory_rejects_empty_or_missing_keywords() {
        use crate::registry::PluginFactory;
        let f = KeywordPluginFactory;
        assert!(matches!(
            f.create_prefilter(&json!({})),
            Err(PluginError::BadParams { .. })
        ));
        assert!(matches!(
            f.create_prefilter(&json!({ "keywords": [] })),
            Err(PluginError::BadParams { .. })
        ));
        assert!(matches!(
            f.create_prefilter(&json!({ "keywords": 42 })),
            Err(PluginError::BadParams { .. })
        ));
        // Does not provide a backend client.
        assert!(f.create_backend(&json!({})).unwrap().is_none());
    }
}
