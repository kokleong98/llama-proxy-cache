//! Configuration model for `lpcache-config`.
//!
//! A TOML-friendly mirror of the `lpcache` env/flag settings
//! (see `lpcache/src/config.rs` and `PARAMETERS.md` in the workspace root).
//! Every field maps 1:1 to an `lpcache` environment variable; defaults are
//! taken from the `lpcache::config` constants so the two never drift apart.
//!
//! The on-disk file layout:
//!
//! ```toml
//! [proxy]        PORT / MODEL_ID / LOG_LEVEL / REQUEST_TIMEOUT /
//!                STREAM_QUEUE_SIZE / COALESCE_REQUESTS / LLAMA_API_KEY
//! [[backend]]    one entry per backend (BACKENDS when >1, LLAMA_URL /
//!                N_SLOTS / SLOT_SAVE_PATH for a single backend)
//! [kv_cache]     WORDS_PER_BLOCK / BIG_THRESHOLD_WORDS / LCP_TH / META_DIR /
//!                META_MAX / SLOT_SAVE_PATH / SAVE_RETRIES /
//!                SAVE_RETRY_DELAY_MS
//! [prefilter]    PREFILTER_BLOCKLIST / PREFILTER_CASE_INSENSITIVE /
//!                PREFILTER_RESULT_CACHE_DIR / PREFILTER_RESULT_CACHE_TTL
//! [request_log]  REQUEST_LOG / REQUEST_LOG_PATH / REQUEST_LOG_MAX_BYTES /
//!                REQUEST_LOG_KEEP / REQUEST_LOG_FIELDS
//! [plugins]      PLUGINS (raw JSON array of plugin specs)
//! ```

use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::path::Path;

/// Default configuration file name (resolved against the CWD).
pub const DEFAULT_CONFIG_FILE: &str = "lpcache.config.toml";

/// The full lpcache configuration, in TOML serialization order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LpcacheConfig {
    pub proxy: ProxyConfig,
    /// TOML array-of-tables section name is `[[backend]]`.
    #[serde(rename = "backend", default)]
    pub backends: Vec<BackendEntry>,
    pub kv_cache: KvCacheConfig,
    pub prefilter: PrefilterConfig,
    pub request_log: RequestLogConfig,
    pub plugins: PluginsConfig,
}

/// One llama.cpp backend (an entry of the `BACKENDS` JSON array, or the
/// `LLAMA_URL` / `N_SLOTS` / `SLOT_SAVE_PATH` fallback for a single one).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackendEntry {
    /// Backend base URL (`BACKENDS[].url` / `LLAMA_URL`).
    pub url: String,
    /// Slot count — must match the backend's `-np/--parallel`.
    pub n_slots: usize,
    /// The backend's `--slot-save-path` dir (`""` = none).
    pub slot_save_path: String,
    /// KV cache save/restore support (`false` for llama-server < 1.05.40).
    pub kv_cache: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyConfig {
    /// `PORT`: proxy listen port.
    pub port: u16,
    /// `MODEL_ID`: model id advertised by `GET /v1/models`.
    pub model_id: String,
    /// `LOG_LEVEL`: TRACE..ERROR (`RUST_LOG` overrides at runtime).
    pub log_level: String,
    /// `REQUEST_TIMEOUT`: per-request timeout to the backend, seconds.
    pub request_timeout_secs: f64,
    /// `STREAM_QUEUE_SIZE`: per-request SSE channel capacity (>= 1).
    pub stream_queue_size: usize,
    /// `COALESCE_REQUESTS`: group concurrent same-key requests.
    pub coalesce_requests: bool,
    /// `LLAMA_API_KEY`: sent to the backend as `Authorization: Bearer <key>`
    /// (`""` = no auth header).
    pub api_key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KvCacheConfig {
    /// `WORDS_PER_BLOCK`: words per SHA256 hash block.
    pub words_per_block: usize,
    /// `BIG_THRESHOLD_WORDS`: word count above which a request is "big".
    pub big_threshold_words: usize,
    /// `LCP_TH`: min. shared-block ratio (0..1) for a restore candidate.
    pub lcp_th: f64,
    /// `META_DIR`: meta file directory (relative to the proxy's CWD).
    pub meta_dir: String,
    /// `META_MAX`: LRU bound on meta files kept (`0` = unlimited).
    pub meta_max: usize,
    /// `SLOT_SAVE_PATH`: backend `--slot-save-path` dir, single-backend
    /// fallback only (`""` = none; with multiple backends use the
    /// per-backend `slot_save_path`).
    pub slot_save_path: String,
    /// `SAVE_RETRIES`: retries after a failed KV cache save (`0` = none).
    pub save_retries: usize,
    /// `SAVE_RETRY_DELAY_MS`: delay between save retry attempts.
    pub save_retry_delay_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PrefilterConfig {
    /// `PREFILTER_BLOCKLIST`: keywords; matching requests are rejected with
    /// 400 before any slot/backend work (`[]` = no keyword prefilter).
    pub blocklist: Vec<String>,
    /// `PREFILTER_CASE_INSENSITIVE`: case-insensitive keyword matching.
    pub case_insensitive: bool,
    /// `PREFILTER_RESULT_CACHE_DIR`: result-cache dir (`""` = disabled).
    pub result_cache_dir: String,
    /// `PREFILTER_RESULT_CACHE_TTL`: per-entry expiry seconds (`0` = never).
    pub result_cache_ttl_secs: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestLogConfig {
    /// `REQUEST_LOG`: enable the W3C-XLF request log.
    pub enabled: bool,
    /// `REQUEST_LOG_PATH`: log file location (relative to the proxy's CWD).
    pub path: String,
    /// `REQUEST_LOG_MAX_BYTES`: rotation threshold (`0` = no rotation).
    pub max_bytes: u64,
    /// `REQUEST_LOG_KEEP`: rotated generations retained (`0` = keep all).
    pub keep: usize,
    /// `REQUEST_LOG_FIELDS`: ordered subset of XLF fields (`[]` = all 33).
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    /// `PLUGINS`: raw JSON array of plugin specs
    /// (`[{"name": "...", "params": {...}}]`). When non-empty, the
    /// `[prefilter]` section above is ignored by the proxy.
    pub json: String,
}

impl Default for BackendEntry {
    fn default() -> Self {
        Self {
            url: lpcache::config::DEFAULT_BACKEND_URL.to_string(),
            n_slots: lpcache::config::DEFAULT_N_SLOTS,
            slot_save_path: String::new(),
            kv_cache: true,
        }
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            port: lpcache::config::DEFAULT_PORT,
            model_id: lpcache::config::DEFAULT_MODEL_ID.to_string(),
            log_level: lpcache::config::DEFAULT_LOG_LEVEL.to_string(),
            request_timeout_secs: lpcache::config::DEFAULT_REQUEST_TIMEOUT_SECS,
            stream_queue_size: lpcache::config::DEFAULT_STREAM_QUEUE_SIZE,
            coalesce_requests: false,
            api_key: String::new(),
        }
    }
}

impl Default for KvCacheConfig {
    fn default() -> Self {
        Self {
            words_per_block: lpcache::config::DEFAULT_WORDS_PER_BLOCK,
            big_threshold_words: lpcache::config::DEFAULT_BIG_THRESHOLD_WORDS,
            lcp_th: lpcache::config::DEFAULT_LCP_TH,
            meta_dir: lpcache::config::DEFAULT_META_DIR.to_string(),
            meta_max: lpcache::config::DEFAULT_META_MAX,
            slot_save_path: String::new(),
            save_retries: lpcache::config::DEFAULT_SAVE_RETRIES,
            save_retry_delay_ms: lpcache::config::DEFAULT_SAVE_RETRY_DELAY_MS,
        }
    }
}

impl Default for PrefilterConfig {
    fn default() -> Self {
        Self {
            blocklist: Vec::new(),
            case_insensitive: lpcache::config::DEFAULT_PREFILTER_CASE_INSENSITIVE,
            result_cache_dir: String::new(),
            result_cache_ttl_secs: lpcache::config::DEFAULT_PREFILTER_RESULT_CACHE_TTL_SECS,
        }
    }
}

impl Default for RequestLogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: lpcache::config::DEFAULT_REQUEST_LOG_PATH.to_string(),
            max_bytes: lpcache::config::DEFAULT_REQUEST_LOG_MAX_BYTES,
            keep: lpcache::config::DEFAULT_REQUEST_LOG_KEEP,
            fields: Vec::new(),
        }
    }
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self { json: String::new() }
    }
}

impl Default for LpcacheConfig {
    fn default() -> Self {
        Self {
            proxy: ProxyConfig::default(),
            backends: vec![BackendEntry::default()],
            kv_cache: KvCacheConfig::default(),
            prefilter: PrefilterConfig::default(),
            request_log: RequestLogConfig::default(),
            plugins: PluginsConfig::default(),
        }
    }
}

impl LpcacheConfig {

    /// `(errors, warnings)` for the current configuration. Errors mean the
    /// proxy would misbehave or refuse to start; warnings usually indicate
    /// a mismatch with the backend's flags.
    pub fn validate(&self) -> (Vec<String>, Vec<String>) {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        if self.backends.is_empty() {
            errors.push("no backends configured — add at least one [[backend]]".into());
        }
        for (i, be) in self.backends.iter().enumerate() {
            let n = i + 1;
            if be.url.trim().is_empty() {
                errors.push(format!("backend {n}: url is empty"));
            } else if !be.url.starts_with("http://") && !be.url.starts_with("https://") {
                errors.push(format!(
                    "backend {n}: url {:?} must start with http:// or https://",
                    be.url
                ));
            }
            if be.n_slots == 0 {
                errors.push(format!("backend {n}: n_slots must be >= 1 (got 0)"));
            }
            if be.kv_cache && be.slot_save_path.trim().is_empty() {
                warnings.push(format!(
                    "backend {n} ({}): kv_cache is enabled but no slot_save_path is set — \
                     KV save/restore needs the backend's --slot-save-path dir",
                    be.url
                ));
            }
        }

        if self.proxy.port == 0 {
            errors.push("proxy.port must be in 1..=65535".into());
        }
        if !matches!(
            self.proxy.log_level.to_ascii_uppercase().as_str(),
            "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR" | "OFF"
        ) {
            warnings.push(format!(
                "proxy.log_level {:?} is not one of TRACE/DEBUG/INFO/WARN/ERROR/OFF",
                self.proxy.log_level
            ));
        }
        if self.proxy.request_timeout_secs <= 0.0 {
            errors.push("proxy.request_timeout_secs must be > 0".into());
        }
        if self.proxy.stream_queue_size == 0 {
            errors.push(
                "proxy.stream_queue_size must be >= 1 (0 is not representable)".into(),
            );
        }

        if self.kv_cache.words_per_block == 0 {
            errors.push("kv_cache.words_per_block must be >= 1".into());
        }
        if !(0.0..=1.0).contains(&self.kv_cache.lcp_th) {
            errors.push(format!("kv_cache.lcp_th {} must be in 0..=1", self.kv_cache.lcp_th));
        }
        if self.kv_cache.meta_dir.trim().is_empty() {
            warnings.push(
                "kv_cache.meta_dir is empty — the proxy would not store meta files".into(),
            );
        }
        if self.backends.len() > 1 && !self.kv_cache.slot_save_path.trim().is_empty() {
            warnings.push(
                "kv_cache.slot_save_path is set, but with multiple backends the \
                 per-backend slot_save_path values are used instead"
                    .into(),
            );
        }
        if self.kv_cache.save_retry_delay_ms == 0 && self.kv_cache.save_retries > 0 {
            warnings.push(
                "kv_cache.save_retries > 0 but save_retry_delay_ms is 0 — \
                 retries are back-to-back"
                    .into(),
            );
        }

        if self.prefilter.result_cache_ttl_secs < 0.0 {
            errors.push("prefilter.result_cache_ttl_secs must be >= 0".into());
        }

        // PLUGINS (when set) takes precedence over the [prefilter] section.
        if !self.plugins.json.trim().is_empty() {
            // Full plugin-aware validation: names, required params, types,
            // enums, ranges, nested chain filters (see `plugin_schema`).
            let (pe, pw) = crate::plugin_schema::validate_plugins(&self.plugins.json);
            errors.extend(pe);
            warnings.extend(pw);
        } else {
            if self.prefilter.result_cache_dir.trim().is_empty()
                && self.prefilter.blocklist.is_empty()
            {
                // nothing to warn about — no prefilter configured (the default)
            }
            if !self.prefilter.result_cache_dir.trim().is_empty()
                && self.prefilter.result_cache_ttl_secs == 0.0
            {
                warnings.push(
                    "prefilter.result_cache_ttl_secs is 0 — cached results never expire".into(),
                );
            }
        }

        (errors, warnings)
    }

    /// The compact `BACKENDS` JSON array for this configuration (defaults
    /// omitted: `slot_save_path` only when set, `kv_cache` only when false).
    pub fn backends_json(&self) -> String {
        let arr: Vec<serde_json::Value> = self
            .backends
            .iter()
            .map(|be| {
                let mut o = serde_json::json!({
                    "url": be.url,
                    "n_slots": be.n_slots,
                });
                if !be.slot_save_path.trim().is_empty() {
                    o["slot_save_path"] = serde_json::json!(be.slot_save_path);
                }
                if !be.kv_cache {
                    o["kv_cache"] = serde_json::json!(false);
                }
                o
            })
            .collect();
        serde_json::to_string(&arr).unwrap_or_default()
    }

    /// The effective lpcache environment variables, in export order:
    /// `(section, KEY, value)`.
    pub fn env_pairs(&self) -> Vec<(&'static str, String, String)> {
        let mut v: Vec<(&'static str, String, String)> = Vec::new();
        let p = &self.proxy;
        v.push(("proxy", "PORT".into(), p.port.to_string()));
        v.push(("proxy", "MODEL_ID".into(), p.model_id.clone()));
        v.push(("proxy", "LOG_LEVEL".into(), p.log_level.to_uppercase()));
        v.push((
            "proxy",
            "REQUEST_TIMEOUT".into(),
            fmt_num(p.request_timeout_secs),
        ));
        v.push((
            "proxy",
            "STREAM_QUEUE_SIZE".into(),
            p.stream_queue_size.to_string(),
        ));
        v.push((
            "proxy",
            "COALESCE_REQUESTS".into(),
            p.coalesce_requests.to_string(),
        ));
        if !p.api_key.trim().is_empty() {
            v.push(("proxy", "LLAMA_API_KEY".into(), p.api_key.clone()));
        }

        let k = &self.kv_cache;
        if self.backends.len() == 1 {
            let be = &self.backends[0];
            v.push(("backend", "LLAMA_URL".into(), be.url.clone()));
            v.push(("backend", "N_SLOTS".into(), be.n_slots.to_string()));
            let save = if be.slot_save_path.trim().is_empty() {
                k.slot_save_path.trim().to_string()
            } else {
                be.slot_save_path.clone()
            };
            if !save.is_empty() {
                v.push(("backend", "SLOT_SAVE_PATH".into(), save));
            }
        } else if !self.backends.is_empty() {
            v.push(("backend", "BACKENDS".into(), self.backends_json()));
        }
        v.push((
            "kv cache",
            "WORDS_PER_BLOCK".into(),
            k.words_per_block.to_string(),
        ));
        v.push((
            "kv cache",
            "BIG_THRESHOLD_WORDS".into(),
            k.big_threshold_words.to_string(),
        ));
        v.push(("kv cache", "LCP_TH".into(), fmt_num(k.lcp_th)));
        v.push(("kv cache", "META_DIR".into(), k.meta_dir.clone()));
        v.push(("kv cache", "META_MAX".into(), k.meta_max.to_string()));
        v.push((
            "kv cache",
            "SAVE_RETRIES".into(),
            k.save_retries.to_string(),
        ));
        v.push((
            "kv cache",
            "SAVE_RETRY_DELAY_MS".into(),
            k.save_retry_delay_ms.to_string(),
        ));

        let f = &self.prefilter;
        if self.plugins.json.trim().is_empty() {
            if !f.blocklist.is_empty() {
                v.push((
                    "prefilter",
                    "PREFILTER_BLOCKLIST".into(),
                    f.blocklist.join(","),
                ));
                v.push((
                    "prefilter",
                    "PREFILTER_CASE_INSENSITIVE".into(),
                    f.case_insensitive.to_string(),
                ));
            }
            if !f.result_cache_dir.trim().is_empty() {
                v.push((
                    "prefilter",
                    "PREFILTER_RESULT_CACHE_DIR".into(),
                    f.result_cache_dir.clone(),
                ));
                v.push((
                    "prefilter",
                    "PREFILTER_RESULT_CACHE_TTL".into(),
                    fmt_num(f.result_cache_ttl_secs),
                ));
            }
        }

        let r = &self.request_log;
        if r.enabled {
            v.push(("request log", "REQUEST_LOG".into(), "true".into()));
            v.push(("request log", "REQUEST_LOG_PATH".into(), r.path.clone()));
            v.push((
                "request log",
                "REQUEST_LOG_MAX_BYTES".into(),
                r.max_bytes.to_string(),
            ));
            v.push(("request log", "REQUEST_LOG_KEEP".into(), r.keep.to_string()));
            if !r.fields.is_empty() {
                v.push((
                    "request log",
                    "REQUEST_LOG_FIELDS".into(),
                    r.fields.join(","),
                ));
            }
        }

        if !self.plugins.json.trim().is_empty() {
            v.push((
                "plugins",
                "PLUGINS".into(),
                self.plugins.json.trim().to_string(),
            ));
        }
        v
    }

    /// The effective `lpcache` command-line flags (every setting explicit,
    /// flag names exactly as in `lpcache --help`), as `"<flag> <value>"`
    /// pairs.
    pub fn cli_args(&self) -> Vec<String> {
        let p = &self.proxy;
        let k = &self.kv_cache;
        let mut a: Vec<String> = vec![
            format!("--port {}", p.port),
            format!("--model-id {}", p.model_id),
            format!("--log-level {}", p.log_level.to_uppercase()),
            format!("--request-timeout {}", fmt_num(p.request_timeout_secs)),
            format!("--stream-queue-size {}", p.stream_queue_size),
            format!("--coalesce-requests {}", p.coalesce_requests),
        ];
        if !p.api_key.trim().is_empty() {
            a.push(format!("--api-key {}", p.api_key));
        }
        if self.backends.len() == 1 {
            let be = &self.backends[0];
            a.push(format!("--llama-url {}", be.url));
            a.push(format!("--n-slots {}", be.n_slots));
            let save = if be.slot_save_path.trim().is_empty() {
                k.slot_save_path.trim().to_string()
            } else {
                be.slot_save_path.clone()
            };
            if !save.is_empty() {
                a.push(format!("--slot-save-path {}", save));
            }
        } else if !self.backends.is_empty() {
            a.push(format!("--backends {}", self.backends_json()));
        }
        a.push(format!("--words-per-block {}", k.words_per_block));
        a.push(format!("--big-threshold-words {}", k.big_threshold_words));
        a.push(format!("--lcp-th {}", fmt_num(k.lcp_th)));
        a.push(format!("--meta-dir {}", k.meta_dir));
        a.push(format!("--meta-max {}", k.meta_max));
        a.push(format!("--save-retries {}", k.save_retries));
        a.push(format!("--save-retry-delay-ms {}", k.save_retry_delay_ms));
        let f = &self.prefilter;
        if self.plugins.json.trim().is_empty() {
            if !f.blocklist.is_empty() {
                a.push(format!("--prefilter-blocklist {}", f.blocklist.join(",")));
                a.push(format!(
                    "--prefilter-case-insensitive {}",
                    f.case_insensitive
                ));
            }
            if !f.result_cache_dir.trim().is_empty() {
                a.push(format!("--prefilter-result-cache-dir {}", f.result_cache_dir));
                a.push(format!(
                    "--prefilter-result-cache-ttl {}",
                    fmt_num(f.result_cache_ttl_secs)
                ));
            }
        }
        let r = &self.request_log;
        if r.enabled {
            a.push("--request-log true".into());
            a.push(format!("--request-log-path {}", r.path));
            a.push(format!("--request-log-max-bytes {}", r.max_bytes));
            a.push(format!("--request-log-keep {}", r.keep));
            if !r.fields.is_empty() {
                a.push(format!("--request-log-fields {}", r.fields.join(",")));
            }
        }
        if !self.plugins.json.trim().is_empty() {
            a.push(format!("--plugins {}", self.plugins.json.trim()));
        }
        a
    }

    /// Human-readable dump (used by `lpcache-config show`).
    pub fn show(&self, source: &str) -> String {
        let (errors, warnings) = self.validate();
        let mut s = String::new();
        let _ = writeln!(s, "lpcache configuration — {source}\n");
        let p = &self.proxy;
        let _ = writeln!(s, "  [proxy]");
        let _ = writeln!(s, "  port                   {}", p.port);
        let _ = writeln!(s, "  model_id               {}", p.model_id);
        let _ = writeln!(s, "  log_level              {}", p.log_level);
        let _ = writeln!(
            s,
            "  request_timeout_secs   {}",
            fmt_num(p.request_timeout_secs)
        );
        let _ = writeln!(s, "  stream_queue_size      {}", p.stream_queue_size);
        let _ = writeln!(s, "  coalesce_requests      {}", p.coalesce_requests);
        let _ = writeln!(
            s,
            "  api_key                {}",
            if p.api_key.trim().is_empty() {
                "(none)"
            } else {
                "********"
            }
        );
        let _ = writeln!(s, "\n  [backends]  ({})", self.backends.len());
        for (i, be) in self.backends.iter().enumerate() {
            let save = if be.slot_save_path.trim().is_empty() {
                "(none)"
            } else {
                be.slot_save_path.as_str()
            };
            let _ = writeln!(
                s,
                "  {}. {}  slots={}  kv_cache={}  slot_save_path={}",
                i + 1,
                be.url,
                be.n_slots,
                be.kv_cache,
                save
            );
        }
        let k = &self.kv_cache;
        let _ = writeln!(s, "\n  [kv_cache]");
        let _ = writeln!(s, "  words_per_block        {}", k.words_per_block);
        let _ = writeln!(s, "  big_threshold_words    {}", k.big_threshold_words);
        let _ = writeln!(s, "  lcp_th                 {}", fmt_num(k.lcp_th));
        let _ = writeln!(s, "  meta_dir               {}", k.meta_dir);
        let _ = writeln!(
            s,
            "  meta_max               {}{}",
            k.meta_max,
            if k.meta_max == 0 { " (unlimited)" } else { "" }
        );
        let _ = writeln!(
            s,
            "  slot_save_path         {}",
            if k.slot_save_path.trim().is_empty() {
                "(none)"
            } else {
                k.slot_save_path.as_str()
            }
        );
        let _ = writeln!(s, "  save_retries           {}", k.save_retries);
        let _ = writeln!(s, "  save_retry_delay_ms    {}", k.save_retry_delay_ms);
        let f = &self.prefilter;
        let _ = writeln!(s, "\n  [prefilter]");
        let blocklist = if f.blocklist.is_empty() {
            "(none)".to_string()
        } else {
            f.blocklist.join(", ")
        };
        let _ = writeln!(s, "  blocklist              {}", blocklist);
        let _ = writeln!(s, "  case_insensitive       {}", f.case_insensitive);
        let _ = writeln!(
            s,
            "  result_cache_dir       {}",
            if f.result_cache_dir.trim().is_empty() {
                "(disabled)"
            } else {
                f.result_cache_dir.as_str()
            }
        );
        let _ = writeln!(
            s,
            "  result_cache_ttl_secs  {}",
            fmt_num(f.result_cache_ttl_secs)
        );
        let r = &self.request_log;
        let _ = writeln!(s, "\n  [request_log]");
        if r.enabled {
            let _ = writeln!(s, "  enabled                true");
            let _ = writeln!(s, "  path                   {}", r.path);
            let _ = writeln!(s, "  max_bytes              {}", r.max_bytes);
            let _ = writeln!(s, "  keep                   {}", r.keep);
            let fields = if r.fields.is_empty() {
                "(all)".to_string()
            } else {
                r.fields.join(", ")
            };
            let _ = writeln!(s, "  fields                 {}", fields);
        } else {
            let _ = writeln!(s, "  enabled                false");
        }
        let _ = writeln!(s, "\n  [plugins]");
        let _ = writeln!(
            s,
            "  json                   {}",
            if self.plugins.json.trim().is_empty() {
                "(none — synthesized from the prefilter settings)"
            } else {
                self.plugins.json.trim()
            }
        );
        if errors.is_empty() && warnings.is_empty() {
            let _ = writeln!(s, "\n  validation: OK");
        } else {
            for e in &errors {
                let _ = writeln!(s, "  validation error:  {e}");
            }
            for w in &warnings {
                let _ = writeln!(s, "  validation warning: {w}");
            }
        }
        s
    }

    /// Get a dot-separated key (`proxy.port`, `kv_cache.lcp_th`, ...).
    /// Vector fields are returned comma-joined; `None` for unknown keys.
    pub fn get_key(&self, key: &str) -> Option<String> {
        match key {
            "proxy.port" => Some(self.proxy.port.to_string()),
            "proxy.model_id" => Some(self.proxy.model_id.clone()),
            "proxy.log_level" => Some(self.proxy.log_level.clone()),
            "proxy.request_timeout_secs" => Some(fmt_num(self.proxy.request_timeout_secs)),
            "proxy.stream_queue_size" => Some(self.proxy.stream_queue_size.to_string()),
            "proxy.coalesce_requests" => Some(self.proxy.coalesce_requests.to_string()),
            "proxy.api_key" => Some(self.proxy.api_key.clone()),
            "kv_cache.words_per_block" => Some(self.kv_cache.words_per_block.to_string()),
            "kv_cache.big_threshold_words" => Some(self.kv_cache.big_threshold_words.to_string()),
            "kv_cache.lcp_th" => Some(fmt_num(self.kv_cache.lcp_th)),
            "kv_cache.meta_dir" => Some(self.kv_cache.meta_dir.clone()),
            "kv_cache.meta_max" => Some(self.kv_cache.meta_max.to_string()),
            "kv_cache.slot_save_path" => Some(self.kv_cache.slot_save_path.clone()),
            "kv_cache.save_retries" => Some(self.kv_cache.save_retries.to_string()),
            "kv_cache.save_retry_delay_ms" => {
                Some(self.kv_cache.save_retry_delay_ms.to_string())
            }
            "prefilter.blocklist" => Some(self.prefilter.blocklist.join(",")),
            "prefilter.case_insensitive" => Some(self.prefilter.case_insensitive.to_string()),
            "prefilter.result_cache_dir" => Some(self.prefilter.result_cache_dir.clone()),
            "prefilter.result_cache_ttl_secs" => {
                Some(fmt_num(self.prefilter.result_cache_ttl_secs))
            }
            "request_log.enabled" => Some(self.request_log.enabled.to_string()),
            "request_log.path" => Some(self.request_log.path.clone()),
            "request_log.max_bytes" => Some(self.request_log.max_bytes.to_string()),
            "request_log.keep" => Some(self.request_log.keep.to_string()),
            "request_log.fields" => Some(self.request_log.fields.join(",")),
            "plugins.json" => Some(self.plugins.json.clone()),
            _ => None,
        }
    }

    /// All valid dot-separated keys, in order.
    pub fn keys() -> &'static [&'static str] {
        &[
            "proxy.port",
            "proxy.model_id",
            "proxy.log_level",
            "proxy.request_timeout_secs",
            "proxy.stream_queue_size",
            "proxy.coalesce_requests",
            "proxy.api_key",
            "kv_cache.words_per_block",
            "kv_cache.big_threshold_words",
            "kv_cache.lcp_th",
            "kv_cache.meta_dir",
            "kv_cache.meta_max",
            "kv_cache.slot_save_path",
            "kv_cache.save_retries",
            "kv_cache.save_retry_delay_ms",
            "prefilter.blocklist",
            "prefilter.case_insensitive",
            "prefilter.result_cache_dir",
            "prefilter.result_cache_ttl_secs",
            "request_log.enabled",
            "request_log.path",
            "request_log.max_bytes",
            "request_log.keep",
            "request_log.fields",
            "plugins.json",
        ]
    }

    /// Set a dot-separated key from its string value; `Err` naming the key
    /// when the value does not parse (or the key is unknown).
    pub fn set_key(&mut self, key: &str, value: &str) -> Result<(), String> {
        let set_bool = |field: &mut bool, v: &str, k: &str| -> Result<(), String> {
            let v = v.trim().to_ascii_lowercase();
            match v.as_str() {
                "true" | "1" | "yes" | "on" => {
                    *field = true;
                    Ok(())
                }
                "false" | "0" | "no" | "off" => {
                    *field = false;
                    Ok(())
                }
                _ => Err(format!("key {k}: {v:?} is not a boolean")),
            }
        };
        let set_list = |field: &mut Vec<String>, v: &str| {
            *field = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            Ok::<(), String>(())
        };
        match key {
            "proxy.port" => self.proxy.port = parse_u16(value, key)?,
            "proxy.model_id" => self.proxy.model_id = value.to_string(),
            "proxy.log_level" => self.proxy.log_level = value.to_uppercase(),
            "proxy.request_timeout_secs" => {
                self.proxy.request_timeout_secs = parse_f64(value, key)?
            }
            "proxy.stream_queue_size" => {
                self.proxy.stream_queue_size = parse_usize(value, key)?
            }
            "proxy.coalesce_requests" => set_bool(&mut self.proxy.coalesce_requests, value, key)?,
            "proxy.api_key" => self.proxy.api_key = value.to_string(),
            "kv_cache.words_per_block" => {
                self.kv_cache.words_per_block = parse_usize(value, key)?
            }
            "kv_cache.big_threshold_words" => {
                self.kv_cache.big_threshold_words = parse_usize(value, key)?
            }
            "kv_cache.lcp_th" => self.kv_cache.lcp_th = parse_f64(value, key)?,
            "kv_cache.meta_dir" => self.kv_cache.meta_dir = value.to_string(),
            "kv_cache.meta_max" => self.kv_cache.meta_max = parse_usize(value, key)?,
            "kv_cache.slot_save_path" => self.kv_cache.slot_save_path = value.to_string(),
            "kv_cache.save_retries" => self.kv_cache.save_retries = parse_usize(value, key)?,
            "kv_cache.save_retry_delay_ms" => {
                self.kv_cache.save_retry_delay_ms = parse_u64(value, key)?
            }
            "prefilter.blocklist" => set_list(&mut self.prefilter.blocklist, value)?,
            "prefilter.case_insensitive" => {
                set_bool(&mut self.prefilter.case_insensitive, value, key)?
            }
            "prefilter.result_cache_dir" => self.prefilter.result_cache_dir = value.to_string(),
            "prefilter.result_cache_ttl_secs" => {
                self.prefilter.result_cache_ttl_secs = parse_f64(value, key)?
            }
            "request_log.enabled" => set_bool(&mut self.request_log.enabled, value, key)?,
            "request_log.path" => self.request_log.path = value.to_string(),
            "request_log.max_bytes" => self.request_log.max_bytes = parse_u64(value, key)?,
            "request_log.keep" => self.request_log.keep = parse_usize(value, key)?,
            "request_log.fields" => set_list(&mut self.request_log.fields, value)?,
            "plugins.json" => self.plugins.json = value.to_string(),
            _ => {
                return Err(format!(
                    "unknown key {key:?} (valid keys: {})",
                    Self::keys().join(", ")
                ))
            }
        }
        Ok(())
    }
}

fn parse_u16(v: &str, key: &str) -> Result<u16, String> {
    v.trim()
        .parse::<u16>()
        .map_err(|_| format!("key {key}: {v:?} is not a valid port (1..=65535)"))
}
fn parse_usize(v: &str, key: &str) -> Result<usize, String> {
    v.trim()
        .parse::<usize>()
        .map_err(|_| format!("key {key}: {v:?} is not a non-negative integer"))
}
fn parse_u64(v: &str, key: &str) -> Result<u64, String> {
    v.trim()
        .parse::<u64>()
        .map_err(|_| format!("key {key}: {v:?} is not a non-negative integer"))
}
fn parse_f64(v: &str, key: &str) -> Result<f64, String> {
    v.trim()
        .parse::<f64>()
        .map_err(|_| format!("key {key}: {v:?} is not a number"))
}

/// Read a config from `path` (missing top-level sections fall back to the
/// defaults, matching `serde(default)`); `Err` on I/O or parse failure.
pub fn load(path: &Path) -> Result<LpcacheConfig, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    toml::from_str(&raw).map_err(|e| format!("{} is not valid TOML: {e}", path.display()))
}

/// Write the config as pretty-printed TOML to `path` (parent dirs created).
pub fn save(path: &Path, cfg: &LpcacheConfig) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
    }
    let raw = toml::to_string_pretty(cfg)
        .map_err(|e| format!("cannot serialize config: {e}"))?;
    std::fs::write(path, raw).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Format a float without a trailing `.0` when it is integral
/// (`600.0` → `600`, `0.1` → `0.1`), matching lpcache env expectations.
pub fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 9e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip() {
        let cfg = LpcacheConfig::default();
        let toml = toml::to_string_pretty(&cfg).unwrap();
        let back: LpcacheConfig = toml::from_str(&toml).unwrap();
        assert_eq!(back.proxy.port, 8081);
        assert_eq!(back.backends.len(), 1);
        assert_eq!(back.backends[0].url, lpcache::config::DEFAULT_BACKEND_URL);
        assert_eq!(back.kv_cache.lcp_th, lpcache::config::DEFAULT_LCP_TH);
        let (errors, _) = cfg.validate();
        assert!(errors.is_empty(), "defaults must be valid: {errors:?}");
    }

    #[test]
    fn set_get_keys() {
        let mut cfg = LpcacheConfig::default();
        cfg.set_key("proxy.port", "9000").unwrap();
        assert_eq!(cfg.get_key("proxy.port").unwrap(), "9000");
        cfg.set_key("prefilter.blocklist", "foo, bar ,").unwrap();
        assert_eq!(cfg.get_key("prefilter.blocklist").unwrap(), "foo,bar");
        cfg.set_key("proxy.coalesce_requests", "yes").unwrap();
        assert_eq!(cfg.get_key("proxy.coalesce_requests").unwrap(), "true");
        assert!(cfg.set_key("nope.nope", "x").is_err());
        assert!(cfg.set_key("proxy.port", "nope").is_err());
    }

    #[test]
    fn env_pairs_single_backend() {
        let mut cfg = LpcacheConfig::default();
        cfg.proxy.api_key = "sekret".into();
        cfg.kv_cache.slot_save_path = "kvcache".into();
        cfg.prefilter.blocklist = vec!["bad".into()];
        let pairs = cfg.env_pairs();
        let get = |k: &str| pairs.iter().find(|(_, key, _)| key == k).map(|(_, _, v)| v.clone());
        assert_eq!(get("LLAMA_URL").as_deref(), Some("http://127.0.0.1:8000"));
        assert_eq!(get("N_SLOTS").as_deref(), Some("1"));
        assert_eq!(get("SLOT_SAVE_PATH").as_deref(), Some("kvcache"));
        assert_eq!(get("LLAMA_API_KEY").as_deref(), Some("sekret"));
        assert_eq!(get("PREFILTER_BLOCKLIST").as_deref(), Some("bad"));
        assert!(get("BACKENDS").is_none());
        assert!(get("REQUEST_LOG").is_none());
    }

    #[test]
    fn env_pairs_multi_backend() {
        let mut cfg = LpcacheConfig::default();
        cfg.backends.push(BackendEntry {
            url: "http://10.0.0.2:8000".into(),
            n_slots: 2,
            slot_save_path: "/data/kv2".into(),
            kv_cache: false,
        });
        let pairs = cfg.env_pairs();
        let get = |k: &str| pairs.iter().find(|(_, key, _)| key == k).map(|(_, _, v)| v.clone());
        assert!(get("LLAMA_URL").is_none());
        let backends = get("BACKENDS").unwrap();
        let v: serde_json::Value = serde_json::from_str(&backends).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert_eq!(v[1]["kv_cache"], false);
        assert_eq!(v[1]["slot_save_path"], "/data/kv2");
        assert_eq!(v[0].get("kv_cache"), None, "default true must be omitted");
    }

    #[test]
    fn validation_catches_basics() {
        let mut cfg = LpcacheConfig::default();
        cfg.backends.clear();
        cfg.proxy.port = 0;
        cfg.kv_cache.lcp_th = 2.0;
        let (errors, _) = cfg.validate();
        assert!(errors.iter().any(|e| e.contains("no backends")));
        assert!(errors.iter().any(|e| e.contains("port")));
        assert!(errors.iter().any(|e| e.contains("lcp_th")));

        let mut cfg = LpcacheConfig::default();
        cfg.kv_cache.slot_save_path.clear();
        let (_, warnings) = cfg.validate();
        assert!(warnings.iter().any(|w| w.contains("slot_save_path")));
    }

    #[test]
    fn cli_args_contain_known_flags() {
        let cfg = LpcacheConfig::default();
        let args = cfg.cli_args();
        assert!(args.contains(&"--port 8081".to_string()));
        assert!(args.contains(&"--llama-url http://127.0.0.1:8000".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("--request-log ")));
    }
}

