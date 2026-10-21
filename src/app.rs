//! HTTP API (port of `app.py`).
//!
//! - `GET /v1/models` -> `{"data": [{"id": MODEL_ID}]}`
//! - `POST /v1/chat/completions`:
//!   - big requests (word count > BIG_THRESHOLD_WORDS): `cache_prompt=true`,
//!     restore candidate lookup, save + meta after completion
//!   - small requests: no restore, no save/meta on the non-stream path
//!   - stream: a background reader pumps raw SSE bytes through a bounded
//!     channel; in its "finally" it always saves + writes meta + releases
//!     the slot (same as the Python reader task). If the client drops its
//!     connection, the reader aborts the in-flight request to llama-server
//!     (drops the upstream stream, closing the TCP connection — the backend
//!     has no cancel endpoint) before running the cleanup
//!   - non-stream: the full response is read before it is sent; if the
//!     client drops its connection meanwhile, hyper drops the in-flight
//!     handler future, which drops the backend-client future and aborts the
//!     upstream request to llama-server the same way
//!   - 503 when no slot could be acquired within the 300 s timeout
//!
//! Request prefilter (optional): when a prefilter plugin is enabled
//! (e.g. the `keyword` and/or `result_cache` plugins from the
//! `lpcache-plugins` crate, configured via `PLUGINS` or the legacy
//! `PREFILTER_*` settings), every request is checked by it right after
//! body parsing / cache-key computation and **before** coalescing, slot
//! acquisition and backend dispatch: rejected requests get an immediate
//! JSON error, and cached results are served directly — in both cases the
//! request never reaches the llama-server backend (no slot, no
//! restore/save, no meta file, no coalescing group). Successful
//! non-streaming backend results are stored in the result store (when the
//! prefilter plugin is backed by one) after the backend call.
//!
//! Concurrent-request coalescing (`COALESCE_REQUESTS`): when enabled,
//! concurrent requests with the same cache key form one group regardless
//! of generation parameters; the first request ("leader") runs the
//! pipeline above and every other request ("follower") waits for the
//! leader and receives its result. Followers never touch slots, backends
//! or meta files. See the `coalesce` module.
//!
//! Request log (optional): when `REQUEST_LOG` is enabled (or a
//! `request_log` spec is present in `PLUGINS`), a middleware wraps every
//! request: the W3C fields (method, URI, client IP, user agent, request
//! bytes, duration, status) are captured by the middleware, and the
//! lpcache pipeline fields (model, stream, key, n_words, is_big, backend,
//! slot, restored, cache_hit, coalesced, reason) are attached by the
//! handlers through the response's extensions (`PipelineLog`). One W3C-XLF
//! record is written through the `RequestLogger` plugin when the response
//! body is done — completed, errored, or the client went away — and
//! logging failures are swallowed: logging never breaks request serving.
//! The built-in plugin size-rotates the log file.
//!
//! The same middleware feeds the loaded dashboard (when one is present)
//! for its aggregates. The dashboard's own routes (its page and
//! `{path}/api/stats`) are its internal polling, not proxy traffic — the
//! middleware skips them, so they are neither written to the request log
//! nor counted in the dashboard's own statistics.

use crate::coalesce::{Flight, SharedOutcome, SingleFlight};
use crate::config::Config;
use crate::hashing;
use crate::slot_manager::{AcquireTimeout, GSlot, SlotGuard, SlotManager};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Query, State};
use axum::http::{Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use futures::StreamExt;
use lpcache_plugins::{
    BackendError, BackendStatus, Dashboard, JsonChat, LlamaBackend, Prefilter, PrefilterDecision,
    PrefilterRequest, ProxyStatus, RequestLogRecord, RequestLogger, RestoreOutcome, ResultStore,
    format_utc,
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

/// Shared application state (Python: `app.state`).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub clients: Vec<Arc<dyn LlamaBackend>>,
    pub sm: Arc<SlotManager>,
    /// In-flight coalesced request groups (active when
    /// `config.coalesce_requests` is enabled).
    pub sf: Arc<SingleFlight>,
    /// Optional prefilter plugin consulted before any slot/backend work
    /// (`None` when no prefilter plugin is configured — the default; see
    /// the `lpcache_plugins` crate).
    pub prefilter: Option<Arc<dyn Prefilter>>,
    /// Optional result store (set when a prefilter plugin is backed by one,
    /// e.g. the `result_cache` plugin): fresh non-streaming backend
    /// results are stored here under the request's cache key, and the
    /// backing prefilter serves hits before any slot/backend work.
    pub result_store: Option<Arc<dyn ResultStore>>,
    /// Optional request logger (the built-in `request_log` plugin, loaded
    /// from `REQUEST_LOG` / the `PLUGINS` settings): one W3C-XLF record is
    /// appended per finished request (`None` = disabled — the default).
    pub request_log: Option<Arc<dyn RequestLogger>>,
    /// Optional dashboard plugin (the built-in `dashboard` plugin, loaded
    /// from the `PLUGINS` settings): serves the real-time statistics web
    /// page at its configured path and receives one
    /// [`RequestLogRecord`] per finished request for its aggregates —
    /// its own page/stats routes are excluded (`None` = disabled — the
    /// default).
    pub dashboard: Option<Arc<dyn Dashboard>>,
}

pub fn router(state: AppState) -> Router {
    let mut router = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat));
    // The dashboard plugin (when loaded) owns its routes: the page at its
    // configured `path` and the stats API at `{path}/api/stats`.
    if let Some(d) = &state.dashboard {
        router = router
            .route(d.path(), get(dashboard_page))
            .route(&d.stats_path(), get(dashboard_stats));
    }
    router
        // `from_fn_with_state` because the middleware extracts
        // `State<AppState>` (plain `from_fn` has no state).
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            request_log_middleware,
        ))
        .with_state(state)
}

async fn models(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(json!({ "data": [{ "id": state.config.model_id }] }))
}

/// Process start (for the dashboard's `uptime_secs`).
fn process_start() -> &'static Instant {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now)
}

/// The dashboard page route (mounted only when a dashboard plugin is
/// loaded).
async fn dashboard_page(State(state): State<AppState>) -> Response {
    let d = state
        .dashboard
        .as_ref()
        .expect("dashboard page route only mounted when a dashboard is loaded");
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        d.html(),
    )
        .into_response()
}

/// The dashboard stats API query: the page's time filter polls with
/// `?since=<unix seconds>` to scope the plugin's aggregates to
/// `[since, now]` (absent = the plugin's default aggregation). A bad
/// value is a 400 (axum's `Query` extractor rejects it).
#[derive(serde::Deserialize)]
struct StatsQuery {
    since: Option<u64>,
}

/// The dashboard stats API route (`{path}/api/stats`): the proxy's live
/// [`ProxyStatus`] merged with the plugin's per-request aggregates,
/// range-scoped by the optional `?since` query parameter.
async fn dashboard_stats(
    State(state): State<AppState>,
    Query(q): Query<StatsQuery>,
) -> impl IntoResponse {
    let d = state
        .dashboard
        .as_ref()
        .expect("dashboard stats route only mounted when a dashboard is loaded");
    axum::Json(d.stats(&capture_status(&state), q.since))
}

/// Snapshot of the proxy's live state for a dashboard stats poll: slot
/// occupancy, backend circuit breakers, coalesced groups and the KV meta
/// cache size (the plugin adds its own request aggregates on top).
pub(crate) fn capture_status(state: &AppState) -> ProxyStatus {
    let cfg = &state.config;
    let slots = state.sm.slot_snapshot();
    let in_flight = slots.iter().filter(|s| s.busy).count();
    let breaker = state.sm.breaker_state();
    let backends: Vec<BackendStatus> = cfg
        .backends
        .iter()
        .enumerate()
        .map(|(be, b)| {
            let br = breaker.get(be).cloned().unwrap_or_default();
            let busy = slots.iter().filter(|s| s.be == be && s.busy).count();
            BackendStatus {
                url: b.url.clone(),
                n_slots: b.n_slots,
                busy_slots: busy,
                down: br.down,
                cooldown_secs: br.cooldown_secs,
                failure_streak: br.failure_streak,
            }
        })
        .collect();
    let meta_files = std::fs::read_dir(&cfg.meta_dir)
        .map(|d| d.filter(|e| e.is_ok()).count())
        .unwrap_or(0);
    ProxyStatus {
        version: crate::config::VERSION.to_string(),
        uptime_secs: process_start().elapsed().as_secs_f64(),
        model_id: cfg.model_id.clone(),
        port: cfg.port,
        coalesce_requests: cfg.coalesce_requests,
        big_threshold_words: cfg.big_threshold_words,
        words_per_block: cfg.words_per_block,
        meta_max: cfg.meta_max,
        meta_files,
        in_flight,
        total_slots: slots.len(),
        active_groups: state.sf.len(),
        backends,
        slots,
    }
}

/// The lpcache pipeline view of one request, carried in the response's
/// extensions to the request-log middleware (see
/// [`request_log_middleware`]). Every field is optional: it is filled in
/// as far as the request got (a rejected request carries no slot, a small
/// request carries no restore outcome, ...), and whatever is missing
/// renders as `-` in the XLF record.
#[derive(Debug, Clone, Default)]
pub(crate) struct PipelineLog {
    /// `lpcache-model`: the model the client asked for.
    pub model: Option<String>,
    /// `lpcache-stream`.
    pub stream: Option<bool>,
    /// `lpcache-key`: the first 16 hex chars of the cache key.
    pub key: Option<String>,
    /// `lpcache-n-words`.
    pub n_words: Option<usize>,
    /// `lpcache-is-big`.
    pub is_big: Option<bool>,
    /// `lpcache-backend`: the backend index the request ran on.
    pub backend: Option<usize>,
    /// `lpcache-slot`.
    pub slot: Option<usize>,
    /// `lpcache-restored`: present when a KV cache restore was attempted
    /// (missing = not attempted, e.g. small requests).
    pub restored: Option<bool>,
    /// `lpcache-cache-hit`: present when a prefilter ran (`true` = served
    /// from the prefilter, `false` = let through).
    pub cache_hit: Option<bool>,
    /// `lpcache-coalesced`: `leader`/`follower` when coalescing is enabled.
    pub coalesced: Option<&'static str>,
    /// `lpcache-reason`: a short pipeline outcome for non-successful
    /// requests (missing = the request completed normally).
    pub reason: Option<String>,
    /// Shared backend-reported performance state (see [`PerfInner`]).
    /// Filled in as data arrives — the completion JSON (non-streaming) or
    /// the SSE chunks (streaming) — and read by the request log when the
    /// response body completes. `None` for requests outside the chat
    /// pipeline (e.g. `GET /v1/models`).
    pub perf: Option<Arc<Mutex<PerfInner>>>,
}

/// Attach the pipeline view to a response's extensions (the request-log
/// middleware reads it there when the response body finishes).
fn with_pipeline_log(mut resp: Response, pl: PipelineLog) -> Response {
    resp.extensions_mut().insert(pl);
    resp
}

/// Fill in the slot-related fields for a request that ran on slot `g`
/// (`restored` is present when a KV cache restore was attempted).
fn with_slot(mut pl: PipelineLog, g: GSlot, restored: Option<bool>) -> PipelineLog {
    pl.backend = Some(g.be);
    pl.slot = Some(g.slot);
    pl.restored = restored;
    pl
}

/// Backend-reported performance data for one request, shared between the
/// pipeline (which fills it in as data arrives) and the request-log
/// middleware (which reads it when the response body completes). Every
/// field is optional: it is set only when the backend reports it —
/// missing on failure paths and on backends that predate the `timings`
/// object.
#[derive(Debug, Default, Clone)]
pub(crate) struct PerfInner {
    /// Proxy-side time to first token (streaming): from the start of
    /// stream processing until the first SSE chunk from the backend.
    pub ttft_ms: Option<f64>,
    /// `usage.prompt_tokens`.
    pub prompt_tokens: Option<u64>,
    /// `usage.completion_tokens`.
    pub completion_tokens: Option<u64>,
    /// `usage.prompt_tokens_details.cached_tokens` (or `timings.cache_n`)
    /// — the visible effect of a KV restore.
    pub cached_tokens: Option<u64>,
    /// `timings.prompt_ms` (backend prompt processing, ms).
    pub prefill_ms: Option<f64>,
    /// `timings.predicted_ms` (backend generation, ms).
    pub gen_ms: Option<f64>,
    /// `timings.prompt_per_second`.
    pub prefill_tps: Option<f64>,
    /// `timings.predicted_per_second`.
    pub gen_tps: Option<f64>,
}

/// Fill `perf` from a backend completion object (a non-streaming response,
/// or one parsed SSE `data:` payload): the `usage` token counts and the
/// `timings` statistics (llama.cpp sends both in the final response/chunk
/// of every chat completion). Fields the backend did not report are left
/// untouched.
fn fill_perf_from_value(perf: &Arc<Mutex<PerfInner>>, value: &Value) {
    let Ok(mut p) = perf.lock() else {
        return;
    };
    if let Some(u) = value.get("usage") {
        p.prompt_tokens = p
            .prompt_tokens
            .or_else(|| u.get("prompt_tokens").and_then(Value::as_u64));
        p.completion_tokens = p
            .completion_tokens
            .or_else(|| u.get("completion_tokens").and_then(Value::as_u64));
        p.cached_tokens = p.cached_tokens.or_else(|| {
            u.pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
        });
    }
    if let Some(t) = value.get("timings") {
        p.cached_tokens = p
            .cached_tokens
            .or_else(|| t.get("cache_n").and_then(Value::as_u64));
        p.prefill_ms = p
            .prefill_ms
            .or_else(|| t.get("prompt_ms").and_then(Value::as_f64));
        p.gen_ms = p
            .gen_ms
            .or_else(|| t.get("predicted_ms").and_then(Value::as_f64));
        p.prefill_tps = p
            .prefill_tps
            .or_else(|| t.get("prompt_per_second").and_then(Value::as_f64));
        p.gen_tps = p
            .gen_tps
            .or_else(|| t.get("predicted_per_second").and_then(Value::as_f64));
    }
}

/// Scan complete SSE `data:` lines in `buf` (consuming them; an incomplete
/// trailing line is kept for the next chunk) and feed each parsed payload
/// to [`fill_perf_from_value`]. The proxy passes the stream through
/// byte-for-byte — this only observes it for the request log.
fn process_sse_perf(buf: &mut String, perf: &Arc<Mutex<PerfInner>>) {
    while let Some(pos) = buf.find('\n') {
        let line: String = buf.drain(..=pos).collect();
        let Some(data) = line.trim().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        fill_perf_from_value(perf, &v);
    }
}

/// Fill `perf` from a coalesced group's terminal outcome (the leader's
/// completion object, or the leader's recorded SSE stream).
fn perf_from_outcome(perf: Option<&Arc<Mutex<PerfInner>>>, outcome: &SharedOutcome) {
    let Some(perf) = perf else {
        return;
    };
    match outcome {
        SharedOutcome::Json(status, value) if *status == 200 => fill_perf_from_value(perf, value),
        SharedOutcome::Sse(bytes) => {
            let mut buf = String::from_utf8_lossy(bytes).into_owned();
            process_sse_perf(&mut buf, perf);
        }
        SharedOutcome::Json(_, _) | SharedOutcome::Raw(..) => {}
    }
}

/// One finished request, pending its XLF record. Rides along in the
/// response body's stream state, so it is dropped exactly when the body
/// is done — the stream completed or the client went away — and the
/// record is written from [`Drop`] (the plugin's `log` is synchronous,
/// and the drop is the only hook a disappearing body offers). Logging
/// failures are warned about and swallowed: request logging never breaks
/// request serving.
struct PendingRequestLog {
    logger: Option<Arc<dyn RequestLogger>>,
    dashboard: Option<Arc<dyn Dashboard>>,
    method: String,
    uri: Option<String>,
    client_ip: Option<String>,
    user_agent: Option<String>,
    request_bytes: Option<u64>,
    status: u16,
    pl: PipelineLog,
    t0: Instant,
    delivered: Arc<AtomicUsize>,
}

impl Drop for PendingRequestLog {
    fn drop(&mut self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        // Read the shared performance state last: by the time the body is
        // done, the streaming reader has observed the final chunks (unless
        // the client went away early — then whatever arrived is logged).
        let perf = self
            .pl
            .perf
            .as_ref()
            .and_then(|p| p.lock().ok().map(|g| g.clone()));
        let record = RequestLogRecord {
            datetime: Some(format_utc(now.as_secs(), now.subsec_nanos())),
            method: Some(self.method.clone()),
            uri: self.uri.clone(),
            client_ip: self.client_ip.clone(),
            user_agent: self.user_agent.clone(),
            request_bytes: self.request_bytes,
            duration_secs: Some(self.t0.elapsed().as_secs_f64()),
            status: Some(self.status),
            response_bytes: Some(self.delivered.load(Ordering::Relaxed) as u64),
            model: self.pl.model.clone(),
            stream: self.pl.stream,
            key: self.pl.key.clone(),
            n_words: self.pl.n_words,
            is_big: self.pl.is_big,
            backend: self.pl.backend,
            slot: self.pl.slot,
            restored: self.pl.restored,
            cache_hit: self.pl.cache_hit,
            coalesced: self.pl.coalesced,
            reason: self.pl.reason.clone(),
            prompt_tokens: perf.as_ref().and_then(|p| p.prompt_tokens),
            completion_tokens: perf.as_ref().and_then(|p| p.completion_tokens),
            cached_tokens: perf.as_ref().and_then(|p| p.cached_tokens),
            prefill_ms: perf.as_ref().and_then(|p| p.prefill_ms),
            gen_ms: perf.as_ref().and_then(|p| p.gen_ms),
            prefill_tps: perf.as_ref().and_then(|p| p.prefill_tps),
            gen_tps: perf.as_ref().and_then(|p| p.gen_tps),
            ttft_ms: perf.as_ref().and_then(|p| p.ttft_ms),
        };
        if let Some(logger) = &self.logger
            && let Err(e) = logger.log(&record)
        {
            tracing::warn!(
                "request_log_write_fail logger={} status={:?}: {e}",
                logger.name(),
                record.status
            );
        }
        // The dashboard (when loaded) gets the same record for its
        // aggregates; a failure only affects the dashboard, not the log.
        if let Some(dashboard) = &self.dashboard
            && let Err(e) = dashboard.record(&record)
        {
            tracing::warn!(
                "dashboard_record_fail dashboard={} status={:?}: {e}",
                dashboard.name(),
                record.status
            );
        }
    }
}

/// Whether `path` is one of the dashboard's own routes (the page at
/// `d.path()` or the stats API at `d.stats_path()` — the only two routes
/// the proxy mounts for a dashboard).
fn is_dashboard_own_route(path: &str, d: &dyn Dashboard) -> bool {
    path == d.path() || path == d.stats_path()
}

/// Request-log / dashboard middleware (active when `AppState::request_log`
/// and/or `AppState::dashboard` is set): captures the W3C fields of every
/// request (method, URI, client IP, user agent, request bytes from
/// `Content-Length`), wraps the response body to count the bytes actually
/// handed to the client, and hands the finished request (see
/// [`PendingRequestLog`]) to the request logger and/or the dashboard when
/// the body is done. The dashboard's own routes (its page and stats API)
/// are skipped: they are not written to the request log and not fed into
/// the dashboard's aggregates, so the page's polling never inflates the
/// statistics.
async fn request_log_middleware(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let logger = state.request_log.clone();
    let dashboard = state.dashboard.clone();
    if logger.is_none() && dashboard.is_none() {
        return next.run(req).await;
    }
    // The dashboard's own routes (the page + the stats API) are its
    // internal polling, not proxy traffic: skip them entirely so they
    // are neither logged nor counted in the dashboard's own statistics.
    if dashboard
        .as_deref()
        .is_some_and(|d| is_dashboard_own_route(req.uri().path(), d))
    {
        return next.run(req).await;
    }
    let t0 = Instant::now();
    let method = req.method().to_string();
    let uri = req.uri().path_and_query().map(|pq| pq.as_str().to_string());
    let client_ip = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.to_string());
    let user_agent = req
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let request_bytes = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let resp = next.run(req).await;
    let (mut parts, body) = resp.into_parts();
    let pl = parts.extensions.remove::<PipelineLog>().unwrap_or_default();
    let status = parts.status.as_u16();
    let delivered = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&delivered);
    let pending = PendingRequestLog {
        logger,
        dashboard,
        method,
        uri,
        client_ip,
        user_agent,
        request_bytes,
        status,
        pl,
        t0,
        delivered,
    };
    // Count the bytes handed to the client; the pending record rides
    // along in the stream state and is dropped (writing the record) when
    // the body is done or the client goes away.
    let stream = futures::stream::unfold(
        (body.into_data_stream(), counter, pending),
        |(mut body, counter, pending)| async move {
            match body.next().await {
                Some(Ok(chunk)) => {
                    counter.fetch_add(chunk.len(), Ordering::Relaxed);
                    Some((Ok(chunk), (body, counter, pending)))
                }
                Some(Err(err)) => Some((Err(err), (body, counter, pending))),
                None => None,
            }
        },
    );
    (parts, Body::from_stream(stream)).into_response()
}

async fn chat(State(state): State<AppState>, req: axum::http::Request<Body>) -> Response {
    let t0 = Instant::now();
    let raw = match to_bytes(req.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => return json_error(400, format!("reading body failed: {e}")),
    };
    let body: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(e) => return json_error(422, format!("invalid json body: {e}")),
    };
    if !body.is_object() {
        return json_error(422, "request body must be a JSON object");
    }

    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let client_model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&state.config.model_id)
        .to_string();

    if state.clients.is_empty() {
        return json_error(503, "no backends configured");
    }
    // model_id is taken from the first backend (used for cache keys only)
    let backend_model_id = state.clients[0].get_model_id().await;

    let prefix = hashing::raw_prefix(&messages);
    let full_for_key = format!("{}\n{}", backend_model_id, prefix);
    let key = hashing::prefix_key_sha256(&full_for_key);
    let blocks = hashing::block_hashes_from_text(&prefix, state.config.words_per_block);
    let n_words = hashing::words_from_text(&prefix).len();
    let is_big = n_words > state.config.big_threshold_words;

    // Request log: the pipeline view of this request (carried in the
    // response's extensions to the request-log middleware); filled in as
    // far as the request gets.
    let mut pl = PipelineLog {
        model: Some(client_model.clone()),
        stream: Some(stream),
        key: Some(short16(&key)),
        n_words: Some(n_words),
        is_big: Some(is_big),
        // Shared performance state (request log); filled in as the backend
        // response arrives.
        perf: Some(Arc::new(Mutex::new(PerfInner::default()))),
        ..Default::default()
    };

    // Prefilter adapter: short-circuit the request before coalescing,
    // slot acquisition, or backend dispatch. A `Reject` gets an immediate
    // JSON error, a `Serve` gets a previously cached backend result — in
    // both cases the request never reaches the llama-server backend (no
    // slot, no restore/save, no meta file, no coalescing group).
    if let Some(pf) = &state.prefilter {
        let preq = PrefilterRequest {
            body: body.clone(),
            messages: Value::Array(messages.clone()),
            model: client_model.clone(),
            backend_model_id: backend_model_id.clone(),
            stream,
            key: key.clone(),
            n_words,
            is_big,
        };
        match pf.check(&preq).await {
            PrefilterDecision::Accept => {
                pl.cache_hit = Some(false);
                tracing::debug!(
                    "prefilter_accept filter={} key={}",
                    pf.name(),
                    short16(&key)
                );
            }
            PrefilterDecision::Reject { status, message } => {
                tracing::info!(
                    "prefilter_reject filter={} key={} status={status} n_words={n_words}",
                    pf.name(),
                    short16(&key)
                );
                pl.cache_hit = Some(false);
                pl.reason = Some("prefilter_reject".into());
                return with_pipeline_log(json_error(status, message), pl);
            }
            PrefilterDecision::Serve { status, body } => {
                tracing::info!(
                    "prefilter_serve filter={} key={} status={status} n_words={n_words}",
                    pf.name(),
                    short16(&key)
                );
                let st = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
                pl.cache_hit = Some(true);
                // The cached backend result carries the original
                // `usage`/`timings` — log them as this request's
                // performance (it never reached the backend).
                if let Some(perf) = &pl.perf {
                    fill_perf_from_value(perf, &body);
                }
                return with_pipeline_log((st, axum::Json(body)).into_response(), pl);
            }
        }
    }

    // Concurrent-request coalescing (`COALESCE_REQUESTS`): concurrent
    // requests with the same cache key share one backend call — the first
    // becomes the group's leader, the rest wait for its result. Grouping is
    // based solely on the cache key (message content + backend model id):
    // generation parameters do NOT break groups, so followers receive the
    // leader's generated result (leader's sampling params, max_tokens, and
    // stream mode apply to the whole group).
    let grouped: Option<(String, Arc<Flight>)> = if state.config.coalesce_requests {
        let fk = key.clone();
        let (flight, leader) = state.sf.enter(&fk);
        if leader {
            tracing::info!("coalesce_lead key={} group={}", short16(&key), short16(&fk));
            Some((fk, flight))
        } else {
            tracing::info!("coalesce_join key={} group={}", short16(&key), short16(&fk));
            pl.coalesced = Some("follower");
            return with_pipeline_log(
                follower_response(
                    flight,
                    &client_model,
                    stream,
                    state.config.stream_queue_size,
                    pl.perf.clone(),
                )
                .await,
                pl,
            );
        }
    } else {
        None
    };

    if let Some((fk, flight)) = grouped {
        // Leader. Run the pipeline in a spawned task so a disconnect of the
        // leader's own client cannot strand the group's followers: the task
        // always runs to completion and finishes the flight.
        pl.coalesced = Some("leader");
        let state2 = state.clone();
        let body2 = body.clone();
        let client_model2 = client_model.clone();
        let backend_model_id2 = backend_model_id.clone();
        let key2 = key.clone();
        let blocks2 = blocks.clone();
        let prefix2 = prefix.clone();
        let pl2 = pl.clone();
        let grouped2 = Some((fk.clone(), Arc::clone(&flight)));
        let handle = tokio::spawn(async move {
            run_pipeline(
                &state2,
                &body2,
                &client_model2,
                stream,
                &backend_model_id2,
                &key2,
                &blocks2,
                &prefix2,
                is_big,
                n_words,
                t0,
                grouped2,
                pl2,
            )
            .await
        });
        let (resp, outcome) = match handle.await {
            Ok(t) => t,
            Err(e) => {
                // The leader task died (panic/abort): unblock the group.
                flight.finish(SharedOutcome::Json(
                    500,
                    json!({ "error": format!("coalesced leader failed: {e}") }),
                ));
                state.sf.forget(&fk);
                pl.reason = Some("coalesce_leader_failed".into());
                return with_pipeline_log(
                    json_error(500, format!("internal error: coalesced leader failed: {e}")),
                    pl,
                );
            }
        };
        if let Some(o) = outcome {
            flight.finish(o);
            state.sf.forget(&fk);
        }
        // `None` outcome: streaming leader — its reader task finishes the
        // flight after the stream ends.
        return resp;
    }

    // Coalescing disabled: run inline (cancellation semantics unchanged).
    let (resp, _outcome) = run_pipeline(
        &state,
        &body,
        &client_model,
        stream,
        &backend_model_id,
        &key,
        &blocks,
        &prefix,
        is_big,
        n_words,
        t0,
        None,
        pl,
    )
    .await;
    resp
}

/// The per-request pipeline (restore lookup -> slot acquire -> backend call
/// -> save/meta -> release). Returns the response and, when coalescing is
/// active, the outcome shared with the group's followers (`None` for a
/// streaming leader: its reader task finishes the flight after the stream
/// ends). `pl` (the request log's pipeline view) is filled in with the
/// slot-related fields plus the failure reason, and attached to the
/// returned response.
#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    state: &AppState,
    body: &Value,
    client_model: &str,
    stream: bool,
    backend_model_id: &str,
    key: &str,
    blocks: &[String],
    prefix: &str,
    is_big: bool,
    n_words: usize,
    t0: Instant,
    grouped: Option<(String, Arc<Flight>)>,
    mut pl: PipelineLog,
) -> (Response, Option<SharedOutcome>) {
    let mut restore_key: Option<String> = None;
    if is_big {
        match hashing::find_best_restore_candidate(
            &state.config.meta_dir,
            blocks,
            state.config.words_per_block,
            state.config.lcp_th,
            backend_model_id,
        ) {
            Some((k, ratio)) => {
                tracing::info!(
                    "restore_candidate basename={} ratio={ratio:.3}",
                    short16(&k)
                );
                restore_key = Some(k);
            }
            None => tracing::info!("restore_candidate none"),
        }
    } else {
        tracing::info!(
            "small_request n_words={n_words} threshold={}",
            state.config.big_threshold_words
        );
    }

    tracing::info!(
        "before_acquire is_big={is_big} restore_key={}",
        restore_key.as_ref().map(|k| short16(k)).unwrap_or_default()
    );

    let mut guard = match state.sm.acquire(restore_key.as_deref()).await {
        Ok(g) => g,
        Err(AcquireTimeout) => {
            tracing::error!(
                "acquire_timeout is_big={is_big} restore_key={}",
                restore_key.as_ref().map(|k| short16(k)).unwrap_or_default()
            );
            pl.reason = Some("slot_timeout".into());
            let (resp, o) = err_outcome(503, "all slots busy, please retry later");
            return (with_pipeline_log(resp, pl), Some(o));
        }
    };
    let mut g: GSlot = guard.slot;
    tracing::info!("after_acquire g={g} restored={:?}", guard.restored);
    // Request log: whether the acquire actually loaded the KV cache
    // (`lpcache-restored`; re-tracked when a failover retry re-acquires).
    let mut restored = guard
        .restored
        .as_ref()
        .map(|o| matches!(o, RestoreOutcome::Restored));
    // LRU bookkeeping: a successful restore counts as a "use" of the
    // cached entry — bump its meta timestamp so the post-save prune of
    // this very request cannot evict a cache that was just used
    // (README: pruning is by last save/restore time). A rejected restore
    // may instead remove a stale meta (KV file gone from all slot-save
    // dirs).
    touch_restored_meta(state, &guard, &restore_key);
    remove_stale_meta(state, &guard, &restore_key);

    // Up to 2 attempts: a connection-level failure (backend unreachable)
    // retries once on a slot of a *different* backend before the client
    // sees an error (deviation from Python, which returned the 500).
    for attempt in 0..2 {
        let client = Arc::clone(&state.clients[g.be]);
        let out_body = build_out_body(body, client_model, is_big, g);

        tracing::info!(
            "dispatch be={} slot={} is_big={is_big} restored={:?} attempt={attempt} backend_model_id={backend_model_id}",
            g.be,
            g.slot,
            guard.restored
        );

        if stream {
            let sc = match client.chat_stream(&out_body, Some(g.slot)).await {
                Ok(sc) => sc,
                Err(e) if matches!(e, BackendError::Other(_)) => {
                    match retry_after_failure(
                        state,
                        key,
                        g,
                        guard,
                        &restore_key,
                        attempt,
                        &e.to_string(),
                    )
                    .await
                    {
                        Some(g2) => {
                            g = g2.slot;
                            restored = g2
                                .restored
                                .as_ref()
                                .map(|o| matches!(o, RestoreOutcome::Restored));
                            touch_restored_meta(state, &g2, &restore_key);
                            remove_stale_meta(state, &g2, &restore_key);
                            guard = g2;
                            continue;
                        }
                        None => {
                            pl = with_slot(pl, g, restored);
                            pl.reason = Some("backend_unreachable".into());
                            let (resp, o) = err_outcome(500, format!("backend unreachable: {e}"));
                            return (with_pipeline_log(resp, pl), Some(o));
                        }
                    }
                }
                Err(e) => {
                    state.sm.mark_used(g);
                    state.sm.release(guard);
                    pl = with_slot(pl, g, restored);
                    pl.reason = Some("backend_error".into());
                    let (resp, o) = err_outcome(500, e.to_string());
                    return (with_pipeline_log(resp, pl), Some(o));
                }
            };
            if sc.status != 200 {
                // non-2xx: drain the error body, release, pass status
                // through. The backend answered, so it is reachable.
                let mut err_txt = String::new();
                let mut s = sc.stream;
                while let Some(chunk) = s.next().await {
                    match chunk {
                        Ok(b) => err_txt.push_str(&String::from_utf8_lossy(&b)),
                        Err(_) => break,
                    }
                    if err_txt.len() > 65536 {
                        break;
                    }
                }
                state.sm.report_success(g);
                state.sm.mark_used(g);
                state.sm.release(guard);
                let status =
                    StatusCode::from_u16(sc.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                let val = json!({ "error": err_txt });
                pl = with_slot(pl, g, restored);
                pl.reason = Some(format!("provider_http_{}", sc.status));
                return (
                    with_pipeline_log((status, axum::Json(val.clone())).into_response(), pl),
                    Some(SharedOutcome::Json(sc.status, val)),
                );
            }

            // Reader task: pumps SSE chunks through a bounded channel. In its
            // "finally" it always does save_after + write_meta + release (the
            // same as the Python reader, including for small requests), then
            // drops the sender so the response stream ends.
            //
            // Client-disconnect handling: the response body carries a oneshot
            // sender; when the client drops its connection to the proxy,
            // hyper drops the body, which drops the sender and wakes the
            // reader. The reader then drops the upstream stream *immediately*
            // (aborting the in-flight request to llama-server — the backend
            // has no cancel endpoint, so the closed TCP connection is the
            // cancellation) before running the cleanup.
            let (tx, rx) =
                mpsc::channel::<Result<Bytes, std::io::Error>>(state.config.stream_queue_size);
            let (client_gone_tx, mut client_gone_rx) = tokio::sync::oneshot::channel::<()>();
            let sm = Arc::clone(&state.sm);
            let cfg = Arc::clone(&state.config);
            let sf = Arc::clone(&state.sf);
            let key2 = key.to_string();
            let prefix2 = prefix.to_string();
            let blocks2: Vec<String> = blocks.to_vec();
            let model_id2 = backend_model_id.to_string();
            // Coalescing: share every SSE chunk with the group's followers
            // and finish their flight when the stream ends.
            let flight: Option<(String, Arc<Flight>)> = grouped.clone();
            if let Some((_, f)) = &flight {
                // The upstream accepted the stream: followers may now commit
                // to a 200 SSE response.
                f.mark_stream_started();
            }
            tracing::info!("stream_reader_start g={g} key={}", short16(&key2));

            // Request log: shared performance state (the reader fills in
            // ttft + the backend `usage`/`timings` as the chunks arrive).
            let perf = pl.perf.clone();
            tokio::spawn(async move {
                let mut s = sc.stream;
                let mut client_gone = false;
                let stream_t0 = Instant::now();
                let mut ttft_ms: Option<f64> = None;
                // Line buffer for SSE perf extraction (a chunk may split a
                // `data:` line; the remainder waits for the next chunk).
                let mut perf_buf = String::new();
                loop {
                    tokio::select! {
                        _ = &mut client_gone_rx => {
                            // the client dropped its connection to the proxy
                            // -> stop pumping and abort the upstream request
                            tracing::warn!(
                                "stream_reader_client_gone g={g} key={} (aborting upstream request)",
                                short16(&key2)
                            );
                            client_gone = true;
                            break;
                        }
                        chunk = s.next() => match chunk {
                            None => break,
                            Some(Ok(bytes)) if !bytes.is_empty() => {
                                if let Some((_, f)) = &flight {
                                    f.append_sse(&bytes);
                                }
                                if let Some(perf) = &perf {
                                    if ttft_ms.is_none() {
                                        ttft_ms =
                                            Some(stream_t0.elapsed().as_secs_f64() * 1000.0);
                                        if let Ok(mut pi) = perf.lock() {
                                            pi.ttft_ms = ttft_ms;
                                        }
                                    }
                                    perf_buf
                                        .push_str(&String::from_utf8_lossy(&bytes));
                                    process_sse_perf(&mut perf_buf, perf);
                                }
                                if tx.send(Ok(bytes)).await.is_err() {
                                    // client went away (body dropped) -> stop
                                    // pumping; still clean up
                                    tracing::warn!(
                                        "stream_reader_client_gone g={g} key={}",
                                        short16(&key2)
                                    );
                                    client_gone = true;
                                    break;
                                }
                            }
                            Some(Ok(_)) => {}
                            Some(Err(e)) => {
                                tracing::warn!(
                                    "stream_reader_error g={g} key={}: {e}",
                                    short16(&key2)
                                );
                                break;
                            }
                        },
                    }
                }
                if client_gone {
                    // Abort the request to llama-server now: dropping the
                    // stream closes the upstream TCP connection; the backend
                    // frees its slot when it notices the closed connection.
                    // The cleanup below (save + meta + release) still runs.
                    drop(s);
                }
                let ok = match sm.save_after(g, &key2).await {
                    Ok(ok) => {
                        sm.report_success(g);
                        ok
                    }
                    Err(e) => {
                        tracing::warn!("save_after_exception g={g} key={}: {e}", short16(&key2));
                        if matches!(e, BackendError::Other(_)) {
                            sm.report_failure(g);
                        }
                        sm.mark_used(g);
                        false
                    }
                };
                if let Err(e) = hashing::write_meta(
                    &cfg.meta_dir,
                    &key2,
                    &prefix2,
                    &blocks2,
                    cfg.words_per_block,
                    &model_id2,
                ) {
                    tracing::warn!("write_meta_exception key={}: {e}", short16(&key2));
                } else {
                    prune_cache(&cfg);
                }
                sm.release(guard);
                tracing::info!("stream_reader_done g={g} key={} saved={ok}", short16(&key2));
                // Finish the coalesced group: followers receive the full
                // stream (whatever was produced before the client went away
                // or the upstream ended).
                if let Some((fk, f)) = &flight {
                    let full = f.sse_bytes();
                    f.finish(SharedOutcome::Sse(full));
                    sf.forget(fk);
                }
                // tx is dropped here -> the response stream ends
            });

            let headers = [
                (header::CONTENT_TYPE, "text/event-stream"),
                (header::CACHE_CONTROL, "no-cache"),
                (header::CONNECTION, "keep-alive"),
            ];
            // bridge the mpsc receiver into a futures Stream (channel close ends
            // it). The oneshot sender rides along in the unfold state, so it is
            // dropped as soon as the client drops the response body — that drop
            // is what wakes the reader to abort the upstream request.
            let stream =
                futures::stream::unfold((rx, client_gone_tx), |(mut rx, gone_tx)| async move {
                    rx.recv().await.map(|item| (item, (rx, gone_tx)))
                });
            pl = with_slot(pl, g, restored);
            return (
                with_pipeline_log((headers, Body::from_stream(stream)).into_response(), pl),
                None,
            );
        } else {
            match client.chat_json(&out_body, Some(g.slot)).await {
                Err(BackendError::HttpStatus { status, body }) => {
                    // mirrors the Python `raise_for_status` path -> 500. The
                    // backend answered, so it is reachable.
                    state.sm.report_success(g);
                    state.sm.mark_used(g);
                    state.sm.release(guard);
                    let body: String = body.chars().take(512).collect();
                    pl = with_slot(pl, g, restored);
                    pl.reason = Some(format!("provider_http_{status}"));
                    let (resp, o) =
                        err_outcome(500, format!("provider returned HTTP {status}: {body}"));
                    return (with_pipeline_log(resp, pl), Some(o));
                }
                Err(e) if matches!(e, BackendError::Other(_)) => {
                    match retry_after_failure(
                        state,
                        key,
                        g,
                        guard,
                        &restore_key,
                        attempt,
                        &e.to_string(),
                    )
                    .await
                    {
                        Some(g2) => {
                            g = g2.slot;
                            restored = g2
                                .restored
                                .as_ref()
                                .map(|o| matches!(o, RestoreOutcome::Restored));
                            touch_restored_meta(state, &g2, &restore_key);
                            remove_stale_meta(state, &g2, &restore_key);
                            guard = g2;
                            continue;
                        }
                        None => {
                            pl = with_slot(pl, g, restored);
                            pl.reason = Some("backend_unreachable".into());
                            let (resp, o) = err_outcome(500, format!("backend unreachable: {e}"));
                            return (with_pipeline_log(resp, pl), Some(o));
                        }
                    }
                }
                Err(e) => {
                    state.sm.mark_used(g);
                    state.sm.release(guard);
                    pl = with_slot(pl, g, restored);
                    pl.reason = Some("backend_error".into());
                    let (resp, o) = err_outcome(500, e.to_string());
                    return (with_pipeline_log(resp, pl), Some(o));
                }
                Ok(JsonChat::NonObject) => {
                    state.sm.report_success(g);
                    state.sm.mark_used(g);
                    state.sm.release(guard);
                    pl = with_slot(pl, g, restored);
                    pl.reason = Some("provider_non_object".into());
                    let (resp, o) = err_outcome(502, "provider non-JSON body");
                    return (with_pipeline_log(resp, pl), Some(o));
                }
                Ok(JsonChat::NonJson { message, raw }) => {
                    // provider answered 200 with a non-JSON / broken body (the
                    // backend is reachable). In the Python app the returned error
                    // dict is still a dict, so big requests run the same save +
                    // write-meta path before the 200.
                    state.sm.report_success(g);
                    let save_ok = if is_big {
                        match save_big(state, g, key, prefix, blocks, backend_model_id).await {
                            Ok(ok) => ok,
                            Err(b) => {
                                let (resp, o) = *b;
                                state.sm.release(guard);
                                pl = with_slot(pl, g, restored);
                                pl.reason = Some("save_failed".into());
                                return (with_pipeline_log(resp, pl), Some(o));
                            }
                        }
                    } else {
                        false
                    };
                    state.sm.release(guard);
                    tracing::info!(
                        "json_done g={g} key={} saved={save_ok} is_big={is_big} dur_ms={}",
                        short16(key),
                        t0.elapsed().as_millis()
                    );
                    let val = json!({ "object": "error", "message": message, "raw": raw });
                    pl = with_slot(pl, g, restored);
                    return (
                        with_pipeline_log(
                            (StatusCode::OK, axum::Json(val.clone())).into_response(),
                            pl,
                        ),
                        Some(SharedOutcome::Json(200, val)),
                    );
                }
                Ok(JsonChat::Object(out)) => {
                    state.sm.report_success(g);
                    let save_ok = if is_big {
                        match save_big(state, g, key, prefix, blocks, backend_model_id).await {
                            Ok(ok) => ok,
                            Err(b) => {
                                let (resp, o) = *b;
                                state.sm.release(guard);
                                pl = with_slot(pl, g, restored);
                                pl.reason = Some("save_failed".into());
                                return (with_pipeline_log(resp, pl), Some(o));
                            }
                        }
                    } else {
                        false
                    };
                    state.sm.release(guard);
                    tracing::info!(
                        "json_done g={g} key={} saved={save_ok} is_big={is_big} dur_ms={}",
                        short16(key),
                        t0.elapsed().as_millis()
                    );
                    // Result cache: store the fresh backend result under the
                    // request's cache key so a later same-key (non-stream)
                    // request is answered by the result-cache prefilter
                    // before any slot/backend work (see the `result_cache`
                    // plugin). Streaming results are not stored.
                    if let Some(store) = &state.result_store
                        && let Err(e) = store.store(key, 200, &out)
                    {
                        tracing::warn!("result_cache_store_fail key={}: {e}", short16(key));
                    }
                    pl = with_slot(pl, g, restored);
                    // Request log: the backend's `usage`/`timings` from the
                    // completion (llama.cpp includes them in the response).
                    if let Some(perf) = &pl.perf {
                        fill_perf_from_value(perf, &out);
                    }
                    return (
                        with_pipeline_log(
                            (StatusCode::OK, axum::Json(out.clone())).into_response(),
                            pl,
                        ),
                        Some(SharedOutcome::Json(200, out)),
                    );
                }
            }
        }
    }
    unreachable!("every attempt returns or schedules the retry")
}

// ---------------------------------------------------------------------------
// Concurrent-request coalescing helpers (COALESCE_REQUESTS)
// ---------------------------------------------------------------------------

/// A JSON error response plus the shared outcome for the group's followers.
fn err_outcome(status: u16, msg: impl std::fmt::Display) -> (Response, SharedOutcome) {
    let msg = msg.to_string();
    (
        json_error(status, msg.clone()),
        SharedOutcome::Json(status, json!({ "error": msg })),
    )
}

/// Wait for the leader's terminal result.
async fn wait_outcome(flight: &Flight) -> SharedOutcome {
    loop {
        let notified = flight.notified();
        tokio::pin!(notified);
        if let Some(o) = flight.outcome() {
            return o;
        }
        notified.await;
    }
}

/// Serve a coalesced (follower) request from the leader's result: no slot
/// is acquired, no backend call is made, and no meta file is written. The
/// leader's `usage`/`timings` (carried in its terminal outcome) are
/// copied into `perf` for the follower's request-log record.
async fn follower_response(
    flight: Arc<Flight>,
    client_model: &str,
    stream: bool,
    stream_queue_size: usize,
    perf: Option<Arc<Mutex<PerfInner>>>,
) -> Response {
    if !stream {
        let outcome = wait_outcome(&flight).await;
        perf_from_outcome(perf.as_ref(), &outcome);
        return render_follower_json(&outcome, client_model);
    }
    // Stream follower: wait until the leader either starts streaming or
    // fails; then tail the live buffer or mirror the error.
    loop {
        let notified = flight.notified();
        tokio::pin!(notified);
        if let Some(o) = flight.outcome() {
            perf_from_outcome(perf.as_ref(), &o);
            return render_stream_follower_terminal(&o, client_model);
        }
        if flight.stream_started() {
            break;
        }
        notified.await;
    }
    let (tx, rx) = mpsc::channel(stream_queue_size);
    let flight2 = Arc::clone(&flight);
    let perf2 = perf;
    tokio::spawn(async move {
        let mut pos = 0usize;
        loop {
            let notified = flight2.notified();
            tokio::pin!(notified);
            let chunk = flight2.drain_sse_from(&mut pos);
            if !chunk.is_empty() && tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                return; // client went away
            }
            if flight2.outcome().is_some() {
                // Final drain in case the last append raced the finish.
                let rest = flight2.drain_sse_from(&mut pos);
                if !rest.is_empty() {
                    let _ = tx.send(Ok(Bytes::from(rest))).await;
                }
                // Request log: the leader's performance data.
                if let Some(o) = flight2.outcome() {
                    perf_from_outcome(perf2.as_ref(), &o);
                }
                return;
            }
            notified.await;
        }
    });
    sse_response_simple(rx)
}

/// Build the body sent to llama.cpp (same fields as app.py), pinned to
/// slot `g`.
fn build_out_body(body: &Value, client_model: &str, is_big: bool, g: GSlot) -> Value {
    let mut out_body = body.clone();
    out_body["model"] = json!(client_model);
    out_body["cache_prompt"] = json!(is_big);
    out_body["n_keep"] = json!(-1);
    let mut opts = out_body
        .get("options")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    opts.insert("slot_id".into(), json!(g.slot));
    opts.insert("id_slot".into(), json!(g.slot));
    opts.insert("n_keep".into(), json!(-1));
    opts.insert("cache_prompt".into(), json!(is_big));
    out_body["options"] = Value::Object(opts);
    out_body
}

/// LRU bookkeeping: a successful restore counts as a "use" of the cached
/// entry — bump its meta-file timestamp (see [`hashing::touch_meta`]).
/// Without this, a just-restored entry could be evicted by this very
/// request's post-save prune (README: pruning is by last save/restore
/// time). A meta file that a concurrent prune already removed is a no-op
/// with a warning.
fn touch_restored_meta(state: &AppState, guard: &SlotGuard, restore_key: &Option<String>) {
    if guard.restored == Some(RestoreOutcome::Restored)
        && let Some(rk) = restore_key
    {
        hashing::touch_meta(&state.config.meta_dir, rk);
    }
}

/// Stale-meta cleanup: a *rejected* restore (HTTP 400) means the backend
/// could not load the KV file (missing, corrupt, or too big for the slot
/// context). When the file is absent from every slot-save dir this host
/// can see, the meta entry can never be restored again: remove it so the
/// next matching request stops retrying a doomed restore (each retry
/// logs an error on the backend, e.g. `llama_state_seq_load_file: failed
/// to open ...`). If any visible dir still holds the file — or no dir is
/// visible locally (remote backend) — keep the meta: the 400 may be
/// transient for that one backend ("no available space in KV cache"), and
/// meta files are not backend-specific, so in a multi-backend setup the
/// file may live in another backend's dir.
fn remove_stale_meta(state: &AppState, guard: &SlotGuard, restore_key: &Option<String>) {
    if guard.restored == Some(RestoreOutcome::Rejected)
        && let Some(rk) = restore_key
    {
        let dirs: Vec<&std::path::Path> = state
            .config
            .slot_save_dirs()
            .into_iter()
            .filter(|d| d.is_dir())
            .collect();
        if dirs.is_empty() {
            // Backend storage not visible from this host: can't verify
            // the file is gone — keep the meta (status quo).
            return;
        }
        if dirs.iter().any(|d| d.join(rk).is_file()) {
            // The file still exists somewhere (or the 400 was "no
            // available space"): it may still restore — keep the meta.
            return;
        }
        let path = state.config.meta_dir.join(format!("{rk}.meta.json"));
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::warn!(
                "stale_meta_removed key={} (restore rejected; KV file missing from all slot-save dirs)",
                short16(rk)
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!("stale_meta_missing key={} (already pruned)", short16(rk));
            }
            Err(e) => tracing::warn!("stale_meta_remove_fail key={}: {e}", short16(rk)),
        }
    }
}

/// Connection-level failure on slot `g`: log it, trip the circuit breaker,
/// mark the slot used, release it, and — on the first attempt — acquire a
/// slot of a *different* backend for the retry (the restore key is
/// re-attempted there). `None` when the retry budget is spent or no other
/// backend has an eligible slot (single-backend deployment, or every other
/// backend cooling down).
async fn retry_after_failure(
    state: &AppState,
    key: &str,
    g: GSlot,
    guard: SlotGuard,
    restore_key: &Option<String>,
    attempt: usize,
    err: &str,
) -> Option<SlotGuard> {
    tracing::warn!("chat_error g={g} key={}: {err}", short16(key));
    state.sm.report_failure(g);
    state.sm.mark_used(g);
    state.sm.release(guard);
    if attempt >= 1 {
        return None;
    }
    let g2 = state
        .sm
        .acquire_excluding(g.be, restore_key.as_deref())
        .await
        .ok()?;
    tracing::info!("chat_retry from_g={g} to_g={} attempt=1", g2.slot);
    Some(g2)
}

/// Runs the big-request `save_after` + `write_meta` sequence.
///
/// Returns the save status on success, or a 500 response (plus its shared
/// coalescing outcome) when either step fails (mirrors the Python
/// `try / except` around `sm.save_after` + `hs.write_meta`, which bubbles
/// up to the outer 500 handler).
async fn save_big(
    state: &AppState,
    g: GSlot,
    key: &str,
    prefix: &str,
    blocks: &[String],
    backend_model_id: &str,
) -> Result<bool, Box<(Response, SharedOutcome)>> {
    let ok = state.sm.save_after(g, key).await.map_err(|e| {
        if matches!(e, BackendError::Other(_)) {
            tracing::warn!("save_error g={g} key={}: {e}", short16(key));
            state.sm.report_failure(g);
        }
        state.sm.mark_used(g);
        let msg = e.to_string();
        Box::new((
            json_error(500, msg.clone()),
            SharedOutcome::Json(500, json!({ "error": msg })),
        ))
    })?;
    hashing::write_meta(
        &state.config.meta_dir,
        key,
        prefix,
        blocks,
        state.config.words_per_block,
        backend_model_id,
    )
    .map_err(|e| {
        let msg = e.to_string();
        Box::new((
            json_error(500, msg.clone()),
            SharedOutcome::Json(500, json!({ "error": msg })),
        ))
    })?;
    prune_cache(&state.config);
    Ok(ok)
}

/// Bounded LRU cache pruning (deviation from Python, which never prunes).
///
/// After a successful save: if `META_DIR` holds more than `meta_max` meta
/// files, the oldest ones are removed — together with the KV files that
/// bear the same names in the configured `--slot-save-path` directories
/// (per-backend `slot_save_path` entries, or `SLOT_SAVE_PATH` for the
/// single-backend fallback). Pruning only ever deletes files, and a later
/// restore of a pruned key simply fails and falls back to a full prefill.
fn prune_cache(config: &Config) {
    if config.meta_max == 0 {
        return;
    }
    let pruned = hashing::prune_meta(&config.meta_dir, config.meta_max);
    if pruned.is_empty() {
        return;
    }
    for key in &pruned {
        for dir in config.slot_save_dirs() {
            // NotFound is normal: the KV file may only exist in some
            // backends' directories (or nowhere if none is configured).
            if let Err(e) = std::fs::remove_file(dir.join(key)) {
                tracing::debug!(
                    "prune_kv_fail dir={} key={}: {e}",
                    dir.display(),
                    short16(key)
                );
            }
        }
    }
}

fn json_error(status: u16, msg: impl Into<String>) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, axum::Json(json!({ "error": msg.into() }))).into_response()
}

fn short16(s: &str) -> String {
    s.chars().take(16).collect()
}

/// Render the leader's result for a non-streaming follower.
fn render_follower_json(outcome: &SharedOutcome, client_model: &str) -> Response {
    match outcome {
        SharedOutcome::Json(status, value) => {
            let mut v = value.clone();
            if *status == 200 {
                // Echo the model name the follower asked for.
                if let Some(o) = v.as_object_mut() {
                    o.insert("model".into(), json!(client_model));
                }
            }
            (
                StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                axum::Json(v),
            )
                .into_response()
        }
        SharedOutcome::Sse(bytes) => {
            // Unreachable in practice (the stream flag is part of the group
            // key): assemble a completion from the stream defensively.
            (
                StatusCode::OK,
                axum::Json(sse_to_completion(bytes, client_model)),
            )
                .into_response()
        }
        SharedOutcome::Raw(status, _mime, bytes) => raw_response(*status, bytes),
    }
}

/// Render the leader's terminal result for a streaming follower (the leader
/// finished, or failed, before this follower started tailing).
fn render_stream_follower_terminal(outcome: &SharedOutcome, client_model: &str) -> Response {
    match outcome {
        // The leader already finished: replay the recorded stream.
        SharedOutcome::Sse(bytes) => sse_response_bytes(bytes),
        SharedOutcome::Json(status, value) => {
            if *status == 200 {
                // Defensive: a stream group normally ends with `Sse`.
                sse_response_bytes(&sse_from_completion(value, client_model))
            } else {
                // The leader failed before streaming: mirror the error.
                (
                    StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    axum::Json(value.clone()),
                )
                    .into_response()
            }
        }
        SharedOutcome::Raw(status, _mime, bytes) => raw_response(*status, bytes),
    }
}

fn raw_response(status: u16, bytes: &[u8]) -> Response {
    let mut resp = Response::new(Body::from(bytes.to_vec()));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    resp
}

/// SSE response for a follower's live tail channel.
fn sse_response_simple(rx: mpsc::Receiver<Result<Bytes, std::io::Error>>) -> Response {
    let headers = [
        (header::CONTENT_TYPE, "text/event-stream"),
        (header::CACHE_CONTROL, "no-cache"),
        (header::CONNECTION, "keep-alive"),
    ];
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    (headers, Body::from_stream(stream)).into_response()
}

/// SSE response replaying already-recorded stream bytes.
fn sse_response_bytes(bytes: &[u8]) -> Response {
    let headers = [
        (header::CONTENT_TYPE, "text/event-stream"),
        (header::CACHE_CONTROL, "no-cache"),
        (header::CONNECTION, "keep-alive"),
    ];
    let b = Bytes::copy_from_slice(bytes);
    let stream = futures::stream::once(async move { Ok::<Bytes, std::io::Error>(b) });
    (headers, Body::from_stream(stream)).into_response()
}

/// Assemble a non-streaming completion object from raw SSE bytes (defensive
/// path: only used if a non-streaming follower ever sees a stream outcome).
fn sse_to_completion(bytes: &[u8], client_model: &str) -> Value {
    let mut content = String::new();
    let mut meta: Option<(String, u64)> = None;
    let mut finish: Option<String> = None;
    let mut usage: Option<Value> = None;
    for line in String::from_utf8_lossy(bytes).lines() {
        let data = match line.trim().strip_prefix("data:") {
            Some(d) => d.trim(),
            None => continue,
        };
        if data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if meta.is_none()
            && let (Some(id), Some(created)) = (
                v.get("id").and_then(Value::as_str),
                v.get("created").and_then(Value::as_u64),
            )
        {
            meta = Some((id.to_string(), created));
        }
        if let Some(choice) = v
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        {
            if let Some(delta) = choice
                .get("delta")
                .and_then(|d| d.get("content"))
                .and_then(Value::as_str)
            {
                content.push_str(delta);
            }
            if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
                finish = Some(fr.to_string());
            }
        }
        if let Some(u) = v.get("usage")
            && !u.is_null()
        {
            usage = Some(u.clone());
        }
    }
    let (id, created) = meta.unwrap_or_else(|| ("chatcmpl-coalesced".to_string(), 0));
    json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": client_model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": finish.unwrap_or_else(|| "stop".to_string()),
        }],
        "usage": usage.unwrap_or_else(|| json!({
            "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0
        })),
    })
}

/// Synthesize a minimal SSE stream (one chunk + `[DONE]`) from a
/// non-streaming completion (defensive path).
fn sse_from_completion(value: &Value, client_model: &str) -> Vec<u8> {
    let content = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let finish = value
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop");
    let id = value
        .get("id")
        .cloned()
        .unwrap_or_else(|| json!("chatcmpl-coalesced"));
    let created = value.get("created").cloned().unwrap_or_else(|| json!(0));
    let usage = value.get("usage").cloned().unwrap_or(Value::Null);
    let chunk = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": client_model,
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": content },
            "finish_reason": finish,
        }],
        "usage": usage,
    });
    format!("data: {chunk}\n\ndata: [DONE]\n\n").into_bytes()
}
