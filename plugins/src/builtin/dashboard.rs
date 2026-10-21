//! `dashboard` plugin — real-time proxy cache statistics web dashboard.
//!
//! The plugin is loaded from the `PLUGINS` configuration like any other
//! plugin; when a dashboard is loaded the proxy serves
//!
//! - `GET {path}` — a self-contained HTML page (inline CSS + JS, no
//!   external assets),
//! - `GET {path}/api/stats` — the current JSON payload: the proxy's live
//!   state (slots, backends, circuit breakers, coalesced groups, meta
//!   files — a [`crate::api::ProxyStatus`] snapshot filled in by the
//!   proxy on every poll) merged with the aggregates below. The endpoint
//!   accepts an optional `?since=<unix seconds>` query parameter that
//!   scopes the aggregates to `[since, now]`; without it the payload is
//!   the lifetime counters plus the trailing `window_secs` series.
//!
//! The proxy feeds the plugin one [`RequestLogRecord`] per finished
//! request (success, error, rejection or disconnect — the same records
//! the request logger receives). The dashboard's own routes (the page
//! and the stats API) are excluded: they are neither logged nor fed
//! into the aggregates. From those it keeps, in memory:
//!
//! - **counters** — total requests by status class, cache hits, KV
//!   restores, big/streamed requests, coalesced followers, token totals,
//!   average duration / time-to-first-token / generation rate;
//! - **time series** — one bucket of `bucket_secs` (full request
//!   counters per bucket) covering the whole process lifetime, bounded
//!   by `MAX_BUCKETS` (oldest dropped first); a range query over many
//!   buckets is coarsened to at most `MAX_SERIES_POINTS` points before
//!   it is served, so the payload stays small for long ranges;
//! - **recent requests** — a ring buffer of the last `max_recent`
//!   finished requests (status, duration, key, flags, tokens, timings).
//!
//! The page polls the stats endpoint every `refresh_ms` and renders stat
//! cards, throughput bar charts, the backend table, a per-slot occupancy
//! grid and the recent-requests table. A time-filter dropdown, grouped
//! into presets (30 seconds, 1 minute, 5 minutes, 15 minutes) and other
//! (custom time, or all time), scopes the
//! cards, charts and recent table to the selected range by polling with
//! `?since=<unix seconds>`. All aggregates reset on proxy restart;
//! nothing is persisted.
//!
//! Params:
//! - `path` (default `/dashboard`): page URL path (must start with `/`;
//!   the stats API is mounted at `{path}/api/stats`),
//! - `refresh_ms` (default `2000`, range 100..=3600000): the page's
//!   polling interval,
//! - `window_secs` (default `300`, range 10..=86400): trailing window
//!   for the series of a stats poll without `?since`,
//! - `bucket_secs` (default `5`, range 1..=3600): time-series bucket
//!   size,
//! - `max_recent` (default `100`, range 0..=10000; `0` = no recent
//!   list): ring buffer size.

use crate::api::{Dashboard, ProxyStatus, RequestLogRecord};
use crate::registry::{PluginError, PluginFactory};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Plugin name (registry key).
pub const NAME: &str = "dashboard";
/// Default page URL path.
pub const DEFAULT_PATH: &str = "/dashboard";
/// Default page polling interval in milliseconds.
pub const DEFAULT_REFRESH_MS: u64 = 2000;
/// Default trailing window (seconds) for stats polls without `?since`.
pub const DEFAULT_WINDOW_SECS: u64 = 300;
/// Default time-series bucket size in seconds.
pub const DEFAULT_BUCKET_SECS: u64 = 5;
/// Default recent-requests ring size.
pub const DEFAULT_MAX_RECENT: usize = 100;
/// Hard cap on the retained time-series buckets (oldest dropped first):
/// bounds the memory of the lifetime series that backs the "all time"
/// filter. At the default 5 s bucket this is ~69 days of history.
pub const MAX_BUCKETS: usize = 50_000;
/// Maximum number of points a range query serves for the time series:
/// longer ranges are coarsened (merged into a multiple of `bucket_secs`)
/// down to this many points, keeping the payload small.
pub const MAX_SERIES_POINTS: usize = 360;

fn default_path() -> String {
    DEFAULT_PATH.to_string()
}
fn default_refresh_ms() -> u64 {
    DEFAULT_REFRESH_MS
}
fn default_window_secs() -> u64 {
    DEFAULT_WINDOW_SECS
}
fn default_bucket_secs() -> u64 {
    DEFAULT_BUCKET_SECS
}
fn default_max_recent() -> usize {
    DEFAULT_MAX_RECENT
}

/// JSON parameters of the `dashboard` plugin.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardParams {
    /// Page URL path (must start with `/`).
    #[serde(default = "default_path")]
    pub path: String,
    /// Page polling interval in milliseconds (100..=3600000).
    #[serde(default = "default_refresh_ms")]
    pub refresh_ms: u64,
    /// Trailing window (seconds) for the time series and rates of a
    /// stats poll without `?since` (10..=86400).
    #[serde(default = "default_window_secs")]
    pub window_secs: u64,
    /// Time-series bucket size in seconds (1..=3600).
    #[serde(default = "default_bucket_secs")]
    pub bucket_secs: u64,
    /// Recent-requests ring size (0..=10000; `0` = no recent list).
    #[serde(default = "default_max_recent")]
    pub max_recent: usize,
}

impl Default for DashboardParams {
    fn default() -> Self {
        Self {
            path: default_path(),
            refresh_ms: default_refresh_ms(),
            window_secs: default_window_secs(),
            bucket_secs: default_bucket_secs(),
            max_recent: default_max_recent(),
        }
    }
}

/// Validate a parsed params set (the factory reports the first problem as
/// [`PluginError::BadParams`]).
pub fn validate(params: &DashboardParams) -> Result<(), String> {
    if !params.path.starts_with('/') {
        return Err(format!(
            "`path` must start with '/' (got {:?})",
            params.path
        ));
    }
    if !(100..=3_600_000).contains(&params.refresh_ms) {
        return Err(format!(
            "`refresh_ms` must be in 100..=3600000 (got {})",
            params.refresh_ms
        ));
    }
    if !(10..=86_400).contains(&params.window_secs) {
        return Err(format!(
            "`window_secs` must be in 10..=86400 (got {})",
            params.window_secs
        ));
    }
    if !(1..=3600).contains(&params.bucket_secs) {
        return Err(format!(
            "`bucket_secs` must be in 1..=3600 (got {})",
            params.bucket_secs
        ));
    }
    if params.max_recent > 10_000 {
        return Err(format!(
            "`max_recent` must be in 0..=10000 (got {})",
            params.max_recent
        ));
    }
    Ok(())
}

/// One time-series bucket of `bucket_secs` seconds: the request
/// counters of everything that finished in `[t, t + bucket_secs)`.
#[derive(Debug, Default, Clone)]
pub struct Bucket {
    /// Bucket start (unix seconds, aligned to the bucket size).
    pub t: u64,
    /// Request counters of the bucket (requests by status class, cache
    /// hits, restores, tokens and timing sums) — summing the buckets of
    /// a range yields the range's counters.
    pub c: Counters,
}

impl Bucket {
    fn new(t: u64) -> Self {
        Self {
            t,
            c: Counters::default(),
        }
    }

    /// Fold one finished record into the bucket.
    fn add(&mut self, record: &RequestLogRecord) {
        self.c.add(record)
    }
}

/// One finished request in the recent-requests ring.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecentRow {
    /// Finish time (unix seconds, fractional).
    pub ts: f64,
    /// Response status code.
    pub status: u16,
    /// Request duration in seconds.
    pub duration_secs: f64,
    pub model: Option<String>,
    /// First 16 hex chars of the KV cache key.
    pub key: Option<String>,
    pub stream: bool,
    /// A KV cache restore was attempted and succeeded.
    pub restored: bool,
    /// The prefilter served the request from its result cache.
    pub cache_hit: bool,
    /// The request joined a coalesced group as a follower.
    pub coalesced: bool,
    /// Short pipeline outcome for failed requests (`slot_timeout`, ...).
    pub reason: Option<String>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub ttft_ms: Option<f64>,
    pub gen_tps: Option<f64>,
}

/// Lifetime aggregate counters.
#[derive(Debug, Default, Clone)]
pub struct Counters {
    pub total: u64,
    pub ok: u64,
    pub client_errors: u64,
    pub server_errors: u64,
    pub cache_hits: u64,
    pub restored: u64,
    pub big_requests: u64,
    pub streamed: u64,
    pub coalesced_followers: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    duration_sum: f64,
    duration_n: u64,
    ttft_sum: f64,
    ttft_n: u64,
    gen_tps_sum: f64,
    gen_tps_n: u64,
    prefill_tps_sum: f64,
    prefill_tps_n: u64,
}

impl Counters {
    /// Fold one finished record into the counters.
    fn add(&mut self, record: &RequestLogRecord) {
        self.total += 1;
        let status = record.status.unwrap_or(0);
        match status {
            200..=399 => self.ok += 1,
            400..=499 => self.client_errors += 1,
            _ => self.server_errors += 1,
        }
        if record.cache_hit == Some(true) {
            self.cache_hits += 1;
        }
        if record.restored == Some(true) {
            self.restored += 1;
        }
        if record.is_big == Some(true) {
            self.big_requests += 1;
        }
        if record.stream == Some(true) {
            self.streamed += 1;
        }
        if record.coalesced == Some("follower") {
            self.coalesced_followers += 1;
        }
        self.prompt_tokens += record.prompt_tokens.unwrap_or(0);
        self.completion_tokens += record.completion_tokens.unwrap_or(0);
        self.cached_tokens += record.cached_tokens.unwrap_or(0);
        if let Some(d) = record.duration_secs {
            self.duration_sum += d;
            self.duration_n += 1;
        }
        if let Some(t) = record.ttft_ms {
            self.ttft_sum += t;
            self.ttft_n += 1;
        }
        if let Some(t) = record.gen_tps {
            self.gen_tps_sum += t;
            self.gen_tps_n += 1;
        }
        if let Some(t) = record.prefill_tps {
            self.prefill_tps_sum += t;
            self.prefill_tps_n += 1;
        }
    }

    /// Add the counters held by one bucket to `self`.
    fn add_bucket(&mut self, b: &Bucket) {
        self.total += b.c.total;
        self.ok += b.c.ok;
        self.client_errors += b.c.client_errors;
        self.server_errors += b.c.server_errors;
        self.cache_hits += b.c.cache_hits;
        self.restored += b.c.restored;
        self.big_requests += b.c.big_requests;
        self.streamed += b.c.streamed;
        self.coalesced_followers += b.c.coalesced_followers;
        self.prompt_tokens += b.c.prompt_tokens;
        self.completion_tokens += b.c.completion_tokens;
        self.cached_tokens += b.c.cached_tokens;
        self.duration_sum += b.c.duration_sum;
        self.duration_n += b.c.duration_n;
        self.ttft_sum += b.c.ttft_sum;
        self.ttft_n += b.c.ttft_n;
        self.gen_tps_sum += b.c.gen_tps_sum;
        self.gen_tps_n += b.c.gen_tps_n;
        self.prefill_tps_sum += b.c.prefill_tps_sum;
        self.prefill_tps_n += b.c.prefill_tps_n;
    }

    /// The sum of the counters held by the given buckets (the counters
    /// of the range they cover).
    pub fn sum<'a>(buckets: impl Iterator<Item = &'a Bucket>) -> Counters {
        let mut c = Counters::default();
        for b in buckets {
            c.add_bucket(b);
        }
        c
    }

    fn avg(n: u64, sum: f64) -> Option<f64> {
        if n == 0 { None } else { Some(sum / n as f64) }
    }
}

/// The JSON shape of a counter set (shared by the lifetime `counters`
/// and the range-scoped `range.counters` of the stats payload).
fn counters_json(c: &Counters) -> Value {
    let total = c.total;
    json!({
        "total": total,
        "ok": c.ok,
        "client_errors": c.client_errors,
        "server_errors": c.server_errors,
        "success_rate": if total == 0 { None } else { Some(c.ok as f64 / total as f64) },
        "cache_hits": c.cache_hits,
        "cache_hit_rate": if total == 0 { None } else { Some(c.cache_hits as f64 / total as f64) },
        "restored": c.restored,
        "big_requests": c.big_requests,
        "streamed": c.streamed,
        "coalesced_followers": c.coalesced_followers,
        "tokens": {
            "prompt": c.prompt_tokens,
            "completion": c.completion_tokens,
            "cached": c.cached_tokens,
        },
        "avg_duration_secs": Counters::avg(c.duration_n, c.duration_sum),
        "avg_ttft_ms": Counters::avg(c.ttft_n, c.ttft_sum),
        "avg_gen_tps": Counters::avg(c.gen_tps_n, c.gen_tps_sum),
        "avg_prefill_tps": Counters::avg(c.prefill_tps_n, c.prefill_tps_sum),
    })
}

/// The slim chart point of one bucket (the fields the page's bar charts
/// need; the full counters travel only in `counters` / `range`).
fn bucket_point(b: &Bucket) -> Value {
    json!({
        "t": b.t,
        "reqs": b.c.total,
        "completions": b.c.completion_tokens,
        "cached_tokens": b.c.cached_tokens,
    })
}

/// The plugin's mutable state (counters, time series, recent ring).
#[derive(Debug, Default)]
pub struct Inner {
    /// Lifetime counters.
    pub counters: Counters,
    /// Time-series buckets, oldest first, covering the trailing window.
    pub buckets: Vec<Bucket>,
    /// The `max_recent` most recent finished requests, oldest first.
    pub recent: Vec<RecentRow>,
}

impl Inner {
    /// Fold one finished request finished at `now` (unix seconds) into
    /// the state: counters, the lifetime time series (advancing the
    /// current bucket, bounded by [`MAX_BUCKETS`] — oldest first) and
    /// the recent-requests ring (bounded by `max_recent`).
    fn add_event(
        &mut self,
        now: u64,
        record: &RequestLogRecord,
        bucket_secs: u64,
        max_recent: usize,
    ) {
        self.counters.add(record);
        let t = (now / bucket_secs) * bucket_secs;
        match self.buckets.last_mut() {
            Some(b) if b.t == t => b.add(record),
            _ => {
                let mut b = Bucket::new(t);
                b.add(record);
                self.buckets.push(b);
            }
        }
        // Bound the lifetime series (it backs the "all time" filter):
        // past the cap the oldest buckets are dropped, while the
        // lifetime `counters` keep counting regardless.
        while self.buckets.len() > MAX_BUCKETS {
            self.buckets.remove(0);
        }
        if max_recent > 0 {
            self.recent.push(RecentRow {
                ts: now as f64,
                status: record.status.unwrap_or(0),
                duration_secs: record.duration_secs.unwrap_or(0.0),
                model: record.model.clone(),
                key: record.key.clone(),
                stream: record.stream.unwrap_or(false),
                restored: record.restored == Some(true),
                cache_hit: record.cache_hit == Some(true),
                coalesced: record.coalesced == Some("follower"),
                reason: record.reason.clone(),
                prompt_tokens: record.prompt_tokens,
                completion_tokens: record.completion_tokens,
                cached_tokens: record.cached_tokens,
                ttft_ms: record.ttft_ms,
                gen_tps: record.gen_tps,
            });
            while self.recent.len() > max_recent {
                self.recent.remove(0);
            }
        }
    }
}

/// The built-in dashboard adapter (see the module docs).
pub struct DashboardPlugin {
    path: String,
    refresh_ms: u64,
    window_secs: u64,
    bucket_secs: u64,
    max_recent: usize,
    inner: Mutex<Inner>,
}

/// Unix time (whole seconds) for the aggregates.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl DashboardPlugin {
    /// Create a plugin from a params set (validates it first).
    pub fn new(params: DashboardParams) -> Result<Arc<Self>, String> {
        validate(&params)?;
        Ok(Arc::new(Self {
            path: params.path,
            refresh_ms: params.refresh_ms,
            window_secs: params.window_secs,
            bucket_secs: params.bucket_secs,
            max_recent: params.max_recent,
            inner: Mutex::new(Inner::default()),
        }))
    }

    /// Record one finished request, timestamped with the current time.
    pub fn record_now(&self, record: &RequestLogRecord) -> Result<(), String> {
        self.record_at(now_secs() as f64, record)
    }

    /// Record one finished request at an explicit finish time (unix
    /// seconds) — test hook for deterministic series/pruning.
    pub fn record_at(&self, now: f64, record: &RequestLogRecord) -> Result<(), String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| format!("dashboard lock poisoned: {e}"))?;
        inner.add_event(now as u64, record, self.bucket_secs, self.max_recent);
        Ok(())
    }

    /// Coarsen `buckets` to at most [`MAX_SERIES_POINTS`] chart points:
    /// beyond the cap, consecutive buckets are merged into a step aligned
    /// to a multiple of `bucket_secs` (raw buckets are already aligned,
    /// so every merged point keeps an exact start time). Returns the
    /// effective step and the slim points.
    fn series_points(buckets: &[Bucket], bucket_secs: u64) -> (u64, Vec<Value>) {
        if buckets.len() <= MAX_SERIES_POINTS {
            return (bucket_secs, buckets.iter().map(bucket_point).collect());
        }
        let step = bucket_secs * buckets.len().div_ceil(MAX_SERIES_POINTS) as u64;
        let mut merged: Vec<Bucket> = Vec::with_capacity(MAX_SERIES_POINTS);
        for b in buckets {
            let t = (b.t / step) * step;
            match merged.last_mut() {
                Some(m) if m.t == t => m.c.add_bucket(b),
                _ => {
                    let mut m = Bucket::new(t);
                    m.c.add_bucket(b);
                    merged.push(m);
                }
            }
        }
        (step, merged.iter().map(bucket_point).collect())
    }

    /// The stats payload: the proxy's live [`ProxyStatus`] merged with
    /// the plugin's aggregates, range-scoped by `since` (unix seconds,
    /// from the `?since` query parameter):
    ///
    /// - `Some(s)` — `counters` stays lifetime; `range` carries the
    ///   counters of `[s, now]` (the lifetime counters themselves when
    ///   `s` is at or before the oldest retained bucket — exact while
    ///   nothing has been dropped past [`MAX_BUCKETS`]) and `series`
    ///   holds the range's buckets, coarsened to at most
    ///   [`MAX_SERIES_POINTS`] points. The page's time filter uses this.
    /// - `None` — the legacy shape: lifetime counters plus the trailing
    ///   `window_secs` of the series (as of now) and its rates.
    pub fn stats_inner(&self, proxy: &ProxyStatus, since: Option<u64>) -> Value {
        let inner = self.inner.lock().expect("dashboard lock poisoned");
        let payload = json!({
            "version": proxy.version,
            "uptime_secs": proxy.uptime_secs,
            "model": proxy.model_id,
            "window_secs": self.window_secs,
            "refresh_ms": self.refresh_ms,
            "proxy": serde_json::to_value(proxy).unwrap_or(Value::Null),
            "counters": counters_json(&inner.counters),
        });
        let mut payload = payload;
        match since {
            Some(s) => {
                // The range `[s, now]` at bucket granularity: the buckets
                // that start at or after `s` (a bucket covers
                // `[t, t + bucket_secs)`).
                let start = inner.buckets.partition_point(|b| b.t < s);
                let range_buckets = &inner.buckets[start..];
                let range_counters = if start == 0 {
                    inner.counters.clone()
                } else {
                    Counters::sum(range_buckets.iter())
                };
                let (step, points) = Self::series_points(range_buckets, self.bucket_secs);
                payload["range"] = json!({
                    "since": s,
                    "counters": counters_json(&range_counters),
                });
                payload["series"] = json!({
                    "bucket_secs": step,
                    "buckets": points,
                });
            }
            None => {
                // Legacy shape: the trailing `window_secs` of the series
                // (as of now) and its rates.
                let now = now_secs();
                let window: Vec<&Bucket> = inner
                    .buckets
                    .iter()
                    .filter(|b| now.saturating_sub(b.t) < self.window_secs)
                    .collect();
                let reqs_window: u64 = window.iter().map(|b| b.c.total).sum();
                let toks_window: u64 = window.iter().map(|b| b.c.completion_tokens).sum();
                let w = self.window_secs as f64;
                payload["series"] = json!({
                    "bucket_secs": self.bucket_secs,
                    "window_reqs": reqs_window,
                    "window_completions": toks_window,
                    "req_per_sec": reqs_window as f64 / w,
                    "tok_per_sec": toks_window as f64 / w,
                    "buckets": window.iter().map(|b| bucket_point(b)).collect::<Vec<_>>(),
                });
            }
        }
        payload["recent"] = serde_json::to_value(&inner.recent).unwrap_or(Value::Null);
        payload
    }
}

impl Dashboard for DashboardPlugin {
    fn name(&self) -> &str {
        NAME
    }

    fn path(&self) -> &str {
        &self.path
    }

    fn html(&self) -> String {
        page_html()
            .replace("__REFRESH_MS__", &self.refresh_ms.to_string())
            .replace("__STATS_PATH__", &self.stats_path())
    }

    fn stats(&self, proxy: &ProxyStatus, since: Option<u64>) -> Value {
        self.stats_inner(proxy, since)
    }

    fn record(&self, record: &RequestLogRecord) -> Result<(), String> {
        self.record_now(record)
    }
}

/// The `dashboard` plugin factory (JSON params per the module docs).
pub struct DashboardPluginFactory;

impl PluginFactory for DashboardPluginFactory {
    fn create_dashboard(&self, params: &Value) -> Result<Option<Arc<dyn Dashboard>>, PluginError> {
        let parsed = match serde_json::from_value::<DashboardParams>(params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return Err(PluginError::BadParams {
                    name: NAME.to_string(),
                    message: format!("invalid params: {e}"),
                });
            }
        };
        match DashboardPlugin::new(parsed) {
            Ok(d) => Ok(Some(d)),
            Err(e) => Err(PluginError::BadParams {
                name: NAME.to_string(),
                message: e,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// The dashboard page (self-contained: inline CSS + JS, no external assets).
// `__REFRESH_MS__` / `__STATS_PATH__` are substituted at render time.
// ---------------------------------------------------------------------------

const PAGE_HEAD: &str = r##"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>lpcache dashboard</title>
<style>
:root { --bg:#0d1117; --panel:#161b22; --border:#30363d; --text:#e6edf3; --dim:#8b949e; --accent:#58a6ff; --ok:#3fb950; --warn:#d29922; --err:#f85149; }
* { box-sizing: border-box; }
body { margin:0; background:var(--bg); color:var(--text); font:14px/1.45 -apple-system, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; }
header { display:flex; justify-content:space-between; align-items:baseline; padding:14px 22px; border-bottom:1px solid var(--border); background:var(--panel); position:sticky; top:0; }
h1 { font-size:17px; margin:0; }
h1 .sub { color:var(--dim); font-weight:400; font-size:13px; }
#meta { display:flex; gap:14px; color:var(--dim); font-size:12px; align-items:center; }
#live { color:var(--ok); }
#live.down { color:var(--err); }
main { padding:16px 22px 30px; display:flex; flex-direction:column; gap:14px; max-width:1280px; margin:0 auto; }
.cards { display:grid; grid-template-columns:repeat(auto-fit, minmax(150px,1fr)); gap:10px; }
.card { background:var(--panel); border:1px solid var(--border); border-radius:8px; padding:10px 12px; }
.card .lbl { color:var(--dim); font-size:11px; text-transform:uppercase; letter-spacing:.05em; }
.card .v { font-size:20px; font-weight:600; margin-top:2px; }
.card .s { color:var(--dim); font-size:11px; margin-top:2px; min-height:14px; }
.panel { background:var(--panel); border:1px solid var(--border); border-radius:8px; padding:12px 14px; }
.panel h2 { font-size:13px; margin:0 0 10px; color:var(--dim); text-transform:uppercase; letter-spacing:.05em; }
table { width:100%; border-collapse:collapse; font-size:12.5px; }
th { text-align:left; color:var(--dim); font-weight:600; padding:4px 8px; border-bottom:1px solid var(--border); white-space:nowrap; }
td { padding:4px 8px; border-bottom:1px solid #21262d; white-space:nowrap; }
tr:last-child td { border-bottom:none; }
.badge { display:inline-block; padding:1px 7px; border-radius:10px; font-size:11px; font-weight:600; }
.b-ok { background:rgba(63,185,80,.15); color:var(--ok); }
.b-warn { background:rgba(210,153,34,.15); color:var(--warn); }
.b-err { background:rgba(248,81,73,.15); color:var(--err); }
.b-dim { background:rgba(139,148,158,.15); color:var(--dim); }
.b-acc { background:rgba(88,166,255,.15); color:var(--accent); }
.slots { display:flex; flex-wrap:wrap; gap:6px; }
.slot { width:42px; height:42px; border-radius:6px; border:1px solid var(--border); background:#21262d; display:flex; align-items:center; justify-content:center; font-size:11px; color:var(--dim); cursor:default; }
.slot.busy { background:rgba(210,153,34,.3); border-color:var(--warn); color:var(--text); }
.slot.down { background:rgba(248,81,73,.25); border-color:var(--err); color:var(--text); }
.chartwrap { margin-bottom:8px; }
.chartwrap .cap { display:flex; justify-content:space-between; color:var(--dim); font-size:11px; margin-bottom:4px; }
canvas { width:100%; height:110px; display:block; }
footer { color:var(--dim); font-size:11px; text-align:center; padding:0 0 18px; }
.empty { color:var(--dim); padding:8px 0; }
.filters { display:flex; align-items:center; gap:10px; flex-wrap:wrap; background:var(--panel); border:1px solid var(--border); border-radius:8px; padding:8px 12px; }
.flbl { color:var(--dim); font-size:11px; text-transform:uppercase; letter-spacing:.05em; }
.fsel { background:var(--bg); border:1px solid var(--border); color:var(--text); border-radius:6px; padding:4px 10px; font-size:13px; cursor:pointer; }
.fsel:hover, .fsel:focus { border-color:var(--accent); outline:none; }
#custom-wrap { display:flex; align-items:center; gap:6px; }
#custom-secs { width:90px; background:var(--bg); border:1px solid var(--border); color:var(--text); border-radius:6px; padding:3px 8px; font-size:12px; }
#custom-apply { background:#21262d; border:1px solid var(--border); color:var(--text); border-radius:6px; padding:3px 10px; font-size:12px; cursor:pointer; }
#custom-apply:hover { border-color:var(--accent); }
</style>
</head>
<body>
"##;
const PAGE_BODY: &str = r##"
<header>
  <h1>lpcache <span class="sub">proxy cache dashboard</span></h1>
  <div id="meta"><span id="version"></span><span id="model"></span><span id="uptime"></span><span id="live">connecting&hellip;</span></div>
</header>
<main>
  <section class="filters">
    <span class="flbl">time</span>
    <select id="filter" class="fsel" aria-label="time range filter">
      <optgroup label="presets">
        <option value="30">30 seconds</option>
        <option value="60">1 minute</option>
        <option value="300" selected>5 minutes</option>
        <option value="900">15 minutes</option>
      </optgroup>
      <optgroup label="other">
        <option value="custom">custom time</option>
        <option value="all">all time</option>
      </optgroup>
    </select>
    <span id="custom-wrap" hidden>
      <input id="custom-secs" type="number" min="1" step="1" placeholder="seconds">
      <button type="button" id="custom-apply">apply</button>
    </span>
  </section>
  <section class="cards">
    <div class="card"><div class="lbl">requests</div><div class="v" id="c-req-v">&ndash;</div><div class="s" id="c-req-s"></div></div>
    <div class="card"><div class="lbl">success</div><div class="v" id="c-succ-v">&ndash;</div><div class="s" id="c-succ-s"></div></div>
    <div class="card"><div class="lbl">cache hits</div><div class="v" id="c-hit-v">&ndash;</div><div class="s" id="c-hit-s"></div></div>
    <div class="card"><div class="lbl">kv restores</div><div class="v" id="c-rest-v">&ndash;</div><div class="s" id="c-rest-s"></div></div>
    <div class="card"><div class="lbl">slots in flight</div><div class="v" id="c-slot-v">&ndash;</div><div class="s" id="c-slot-s"></div></div>
    <div class="card"><div class="lbl">coalesced groups</div><div class="v" id="c-grp-v">&ndash;</div><div class="s" id="c-grp-s"></div></div>
    <div class="card"><div class="lbl">request rate</div><div class="v" id="c-rate-v">&ndash;</div><div class="s" id="c-rate-s">req/s &middot; window avg</div></div>
    <div class="card"><div class="lbl">token rate</div><div class="v" id="c-tok-v">&ndash;</div><div class="s" id="c-tok-s">completion tok/s</div></div>
    <div class="card"><div class="lbl">kv meta files</div><div class="v" id="c-meta-v">&ndash;</div><div class="s" id="c-meta-s"></div></div>
    <div class="card"><div class="lbl">avg duration</div><div class="v" id="c-dur-v">&ndash;</div><div class="s">end to end</div></div>
    <div class="card"><div class="lbl">avg ttft</div><div class="v" id="c-ttft-v">&ndash;</div><div class="s">streaming only</div></div>
    <div class="card"><div class="lbl">avg gen rate</div><div class="v" id="c-gtps-v">&ndash;</div><div class="s">tok/s (backend)</div></div>
  </section>
  <section class="panel">
    <h2>throughput</h2>
    <div class="chartwrap"><div class="cap"><span>requests per bucket</span><span id="cap-req"></span></div><canvas id="chart-req"></canvas></div>
    <div class="chartwrap"><div class="cap"><span>completion tokens per bucket</span><span id="cap-tok"></span></div><canvas id="chart-tok"></canvas></div>
    <div class="chartwrap"><div class="cap"><span>cached (restored) prompt tokens per bucket</span><span id="cap-cached"></span></div><canvas id="chart-cached"></canvas></div>
  </section>
  <section class="panel"><h2>backends</h2><div id="backends"></div></section>
  <section class="panel"><h2>slots</h2><div id="slots" class="slots"></div></section>
  <section class="panel"><h2>recent requests</h2><div id="recent"></div></section>
</main>
<footer id="foot"></footer>
"##;
const PAGE_SCRIPT: &str = r##"
<script>
const REFRESH = __REFRESH_MS__;
const STATS = "__STATS_PATH__";
const $ = (id) => document.getElementById(id);
// Time filter: "30" | "60" | "300" | "900" | "custom" | "all". The
// selection scopes the cards, charts and recent table: every poll is
// sent to `{stats}?since=<unix seconds>` (no param = all time).
let FILTER = "300";
let CUSTOM_SECS = 600;
function rangeSecs(){
  if(FILTER==="all") return null;
  if(FILTER==="custom") return CUSTOM_SECS;
  return parseInt(FILTER, 10);
}
function rangeLabel(){
  if(FILTER==="all") return "all time";
  if(FILTER==="custom") return "custom time (last "+CUSTOM_SECS+" s)";
  const m = {"30":"30 seconds","60":"1 minute","300":"5 minutes","900":"15 minutes"};
  return "last "+(m[FILTER] || FILTER+" s");
}
function statsUrl(){
  const rs = rangeSecs();
  return rs==null ? STATS : STATS+"?since="+Math.max(0, Math.floor(Date.now()/1000)-rs);
}
function fmtN(v){ return v==null ? "\u2013" : Math.round(v).toLocaleString(); }
function fmtDur(v){ if(v==null) return "\u2013"; if(v<1) return Math.round(v*1000)+" ms"; if(v<120) return v.toFixed(1)+" s"; if(v<7200) return Math.floor(v/60)+" m "+Math.round(v%60)+" s"; return Math.floor(v/3600)+" h "+Math.floor((v%3600)/60)+" m"; }
function fmtUp(v){ if(v==null) return "\u2013"; const s=Math.floor(v); if(s<3600) return Math.floor(s/60)+" m "+(s%60)+" s"; if(s<86400) return Math.floor(s/3600)+" h "+Math.floor((s%3600)/60)+" m"; return Math.floor(s/86400)+" d "+Math.floor((s%86400)/3600)+" h"; }
function pct(v){ return v==null ? "\u2013" : (v*100).toFixed(1)+" %"; }
function card(id, v, s){ $(id+"-v").textContent=v; if(s!=null) $(id+"-s").textContent=s; }
function esc(s){ const d=document.createElement("span"); d.textContent=s==null?"":s; return d.innerHTML; }
function statusBadge(st){ const cls = st<400 ? "b-ok" : (st<500 ? "b-warn" : "b-err"); return '<span class="badge '+cls+'">'+st+'</span>'; }
// Re-align sparse buckets onto a gapless timeline (missing = zero).
function fillSeries(buckets, bucketSecs){
  if(!buckets || !buckets.length) return [];
  const out = [];
  const first = buckets[0].t, last = buckets[buckets.length-1].t;
  for(let t=first; t<=last; t+=bucketSecs){
    let b = null;
    for(const x of buckets){ if(x.t===t){ b=x; break; } }
    out.push(b || {t:t, reqs:0, completions:0, cached_tokens:0});
  }
  return out;
}
function drawBars(cv, values, color){
  const dpr = window.devicePixelRatio || 1;
  const w = cv.clientWidth || 700, h = 110;
  cv.width = w*dpr; cv.height = h*dpr;
  const ctx = cv.getContext("2d");
  ctx.scale(dpr,dpr);
  ctx.clearRect(0,0,w,h);
  if(!values.length){ ctx.fillStyle="#8b949e"; ctx.font="11px sans-serif"; ctx.fillText("no traffic yet", 2, 12); return; }
  let max = 1;
  for(const v of values){ if(v>max) max=v; }
  const n = values.length, bw = w/n;
  values.forEach((v,i)=>{
    const bh = v>0 ? Math.max(2, Math.round((v/max)*(h-18))) : 0;
    ctx.fillStyle = color;
    ctx.fillRect(i*bw+1, h-4-bh, Math.max(1,bw-2), bh);
  });
  ctx.fillStyle = "#8b949e";
  ctx.font = "11px sans-serif";
  ctx.fillText("max "+max, 2, 12);
  ctx.strokeStyle = "#30363d";
  ctx.beginPath(); ctx.moveTo(0,h-3.5); ctx.lineTo(w,h-3.5); ctx.stroke();
}
function render(d){
  const p=d.proxy, s=d.series;
  // The selection scopes the cards/charts: with `?since` the payload
  // carries `range.counters` for the selected range (fall back to the
  // lifetime counters only if the server predates the range endpoint).
  const ranged = d.range != null;
  const c = ranged ? d.range.counters : d.counters;
  const secs = ranged ? (rangeSecs()==null ? Math.max(1, Math.floor(d.uptime_secs)||1) : Math.max(1, rangeSecs())) : Math.max(1, d.window_secs);
  $("version").textContent = "v"+d.version;
  $("model").textContent = d.model;
  $("uptime").textContent = "up "+fmtUp(d.uptime_secs);
  card("c-req", fmtN(c.total), "ok "+fmtN(c.ok)+" \u00b7 4xx "+fmtN(c.client_errors)+" \u00b7 5xx "+fmtN(c.server_errors));
  card("c-succ", pct(c.success_rate), "errors "+fmtN(c.client_errors+c.server_errors));
  card("c-hit", pct(c.cache_hit_rate), "hits "+fmtN(c.cache_hits)+" of "+fmtN(c.total));
  card("c-rest", fmtN(c.restored), "cached tokens "+fmtN(c.tokens.cached));
  card("c-slot", p.in_flight+" / "+p.total_slots, "held / total slots");
  card("c-grp", p.active_groups, "coalesce "+(p.coalesce_requests?"on":"off")+" \u00b7 followers "+fmtN(c.coalesced_followers));
  const rps = ranged ? c.total/secs : s.req_per_sec;
  const tps = ranged ? c.tokens.completion/secs : s.tok_per_sec;
  card("c-rate", rps.toFixed(2), "req/s \u00b7 "+rangeLabel());
  card("c-tok", tps.toFixed(1), "completion tok/s \u00b7 "+rangeLabel());
  card("c-meta", p.meta_files, p.meta_max>0 ? "lru max "+p.meta_max : "unlimited");
  card("c-dur", fmtDur(c.avg_duration_secs));
  card("c-ttft", c.avg_ttft_ms==null ? "\u2013" : Math.round(c.avg_ttft_ms)+" ms");
  card("c-gtps", c.avg_gen_tps==null ? "\u2013" : c.avg_gen_tps.toFixed(1));
  const series = fillSeries(s.buckets, s.bucket_secs);
  drawBars($("chart-req"), series.map(b=>b.reqs), "#58a6ff");
  drawBars($("chart-tok"), series.map(b=>b.completions), "#3fb950");
  drawBars($("chart-cached"), series.map(b=>b.cached_tokens), "#d29922");
  const span = series.length ? series.length*s.bucket_secs : 0;
  const spanLabel = span ? (ranged ? rangeLabel() : span+" s window")+" \u00b7 "+s.bucket_secs+" s buckets" : "";
  $("cap-req").textContent = spanLabel;
  $("cap-tok").textContent = spanLabel;
  $("cap-cached").textContent = spanLabel;
  let bt = "<table><tr><th>url</th><th>state</th><th>cooldown</th><th>fail streak</th><th>slots</th></tr>";
  p.backends.forEach((b,i)=>{
    bt += "<tr><td>"+esc(b.url)+"</td><td>"+(b.down ? '<span class="badge b-err">DOWN</span>' : '<span class="badge b-ok">UP</span>')+"</td><td>"+(b.cooldown_secs>0 ? b.cooldown_secs.toFixed(0)+" s" : "\u2013")+"</td><td>"+(b.failure_streak||"\u2013")+"</td><td>"+b.busy_slots+" / "+b.n_slots+"</td></tr>";
  });
  bt += "</table>";
  $("backends").innerHTML = bt;
  let sl = "";
  p.slots.forEach(g=>{
    const be = p.backends[g.be];
    const down = be && be.down;
    const cls = down ? "slot down" : (g.busy ? "slot busy" : "slot");
    const age = g.last_used_secs==null ? "never used" : ("used "+Math.round(g.last_used_secs)+" s ago");
    sl += '<div class="'+cls+'" title="backend '+g.be+" slot "+g.slot+" \u00b7 "+(g.busy?"busy":"idle")+" \u00b7 "+age+'">'+g.slot+'</div>';
  });
  $("slots").innerHTML = sl || '<div class="empty">no slots</div>';
  let rows = (d.recent||[]).slice().reverse();
  if(ranged) rows = rows.filter(r=>r.ts >= (d.range.since==null ? 0 : d.range.since));
  if(!rows.length){ $("recent").innerHTML = '<div class="empty">no finished requests yet</div>'; }
  else {
    let rt = "<table><tr><th>time</th><th>status</th><th>duration</th><th>model</th><th>key</th><th>flags</th><th>tokens (prompt/compl/cached)</th><th>ttft</th><th>gen tps</th><th>note</th></tr>";
    rows.forEach(r=>{
      const flags = [];
      if(r.cache_hit) flags.push('<span class="badge b-acc">cache</span>');
      if(r.restored) flags.push('<span class="badge b-ok">restore</span>');
      if(r.coalesced) flags.push('<span class="badge b-dim">follower</span>');
      if(r.stream) flags.push('<span class="badge b-dim">stream</span>');
      const when = new Date(r.ts*1000).toLocaleTimeString();
      const toks = r.prompt_tokens!=null ? r.prompt_tokens+" / "+(r.completion_tokens!=null?r.completion_tokens:"?")+" / "+(r.cached_tokens!=null?r.cached_tokens:0) : "\u2013";
      rt += "<tr><td>"+when+"</td><td>"+statusBadge(r.status)+"</td><td>"+fmtDur(r.duration_secs)+"</td><td>"+esc(r.model)+"</td><td>"+(r.key?esc(r.key):"\u2013")+"</td><td>"+flags.join(" ")+"</td><td>"+toks+"</td><td>"+(r.ttft_ms!=null?Math.round(r.ttft_ms)+" ms":"\u2013")+"</td><td>"+(r.gen_tps!=null?r.gen_tps.toFixed(1):"\u2013")+"</td><td>"+(r.reason?esc(r.reason):"")+"</td></tr>";
    });
    rt += "</table>";
    $("recent").innerHTML = rt;
  }
  $("foot").textContent = "filter "+rangeLabel()+" \u00b7 buckets "+s.bucket_secs+" s \u00b7 poll "+d.refresh_ms+" ms";
  $("live").textContent = "\u25cf live";
  $("live").className = "";
}
async function poll(){
  try {
    const r = await fetch(statsUrl(), {cache:"no-store"});
    if(!r.ok) throw new Error("HTTP "+r.status);
    render(await r.json());
  } catch(e) {
    $("live").textContent = "\u25cf offline ("+e+")";
    $("live").className = "down";
  }
}
async function tick(){
  await poll();
  setTimeout(tick, REFRESH);
}
// Time filter dropdown: changing it re-renders immediately (the
// periodic tick above keeps running on its own schedule).
function setFilter(r){
  FILTER = r;
  $("filter").value = r;
  $("custom-wrap").hidden = (r!=="custom");
  if(r==="custom") $("custom-secs").focus();
}
$("filter").addEventListener("change", ()=>{ setFilter($("filter").value); poll(); });
function applyCustom(){
  const v = parseInt($("custom-secs").value, 10);
  if(isNaN(v) || v<1) return;
  CUSTOM_SECS = v;
  $("custom-secs").value = v;
  setFilter("custom");
  poll();
}
$("custom-apply").addEventListener("click", applyCustom);
$("custom-secs").addEventListener("keydown", e=>{ if(e.key==="Enter") applyCustom(); });
tick();
</script>
</body>
</html>
"##;

/// The full dashboard page template (assembled from the parts above).
fn page_html() -> String {
    let mut s = String::with_capacity(PAGE_HEAD.len() + PAGE_BODY.len() + PAGE_SCRIPT.len());
    s.push_str(PAGE_HEAD);
    s.push_str(PAGE_BODY);
    s.push_str(PAGE_SCRIPT);
    s
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(p: Value) -> DashboardParams {
        let mut base = DashboardParams::default();
        if let Some(v) = p.get("path") {
            base.path = v.as_str().unwrap().to_string();
        }
        if let Some(v) = p.get("refresh_ms") {
            base.refresh_ms = v.as_u64().unwrap();
        }
        if let Some(v) = p.get("window_secs") {
            base.window_secs = v.as_u64().unwrap();
        }
        if let Some(v) = p.get("bucket_secs") {
            base.bucket_secs = v.as_u64().unwrap();
        }
        if let Some(v) = p.get("max_recent") {
            base.max_recent = v.as_u64().unwrap() as usize;
        }
        base
    }

    fn record(status: u16) -> RequestLogRecord {
        RequestLogRecord {
            method: Some("POST".into()),
            uri: Some("/v1/chat/completions".into()),
            duration_secs: Some(1.5),
            status: Some(status),
            model: Some("llama.cpp".into()),
            stream: Some(false),
            key: Some("ab12cd34ef56ab78".into()),
            n_words: Some(900),
            is_big: Some(true),
            restored: Some(true),
            cache_hit: Some(false),
            coalesced: Some("follower"),
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            cached_tokens: Some(80),
            ttft_ms: Some(120.0),
            gen_tps: Some(25.0),
            prefill_tps: Some(500.0),
            ..Default::default()
        }
    }

    fn proxy() -> ProxyStatus {
        ProxyStatus {
            version: "test".into(),
            uptime_secs: 42.0,
            model_id: "llama.cpp".into(),
            port: 8081,
            coalesce_requests: true,
            big_threshold_words: 500,
            words_per_block: 100,
            meta_max: 10,
            meta_files: 3,
            in_flight: 1,
            total_slots: 2,
            active_groups: 0,
            backends: vec![crate::api::BackendStatus {
                url: "http://127.0.0.1:8000".into(),
                n_slots: 2,
                busy_slots: 1,
                down: false,
                cooldown_secs: 0.0,
                failure_streak: 0,
            }],
            slots: vec![
                crate::api::SlotStatus {
                    be: 0,
                    slot: 0,
                    busy: true,
                    last_used_secs: Some(5.0),
                },
                crate::api::SlotStatus {
                    be: 0,
                    slot: 1,
                    busy: false,
                    last_used_secs: None,
                },
            ],
        }
    }

    #[test]
    fn factory_defaults_and_validation() {
        let f = DashboardPluginFactory;
        let d = match f.create_dashboard(&json!({})) {
            Ok(Some(d)) => d,
            _ => panic!("expected a dashboard with default params"),
        };
        assert_eq!(d.name(), NAME);
        assert_eq!(d.path(), "/dashboard");
        assert!(d.html().contains("lpcache"));
        // `dyn Dashboard` is not `Debug`, so match instead of unwrap_err.
        let err = match f.create_dashboard(&json!({ "path": "nope" })) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert_eq!(
            err.to_string(),
            "invalid params for plugin \"dashboard\": `path` must start with '/' (got \"nope\")"
        );
        // an unknown field is rejected
        let err = match f.create_dashboard(&json!({ "nope": 1 })) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(err.to_string().contains("invalid params"));
    }

    #[test]
    fn stats_path_joins_correctly() {
        let d = DashboardPlugin::new(params(json!({}))).unwrap();
        assert_eq!(d.stats_path(), "/dashboard/api/stats");
        let d = DashboardPlugin::new(params(json!({ "path": "/stats/" }))).unwrap();
        assert_eq!(d.stats_path(), "/stats/api/stats");
    }

    #[test]
    fn record_updates_counters_series_and_recent() {
        let d = DashboardPlugin::new(params(
            json!({ "window_secs": 30, "bucket_secs": 10, "max_recent": 2 }),
        ))
        .unwrap();
        // t=100 -> bucket 100; t=109 -> still bucket 100; t=125 -> bucket 120
        d.record_at(100.0, &record(200)).unwrap();
        d.record_at(109.0, &record(200)).unwrap();
        d.record_at(125.0, &record(400)).unwrap();
        // `since=0` = the whole retained history (the "all time" filter).
        let v = d.stats(&proxy(), Some(0));
        assert_eq!(v["counters"]["total"], 3);
        assert_eq!(v["counters"]["ok"], 2);
        assert_eq!(v["counters"]["client_errors"], 1);
        assert_eq!(v["counters"]["cache_hits"], 0);
        assert_eq!(v["counters"]["restored"], 3);
        assert_eq!(v["counters"]["coalesced_followers"], 3);
        assert_eq!(v["counters"]["tokens"]["completion"], 150);
        assert_eq!(v["counters"]["avg_ttft_ms"], 120.0);
        assert_eq!(v["series"]["buckets"].as_array().unwrap().len(), 2);
        assert_eq!(v["series"]["buckets"][0]["t"], 100);
        assert_eq!(v["series"]["buckets"][0]["reqs"], 2);
        assert_eq!(v["series"]["buckets"][1]["t"], 120);
        assert_eq!(v["series"]["buckets"][1]["reqs"], 1);
        // the recent ring keeps the two newest (ring size 2)
        let recent = v["recent"].as_array().unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0]["status"], 200);
        assert_eq!(recent[1]["status"], 400);
        // the proxy snapshot rides along
        assert_eq!(v["proxy"]["in_flight"], 1);
        assert_eq!(v["proxy"]["slots"][0]["busy"], true);
        // success_rate = 2/3
        let r = v["counters"]["success_rate"].as_f64().unwrap();
        assert!((r - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn since_scopes_range_counters_and_series() {
        let d = DashboardPlugin::new(params(
            json!({ "window_secs": 30, "bucket_secs": 10, "max_recent": 0 }),
        ))
        .unwrap();
        d.record_at(100.0, &record(200)).unwrap();
        d.record_at(200.0, &record(400)).unwrap();
        // `since=150` covers only the bucket at 200 (t=100 starts before it).
        let v = d.stats(&proxy(), Some(150));
        assert_eq!(v["range"]["since"], 150);
        assert_eq!(v["range"]["counters"]["total"], 1);
        assert_eq!(v["range"]["counters"]["client_errors"], 1);
        let buckets = v["series"]["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0]["t"], 200);
        // max_recent 0 -> no recent list at all
        assert!(v["recent"].as_array().unwrap().is_empty());
        // counters are lifetime — still 2
        assert_eq!(v["counters"]["total"], 2);
        // `since` at or before the oldest bucket yields the lifetime counters
        let v = d.stats(&proxy(), Some(0));
        assert_eq!(v["range"]["counters"]["total"], 2);
        assert_eq!(v["series"]["buckets"].as_array().unwrap().len(), 2);
        // a future `since` is an empty range
        let v = d.stats(&proxy(), Some(2000));
        assert_eq!(v["range"]["counters"]["total"], 0);
        assert!(v["range"]["counters"]["success_rate"].is_null());
        assert!(v["series"]["buckets"].as_array().unwrap().is_empty());
    }

    #[test]
    fn long_range_series_is_coarsened() {
        let d = DashboardPlugin::new(params(
            json!({ "window_secs": 10, "bucket_secs": 1, "max_recent": 0 }),
        ))
        .unwrap();
        // 1000 one-second buckets — far more than MAX_SERIES_POINTS.
        for t in 0..1000u64 {
            d.record_at(t as f64, &record(200)).unwrap();
        }
        let v = d.stats(&proxy(), Some(0));
        assert_eq!(v["range"]["counters"]["total"], 1000);
        // step = 1 s * ceil(1000/360) = 3 s; groups 0, 3, ..., 999 -> 334
        assert_eq!(v["series"]["bucket_secs"], 3);
        let buckets = v["series"]["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 334);
        assert!(buckets.len() <= MAX_SERIES_POINTS);
        let reqs: u64 = buckets.iter().map(|b| b["reqs"].as_u64().unwrap()).sum();
        assert_eq!(reqs, 1000);
    }

    #[test]
    fn oldest_buckets_are_capped() {
        // Feed the inner state directly (bypassing the wall clock):
        // past MAX_BUCKETS the oldest buckets are dropped while the
        // lifetime counters keep counting.
        let rec = record(200);
        let mut inner = Inner::default();
        for t in 0..(MAX_BUCKETS as u64 + 10) {
            inner.add_event(t, &rec, 1, 0);
        }
        assert_eq!(inner.buckets.len(), MAX_BUCKETS);
        assert_eq!(inner.buckets[0].t, 10);
        assert_eq!(inner.counters.total, MAX_BUCKETS as u64 + 10);
    }

    #[test]
    fn html_is_self_contained_and_configured() {
        let d = DashboardPlugin::new(params(json!({ "refresh_ms": 750 }))).unwrap();
        let page = d.html();
        assert!(page.contains("const REFRESH = 750;"));
        assert!(page.contains("const STATS = \"/dashboard/api/stats\";"));
        assert!(page.contains("<!doctype html>"));
        assert!(page.ends_with("</html>\n"));
        // the time filter dropdown and its custom-range control
        assert!(page.contains("<select id=\"filter\""));
        // options are grouped into presets and other
        assert!(page.contains("<optgroup label=\"presets\">"));
        assert!(page.contains("<optgroup label=\"other\">"));
        for r in ["30", "60", "300", "900", "custom", "all"] {
            assert!(page.contains(&format!("value=\"{r}\"")), "filter {r}");
        }
        assert!(page.contains("id=\"custom-secs\""));
        assert!(!page.contains("http://")); // no external assets
        assert!(!page.contains("__REFRESH_MS__"));
        assert!(!page.contains("__STATS_PATH__"));
    }
}
