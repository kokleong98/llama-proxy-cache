//! Adapter API: the traits every lpcache plugin implements.
//!
//! Six adapter kinds exist:
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
//! - [`RequestLogger`] — one W3C Extended Log Format (XLF) record per
//!   finished request; the built-in `request_log` plugin writes the
//!   records to a (size-rotated) log file.
//! - [`Dashboard`] — the real-time statistics web dashboard; the proxy
//!   feeds it one [`RequestLogRecord`] per finished request and hands it
//!   a live [`ProxyStatus`] snapshot on every stats poll, and serves its
//!   HTML page + JSON stats endpoint on the paths it declares. The
//!   built-in `dashboard` plugin provides it.
//!
//! All traits are object-safe and used behind `Arc<dyn …>` in the proxy's
//! `AppState`.

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use serde::Serialize;
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

// ---------------------------------------------------------------------------
// RequestLogger
// ---------------------------------------------------------------------------

/// Format a unix timestamp as the request log's W3C-XLF datetime
/// (`%Y-%m-%dT%H:%M:%S.%fZ` — UTC, millisecond precision; the `#Date-format`
/// declared by the built-in `request_log` plugin).
pub fn format_utc(secs: u64, nanos: u32) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm): days since 1970-01-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    let millis = nanos / 1_000_000;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// One finished HTTP request, rendered as a single W3C Extended Log Format
/// (XLF) line by a [`RequestLogger`].
///
/// The W3C-style fields (`datetime`, `cs-method`, ...) describe the HTTP
/// transaction; the `lpcache-*` fields carry the KV-cache pipeline's view
/// of the request (filled in as far as the request got). `None` values
/// render as `-` ("not available") in the log file.
#[derive(Debug, Clone, Default)]
pub struct RequestLogRecord {
    // W3C XLF standard fields.
    /// `datetime`: request completion timestamp (UTC,
    /// `%Y-%m-%dT%H:%M:%S.%fZ`; see [`format_utc`]).
    pub datetime: Option<String>,
    /// `cs-method`: the HTTP method (`POST`, `GET`, ...).
    pub method: Option<String>,
    /// `cs-uri`: the request target (path + query string).
    pub uri: Option<String>,
    /// `c-ip`: the client address.
    pub client_ip: Option<String>,
    /// `cs(User-Agent)`: the `User-Agent` request header.
    pub user_agent: Option<String>,
    /// `cs-bytes`: the request body size in bytes (Content-Length; `-`
    /// when the client did not announce one).
    pub request_bytes: Option<u64>,
    /// `cs-time`: request duration in seconds, until the response body is
    /// complete (or the client went away early).
    pub duration_secs: Option<f64>,
    /// `sc-status`: the response status code.
    pub status: Option<u16>,
    /// `sc-bytes`: the response body size in bytes (the bytes actually
    /// handed to the client).
    pub response_bytes: Option<u64>,
    // lpcache pipeline fields.
    /// `lpcache-model`: the model the client asked for (the proxy model id
    /// when the body has none).
    pub model: Option<String>,
    /// `lpcache-stream`: `true`/`false` once the request body was parsed.
    pub stream: Option<bool>,
    /// `lpcache-key`: the request's KV cache key (first 16 hex chars).
    pub key: Option<String>,
    /// `lpcache-n-words`: word count of the concatenated message contents.
    pub n_words: Option<usize>,
    /// `lpcache-is-big`: `true` when the request is cache-eligible
    /// (more than `BIG_THRESHOLD_WORDS` words).
    pub is_big: Option<bool>,
    /// `lpcache-backend`: the backend index the request ran on.
    pub backend: Option<usize>,
    /// `lpcache-slot`: the slot id the request ran on.
    pub slot: Option<usize>,
    /// `lpcache-restored`: `true`/`false` when a KV cache restore was
    /// attempted (missing = no restore attempted, e.g. small requests).
    pub restored: Option<bool>,
    /// `lpcache-cache-hit`: `true` when a prefilter served the request
    /// (e.g. the result cache) without a backend call; `false` when a
    /// prefilter ran and let the request through (missing = no prefilter).
    pub cache_hit: Option<bool>,
    /// `lpcache-coalesced`: `leader`/`follower` when concurrent-request
    /// coalescing is enabled (missing = the request ran on its own).
    pub coalesced: Option<&'static str>,
    /// `lpcache-reason`: a short pipeline outcome for non-successful
    /// requests (`slot_timeout`, `backend_unreachable`, `provider_http_*`,
    /// `save_failed`, ...; missing = the request completed normally).
    pub reason: Option<String>,
    // lpcache performance fields (backend-reported token counts and
    // timings; missing when the backend did not report them — failure
    // paths, or backends that predate the `timings` object).
    /// `lpcache-prompt-tokens`: the prompt token count
    /// (`usage.prompt_tokens`).
    pub prompt_tokens: Option<u64>,
    /// `lpcache-completion-tokens`: the generated token count
    /// (`usage.completion_tokens`).
    pub completion_tokens: Option<u64>,
    /// `lpcache-cached-tokens`: prompt tokens served from the backend's KV
    /// cache (`usage.prompt_tokens_details.cached_tokens`, or
    /// `timings.cache_n`) — the visible effect of a KV restore.
    pub cached_tokens: Option<u64>,
    /// `lpcache-prefill-ms`: backend prompt-processing time in
    /// milliseconds (`timings.prompt_ms`).
    pub prefill_ms: Option<f64>,
    /// `lpcache-gen-ms`: backend generation time in milliseconds
    /// (`timings.predicted_ms`).
    pub gen_ms: Option<f64>,
    /// `lpcache-prefill-tps`: backend prompt tokens per second
    /// (`timings.prompt_per_second`).
    pub prefill_tps: Option<f64>,
    /// `lpcache-gen-tps`: backend generation tokens per second
    /// (`timings.predicted_per_second`).
    pub gen_tps: Option<f64>,
    /// `lpcache-ttft-ms`: proxy-side time to first token in milliseconds —
    /// from the start of stream processing until the first SSE chunk
    /// received from the backend (streaming requests only).
    pub ttft_ms: Option<f64>,
}

/// A request-log adapter: receives one [`RequestLogRecord`] per finished
/// request and renders it — the built-in `request_log` plugin writes one
/// XLF line per record to a file, rotating it when it reaches a max size
/// (see the `builtin` module).
///
/// Implementations must be `Send + Sync` — they are shared across all
/// requests behind an `Arc` (see `AppState::request_log` in the `lpcache`
/// crate). Failures are logged by the proxy and swallowed: request logging
/// never breaks request serving.
pub trait RequestLogger: Send + Sync {
    /// The plugin name (for diagnostics).
    fn name(&self) -> &str;

    /// Prepare the backing store (create the parent dir, open the current
    /// file, write the XLF header for a new file). Called once at startup;
    /// [`Self::log`] also works when `prepare` was never called (the file
    /// is opened lazily).
    fn prepare(&self) -> Result<(), String>;

    /// Record one finished request.
    fn log(&self, record: &RequestLogRecord) -> Result<(), String>;
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

/// Live state of one proxy slot (the dashboard's slot grid).
#[derive(Debug, Clone, Serialize)]
pub struct SlotStatus {
    /// Backend index the slot belongs to.
    pub be: usize,
    /// Local slot id on that backend.
    pub slot: usize,
    /// `true` while a request holds the slot.
    pub busy: bool,
    /// Seconds since the slot was last used (`None` = never used).
    pub last_used_secs: Option<f64>,
}

/// Circuit-breaker state of one backend (index = backend index).
#[derive(Debug, Default, Clone, Serialize)]
pub struct BackendBreaker {
    /// `true` while the breaker excludes the backend from slot selection.
    pub down: bool,
    /// Seconds remaining in the cooldown (`0.0` = healthy).
    pub cooldown_secs: f64,
    /// Consecutive connection-failure count (drives the escalating
    /// cooldown; reset by any success).
    pub failure_streak: u32,
}

/// Live state of one backend (the dashboard's backend table).
#[derive(Debug, Clone, Serialize)]
pub struct BackendStatus {
    /// The backend URL (from `BACKENDS`).
    pub url: String,
    /// Configured slot count for this backend.
    pub n_slots: usize,
    /// Slots currently held by in-flight requests.
    pub busy_slots: usize,
    /// `true` while the circuit breaker excludes the backend.
    pub down: bool,
    /// Seconds remaining in the breaker cooldown (`0.0` = healthy).
    pub cooldown_secs: f64,
    /// Consecutive connection-failure count (reset by any success).
    pub failure_streak: u32,
}

/// Snapshot of the proxy's live state, handed to a [`Dashboard`] plugin on
/// every stats request. The proxy fills this in (it knows the slot
/// manager, the coalescing table and the config); the plugin merges it
/// with its own per-request aggregates into the JSON the dashboard polls.
#[derive(Debug, Clone, Serialize)]
pub struct ProxyStatus {
    /// The proxy version string.
    pub version: String,
    /// Seconds since the proxy started.
    pub uptime_secs: f64,
    /// The proxy model id (`MODEL_ID`).
    pub model_id: String,
    /// The proxy listen port.
    pub port: u16,
    /// Whether concurrent-request coalescing is enabled.
    pub coalesce_requests: bool,
    /// The big-request word threshold (`BIG_THRESHOLD_WORDS`).
    pub big_threshold_words: usize,
    /// KV cache words per block (`WORDS_PER_BLOCK`).
    pub words_per_block: usize,
    /// LRU bound on meta files (`META_MAX`; `0` = unlimited).
    pub meta_max: usize,
    /// KV meta files currently on disk (`META_DIR`).
    pub meta_files: usize,
    /// Slots currently held by in-flight requests.
    pub in_flight: usize,
    /// Total slots across all backends.
    pub total_slots: usize,
    /// Active coalesced request groups (`0` when coalescing is disabled).
    pub active_groups: usize,
    /// One entry per backend (index = backend index).
    pub backends: Vec<BackendStatus>,
    /// One entry per slot across all backends.
    pub slots: Vec<SlotStatus>,
}

/// A dashboard adapter: serves the proxy's real-time statistics as a web
/// page. The proxy mounts two routes for a loaded dashboard plugin —
/// `GET {path}` (the page) and `GET {path}/api/stats` (the JSON payload
/// produced by [`Self::stats`]) — and feeds it one
/// [`RequestLogRecord`] per finished request through [`Self::record`]
/// (the same records the request logger receives). The dashboard's own
/// two routes are excluded: the proxy neither logs nor feeds its page
/// and stats requests, so the page's polling never inflates the
/// aggregates.
///
/// The built-in `dashboard` plugin keeps request counters, throughput
/// time series and a recent-requests ring in memory and renders a
/// self-contained HTML page (no external assets) that polls the stats
/// endpoint.
///
/// Implementations must be `Send + Sync` — they are shared across all
/// requests behind an `Arc` (see `AppState::dashboard` in the `lpcache`
/// crate).
pub trait Dashboard: Send + Sync {
    /// The plugin name (for diagnostics).
    fn name(&self) -> &str;

    /// The URL path the proxy serves the dashboard page on (e.g.
    /// `/dashboard`; must start with `/`). The stats API is mounted at
    /// `{path}/api/stats` automatically.
    fn path(&self) -> &str;

    /// The URL path of the stats JSON endpoint: `{path}/api/stats`
    /// (trailing slashes on `path` are dropped, so it always matches the
    /// route the proxy mounts).
    fn stats_path(&self) -> String {
        format!("{}/api/stats", self.path().trim_end_matches('/'))
    }

    /// The dashboard page: a complete, self-contained HTML document (CSS
    /// and JS inline, no external assets) served as `text/html`.
    fn html(&self) -> String;

    /// The current stats payload, served as JSON from `{path}/api/stats`,
    /// optionally range-scoped by `since` (unix seconds): the proxy's
    /// live [`ProxyStatus`] merged with the plugin's aggregates over
    /// `[since, now]`. `None` selects the plugin's default aggregation
    /// (the built-in dashboard: lifetime counters plus the trailing
    /// `window_secs` series). The built-in dashboard's page polls with
    /// `?since=<unix seconds>` so its time filter can scope the KPIs,
    /// charts and recent-requests table to the selected range.
    fn stats(&self, proxy: &ProxyStatus, since: Option<u64>) -> Value;

    /// Feed one finished request into the plugin's aggregates. Called
    /// (synchronously) exactly once per finished request — success,
    /// error, rejection or client disconnect alike — but never for the
    /// dashboard's own page/stats routes. The default is a no-op for
    /// dashboards that only report live proxy state.
    fn record(&self, record: &RequestLogRecord) -> Result<(), String> {
        let _ = record;
        Ok(())
    }
}
