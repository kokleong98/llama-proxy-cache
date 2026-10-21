//! Configuration (port of `config.py`).
//!
//! Environment variables (same names/defaults as the Python version):
//! - `BACKENDS`: JSON array of `{"url": "...", "n_slots": N}`; a parse error
//!   yields an empty backend list (same as Python).
//! - `LLAMA_URL` / `N_SLOTS`: fallback when `BACKENDS` is not set
//!   (defaults `http://127.0.0.1:8000`, `1`).
//! - `WORDS_PER_BLOCK` (100), `BIG_THRESHOLD_WORDS` (500), `LCP_TH` (0.1)
//! - `META_DIR` (`kv_meta`, joined to the current working directory)
//! - `META_MAX` (10; LRU bound on the number of meta files kept,
//!   0 = unlimited)
//! - `SLOT_SAVE_PATH` (optional; the backend's `--slot-save-path`
//!   directory — pruned meta files' KV files are removed from it)
//! - `REQUEST_TIMEOUT` (600 s), `MODEL_ID` (`llama.cpp`), `PORT` (8081)
//! - `LLAMA_API_KEY` (optional: when set, the proxy sends
//!   `Authorization: Bearer <key>` to the backend — llama-server's
//!   `--api-key`)
//! - `COALESCE_REQUESTS` (`false`; when true, concurrent requests with the
//!   same KV cache key are grouped into a single backend call regardless of
//!   generation parameters — see the `coalesce` module)
//! - `SAVE_RETRIES` (3) and `SAVE_RETRY_DELAY_MS` (1000): a failed KV cache
//!   save (backend 500 or network/other error) is retried by the slot
//!   manager up to `SAVE_RETRIES` times, waiting `SAVE_RETRY_DELAY_MS`
//!   milliseconds between attempts (`0` retries = disabled)
//! - `STREAM_QUEUE_SIZE` (16; capacity of the per-request bounded channel
//!   that buffers streamed SSE bytes between the background reader and the
//!   HTTP response — smaller values backpressure the backend faster)
//! - `PREFILTER_BLOCKLIST` (legacy; maps to the `keyword` plugin:
//!   comma-separated keywords; when set, requests whose message contents
//!   contain any keyword are rejected with `400` before any slot/backend
//!   work)
//! - `PREFILTER_CASE_INSENSITIVE` (`true`; keyword matching is
//!   case-insensitive)
//! - `PREFILTER_RESULT_CACHE_DIR` (legacy; maps to the `result_cache`
//!   plugin: the cache-result path; when set, the result-cache prefilter
//!   answers fresh same-key requests with a previously cached backend
//!   result before any slot/backend work, and fresh non-streaming results
//!   are stored under `{dir}/{key}.json`)
//! - `PREFILTER_RESULT_CACHE_TTL` (300; per-entry expiry in seconds,
//!   `0` = entries never expire)
//! - `PLUGINS` (optional: a JSON array of plugin specs
//!   `[{"name": "...", "params": {...}}, ...]` — the prefilter plugins to
//!   load from the `lpcache-plugins` registry (built-ins: `keyword`,
//!   `chain`, `result_cache`) plus, when present, the `request_log`
//!   plugin (a spec in `PLUGINS` takes precedence over the `REQUEST_LOG`
//!   settings) and at most one `dashboard` plugin (the real-time
//!   statistics web page; when present, the page is served at its
//!   configured `path`, default `/dashboard`). Custom plugin crates can
//!   register more.
//!   When set, the specs are used as-is and the legacy `PREFILTER_*`
//!   settings above are **ignored**; when unset, the specs are
//!   synthesized from them (see [`Config::plugin_specs`])
//! - `REQUEST_LOG` (`false`; when true, one W3C Extended Log Format (XLF)
//!   record per finished request is appended to the file named by
//!   `REQUEST_LOG_PATH` — the built-in `request_log` plugin)
//! - `REQUEST_LOG_PATH` (`lpcache-request.log`; relative paths resolve
//!   against the current working directory; the parent dir is created)
//! - `REQUEST_LOG_MAX_BYTES` (10 MiB; the file is rotated when it grows
//!   beyond this size — `<path>.1`, `<path>.2`, ...; `0` = no rotation)
//! - `REQUEST_LOG_KEEP` (5; rotated generations retained, the oldest is
//!   dropped; `0` = keep them all)
//! - `REQUEST_LOG_FIELDS` (optional; comma-separated ordered subset of the
//!   known XLF field names — W3C fields `datetime`, `cs-method`,
//!   `cs-uri`, `c-ip`, `cs(User-Agent)`, `cs-bytes`, `cs-time`,
//!   `sc-status`, `sc-bytes` plus lpcache pipeline fields `lpcache-model`,
//!   `lpcache-stream`, `lpcache-key`, `lpcache-n-words`, `lpcache-is-big`,
//!   `lpcache-backend`, `lpcache-slot`, `lpcache-restored`,
//!   `lpcache-cache-hit`, `lpcache-coalesced`, `lpcache-reason` and
//!   lpcache performance fields `lpcache-prompt-tokens`,
//!   `lpcache-completion-tokens`, `lpcache-cached-tokens`,
//!   `lpcache-prefill-ms`, `lpcache-gen-ms`, `lpcache-prefill-tps`,
//!   `lpcache-gen-tps`, `lpcache-ttft-ms`; default: all of them)
//! - `LOG_LEVEL` (`INFO`)
//!
//! Every variable can also be set with a command-line flag (see [`Cli`]);
//! explicit flags take precedence over the environment, which takes
//! precedence over the built-in defaults.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_BACKEND_URL: &str = "http://127.0.0.1:8000";
pub const DEFAULT_N_SLOTS: usize = 1;
pub const DEFAULT_WORDS_PER_BLOCK: usize = 100;
pub const DEFAULT_BIG_THRESHOLD_WORDS: usize = 500;
pub const DEFAULT_LCP_TH: f64 = 0.1;
pub const DEFAULT_META_DIR: &str = "kv_meta";
pub const DEFAULT_META_MAX: usize = 10;
pub const DEFAULT_REQUEST_TIMEOUT_SECS: f64 = 600.0;
pub const DEFAULT_MODEL_ID: &str = "llama.cpp";
pub const DEFAULT_PORT: u16 = 8081;
pub const DEFAULT_LOG_LEVEL: &str = "INFO";
pub const DEFAULT_STREAM_QUEUE_SIZE: usize = 16;
/// `PREFILTER_CASE_INSENSITIVE` default: keyword matching is
/// case-insensitive.
pub const DEFAULT_PREFILTER_CASE_INSENSITIVE: bool = true;
/// `PREFILTER_RESULT_CACHE_TTL` default: per-entry expiry in seconds
/// (`0` = entries never expire).
pub const DEFAULT_PREFILTER_RESULT_CACHE_TTL_SECS: f64 = 300.0;
/// `SAVE_RETRIES` default: retries after a failed KV cache save (backend
/// 500 or network/other error) before giving up (`0` = no retry).
pub const DEFAULT_SAVE_RETRIES: usize = 3;
/// `SAVE_RETRY_DELAY_MS` default: delay in milliseconds between save retry
/// attempts.
pub const DEFAULT_SAVE_RETRY_DELAY_MS: u64 = 1000;
/// `REQUEST_LOG_PATH` default: the request-log file location.
pub const DEFAULT_REQUEST_LOG_PATH: &str = "lpcache-request.log";
/// `REQUEST_LOG_MAX_BYTES` default: rotation threshold (10 MiB);
/// `0` disables rotation.
pub const DEFAULT_REQUEST_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// `REQUEST_LOG_KEEP` default: rotated generations retained (`0` keeps
/// them all).
pub const DEFAULT_REQUEST_LOG_KEEP: usize = 5;

/// Crate version from `Cargo.toml`, reported by `-V` / `--version` and
/// logged at startup in the `app_start` line.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// One-line version output of `-V` / `--version` (crate name + version).
pub fn version_string() -> String {
    format!("{} {}", env!("CARGO_PKG_NAME"), VERSION)
}

/// One llama.cpp backend (an entry of the `BACKENDS` JSON array).
#[derive(Debug, Clone, Deserialize)]
pub struct BackendConf {
    pub url: String,
    pub n_slots: usize,
    /// Optional `--slot-save-path` directory of this backend. When set,
    /// pruning a meta file also removes the KV file with that name from
    /// this directory.
    #[serde(default)]
    pub slot_save_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub backends: Vec<BackendConf>,
    pub words_per_block: usize,
    pub big_threshold_words: usize,
    pub lcp_th: f64,
    pub meta_dir: PathBuf,
    /// LRU bound on the number of meta files kept (0 = unlimited).
    pub meta_max: usize,
    /// `SLOT_SAVE_PATH` env value (single-backend fallback only).
    pub slot_save_path: Option<PathBuf>,
    pub request_timeout: Duration,
    pub model_id: String,
    /// `LLAMA_API_KEY`: when set, every request to the backend carries
    /// `Authorization: Bearer <key>` (llama-server's `--api-key`).
    pub api_key: Option<String>,
    pub port: u16,
    pub log_level: String,
    /// `STREAM_QUEUE_SIZE`: capacity of the per-request bounded channel that
    /// buffers streamed SSE bytes between the background reader and the
    /// HTTP response (always >= 1).
    pub stream_queue_size: usize,
    /// `COALESCE_REQUESTS`: group concurrent requests with the same KV cache
    /// key into a single backend call, regardless of generation parameters
    /// (followers receive the leader's result — see the `coalesce` module).
    pub coalesce_requests: bool,
    /// `PLUGINS`: raw JSON array of plugin specs
    /// (`[{"name": "...", "params": {...}}]`). When set, the prefilter
    /// plugins are loaded from it via the `lpcache-plugins` registry and
    /// the legacy `PREFILTER_*` settings below are ignored; when `None`,
    /// the specs are synthesized from them (see [`Config::plugin_specs`]).
    pub plugins: Option<String>,
    /// `PREFILTER_BLOCKLIST` (legacy; maps to the `keyword` plugin):
    /// comma-separated keywords; when non-empty the proxy rejects requests
    /// whose message contents contain any of them before any slot/backend
    /// work.
    pub prefilter_blocklist: Vec<String>,
    /// `PREFILTER_CASE_INSENSITIVE` (legacy; `keyword` plugin param):
    /// case-insensitive keyword matching.
    pub prefilter_case_insensitive: bool,
    /// `PREFILTER_RESULT_CACHE_DIR` (legacy; maps to the `result_cache`
    /// plugin): the cache-result path; when set, the result-cache
    /// prefilter serves fresh same-key requests from previously cached
    /// backend results, and fresh non-streaming results are stored in this
    /// directory. `None` = result cache disabled (the default).
    pub prefilter_result_cache_dir: Option<PathBuf>,
    /// `PREFILTER_RESULT_CACHE_TTL` (legacy; `result_cache` plugin param):
    /// per-entry expiry in seconds (`0` = entries never expire).
    pub prefilter_result_cache_ttl_secs: f64,
    /// `SAVE_RETRIES`: retries after a failed KV cache save (backend 500 or
    /// network/other error) before giving up (`0` = no retry).
    pub save_retries: usize,
    /// `SAVE_RETRY_DELAY_MS`: delay in milliseconds between save retry
    /// attempts.
    pub save_retry_delay_ms: u64,
    /// `REQUEST_LOG`: when true, every finished request is recorded as one
    /// W3C-XLF line in the request-log file (the built-in `request_log`
    /// plugin; see the `lpcache-plugins` crate for the record format).
    /// A `request_log` spec in `PLUGINS` is equivalent to enabling this
    /// (and takes precedence over the `REQUEST_LOG_*` settings).
    pub request_log_enabled: bool,
    /// `REQUEST_LOG_PATH`: the request-log file location (relative paths
    /// resolve against the current working directory).
    pub request_log_path: PathBuf,
    /// `REQUEST_LOG_MAX_BYTES`: the request-log rotation threshold in
    /// bytes (`0` = no rotation).
    pub request_log_max_bytes: u64,
    /// `REQUEST_LOG_KEEP`: rotated log generations retained
    /// (`0` = keep them all).
    pub request_log_keep: usize,
    /// `REQUEST_LOG_FIELDS`: ordered subset of XLF field names to emit
    /// (empty = all known fields).
    pub request_log_fields: Vec<String>,
}

/// Command-line options, one-to-one with the environment variables above.
///
/// Parsing is deliberately minimal (no external CLI crate): each option
/// takes a value in `--flag value` or `--flag=value` form. An explicit
/// option overrides the corresponding environment variable; unset options
/// fall back to the environment, then to the built-in defaults.
#[derive(Debug, Clone, Default)]
pub struct Cli {
    pub backends: Option<String>,
    pub llama_url: Option<String>,
    pub n_slots: Option<String>,
    pub words_per_block: Option<String>,
    pub big_threshold_words: Option<String>,
    pub lcp_th: Option<String>,
    pub meta_dir: Option<String>,
    pub meta_max: Option<String>,
    pub slot_save_path: Option<String>,
    pub request_timeout: Option<String>,
    pub model_id: Option<String>,
    pub port: Option<String>,
    /// Maps to `LLAMA_API_KEY`.
    pub api_key: Option<String>,
    pub log_level: Option<String>,
    /// `-h` / `--help` was passed.
    pub help: bool,
    /// `-V` / `--version` was passed.
    pub version: bool,
    /// Maps to `STREAM_QUEUE_SIZE` (per-request SSE channel capacity, >= 1).
    pub stream_queue_size: Option<String>,
    /// Maps to `COALESCE_REQUESTS` (`true`/`false`, or `1`/`0`).
    pub coalesce_requests: Option<String>,
    /// Maps to `PLUGINS` (JSON array of plugin specs; when set, the legacy
    /// `PREFILTER_*` settings are ignored).
    pub plugins: Option<String>,
    /// Maps to `PREFILTER_BLOCKLIST` (comma-separated keywords).
    pub prefilter_blocklist: Option<String>,
    /// Maps to `PREFILTER_CASE_INSENSITIVE` (`true`/`false`, or `1`/`0`).
    pub prefilter_case_insensitive: Option<String>,
    /// Maps to `PREFILTER_RESULT_CACHE_DIR` (the cache-result path).
    pub prefilter_result_cache_dir: Option<String>,
    /// Maps to `PREFILTER_RESULT_CACHE_TTL` (per-entry expiry, seconds).
    pub prefilter_result_cache_ttl: Option<String>,
    /// Maps to `SAVE_RETRIES` (retries after a failed KV cache save).
    pub save_retries: Option<String>,
    /// Maps to `SAVE_RETRY_DELAY_MS` (delay between save retry attempts).
    pub save_retry_delay_ms: Option<String>,
    /// Maps to `REQUEST_LOG` (`true`/`false`, or `1`/`0`).
    pub request_log: Option<String>,
    /// Maps to `REQUEST_LOG_PATH` (the request-log file location).
    pub request_log_path: Option<String>,
    /// Maps to `REQUEST_LOG_MAX_BYTES` (rotation threshold; `0` = none).
    pub request_log_max_bytes: Option<String>,
    /// Maps to `REQUEST_LOG_KEEP` (rotated generations retained; `0` = all).
    pub request_log_keep: Option<String>,
    /// Maps to `REQUEST_LOG_FIELDS` (comma-separated field subset).
    pub request_log_fields: Option<String>,
}

impl Config {
    /// Constructor for tests / programmatic use.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backends: Vec<BackendConf>,
        words_per_block: usize,
        big_threshold_words: usize,
        lcp_th: f64,
        meta_dir: PathBuf,
        request_timeout: Duration,
        model_id: String,
        port: u16,
    ) -> Self {
        Self {
            backends,
            words_per_block,
            big_threshold_words,
            lcp_th,
            meta_dir,
            meta_max: DEFAULT_META_MAX,
            slot_save_path: None,
            request_timeout,
            model_id,
            api_key: None,
            port,
            log_level: DEFAULT_LOG_LEVEL.to_string(),
            stream_queue_size: DEFAULT_STREAM_QUEUE_SIZE,
            coalesce_requests: false,
            plugins: None,
            prefilter_blocklist: Vec::new(),
            prefilter_case_insensitive: DEFAULT_PREFILTER_CASE_INSENSITIVE,
            prefilter_result_cache_dir: None,
            prefilter_result_cache_ttl_secs: DEFAULT_PREFILTER_RESULT_CACHE_TTL_SECS,
            save_retries: DEFAULT_SAVE_RETRIES,
            save_retry_delay_ms: DEFAULT_SAVE_RETRY_DELAY_MS,
            request_log_enabled: false,
            request_log_path: PathBuf::from(DEFAULT_REQUEST_LOG_PATH),
            request_log_max_bytes: DEFAULT_REQUEST_LOG_MAX_BYTES,
            request_log_keep: DEFAULT_REQUEST_LOG_KEEP,
            request_log_fields: Vec::new(),
        }
    }

    /// Override the LRU meta-file bound (`META_MAX`).
    pub fn with_meta_max(mut self, meta_max: usize) -> Self {
        self.meta_max = meta_max;
        self
    }

    /// Override the concurrent-request coalescing flag
    /// (`COALESCE_REQUESTS`).
    pub fn with_coalesce_requests(mut self, on: bool) -> Self {
        self.coalesce_requests = on;
        self
    }

    /// Override the plugin spec list (`PLUGINS`); when set, the prefilter
    /// plugins are loaded from this JSON array and the legacy
    /// `PREFILTER_*` settings are ignored.
    pub fn with_plugins(mut self, plugins: Option<String>) -> Self {
        self.plugins = plugins;
        self
    }

    /// Override the stream queue capacity (`STREAM_QUEUE_SIZE`); values
    /// below 1 are clamped to 1.
    pub fn with_stream_queue_size(mut self, size: usize) -> Self {
        self.stream_queue_size = size.max(1);
        self
    }

    /// Override the prefilter blocklist (`PREFILTER_BLOCKLIST`).
    pub fn with_prefilter_blocklist(mut self, keywords: Vec<String>) -> Self {
        self.prefilter_blocklist = keywords;
        self
    }

    /// Override the prefilter case-sensitivity flag
    /// (`PREFILTER_CASE_INSENSITIVE`).
    pub fn with_prefilter_case_insensitive(mut self, on: bool) -> Self {
        self.prefilter_case_insensitive = on;
        self
    }

    /// Override the result-cache directory (`PREFILTER_RESULT_CACHE_DIR`);
    /// `None` disables the result cache (the default).
    pub fn with_prefilter_result_cache_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.prefilter_result_cache_dir = dir;
        self
    }

    /// Override the per-entry result-cache expiry in seconds
    /// (`PREFILTER_RESULT_CACHE_TTL`; `0` = entries never expire).
    pub fn with_prefilter_result_cache_ttl_secs(mut self, secs: f64) -> Self {
        self.prefilter_result_cache_ttl_secs = secs.max(0.0);
        self
    }

    /// Enable the request log (`REQUEST_LOG`) with the given file settings
    /// (`fields` is an ordered subset of the known XLF field names;
    /// `None` = all of them).
    pub fn with_request_log(
        mut self,
        path: PathBuf,
        max_bytes: u64,
        keep: usize,
        fields: Option<Vec<String>>,
    ) -> Self {
        self.request_log_enabled = true;
        self.request_log_path = path;
        self.request_log_max_bytes = max_bytes;
        self.request_log_keep = keep;
        self.request_log_fields = fields.unwrap_or_default();
        self
    }

    /// The prefilter plugin specs to load from the `lpcache-plugins`
    /// registry, in evaluation order:
    /// - when `PLUGINS` is set, its JSON array is parsed and used as-is
    ///   (the legacy `PREFILTER_*` settings are ignored);
    /// - otherwise the specs are synthesized from the legacy settings: a
    ///   `keyword` spec when `PREFILTER_BLOCKLIST` is non-empty and/or a
    ///   `result_cache` spec when `PREFILTER_RESULT_CACHE_DIR` is set
    ///   (blocklist first: a blocked request is rejected with `400` before
    ///   a cache hit could serve it).
    ///
    /// When `REQUEST_LOG` is enabled and the list does not already contain
    /// a `request_log` spec, one is appended (a spec in `PLUGINS` always
    /// wins: its params are used as-is).
    ///
    /// An empty list means no prefilter and no request logging (the
    /// default). `Err` when `PLUGINS` is set but is not a JSON array of
    /// spec objects (`{"name": "...", "params": {...}}`).
    pub fn plugin_specs(&self) -> Result<Vec<lpcache_plugins::PluginSpec>, String> {
        use lpcache_plugins::PluginSpec;
        let mut specs: Vec<PluginSpec> = if let Some(raw) =
            self.plugins.as_deref().filter(|v| !v.trim().is_empty())
        {
            let arr: Vec<serde_json::Value> =
                serde_json::from_str(raw).map_err(|e| format!("PLUGINS is not valid JSON: {e}"))?;
            arr.into_iter()
                .enumerate()
                .map(|(i, v)| PluginSpec::try_from(&v).map_err(|e| format!("PLUGINS[{i}]: {e}")))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            let mut specs = Vec::new();
            if !self.prefilter_blocklist.is_empty() {
                specs.push(PluginSpec::new(
                    "keyword",
                    serde_json::json!({
                        "keywords": self.prefilter_blocklist,
                        "case_insensitive": self.prefilter_case_insensitive,
                    }),
                ));
            }
            if let Some(dir) = &self.prefilter_result_cache_dir {
                specs.push(PluginSpec::new(
                    "result_cache",
                    serde_json::json!({
                        "dir": dir.to_string_lossy(),
                        "ttl_secs": self.prefilter_result_cache_ttl_secs,
                    }),
                ));
            }
            specs
        };
        if self.request_log_enabled && !specs.iter().any(|s| s.name == "request_log") {
            specs.push(self.request_log_spec());
        }
        Ok(specs)
    }

    /// The `request_log` plugin spec synthesized from the `REQUEST_LOG_*`
    /// settings (used by [`Config::plugin_specs`] when `REQUEST_LOG` is
    /// enabled and `PLUGINS` has no `request_log` spec).
    fn request_log_spec(&self) -> lpcache_plugins::PluginSpec {
        use lpcache_plugins::PluginSpec;
        let mut params = serde_json::json!({
            "path": self.request_log_path.to_string_lossy(),
            "max_bytes": self.request_log_max_bytes,
            "keep": self.request_log_keep,
        });
        if !self.request_log_fields.is_empty() {
            params["fields"] = serde_json::json!(self.request_log_fields);
        }
        PluginSpec::new("request_log", params)
    }

    /// `--slot-save-path` directories known to the proxy, used to remove
    /// KV files when meta files are pruned: the per-backend
    /// `slot_save_path` entries, or — when none of them is set — the
    /// single-backend `SLOT_SAVE_PATH` value.
    pub fn slot_save_dirs(&self) -> Vec<&std::path::Path> {
        let mut dirs: Vec<&std::path::Path> = self
            .backends
            .iter()
            .filter_map(|b| b.slot_save_path.as_ref().map(|s| s.as_ref()))
            .collect();
        if dirs.is_empty()
            && let Some(p) = &self.slot_save_path
        {
            dirs.push(p);
        }
        dirs
    }

    /// Load configuration from the process environment (semantics of `config.py`).
    pub fn from_env() -> Self {
        let vars: HashMap<String, String> = std::env::vars().collect();
        Self::from_env_map(&vars)
    }

    /// Same as [`Config::from_env`], but from an explicit map (testable).
    pub fn from_env_map(vars: &HashMap<String, String>) -> Self {
        let backends = match vars.get("BACKENDS") {
            Some(raw) => match serde_json::from_str::<Vec<BackendConf>>(raw) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("BACKENDS parse failed, using no backends: {e}");
                    Vec::new()
                }
            },
            None => {
                let url = vars
                    .get("LLAMA_URL")
                    .cloned()
                    .unwrap_or_else(|| DEFAULT_BACKEND_URL.to_string());
                let n_slots = env_int(vars, "N_SLOTS", DEFAULT_N_SLOTS);
                vec![BackendConf {
                    url,
                    n_slots,
                    slot_save_path: None,
                }]
            }
        };
        let meta_dir = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(
                vars.get("META_DIR")
                    .cloned()
                    .unwrap_or_else(|| DEFAULT_META_DIR.to_string()),
            );
        // Only meaningful for the single-backend fallback (like
        // LLAMA_URL/N_SLOTS); per-backend dirs come from the BACKENDS JSON.
        let slot_save_path = if vars.contains_key("BACKENDS") {
            None
        } else {
            vars.get("SLOT_SAVE_PATH").map(PathBuf::from)
        };
        // Blank/whitespace-only values are treated as unset (no header sent).
        let api_key = vars
            .get("LLAMA_API_KEY")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        Self {
            backends,
            words_per_block: env_int(vars, "WORDS_PER_BLOCK", DEFAULT_WORDS_PER_BLOCK),
            big_threshold_words: env_int(vars, "BIG_THRESHOLD_WORDS", DEFAULT_BIG_THRESHOLD_WORDS),
            lcp_th: env_float(vars, "LCP_TH", DEFAULT_LCP_TH),
            meta_dir,
            meta_max: env_int(vars, "META_MAX", DEFAULT_META_MAX),
            slot_save_path,
            request_timeout: Duration::from_secs_f64(env_float(
                vars,
                "REQUEST_TIMEOUT",
                DEFAULT_REQUEST_TIMEOUT_SECS,
            )),
            model_id: vars
                .get("MODEL_ID")
                .cloned()
                .unwrap_or_else(|| DEFAULT_MODEL_ID.to_string()),
            api_key,
            port: env_int(vars, "PORT", DEFAULT_PORT as usize) as u16,
            log_level: vars
                .get("LOG_LEVEL")
                .cloned()
                .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_string()),
            stream_queue_size: env_stream_queue_size(vars),
            coalesce_requests: env_bool(vars, "COALESCE_REQUESTS", false),
            // Raw JSON array of plugin specs; parsed by `plugin_specs`.
            // Unset = synthesize the specs from the PREFILTER_* settings.
            plugins: vars.get("PLUGINS").cloned(),
            // Comma-separated keywords; empty entries dropped. Unset /
            // blank list = prefilter disabled.
            prefilter_blocklist: vars
                .get("PREFILTER_BLOCKLIST")
                .map(|raw| {
                    raw.split(',')
                        .map(|k| k.trim().to_string())
                        .filter(|k| !k.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            prefilter_case_insensitive: env_bool(
                vars,
                "PREFILTER_CASE_INSENSITIVE",
                DEFAULT_PREFILTER_CASE_INSENSITIVE,
            ),
            // Unset / blank dir = result cache disabled.
            prefilter_result_cache_dir: vars
                .get("PREFILTER_RESULT_CACHE_DIR")
                .map(|raw| raw.trim().to_string())
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            prefilter_result_cache_ttl_secs: env_result_cache_ttl(vars),
            save_retries: env_int(vars, "SAVE_RETRIES", DEFAULT_SAVE_RETRIES),
            save_retry_delay_ms: env_int(
                vars,
                "SAVE_RETRY_DELAY_MS",
                DEFAULT_SAVE_RETRY_DELAY_MS as usize,
            ) as u64,
            request_log_enabled: env_bool(vars, "REQUEST_LOG", false),
            request_log_path: vars
                .get("REQUEST_LOG_PATH")
                .map(|raw| raw.trim().to_string())
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_REQUEST_LOG_PATH)),
            request_log_max_bytes: env_int(
                vars,
                "REQUEST_LOG_MAX_BYTES",
                DEFAULT_REQUEST_LOG_MAX_BYTES as usize,
            ) as u64,
            request_log_keep: env_int(vars, "REQUEST_LOG_KEEP", DEFAULT_REQUEST_LOG_KEEP),
            request_log_fields: vars
                .get("REQUEST_LOG_FIELDS")
                .map(|raw| {
                    raw.split(',')
                        .map(|f| f.trim().to_string())
                        .filter(|f| !f.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        }
    }

    /// Same as [`Config::from_env`], but with explicit command-line
    /// options layered on top: CLI > environment > built-in defaults.
    pub fn from_cli(cli: &Cli) -> Self {
        let base: HashMap<String, String> = std::env::vars().collect();
        Self::from_env_map(&cli.merged_env(&base))
    }
}

fn env_int(vars: &HashMap<String, String>, key: &str, default: usize) -> usize {
    match vars.get(key) {
        Some(v) => match v.trim().parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!("env {key}={v:?} is not a non-negative integer, using {default}");
                default
            }
        },
        None => default,
    }
}

fn env_float(vars: &HashMap<String, String>, key: &str, default: f64) -> f64 {
    match vars.get(key) {
        Some(v) => match v.trim().parse::<f64>() {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!("env {key}={v:?} is not a float, using {default}");
                default
            }
        },
        None => default,
    }
}

/// Parse `STREAM_QUEUE_SIZE` (the per-request SSE channel capacity): like
/// [`env_int`], but the value must be >= 1 — a channel capacity of 0 is not
/// representable, so `0` (as well as invalid values) falls back to the
/// default with a warning.
fn env_stream_queue_size(vars: &HashMap<String, String>) -> usize {
    let n = env_int(vars, "STREAM_QUEUE_SIZE", DEFAULT_STREAM_QUEUE_SIZE);
    if n == 0 {
        tracing::warn!(
            "env STREAM_QUEUE_SIZE=0 is not allowed (capacity must be >= 1), using {}",
            DEFAULT_STREAM_QUEUE_SIZE
        );
        DEFAULT_STREAM_QUEUE_SIZE
    } else {
        n
    }
}

/// Parse `PREFILTER_RESULT_CACHE_TTL` (the per-entry result-cache expiry
/// in seconds, `0` = never expire): like [`env_float`], but negative
/// values fall back to the default with a warning.
fn env_result_cache_ttl(vars: &HashMap<String, String>) -> f64 {
    let n = env_float(
        vars,
        "PREFILTER_RESULT_CACHE_TTL",
        DEFAULT_PREFILTER_RESULT_CACHE_TTL_SECS,
    );
    if n < 0.0 {
        tracing::warn!(
            "env PREFILTER_RESULT_CACHE_TTL={n} is negative, using {DEFAULT_PREFILTER_RESULT_CACHE_TTL_SECS}"
        );
        DEFAULT_PREFILTER_RESULT_CACHE_TTL_SECS
    } else {
        n
    }
}

/// Parse a boolean env var (`1/true/yes/on` = true, `0/false/no/off` =
/// false; anything else warns and falls back to `default`).
fn env_bool(vars: &HashMap<String, String>, key: &str, default: bool) -> bool {
    match vars.get(key) {
        Some(v) => match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            other => {
                tracing::warn!("env {key}={other:?} is not a boolean, using {default}");
                default
            }
        },
        None => default,
    }
}

impl Cli {
    /// Parse the process command line (everything after the program name).
    pub fn from_args() -> Result<Self, String> {
        Self::parse(std::env::args().skip(1))
    }

    /// Parse arguments (testable). Unknown flags and flags with a missing
    /// value are errors; when a flag is given twice, the last one wins.
    pub fn parse<I>(args: I) -> Result<Self, String>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let mut cli = Cli::default();
        let mut it = args.into_iter();
        while let Some(raw) = it.next() {
            let arg = raw.as_ref();
            // `--flag=value` form; split on the first '=' only, so values
            // containing '=' survive.
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) if name.starts_with("--") => (name, Some(value)),
                _ => (arg, None),
            };
            if name == "-h" || name == "--help" {
                cli.help = true;
                continue;
            }
            if name == "-V" || name == "--version" {
                cli.version = true;
                continue;
            }
            let value = match inline {
                Some(v) => v.to_string(),
                None => match it.next() {
                    Some(next) => next.as_ref().to_string(),
                    None => return Err(format!("missing value for {name}")),
                },
            };
            match name {
                "--backends" => cli.backends = Some(value),
                "--llama-url" => cli.llama_url = Some(value),
                "--n-slots" => cli.n_slots = Some(value),
                "--words-per-block" => cli.words_per_block = Some(value),
                "--big-threshold-words" => cli.big_threshold_words = Some(value),
                "--lcp-th" => cli.lcp_th = Some(value),
                "--meta-dir" => cli.meta_dir = Some(value),
                "--meta-max" => cli.meta_max = Some(value),
                "--slot-save-path" => cli.slot_save_path = Some(value),
                "--request-timeout" => cli.request_timeout = Some(value),
                "--model-id" => cli.model_id = Some(value),
                "--port" => cli.port = Some(value),
                "--api-key" => cli.api_key = Some(value),
                "--log-level" => cli.log_level = Some(value),
                "--stream-queue-size" => cli.stream_queue_size = Some(value),
                "--coalesce-requests" => cli.coalesce_requests = Some(value),
                "--plugins" => cli.plugins = Some(value),
                "--prefilter-blocklist" => cli.prefilter_blocklist = Some(value),
                "--prefilter-case-insensitive" => cli.prefilter_case_insensitive = Some(value),
                "--prefilter-result-cache-dir" => cli.prefilter_result_cache_dir = Some(value),
                "--prefilter-result-cache-ttl" => cli.prefilter_result_cache_ttl = Some(value),
                "--save-retries" => cli.save_retries = Some(value),
                "--save-retry-delay-ms" => cli.save_retry_delay_ms = Some(value),
                "--request-log" => cli.request_log = Some(value),
                "--request-log-path" => cli.request_log_path = Some(value),
                "--request-log-max-bytes" => cli.request_log_max_bytes = Some(value),
                "--request-log-keep" => cli.request_log_keep = Some(value),
                "--request-log-fields" => cli.request_log_fields = Some(value),
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(cli)
    }

    /// `-h` / `--help` text.
    pub fn usage() -> &'static str {
        r#"Usage: lpcache [OPTIONS]

Every option maps to an environment variable of the same setting.
Explicit options take precedence over environment variables, which take
precedence over the built-in defaults.

Options:
  --backends <JSON>             BACKENDS             JSON array of backend objects: url, n_slots, slot_save_path
  --llama-url <URL>             LLAMA_URL            single-backend fallback (default http://127.0.0.1:8000)
  --n-slots <N>                 N_SLOTS              slot count for the fallback backend (default 1)
  --words-per-block <N>         WORDS_PER_BLOCK      words per hash block (default 100)
  --big-threshold-words <N>     BIG_THRESHOLD_WORDS  word count making a request "big" (default 500)
  --lcp-th <F>                  LCP_TH               min. shared-block ratio 0..1 to restore a cache (default 0.1)
  --meta-dir <PATH>             META_DIR             meta file dir (default ./kv_meta, relative to CWD)
  --meta-max <N>                META_MAX             max meta files kept; oldest pruned (default 10, 0 = unlimited)
  --slot-save-path <PATH>       SLOT_SAVE_PATH       backend --slot-save-path dir (single-backend fallback)
  --request-timeout <SECS>      REQUEST_TIMEOUT      per-request timeout to the backend (default 600)
  --model-id <ID>               MODEL_ID             model id advertised by /v1/models (default llama.cpp)
  --port <PORT>                 PORT                 proxy listen port (default 8081)
  --api-key <KEY>               LLAMA_API_KEY        sent as Authorization: Bearer <KEY>
  --log-level <LEVEL>           LOG_LEVEL            TRACE..ERROR (default INFO; RUST_LOG overrides)
  --stream-queue-size <N>       STREAM_QUEUE_SIZE    per-request SSE channel capacity (default 16, must be >= 1)
  --coalesce-requests <BOOL>    COALESCE_REQUESTS    group concurrent same-key requests into one backend call (default false)
  --plugins <JSON>              PLUGINS              JSON array of plugin specs [{"name": "...", "params": {...}}] selecting the prefilter plugins (built-ins: keyword, chain, result_cache) plus optional request_log and dashboard plugins; when set, the legacy PREFILTER_* settings are ignored (default: synthesized from them)
  --prefilter-blocklist <LIST>  PREFILTER_BLOCKLIST  (legacy: keyword plugin) comma-separated keywords; matching requests are rejected (400) before the backend (default: disabled)
  --prefilter-case-insensitive <BOOL>  PREFILTER_CASE_INSENSITIVE  keyword matching is case-insensitive (default true)
  --prefilter-result-cache-dir <PATH>  PREFILTER_RESULT_CACHE_DIR  dir for cached backend results ({key}.json); when set, fresh same-key non-stream requests are answered from the cache before the backend (default: disabled)
  --prefilter-result-cache-ttl <SECS>  PREFILTER_RESULT_CACHE_TTL  cached-result expiry in seconds (default 300, 0 = never)
  --save-retries <N>            SAVE_RETRIES         retries after a failed KV cache save (backend 500 or other error) (default 3, 0 = no retry)
  --save-retry-delay-ms <MS>    SAVE_RETRY_DELAY_MS  delay in milliseconds between save retry attempts (default 1000)
  --request-log <BOOL>          REQUEST_LOG          write one W3C-XLF record per finished request to the request-log file (default false)
  --request-log-path <PATH>     REQUEST_LOG_PATH     request-log file location; relative paths resolve against the CWD (default lpcache-request.log)
  --request-log-max-bytes <N>   REQUEST_LOG_MAX_BYTES rotation threshold in bytes; the file is rotated when exceeded (default 10 MiB, 0 = no rotation)
  --request-log-keep <N>        REQUEST_LOG_KEEP     rotated generations retained (default 5, 0 = keep all)
  --request-log-fields <LIST>   REQUEST_LOG_FIELDS   comma-separated ordered subset of XLF field names (default: all fields)
  -V, --version                 show version and exit
  -h, --help                    show this help and exit

Notes:
  * every option maps to an environment variable of the same setting;
    both --flag value and --flag=value are accepted
  * with BACKENDS set, LLAMA_URL / N_SLOTS / SLOT_SAVE_PATH are ignored —
    per-backend values in the JSON array win
  * LLAMA_API_KEY is sent to the backend as "Authorization: Bearer <key>"
    (llama-server --api-key); a blank value means no auth
  * META_DIR and a relative SLOT_SAVE_PATH resolve against the CWD
  * invalid numeric/boolean values fall back to the default with a warning;
    booleans accept 1/true/yes/on and 0/false/no/off
  * META_MAX=0 disables pruning; a restore of a pruned key falls back to a
    full prefill
  * PREFILTER_BLOCKLIST is a comma-separated keyword list (surrounding
    whitespace ignored); when set, requests whose message contents contain
    any keyword are rejected with 400 before slot acquisition or backend
    dispatch; unset/blank = no prefilter
  * PREFILTER_RESULT_CACHE_DIR enables the result-cache prefilter: fresh
    (non-expired) same-key requests are answered with the previously
    cached backend result before any slot/backend work; fresh non-streaming
    results are stored as {dir}/{key}.json; streaming requests are never
    served from the cache; PREFILTER_RESULT_CACHE_TTL is the per-entry
    expiry in seconds (0 = never); when both PREFILTER_BLOCKLIST and the
    result cache are set, the blocklist runs first (reject wins over serve)
  * REQUEST_LOG=1 enables the built-in request_log plugin: each finished
    request (success, error, 400 block, abort or disconnect) appends one
    W3C-XLF record (header + space-separated values, missing values as -)
    to REQUEST_LOG_PATH; the file is size-rotated at REQUEST_LOG_MAX_BYTES
    (current becomes .1, older generations shift up, REQUEST_LOG_KEEP
    retained); REQUEST_LOG_FIELDS selects an ordered subset of fields
    (defaults to all W3C + lpcache-* pipeline fields); a request_log spec
    in PLUGINS takes precedence over the REQUEST_LOG_* settings
  * the backend should be started with matching llama-server flags:
    -np/--parallel (N_SLOTS), --api-key (LLAMA_API_KEY) and a
    --slot-save-path directory (SLOT_SAVE_PATH)
  * slot acquire timeout (300 s) and circuit-breaker cooldowns (5..60 s)
    are built-in behaviour, not configurable
"#
    }

    /// Effective setting map: `base` (usually the process environment)
    /// with every explicitly-set option layered on top.
    pub fn merged_env(&self, base: &HashMap<String, String>) -> HashMap<String, String> {
        let mut m = base.clone();
        let opts: &[(&str, &Option<String>)] = &[
            ("BACKENDS", &self.backends),
            ("LLAMA_URL", &self.llama_url),
            ("N_SLOTS", &self.n_slots),
            ("WORDS_PER_BLOCK", &self.words_per_block),
            ("BIG_THRESHOLD_WORDS", &self.big_threshold_words),
            ("LCP_TH", &self.lcp_th),
            ("META_DIR", &self.meta_dir),
            ("META_MAX", &self.meta_max),
            ("SLOT_SAVE_PATH", &self.slot_save_path),
            ("REQUEST_TIMEOUT", &self.request_timeout),
            ("MODEL_ID", &self.model_id),
            ("PORT", &self.port),
            ("LLAMA_API_KEY", &self.api_key),
            ("LOG_LEVEL", &self.log_level),
            ("STREAM_QUEUE_SIZE", &self.stream_queue_size),
            ("COALESCE_REQUESTS", &self.coalesce_requests),
            ("PLUGINS", &self.plugins),
            ("PREFILTER_BLOCKLIST", &self.prefilter_blocklist),
            (
                "PREFILTER_CASE_INSENSITIVE",
                &self.prefilter_case_insensitive,
            ),
            (
                "PREFILTER_RESULT_CACHE_DIR",
                &self.prefilter_result_cache_dir,
            ),
            (
                "PREFILTER_RESULT_CACHE_TTL",
                &self.prefilter_result_cache_ttl,
            ),
            ("SAVE_RETRIES", &self.save_retries),
            ("SAVE_RETRY_DELAY_MS", &self.save_retry_delay_ms),
            ("REQUEST_LOG", &self.request_log),
            ("REQUEST_LOG_PATH", &self.request_log_path),
            ("REQUEST_LOG_MAX_BYTES", &self.request_log_max_bytes),
            ("REQUEST_LOG_KEEP", &self.request_log_keep),
            ("REQUEST_LOG_FIELDS", &self.request_log_fields),
        ];
        for (env_name, value) in opts {
            if let Some(v) = value {
                m.insert(env_name.to_string(), v.clone());
            }
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// A minimal `Config` (all defaults) for exercising the builder methods.
    fn base_cfg() -> Config {
        Config::new(
            vec![],
            100,
            500,
            0.1,
            std::path::PathBuf::from("kv_meta"),
            std::time::Duration::from_secs(600),
            "llama.cpp".to_string(),
            8081,
        )
    }

    #[test]
    fn defaults_when_empty() {
        let c = Config::from_env_map(&HashMap::new());
        assert_eq!(c.backends.len(), 1);
        assert_eq!(c.backends[0].url, DEFAULT_BACKEND_URL);
        assert_eq!(c.backends[0].n_slots, 1);
        assert_eq!(c.words_per_block, 100);
        assert_eq!(c.big_threshold_words, 500);
        assert!((c.lcp_th - 0.1).abs() < f64::EPSILON);
        assert_eq!(
            c.meta_dir.file_name().and_then(|f| f.to_str()),
            Some("kv_meta")
        );
        assert_eq!(c.request_timeout, Duration::from_secs(600));
        assert_eq!(c.model_id, "llama.cpp");
        assert_eq!(c.port, 8081);
        assert_eq!(c.log_level, "INFO");
        assert_eq!(c.stream_queue_size, 16);
        assert!(c.plugins.is_none());
        assert!(c.prefilter_blocklist.is_empty());
        assert_eq!(c.save_retries, DEFAULT_SAVE_RETRIES);
        assert_eq!(c.save_retry_delay_ms, DEFAULT_SAVE_RETRY_DELAY_MS);
        assert!(!c.request_log_enabled);
        assert_eq!(c.request_log_path, PathBuf::from(DEFAULT_REQUEST_LOG_PATH));
        assert_eq!(c.request_log_max_bytes, DEFAULT_REQUEST_LOG_MAX_BYTES);
        assert_eq!(c.request_log_keep, DEFAULT_REQUEST_LOG_KEEP);
        assert!(c.request_log_fields.is_empty());
    }

    #[test]
    fn backends_json_parsed() {
        let c = Config::from_env_map(&vars(&[(
            "BACKENDS",
            r#"[{"url":"http://a:1","n_slots":3},{"url":"http://b:2","n_slots":1}]"#,
        )]));
        assert_eq!(c.backends.len(), 2);
        assert_eq!(c.backends[0].url, "http://a:1");
        assert_eq!(c.backends[0].n_slots, 3);
        assert_eq!(c.backends[1].url, "http://b:2");
        assert_eq!(c.backends[1].n_slots, 1);
    }

    #[test]
    fn backends_invalid_json_falls_back_to_empty() {
        let c = Config::from_env_map(&vars(&[("BACKENDS", "not json")]));
        assert!(c.backends.is_empty());
    }

    #[test]
    fn llama_url_and_n_slots_fallback() {
        let c = Config::from_env_map(&vars(&[("LLAMA_URL", "http://x:9"), ("N_SLOTS", "4")]));
        assert_eq!(c.backends[0].url, "http://x:9");
        assert_eq!(c.backends[0].n_slots, 4);
    }

    #[test]
    fn invalid_numbers_use_defaults() {
        let c = Config::from_env_map(&vars(&[
            ("WORDS_PER_BLOCK", "oops"),
            ("LCP_TH", "abc"),
            ("PORT", "80"),
        ]));
        assert_eq!(c.words_per_block, 100);
        assert!((c.lcp_th - 0.1).abs() < f64::EPSILON);
        assert_eq!(c.port, 80);
    }

    #[test]
    fn meta_dir_custom() {
        let c = Config::from_env_map(&vars(&[("META_DIR", "my_meta")]));
        assert_eq!(
            c.meta_dir.file_name().and_then(|f| f.to_str()),
            Some("my_meta")
        );
    }

    #[test]
    fn meta_max_default_and_env() {
        assert_eq!(
            Config::from_env_map(&HashMap::new()).meta_max,
            DEFAULT_META_MAX
        );
        assert_eq!(
            Config::from_env_map(&vars(&[("META_MAX", "7")])).meta_max,
            7
        );
        assert_eq!(
            Config::from_env_map(&vars(&[("META_MAX", "0")])).meta_max,
            0
        );
        // invalid -> default
        assert_eq!(
            Config::from_env_map(&vars(&[("META_MAX", "oops")])).meta_max,
            DEFAULT_META_MAX
        );
    }

    #[test]
    fn slot_save_path_fallback_env() {
        let c = Config::from_env_map(&vars(&[("SLOT_SAVE_PATH", "/var/kv")]));
        assert_eq!(
            c.slot_save_path.as_deref(),
            Some(std::path::Path::new("/var/kv"))
        );
        assert_eq!(c.slot_save_dirs(), vec![std::path::Path::new("/var/kv")]);
        // BACKENDS set -> the env var is ignored (like LLAMA_URL/N_SLOTS)
        let c = Config::from_env_map(&vars(&[
            ("BACKENDS", r#"[{"url":"http://a:1","n_slots":1}]"#),
            ("SLOT_SAVE_PATH", "/ignored"),
        ]));
        assert_eq!(c.slot_save_path, None);
        assert!(c.slot_save_dirs().is_empty());
    }

    #[test]
    fn backends_slot_save_path_json() {
        let c = Config::from_env_map(&vars(&[(
            "BACKENDS",
            r#"[{"url":"http://a:1","n_slots":3,"slot_save_path":"/var/kv/a"},{"url":"http://b:2","n_slots":1}]"#,
        )]));
        assert_eq!(c.backends[0].slot_save_path.as_deref(), Some("/var/kv/a"));
        assert_eq!(c.backends[1].slot_save_path, None);
        assert_eq!(c.slot_save_dirs(), vec![std::path::Path::new("/var/kv/a")]);
    }

    #[test]
    fn api_key_env() {
        assert_eq!(Config::from_env_map(&HashMap::new()).api_key, None);
        assert_eq!(
            Config::from_env_map(&vars(&[("LLAMA_API_KEY", "777")])).api_key,
            Some("777".to_string())
        );
        // blank -> None (no Authorization header sent)
        assert_eq!(
            Config::from_env_map(&vars(&[("LLAMA_API_KEY", "   ")])).api_key,
            None
        );
    }

    #[test]
    fn cli_parses_space_form() {
        let cli = Cli::parse(vec![
            "--port".to_string(),
            "9000".to_string(),
            "--llama-url".to_string(),
            "http://x:1".to_string(),
            "--n-slots".to_string(),
            "4".to_string(),
            "--log-level".to_string(),
            "DEBUG".to_string(),
        ])
        .unwrap();
        assert_eq!(cli.port.as_deref(), Some("9000"));
        assert_eq!(cli.llama_url.as_deref(), Some("http://x:1"));
        assert_eq!(cli.n_slots.as_deref(), Some("4"));
        assert_eq!(cli.log_level.as_deref(), Some("DEBUG"));
        assert!(cli.backends.is_none());
        assert!(!cli.help);
    }

    #[test]
    fn cli_parses_equals_form() {
        let cli = Cli::parse(vec![
            "--port=9001".to_string(),
            "--backends=[{\"url\":\"http://a:1\",\"n_slots\":2}]".to_string(),
        ])
        .unwrap();
        assert_eq!(cli.port.as_deref(), Some("9001"));
        assert_eq!(
            cli.backends.as_deref(),
            Some(r#"[{"url":"http://a:1","n_slots":2}]"#)
        );
    }

    #[test]
    fn cli_equals_form_keeps_extra_equals_in_value() {
        let cli = Cli::parse(vec!["--llama-url=http://a:1?x=1".to_string()]).unwrap();
        assert_eq!(cli.llama_url.as_deref(), Some("http://a:1?x=1"));
    }

    #[test]
    fn cli_flag_twice_last_wins() {
        let cli = Cli::parse(vec![
            "--port".to_string(),
            "1".to_string(),
            "--port".to_string(),
            "2".to_string(),
        ])
        .unwrap();
        assert_eq!(cli.port.as_deref(), Some("2"));
    }

    #[test]
    fn cli_unknown_flag_errors() {
        let err = Cli::parse(vec!["--nope".to_string()]).unwrap_err();
        assert!(err.contains("--nope"), "err: {err}");
    }

    #[test]
    fn cli_missing_value_errors() {
        let err = Cli::parse(vec!["--port".to_string()]).unwrap_err();
        assert!(err.contains("--port"), "err: {err}");
    }

    #[test]
    fn cli_help_flags() {
        assert!(Cli::parse(vec!["--help".to_string()]).unwrap().help);
        assert!(Cli::parse(vec!["-h".to_string()]).unwrap().help);
        // help can appear among other options
        assert!(
            Cli::parse(vec![
                "--port".to_string(),
                "1".to_string(),
                "-h".to_string()
            ])
            .unwrap()
            .help
        );
    }

    #[test]
    fn cli_version_flags() {
        assert!(!Cli::default().version);
        assert!(Cli::parse(vec!["--version".to_string()]).unwrap().version);
        assert!(Cli::parse(vec!["-V".to_string()]).unwrap().version);
        // version can appear among other options
        assert!(
            Cli::parse(vec![
                "--port".to_string(),
                "1".to_string(),
                "-V".to_string()
            ])
            .unwrap()
            .version
        );
    }

    #[test]
    fn version_string_is_crate_name_and_version() {
        let s = version_string();
        assert!(s.starts_with("lpcache "), "s: {s}");
        assert!(s.ends_with(VERSION), "s: {s}");
    }

    #[test]
    fn cli_usage_lists_all_flags_and_env_vars() {
        let usage = Cli::usage();
        for flag in [
            "--backends",
            "--llama-url",
            "--n-slots",
            "--words-per-block",
            "--big-threshold-words",
            "--lcp-th",
            "--meta-dir",
            "--meta-max",
            "--slot-save-path",
            "--request-timeout",
            "--model-id",
            "--port",
            "--api-key",
            "--log-level",
            "--stream-queue-size",
            "--coalesce-requests",
            "--plugins",
            "--prefilter-blocklist",
            "--prefilter-case-insensitive",
            "--prefilter-result-cache-dir",
            "--prefilter-result-cache-ttl",
            "--save-retries",
            "--save-retry-delay-ms",
            "--version",
            "--help",
        ] {
            assert!(usage.contains(flag), "usage missing {flag}");
        }
        for env in [
            "BACKENDS",
            "LLAMA_URL",
            "N_SLOTS",
            "WORDS_PER_BLOCK",
            "BIG_THRESHOLD_WORDS",
            "LCP_TH",
            "META_DIR",
            "META_MAX",
            "SLOT_SAVE_PATH",
            "REQUEST_TIMEOUT",
            "MODEL_ID",
            "PORT",
            "LLAMA_API_KEY",
            "LOG_LEVEL",
            "STREAM_QUEUE_SIZE",
            "COALESCE_REQUESTS",
            "PLUGINS",
            "PREFILTER_BLOCKLIST",
            "PREFILTER_CASE_INSENSITIVE",
            "PREFILTER_RESULT_CACHE_DIR",
            "PREFILTER_RESULT_CACHE_TTL",
            "SAVE_RETRIES",
            "SAVE_RETRY_DELAY_MS",
        ] {
            assert!(usage.contains(env), "usage missing {env}");
        }
    }

    #[test]
    fn coalesce_requests_flag() {
        // default off
        assert!(!Config::from_env_map(&HashMap::new()).coalesce_requests);
        // env on / off
        assert!(Config::from_env_map(&vars(&[("COALESCE_REQUESTS", "true")])).coalesce_requests);
        assert!(Config::from_env_map(&vars(&[("COALESCE_REQUESTS", "1")])).coalesce_requests);
        assert!(!Config::from_env_map(&vars(&[("COALESCE_REQUESTS", "0")])).coalesce_requests);
        // invalid -> default
        assert!(!Config::from_env_map(&vars(&[("COALESCE_REQUESTS", "oops")])).coalesce_requests);
        // CLI wins over env
        let cli = Cli::parse(vec!["--coalesce-requests".to_string(), "false".to_string()]).unwrap();
        let merged = cli.merged_env(&vars(&[("COALESCE_REQUESTS", "1")]));
        assert!(!Config::from_env_map(&merged).coalesce_requests);
    }

    #[test]
    fn plugins_env_and_cli_stored_raw() {
        // unset -> None (synthesized from the PREFILTER_* settings instead)
        assert_eq!(Config::from_env_map(&HashMap::new()).plugins, None);
        // env stores the raw JSON; parsing happens in `plugin_specs`
        let raw = r#"[{"name":"keyword","params":{"keywords":["a"]}}]"#;
        assert_eq!(
            Config::from_env_map(&vars(&[("PLUGINS", raw)]))
                .plugins
                .as_deref(),
            Some(raw)
        );
        // CLI flag
        let cli = Cli::parse(vec!["--plugins".to_string(), raw.to_string()]).unwrap();
        assert_eq!(cli.plugins.as_deref(), Some(raw));
        // merged env carries the flag value
        let merged = cli.merged_env(&HashMap::new());
        assert_eq!(merged.get("PLUGINS").map(String::as_str), Some(raw));
        // builder
        assert_eq!(
            Config::new(
                Vec::new(),
                100,
                500,
                0.1,
                std::path::PathBuf::from("m"),
                Duration::from_secs(600),
                "m".to_string(),
                10
            )
            .with_plugins(Some(raw.to_string()))
            .plugins
            .as_deref(),
            Some(raw)
        );
    }

    #[test]
    fn plugin_specs_parsed_from_plugins_json() {
        let c = Config::from_env_map(&vars(&[(
            "PLUGINS",
            r#"[{"name":"keyword","params":{"keywords":["a"],"case_insensitive":false}},{"name":"result_cache","params":{"dir":"/var/rc","ttl_secs":120}}]"#,
        )]));
        let specs = c.plugin_specs().expect("specs");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "keyword");
        assert_eq!(specs[0].params["keywords"], serde_json::json!(["a"]));
        assert_eq!(specs[0].params["case_insensitive"], false);
        assert_eq!(specs[1].name, "result_cache");
        assert_eq!(specs[1].params["dir"], "/var/rc");
        assert_eq!(specs[1].params["ttl_secs"], 120);
    }

    #[test]
    fn plugin_specs_plugins_take_precedence_over_legacy() {
        let c = Config::from_env_map(&vars(&[
            (
                "PLUGINS",
                r#"[{"name":"keyword","params":{"keywords":["x"]}}]"#,
            ),
            ("PREFILTER_BLOCKLIST", "ignored"),
            ("PREFILTER_RESULT_CACHE_DIR", "/ignored"),
        ]));
        let specs = c.plugin_specs().expect("specs");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "keyword");
        assert_eq!(specs[0].params["keywords"], serde_json::json!(["x"]));
        // an empty spec list is valid and disables the prefilter
        let c = Config::from_env_map(&vars(&[
            ("PLUGINS", "[]"),
            ("PREFILTER_BLOCKLIST", "ignored"),
        ]));
        assert!(c.plugin_specs().expect("specs").is_empty());
    }

    #[test]
    fn plugin_specs_invalid_json_errors() {
        // not JSON at all
        let c = Config::from_env_map(&vars(&[("PLUGINS", "not json")]));
        assert!(c.plugin_specs().is_err());
        // JSON, but not an array
        let c = Config::from_env_map(&vars(&[("PLUGINS", r#"{"name":"keyword"}"#)]));
        assert!(c.plugin_specs().is_err());
        // array entry without a `name` string
        let c = Config::from_env_map(&vars(&[("PLUGINS", r#"[{"params":{}}]"#)]));
        let err = c.plugin_specs().unwrap_err();
        assert!(err.contains("PLUGINS[0]"), "err: {err}");
        // blank value = unset -> synthesize from the (unset) legacy settings
        let c = Config::from_env_map(&vars(&[("PLUGINS", "   ")]));
        assert!(c.plugin_specs().expect("blank = unset").is_empty());
    }

    #[test]
    fn plugin_specs_synthesized_from_legacy_settings() {
        // neither legacy setting -> no prefilter (the default)
        assert!(
            Config::from_env_map(&HashMap::new())
                .plugin_specs()
                .expect("specs")
                .is_empty()
        );
        // blocklist only -> one `keyword` spec
        let c = Config::from_env_map(&vars(&[("PREFILTER_BLOCKLIST", "a, b")]));
        let specs = c.plugin_specs().expect("specs");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "keyword");
        assert_eq!(specs[0].params["keywords"], serde_json::json!(["a", "b"]));
        // default case_insensitive=true
        assert_eq!(specs[0].params["case_insensitive"], true);
        // both set -> keyword first, then result_cache
        let c = Config::from_env_map(&vars(&[
            ("PREFILTER_BLOCKLIST", "a"),
            ("PREFILTER_CASE_INSENSITIVE", "false"),
            ("PREFILTER_RESULT_CACHE_DIR", "/var/rc"),
            ("PREFILTER_RESULT_CACHE_TTL", "60"),
        ]));
        let specs = c.plugin_specs().expect("specs");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "keyword");
        assert_eq!(specs[0].params["case_insensitive"], false);
        assert_eq!(specs[1].name, "result_cache");
        assert_eq!(specs[1].params["dir"], "/var/rc");
        assert_eq!(specs[1].params["ttl_secs"], 60.0);
    }

    #[test]
    fn stream_queue_size_env_and_cli() {
        // default
        assert_eq!(Config::from_env_map(&HashMap::new()).stream_queue_size, 16);
        // env value
        assert_eq!(
            Config::from_env_map(&vars(&[("STREAM_QUEUE_SIZE", "64")])).stream_queue_size,
            64
        );
        // 0 is not a representable capacity -> default
        assert_eq!(
            Config::from_env_map(&vars(&[("STREAM_QUEUE_SIZE", "0")])).stream_queue_size,
            16
        );
        // invalid -> default
        assert_eq!(
            Config::from_env_map(&vars(&[("STREAM_QUEUE_SIZE", "oops")])).stream_queue_size,
            16
        );
        // CLI wins over env
        let cli = Cli::parse(vec!["--stream-queue-size".to_string(), "8".to_string()]).unwrap();
        let merged = cli.merged_env(&vars(&[("STREAM_QUEUE_SIZE", "64")]));
        assert_eq!(Config::from_env_map(&merged).stream_queue_size, 8);
        // builder clamps below 1 to 1
        let c = Config::from_env_map(&HashMap::new()).with_stream_queue_size(0);
        assert_eq!(c.stream_queue_size, 1);
    }

    #[test]
    fn save_retry_env_and_cli() {
        // defaults
        let c = Config::from_env_map(&HashMap::new());
        assert_eq!(c.save_retries, DEFAULT_SAVE_RETRIES);
        assert_eq!(c.save_retry_delay_ms, DEFAULT_SAVE_RETRY_DELAY_MS);
        // env values
        let c = Config::from_env_map(&vars(&[
            ("SAVE_RETRIES", "5"),
            ("SAVE_RETRY_DELAY_MS", "250"),
        ]));
        assert_eq!(c.save_retries, 5);
        assert_eq!(c.save_retry_delay_ms, 250);
        // 0 disables retries
        assert_eq!(
            Config::from_env_map(&vars(&[("SAVE_RETRIES", "0")])).save_retries,
            0
        );
        // invalid -> defaults
        let c = Config::from_env_map(&vars(&[
            ("SAVE_RETRIES", "oops"),
            ("SAVE_RETRY_DELAY_MS", "bad"),
        ]));
        assert_eq!(c.save_retries, DEFAULT_SAVE_RETRIES);
        assert_eq!(c.save_retry_delay_ms, DEFAULT_SAVE_RETRY_DELAY_MS);
        // CLI wins over env
        let cli = Cli::parse(vec![
            "--save-retries".to_string(),
            "1".to_string(),
            "--save-retry-delay-ms".to_string(),
            "50".to_string(),
        ])
        .unwrap();
        let merged = cli.merged_env(&vars(&[
            ("SAVE_RETRIES", "5"),
            ("SAVE_RETRY_DELAY_MS", "250"),
        ]));
        let c = Config::from_env_map(&merged);
        assert_eq!(c.save_retries, 1);
        assert_eq!(c.save_retry_delay_ms, 50);
    }

    #[test]
    fn prefilter_env_and_cli() {
        // default: disabled
        let c = Config::from_env_map(&HashMap::new());
        assert!(c.prefilter_blocklist.is_empty());
        assert!(c.prefilter_case_insensitive);
        assert!(c.plugin_specs().expect("specs").is_empty());

        // env: comma list trimmed, empty entries dropped
        let c = Config::from_env_map(&vars(&[
            ("PREFILTER_BLOCKLIST", "a, b ,c,,  "),
            ("PREFILTER_CASE_INSENSITIVE", "false"),
        ]));
        assert_eq!(
            c.prefilter_blocklist,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert!(!c.prefilter_case_insensitive);
        let specs = c.plugin_specs().expect("specs");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "keyword");
        assert_eq!(
            specs[0].params["keywords"],
            serde_json::json!(["a", "b", "c"])
        );
        assert_eq!(specs[0].params["case_insensitive"], false);

        // blank list = disabled
        assert!(
            Config::from_env_map(&vars(&[("PREFILTER_BLOCKLIST", " , ")]))
                .plugin_specs()
                .expect("specs")
                .is_empty()
        );

        // invalid boolean -> default (case-insensitive)
        assert!(
            Config::from_env_map(&vars(&[("PREFILTER_CASE_INSENSITIVE", "oops")]))
                .prefilter_case_insensitive
        );

        // CLI wins over env
        let cli = Cli::parse(vec![
            "--prefilter-blocklist".to_string(),
            "cli-kw".to_string(),
            "--prefilter-case-insensitive".to_string(),
            "false".to_string(),
        ])
        .unwrap();
        let merged = cli.merged_env(&vars(&[("PREFILTER_BLOCKLIST", "env-kw")]));
        let c = Config::from_env_map(&merged);
        assert_eq!(c.prefilter_blocklist, vec!["cli-kw".to_string()]);
        assert!(!c.prefilter_case_insensitive);
        let specs = c.plugin_specs().expect("specs");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].params["keywords"], serde_json::json!(["cli-kw"]));
    }

    #[test]
    fn prefilter_result_cache_env_and_cli() {
        // default: disabled
        let c = Config::from_env_map(&HashMap::new());
        assert!(c.prefilter_result_cache_dir.is_none());
        assert!((c.prefilter_result_cache_ttl_secs - 300.0).abs() < f64::EPSILON);
        assert!(c.plugin_specs().expect("specs").is_empty());

        // env: dir + ttl
        let c = Config::from_env_map(&vars(&[
            ("PREFILTER_RESULT_CACHE_DIR", "my_cache"),
            ("PREFILTER_RESULT_CACHE_TTL", "60"),
        ]));
        assert_eq!(
            c.prefilter_result_cache_dir.as_deref(),
            Some(std::path::Path::new("my_cache"))
        );
        assert!((c.prefilter_result_cache_ttl_secs - 60.0).abs() < f64::EPSILON);
        let specs = c.plugin_specs().expect("specs");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "result_cache");
        assert_eq!(specs[0].params["dir"], "my_cache");
        assert_eq!(specs[0].params["ttl_secs"], 60.0);

        // blank dir = disabled; ttl 0 = never expire
        let c = Config::from_env_map(&vars(&[
            ("PREFILTER_RESULT_CACHE_DIR", "   "),
            ("PREFILTER_RESULT_CACHE_TTL", "0"),
        ]));
        assert!(c.prefilter_result_cache_dir.is_none());
        assert!(c.prefilter_result_cache_ttl_secs == 0.0);
        assert!(c.plugin_specs().expect("specs").is_empty());

        // invalid / negative ttl -> default
        assert!(
            (Config::from_env_map(&vars(&[("PREFILTER_RESULT_CACHE_TTL", "oops")]))
                .prefilter_result_cache_ttl_secs
                - 300.0)
                .abs()
                < f64::EPSILON
        );
        assert!(
            (Config::from_env_map(&vars(&[("PREFILTER_RESULT_CACHE_TTL", "-5")]))
                .prefilter_result_cache_ttl_secs
                - 300.0)
                .abs()
                < f64::EPSILON
        );

        // CLI wins over env
        let cli = Cli::parse(vec![
            "--prefilter-result-cache-dir".to_string(),
            "cli_dir".to_string(),
            "--prefilter-result-cache-ttl".to_string(),
            "5".to_string(),
        ])
        .unwrap();
        let merged = cli.merged_env(&vars(&[
            ("PREFILTER_RESULT_CACHE_DIR", "env_dir"),
            ("PREFILTER_RESULT_CACHE_TTL", "7"),
        ]));
        let c = Config::from_env_map(&merged);
        assert_eq!(
            c.prefilter_result_cache_dir.as_deref(),
            Some(std::path::Path::new("cli_dir"))
        );
        assert!((c.prefilter_result_cache_ttl_secs - 5.0).abs() < f64::EPSILON);
        let specs = c.plugin_specs().expect("specs");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "result_cache");
        assert_eq!(specs[0].params["dir"], "cli_dir");
        assert_eq!(specs[0].params["ttl_secs"], 5.0);

        // builder clamps negative ttl to 0 (never expire)
        let c = Config::new(
            vec![],
            100,
            500,
            0.1,
            std::path::PathBuf::from("kv_meta"),
            std::time::Duration::from_secs(600),
            "llama.cpp".to_string(),
            8081,
        )
        .with_prefilter_result_cache_dir(Some(std::path::PathBuf::from("rc")))
        .with_prefilter_result_cache_ttl_secs(-3.0);
        assert_eq!(c.prefilter_result_cache_ttl_secs, 0.0);
        let specs = c.plugin_specs().expect("specs");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].params["ttl_secs"], 0.0);
    }

    #[test]
    fn plugin_specs_combine_keyword_and_result_cache() {
        // only the blocklist -> one keyword spec
        let c = Config::from_env_map(&vars(&[("PREFILTER_BLOCKLIST", "bad")]));
        let specs = c.plugin_specs().expect("specs");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "keyword");

        // only the result cache -> one result_cache spec
        let c = Config::from_env_map(&vars(&[("PREFILTER_RESULT_CACHE_DIR", "rc")]));
        let specs = c.plugin_specs().expect("specs");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "result_cache");

        // both -> two specs in order (blocklist first: a blocked request is
        // rejected with 400 before a cache hit could serve it); the
        // registry chains them into one prefilter at load time
        let c = Config::from_env_map(&vars(&[
            ("PREFILTER_BLOCKLIST", "bad"),
            ("PREFILTER_RESULT_CACHE_DIR", "rc"),
        ]));
        let specs = c.plugin_specs().expect("specs");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "keyword");
        assert_eq!(specs[1].name, "result_cache");
    }

    #[test]
    fn config_precedence_cli_over_env_over_defaults() {
        let base = vars(&[
            ("PORT", "8081"),
            ("LCP_TH", "0.5"),
            ("META_MAX", "11"),
            ("BACKENDS", "old"),
        ]);
        let cli = Cli::parse(vec![
            "--port".to_string(),
            "9000".to_string(),
            "--meta-dir".to_string(),
            "cli_dir".to_string(),
        ])
        .unwrap();
        let merged = cli.merged_env(&base);
        // merged env: set flags override, untouched entries keep the env
        // value, unset flags add nothing
        assert_eq!(merged.get("PORT").map(String::as_str), Some("9000"));
        assert_eq!(merged.get("META_DIR").map(String::as_str), Some("cli_dir"));
        assert_eq!(merged.get("BACKENDS").map(String::as_str), Some("old"));
        assert!(!merged.contains_key("LLAMA_API_KEY"));
        // config: CLI > env > default
        let c = Config::from_env_map(&merged);
        assert_eq!(c.port, 9000); // CLI wins
        assert!((c.lcp_th - 0.5).abs() < f64::EPSILON); // env still applies
        assert_eq!(c.meta_max, 11); // env still applies
        assert_eq!(c.model_id, DEFAULT_MODEL_ID); // default when neither set
    }

    #[test]
    fn config_from_cli_overrides_process_env() {
        let cli = Cli::parse(vec!["--port".to_string(), "1234".to_string()]).unwrap();
        assert_eq!(Config::from_cli(&cli).port, 1234);
        let empty = Cli::default();
        assert_eq!(Config::from_cli(&empty).port, Config::from_env().port);
    }

    #[test]
    fn request_log_env_settings() {
        let c = Config::from_env_map(&vars(&[
            ("REQUEST_LOG", "true"),
            ("REQUEST_LOG_PATH", " logs/req.log "),
            ("REQUEST_LOG_MAX_BYTES", "12345"),
            ("REQUEST_LOG_KEEP", "7"),
            (
                "REQUEST_LOG_FIELDS",
                " cs-method , sc-status ,, lpcache-key ",
            ),
        ]));
        assert!(c.request_log_enabled);
        assert_eq!(c.request_log_path.to_string_lossy(), "logs/req.log");
        assert_eq!(c.request_log_max_bytes, 12345);
        assert_eq!(c.request_log_keep, 7);
        assert_eq!(
            c.request_log_fields,
            vec!["cs-method", "sc-status", "lpcache-key"]
        );
        // Invalid values fall back to the defaults with a warning.
        let c = Config::from_env_map(&vars(&[
            ("REQUEST_LOG", "maybe"),
            ("REQUEST_LOG_MAX_BYTES", "oops"),
            ("REQUEST_LOG_KEEP", "2.5"),
        ]));
        assert!(!c.request_log_enabled);
        assert_eq!(c.request_log_max_bytes, DEFAULT_REQUEST_LOG_MAX_BYTES);
        assert_eq!(c.request_log_keep, DEFAULT_REQUEST_LOG_KEEP);
        assert_eq!(c.request_log_path, PathBuf::from(DEFAULT_REQUEST_LOG_PATH));
        assert!(c.request_log_fields.is_empty());
    }

    #[test]
    fn request_log_spec_synthesized_when_enabled() {
        let c = base_cfg()
            .with_request_log(
                PathBuf::from("/var/log/lpcache/req.log"),
                1000,
                2,
                Some(vec!["cs-method".to_string()]),
            )
            .with_prefilter_blocklist(vec!["foo".to_string()]);
        let specs = c.plugin_specs().unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "keyword");
        let log = &specs[1];
        assert_eq!(log.name, "request_log");
        assert_eq!(log.params["path"], "/var/log/lpcache/req.log");
        assert_eq!(log.params["max_bytes"], 1000);
        assert_eq!(log.params["keep"], 2);
        assert_eq!(log.params["fields"], serde_json::json!(["cs-method"]));
        // Disabled by default: no spec at all (and no legacy plugins here).
        assert!(base_cfg().plugin_specs().unwrap().is_empty());
    }

    #[test]
    fn request_log_spec_in_plugins_wins() {
        let explicit = r#"[{"name": "request_log", "params": {"path": "/explicit/req.log"}}]"#;
        // A request_log spec in PLUGINS takes precedence: the synthesized
        // one (from the enabled REQUEST_LOG_* settings) is not appended.
        let c = base_cfg()
            .with_plugins(Some(explicit.to_string()))
            .with_request_log(PathBuf::from("/synthesized/req.log"), 1000, 2, None);
        let specs = c.plugin_specs().unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "request_log");
        assert_eq!(specs[0].params["path"], "/explicit/req.log");
        // A request_log spec in PLUGINS is loaded on its own (the
        // REQUEST_LOG* settings stay at their defaults).
        let c = base_cfg().with_plugins(Some(explicit.to_string()));
        assert!(!c.request_log_enabled);
        let specs = c.plugin_specs().unwrap();
        assert_eq!(specs.len(), 1);
        assert!(specs.iter().any(|s| s.name == "request_log"));
    }
}
