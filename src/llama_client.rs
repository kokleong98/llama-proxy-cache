//! HTTP client for llama.cpp (port of `llama_client.py`).
//!
//! - `chat_stream` / `chat_json`: POST `/v1/chat/completions`
//! - `save_slot` / `restore_slot`: POST `/slots/{id}?action=save|restore`
//!   with the file basename in the JSON body (`{"filename": ...}`)
//! - `get_model_id`: GET `/v1/models`
//! - The slot pin is duplicated in body root, `options` and query params.

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use std::pin::Pin;
use std::time::Duration;

/// Errors from backend HTTP calls.
#[derive(Debug)]
pub enum BackendError {
    /// Non-2xx status from the backend (body captured for diagnostics).
    HttpStatus { status: u16, body: String },
    /// Network / request-level error.
    Other(String),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::HttpStatus { status, body } => {
                let body: String = body.chars().take(512).collect();
                write!(f, "provider returned HTTP {status}: {body}")
            }
            BackendError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BackendError {}

/// Outcome of a non-streaming chat call.
#[derive(Debug)]
pub enum JsonChat {
    /// 200 + JSON object — normal response.
    Object(Value),
    /// 200 + valid JSON but not an object (array/scalar) -> proxy 502.
    NonObject,
    /// 200 but the body is not usable JSON -> proxy 200 with an error payload.
    NonJson { message: &'static str, raw: String },
}

/// A streaming chat response (raw SSE bytes).
pub struct StreamChat {
    pub status: u16,
    pub stream: Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>>,
}

/// Abstraction over a llama.cpp backend (implemented by [`LlamaClient`]).
#[async_trait]
pub trait LlamaBackend: Send + Sync {
    /// `Ok(false)` on HTTP 500 (some builds fail that way), `Err` on other
    /// non-2xx / network errors — mirrors `raise_for_status` after the 500 check.
    async fn save_slot(&self, slot_id: usize, basename: &str) -> Result<bool, BackendError>;
    /// `false` on any failure (non-200 or network error).
    ///
    /// Deliberate deviation: Python's `restore_slot` propagates network
    /// errors, which in `app.py` becomes an unhandled 500 that also leaks
    /// the slot lock (the slot is never released). Here we log and return
    /// `false` so the chat proceeds without the restore.
    async fn restore_slot(&self, slot_id: usize, basename: &str) -> bool;
    /// Model id reported by the backend; `"unknown"` on failure.
    async fn get_model_id(&self) -> String;
    async fn chat_stream(
        &self,
        body: &Value,
        slot_id: Option<usize>,
    ) -> Result<StreamChat, BackendError>;
    async fn chat_json(
        &self,
        body: &Value,
        slot_id: Option<usize>,
    ) -> Result<JsonChat, BackendError>;
}

/// reqwest-based client for one llama.cpp backend.
///
/// All calls are cancel-safe: dropping the returned future (request or
/// response stream) aborts the in-flight HTTP request to llama-server —
/// the TCP connection is closed and the backend frees its slot when it
/// notices the close. The backend has no cancel endpoint, so this is how
/// the proxy cancels a backend call when its own client goes away.
pub struct LlamaClient {
    base_url: String,
    /// When set, every request carries `Authorization: Bearer <key>`
    /// (llama-server's `--api-key`).
    api_key: Option<String>,
    client: reqwest::Client,
}

impl LlamaClient {
    /// `api_key` is optional: when `Some`, every request to the backend
    /// carries `Authorization: Bearer <key>` (llama-server's `--api-key`).
    pub fn new(
        base_url: &str,
        timeout: Duration,
        api_key: Option<&str>,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            // same keep-alive sizing as httpx.Limits(max_keepalive=20)
            .pool_max_idle_per_host(20)
            .build()?;
        let base_url = base_url.trim_end_matches('/').to_string();
        tracing::info!("client_init url={base_url} api_key={}", api_key.is_some());
        Ok(Self {
            base_url,
            api_key: api_key.map(str::to_string),
            client,
        })
    }

    /// Applies the configured `Authorization` header (no-op when unset).
    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => req.header(reqwest::header::AUTHORIZATION, format!("Bearer {key}")),
            None => req,
        }
    }

    /// Port of `_with_slot_id`: duplicate the slot pin in body root,
    /// `options` and query params.
    pub(crate) fn with_slot_id(
        body: &Value,
        slot_id: Option<usize>,
    ) -> (Value, Vec<(String, String)>) {
        let Some(sid) = slot_id else {
            return (body.clone(), Vec::new());
        };
        let mut new_body = body.clone();
        if let Some(obj) = new_body.as_object_mut() {
            obj.insert("_slot_id".into(), json!(sid));
            obj.insert("slot_id".into(), json!(sid));
            obj.insert("id_slot".into(), json!(sid));
        }
        let mut opts = new_body
            .get("options")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        opts.insert("slot_id".into(), json!(sid));
        opts.insert("id_slot".into(), json!(sid));
        new_body["options"] = Value::Object(opts);
        let s = sid.to_string();
        let query = vec![("slot_id".into(), s.clone()), ("id_slot".into(), s)];
        (new_body, query)
    }
}
#[async_trait]
impl LlamaBackend for LlamaClient {
    async fn save_slot(&self, slot_id: usize, basename: &str) -> Result<bool, BackendError> {
        let req = self.apply_auth(
            self.client
                .post(format!("{}/slots/{slot_id}", self.base_url))
                .query(&[("action", "save")])
                .json(&json!({ "filename": basename })),
        );
        let resp = req
            .send()
            .await
            .map_err(|e| BackendError::Other(e.to_string()))?;
        let status = resp.status().as_u16();
        if status == 500 {
            // JSON body with "filename" avoids 500 parse errors on some builds;
            // a real 500 here means save failed -> Ok(false) (same as Python).
            tracing::warn!(
                "save_slot_500 slot={slot_id} basename={}",
                &basename[..basename.len().min(16)]
            );
            return Ok(false);
        }
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(BackendError::HttpStatus { status, body });
        }
        Ok(true)
    }

    async fn restore_slot(&self, slot_id: usize, basename: &str) -> bool {
        let req = self.apply_auth(
            self.client
                .post(format!("{}/slots/{slot_id}", self.base_url))
                .query(&[("action", "restore")])
                .json(&json!({ "filename": basename })),
        );
        match req.send().await {
            Err(e) => {
                tracing::warn!(
                    "restore_slot_fail slot={slot_id} basename={} err={e}",
                    &basename[..basename.len().min(16)]
                );
                false
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status != 200 {
                    tracing::warn!(
                        "restore_slot_status={status} slot={slot_id} basename={}",
                        &basename[..basename.len().min(16)]
                    );
                    return false;
                }
                true
            }
        }
    }
    async fn get_model_id(&self) -> String {
        // Used only for internal caching keys; the proxy keeps answering
        // MODEL_ID from its own configuration externally.
        let req = self.apply_auth(self.client.get(format!("{}/v1/models", self.base_url)));
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("get_model_id_fail base_url={} err={e}", self.base_url);
                return "unknown".to_string();
            }
        };
        if !resp.status().is_success() {
            tracing::warn!(
                "get_model_id_fail base_url={} status={}",
                self.base_url,
                resp.status()
            );
            return "unknown".to_string();
        }
        match resp.json::<Value>().await {
            Ok(data) => {
                let models = data
                    .get("data")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mid = models
                    .iter()
                    .find_map(|m| m.get("id").and_then(Value::as_str))
                    .unwrap_or("unknown")
                    .to_string();
                tracing::debug!("get_model_id base_url={} id={mid}", self.base_url);
                mid
            }
            Err(e) => {
                tracing::warn!("get_model_id_fail base_url={} err={e}", self.base_url);
                "unknown".to_string()
            }
        }
    }

    async fn chat_stream(
        &self,
        body: &Value,
        slot_id: Option<usize>,
    ) -> Result<StreamChat, BackendError> {
        let (body2, query) = Self::with_slot_id(body, slot_id);
        let req = self.apply_auth(
            self.client
                .post(format!("{}/v1/chat/completions", self.base_url))
                .query(&query)
                .json(&body2),
        );
        let resp = req
            .send()
            .await
            .map_err(|e| BackendError::Other(e.to_string()))?;
        let status = resp.status().as_u16();
        Ok(StreamChat {
            status,
            stream: Box::pin(resp.bytes_stream().map(|r| r.map_err(|e| e.to_string()))),
        })
    }
    async fn chat_json(
        &self,
        body: &Value,
        slot_id: Option<usize>,
    ) -> Result<JsonChat, BackendError> {
        let (body2, query) = Self::with_slot_id(body, slot_id);
        let req = self.apply_auth(
            self.client
                .post(format!("{}/v1/chat/completions", self.base_url))
                .query(&query)
                .json(&body2),
        );
        let resp = req
            .send()
            .await
            .map_err(|e| BackendError::Other(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            // mirrors raise_for_status(): the app turns this into a 500
            let body = resp.text().await.unwrap_or_default();
            return Err(BackendError::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let text = resp
            .text()
            .await
            .map_err(|e| BackendError::Other(e.to_string()))?;
        if !ctype.contains("application/json") {
            tracing::error!(
                "non_stream_non_json content_type={ctype} raw_len={}",
                text.len()
            );
            return Ok(JsonChat::NonJson {
                message: "provider returned non-JSON",
                raw: text.chars().take(2048).collect(),
            });
        }
        match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.is_object() => Ok(JsonChat::Object(v)),
            Ok(_) => Ok(JsonChat::NonObject),
            Err(e) => {
                tracing::error!(
                    "non_stream_json_parse_error status={} raw_len={} err={e}",
                    status.as_u16(),
                    text.len()
                );
                Ok(JsonChat::NonJson {
                    message: "invalid json from provider",
                    raw: text.chars().take(2048).collect(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slot_pinning_none() {
        let body = json!({"model": "m", "options": {"temperature": 0.5}});
        let (b2, q) = LlamaClient::with_slot_id(&body, None);
        assert_eq!(b2, body);
        assert!(q.is_empty());
    }

    #[test]
    fn slot_pinning_duplicated_in_root_options_query() {
        let body = json!({"model": "m", "options": {"temperature": 0.5}});
        let (b2, q) = LlamaClient::with_slot_id(&body, Some(1));
        assert_eq!(b2["_slot_id"], 1);
        assert_eq!(b2["slot_id"], 1);
        assert_eq!(b2["id_slot"], 1);
        assert_eq!(b2["options"]["slot_id"], 1);
        assert_eq!(b2["options"]["id_slot"], 1);
        // existing fields preserved
        assert_eq!(b2["options"]["temperature"], 0.5);
        assert_eq!(b2["model"], "m");
        assert_eq!(
            q,
            vec![
                ("slot_id".to_string(), "1".to_string()),
                ("id_slot".to_string(), "1".to_string())
            ]
        );
    }

    #[test]
    fn slot_pinning_adds_options_when_missing() {
        let body = json!({"model": "m"});
        let (b2, _) = LlamaClient::with_slot_id(&body, Some(3));
        assert_eq!(b2["options"]["slot_id"], 3);
        assert_eq!(b2["options"]["id_slot"], 3);
        assert!(b2["options"].as_object().unwrap().len() == 2);
    }
}
