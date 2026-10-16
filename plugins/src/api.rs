//! Adapter API: the traits every lpcache plugin implements.
//!
//! Four adapter kinds exist:
//!
//! - [`Prefilter`] — request prefilter, consulted **before** any slot
//!   acquisition, KV-cache restore/save, or backend dispatch. Built-ins:
//!   `keyword`, `chain`, `result_cache` (see the `builtin` module).
//! - [`ResultStore`] — stores fresh non-streaming backend results (the
//!   `result_cache` plugin's backing cache implements it; the proxy stores
//!   a result after every successful non-streaming backend call).
//! - [`PostCleanup`] — post-serve cleanup for a cache hit (the
//!   `result_cache` plugin can run one after a cached result is read).
//! - [`LlamaBackend`] — one llama.cpp backend (chat stream/JSON, slot
//!   save/restore, model id); the built-in `llama` plugin provides the
//!   reqwest-based [`crate::LlamaClient`].
//!
//! All traits are object-safe and used behind `Arc<dyn …>` in the proxy's
//! `AppState`.

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use serde_json::Value;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Prefilter
// ---------------------------------------------------------------------------

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
#[derive(Debug, Clone, PartialEq)]
pub enum PrefilterDecision {
    /// Forward the request to the normal pipeline.
    Accept,
    /// Reject the request: the client receives a JSON error with this
    /// status and message. The request never reaches the llama-server
    /// backend — no slot is acquired, no cache is restored or saved, no
    /// meta file is written, and (because the check runs before
    /// coalescing) the request never leads or joins a coalesced group.
    Reject { status: u16, message: String },
    /// Serve the client with a previously cached backend result (a full
    /// JSON response with its original status code) — same short-circuit
    /// semantics as [`PrefilterDecision::Reject`], but with a successful
    /// payload instead of an error.
    Serve { status: u16, body: Value },
}

/// A prefilter adapter: an optional short-circuit consulted before any
/// slot/backend work (see the module docs).
///
/// Implementations must be `Send + Sync` — they are shared across all
/// requests behind an `Arc` (see `AppState::prefilter` in the `lpcache`
/// crate).
///
/// ```ignore
/// #[async_trait::async_trait]
/// impl Prefilter for MyFilter {
///     fn name(&self) -> &str { "my_filter" }
///     async fn check(&self, req: &PrefilterRequest) -> PrefilterDecision {
///         if req.n_words > 1000 {
///             return PrefilterDecision::Reject {
///                 status: 413,
///                 message: "prompt too long".to_string(),
///             };
///         }
///         PrefilterDecision::Accept
///     }
/// }
/// ```
#[async_trait]
pub trait Prefilter: Send + Sync {
    /// Short identifier used in log lines (`prefilter_reject filter=…`).
    fn name(&self) -> &str;

    /// Inspect the request and decide: accept (normal pipeline), reject
    /// (immediate JSON error) or serve (immediate cached result).
    async fn check(&self, req: &PrefilterRequest) -> PrefilterDecision;

    /// When this adapter (or a member of it, for chains) is backed by a
    /// result store, the store the proxy should use to persist fresh
    /// non-streaming backend results — `None` for adapters that don't
    /// store results (the default).
    fn result_store(&self) -> Option<Arc<dyn ResultStore>> {
        None
    }
}

// ---------------------------------------------------------------------------
// Result store / post-serve cleanup
// ---------------------------------------------------------------------------

/// Storage of fresh non-streaming backend results, keyed by the request's
/// KV cache key (sha256 hex).
///
/// The proxy calls [`ResultStore::prepare`] once after the plugins are
/// loaded and [`ResultStore::store`] after every successful non-streaming
/// backend call; a prefilter plugin backed by the same store (see
/// [`Prefilter::result_store`]) answers fresh same-key requests from it
/// before any slot/backend work.
pub trait ResultStore: Send + Sync {
    /// Prepare the store for use (e.g. create its backing directory) so a
    /// store failure is visible at startup instead of on the first write.
    /// The proxy calls this once after loading the plugins; a failure is
    /// logged and does not prevent startup. The default is a no-op for
    /// stores that need no preparation.
    fn prepare(&self) -> Result<(), String> {
        Ok(())
    }

    /// Store (or refresh) the result for `key`. Failures are logged by the
    /// proxy; they never affect the in-flight response.
    fn store(&self, key: &str, status: u16, body: &Value) -> Result<(), String>;
}

/// Post-serve cleanup adapter: invoked after the proxy has read a cached
/// result to answer a request (the entry bytes are already in memory, so
/// cleanup may safely remove the file before the response reaches the
/// client). Implementations must be `Send + Sync` — they are shared across
/// all requests behind an `Arc`.
#[async_trait]
pub trait PostCleanup: Send + Sync {
    /// Short identifier used in log lines.
    fn name(&self) -> &str;

    /// Run after a cached result for `key` was served from `path`.
    /// Failures are logged by the adapter; they never affect the response.
    async fn cleanup(&self, key: &str, path: &Path);
}

// ---------------------------------------------------------------------------
// Llama backend
// ---------------------------------------------------------------------------

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

/// Outcome of a restore attempt.
///
/// Python only distinguished "failed"; the `Rejected` (HTTP 400) case is
/// split out so the proxy can tell a *useless KV file* (missing, corrupt,
/// or too big for the slot's context) from a *transient* failure and clean
/// up the stale meta entry (see `lpcache::app::remove_stale_meta`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// The backend loaded the KV file into the slot (HTTP 200).
    Restored,
    /// The backend rejected the file (HTTP 400): it could not be loaded
    /// on that backend (missing, corrupt, or no available space in the
    /// slot's KV cache).
    Rejected,
    /// Any other failure (non-2xx other than 400, or network error) —
    /// treat as transient.
    Failed,
}

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

/// Abstraction over a llama.cpp backend (implemented by the built-in
/// `llama` plugin — `LlamaClient`).
#[async_trait]
pub trait LlamaBackend: Send + Sync {
    /// `Ok(false)` on HTTP 500 (some builds fail that way), `Err` on other
    /// non-2xx / network errors — mirrors `raise_for_status` after the 500 check.
    async fn save_slot(&self, slot_id: usize, basename: &str) -> Result<bool, BackendError>;
    /// Restore a saved KV file into the slot.
    ///
    /// `Rejected` on HTTP 400 (the backend could not load the file),
    /// `Failed` on any other non-2xx or network error.
    ///
    /// Deliberate deviation: Python's `restore_slot` propagates network
    /// errors, which in `app.py` becomes an unhandled 500 that also leaks
    /// the slot lock (the slot is never released). Here we log and return
    /// a non-`Restored` outcome so the chat proceeds without the restore.
    async fn restore_slot(&self, slot_id: usize, basename: &str) -> RestoreOutcome;
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
