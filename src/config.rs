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
//! - `STREAM_QUEUE_SIZE` (16; capacity of the per-request bounded channel
//!   that buffers streamed SSE bytes between the background reader and the
//!   HTTP response — smaller values backpressure the backend faster)
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

    /// Override the stream queue capacity (`STREAM_QUEUE_SIZE`); values
    /// below 1 are clamped to 1.
    pub fn with_stream_queue_size(mut self, size: usize) -> Self {
        self.stream_queue_size = size.max(1);
        self
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
}
