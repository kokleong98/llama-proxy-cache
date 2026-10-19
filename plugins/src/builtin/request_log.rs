//! `request_log` plugin — W3C Extended Log Format (XLF) request log to a
//! file, rotated when the file reaches a max size.
//!
//! Every finished HTTP request becomes one record (one XLF line):
//! W3C-standard fields (`datetime`, `cs-method`, `cs-uri`, `c-ip`,
//! `cs(User-Agent)`, `cs-bytes`, `cs-time`, `sc-status`, `sc-bytes`) plus
//! the lpcache pipeline fields (`lpcache-model`, `lpcache-stream`,
//! `lpcache-key`, `lpcache-n-words`, `lpcache-is-big`, `lpcache-backend`,
//! `lpcache-slot`, `lpcache-restored`, `lpcache-cache-hit`,
//! `lpcache-coalesced`, `lpcache-reason`) and the lpcache performance
//! fields (`lpcache-prompt-tokens`, `lpcache-completion-tokens`,
//! `lpcache-cached-tokens`, `lpcache-prefill-ms`, `lpcache-gen-ms`,
//! `lpcache-prefill-tps`, `lpcache-gen-tps`, `lpcache-ttft-ms` —
//! backend-reported token counts/timings plus the proxy-side time to
//! first token).
//!
//! The file starts with the XLF header:
//!
//! ```text
//! #Software: lpcache-plugins/0.1.0
//! #Version: 1.0
//! #Date-format: %Y-%m-%dT%H:%M:%S.%fZ
//! #Fields: datetime cs-method cs-uri ... lpcache-reason
//! ```
//!
//! Field values are space-separated; a missing value renders as `-`
//! ("not available"); a value containing whitespace, a quote, a backslash,
//! or starting with `-`/`"` is enclosed in double quotes with `\`, `"`, CR
//! and LF escaped (the XLF escaping rules).
//!
//! Rotation: when the current file grows beyond `max_bytes` it is renamed
//! to `<path>.1`, the existing `<path>.1` moves to `<path>.2`, and so on —
//! at most `keep` rotated generations are retained (the oldest one is
//! dropped; `keep` = `0` keeps them all). The fresh current file starts
//! with the XLF header again.
//!
//! Params:
//! - `path` (default `lpcache-request.log`): the log file location
//!   (relative paths resolve against the proxy's working directory; the
//!   parent dir is created).
//! - `max_bytes` (default 10 MiB): rotation threshold; `0` = no rotation.
//! - `keep` (default `5`): rotated generations kept (`0` = keep all).
//! - `fields` (optional): ordered list of field names to emit — any
//!   subset of the 28 known fields (default: all of them, W3C fields
//!   first).
//!
//! The `datetime` field uses UTC with millisecond precision
//! (`%Y-%m-%dT%H:%M:%S.%fZ`, matching the declared `#Date-format`).

use crate::api::{RequestLogRecord, RequestLogger};
use crate::registry::{PluginError, PluginFactory};
use serde::Deserialize;
use serde_json::Value;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Plugin name (registry key).
pub const NAME: &str = "request_log";
/// Default log file location.
pub const DEFAULT_PATH: &str = "lpcache-request.log";
/// Default rotation threshold (10 MiB); `0` disables rotation.
pub const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Default number of rotated generations kept; `0` keeps them all.
pub const DEFAULT_KEEP: usize = 5;
/// Default field order: W3C-style fields first, lpcache pipeline fields
/// after.
pub const DEFAULT_FIELDS: &[&str] = &[
    "datetime",
    "cs-method",
    "cs-uri",
    "c-ip",
    "cs(User-Agent)",
    "cs-bytes",
    "cs-time",
    "sc-status",
    "sc-bytes",
    "lpcache-model",
    "lpcache-stream",
    "lpcache-key",
    "lpcache-n-words",
    "lpcache-is-big",
    "lpcache-backend",
    "lpcache-slot",
    "lpcache-restored",
    "lpcache-cache-hit",
    "lpcache-coalesced",
    "lpcache-reason",
    "lpcache-prompt-tokens",
    "lpcache-completion-tokens",
    "lpcache-cached-tokens",
    "lpcache-prefill-ms",
    "lpcache-gen-ms",
    "lpcache-prefill-tps",
    "lpcache-gen-tps",
    "lpcache-ttft-ms",
];

/// Every known field name (the `fields` parameter is validated against
/// this list).
pub const KNOWN_FIELDS: &[&str] = DEFAULT_FIELDS;

/// W3C-XLF escaping: a value containing whitespace, a double quote, or a
/// backslash — or starting with `-`/`"` (ambiguous with "not available" /
/// another quoted value) — is enclosed in double quotes, with `\`, `"`,
/// CR and LF escaped. An empty value renders as `-`.
pub fn escape_value(value: &str) -> String {
    if value.is_empty() {
        return "-".to_string();
    }
    let needs_quote = value.starts_with('-')
        || value.starts_with('"')
        || value
            .bytes()
            .any(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'"' | b'\\'));
    if !needs_quote {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// `cs-time` rendering: seconds with millisecond precision (`0.123`).
fn format_secs(secs: f64) -> String {
    format!("{secs:.3}")
}

/// Millisecond rendering with two decimals (`2670.62`).
fn format_ms(ms: f64) -> String {
    format!("{ms:.2}")
}

/// Tokens-per-second rendering with one decimal (`907.6`).
fn format_tps(tps: f64) -> String {
    format!("{tps:.1}")
}

/// The value of one field for `record` (`None` = not available → `-`).
fn field_value(record: &RequestLogRecord, field: &str) -> Option<String> {
    match field {
        "datetime" => record.datetime.clone(),
        "cs-method" => record.method.clone(),
        "cs-uri" => record.uri.clone(),
        "c-ip" => record.client_ip.clone(),
        "cs(User-Agent)" => record.user_agent.clone(),
        "cs-bytes" => record.request_bytes.map(|v| v.to_string()),
        "cs-time" => record.duration_secs.map(format_secs),
        "sc-status" => record.status.map(|v| v.to_string()),
        "sc-bytes" => record.response_bytes.map(|v| v.to_string()),
        "lpcache-model" => record.model.clone(),
        "lpcache-stream" => record.stream.map(|v| v.to_string()),
        "lpcache-key" => record.key.clone(),
        "lpcache-n-words" => record.n_words.map(|v| v.to_string()),
        "lpcache-is-big" => record.is_big.map(|v| v.to_string()),
        "lpcache-backend" => record.backend.map(|v| v.to_string()),
        "lpcache-slot" => record.slot.map(|v| v.to_string()),
        "lpcache-restored" => record.restored.map(|v| v.to_string()),
        "lpcache-cache-hit" => record.cache_hit.map(|v| v.to_string()),
        "lpcache-coalesced" => record.coalesced.map(|v| v.to_string()),
        "lpcache-reason" => record.reason.clone(),
        "lpcache-prompt-tokens" => record.prompt_tokens.map(|v| v.to_string()),
        "lpcache-completion-tokens" => record.completion_tokens.map(|v| v.to_string()),
        "lpcache-cached-tokens" => record.cached_tokens.map(|v| v.to_string()),
        "lpcache-prefill-ms" => record.prefill_ms.map(format_ms),
        "lpcache-gen-ms" => record.gen_ms.map(format_ms),
        "lpcache-prefill-tps" => record.prefill_tps.map(format_tps),
        "lpcache-gen-tps" => record.gen_tps.map(format_tps),
        "lpcache-ttft-ms" => record.ttft_ms.map(format_ms),
        // Unreachable: the field list is validated at construction.
        _ => None,
    }
}

/// File state behind the write mutex: the open file (lazy) and the number
/// of bytes currently in it (tracked, not stat'ed — rotation depends on
/// it).
#[derive(Default)]
struct Inner {
    file: Option<File>,
    size: u64,
}

/// Built-in request logger: one W3C-XLF line per finished request,
/// appended to `path`, rotated at `max_bytes` with `keep` generations
/// retained (see the module docs).
///
/// All file operations run under one mutex, so concurrent records are
/// serialized and never interleave. The file is opened lazily (append
/// mode); `prepare` opens it early so a bad path fails at startup.
pub struct RequestLog {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    fields: Vec<String>,
    inner: Mutex<Inner>,
}

impl RequestLog {
    /// Build a logger over `path` (created with its parent dir on first
    /// use), rotating at `max_bytes` (`0` = no rotation) with `keep`
    /// rotated generations retained (`0` = keep all), emitting the
    /// `fields` in the given order.
    ///
    /// `Err` when `fields` is empty, contains duplicates, or names a
    /// field that is not in [`KNOWN_FIELDS`].
    pub fn new(
        path: PathBuf,
        max_bytes: u64,
        keep: usize,
        fields: Vec<String>,
    ) -> Result<Self, String> {
        if fields.is_empty() {
            return Err("`fields` must not be empty".to_string());
        }
        for f in &fields {
            if !KNOWN_FIELDS.contains(&f.as_str()) {
                return Err(format!(
                    "unknown field {f:?} (known fields: {})",
                    KNOWN_FIELDS.join(", ")
                ));
            }
        }
        {
            let mut seen = std::collections::HashSet::new();
            for f in &fields {
                if !seen.insert(f.as_str()) {
                    return Err(format!("duplicate field {f:?} in `fields`"));
                }
            }
        }
        Ok(Self {
            path,
            max_bytes,
            keep,
            fields,
            inner: Mutex::new(Inner::default()),
        })
    }

    /// The log file location.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The rotation threshold in bytes (`0` = no rotation).
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// The number of rotated generations kept (`0` = keep all).
    pub fn keep(&self) -> usize {
        self.keep
    }

    /// The field order used for the `#Fields:` header and each record.
    pub fn fields(&self) -> &[String] {
        &self.fields
    }

    /// The XLF header block (`#Software`, `#Version`, `#Date-format`,
    /// `#Fields`).
    fn header(&self) -> String {
        format!(
            "#Software: {}/{}\n#Version: 1.0\n#Date-format: %Y-%m-%dT%H:%M:%S.%fZ\n#Fields: {}\n",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
            self.fields.join(" ")
        )
    }

    /// Render one record as a complete XLF line (values in `fields`
    /// order, space-separated, `\n`-terminated).
    fn render_line(&self, record: &RequestLogRecord) -> String {
        let mut line = String::new();
        for (i, field) in self.fields.iter().enumerate() {
            if i > 0 {
                line.push(' ');
            }
            match field_value(record, field) {
                Some(v) => line.push_str(&escape_value(&v)),
                None => line.push('-'),
            }
        }
        line.push('\n');
        line
    }

    /// Open the file (append mode) when closed, remembering its size;
    /// write the XLF header when the file is empty (new or externally
    /// truncated).
    fn ensure_open(&self, inner: &mut Inner) -> std::io::Result<()> {
        if inner.file.is_none() {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            inner.size = file.metadata()?.len();
            inner.file = Some(file);
        }
        if inner.size == 0 {
            let header = self.header();
            let file = inner.file.as_mut().expect("file open after create");
            file.write_all(header.as_bytes())?;
            inner.size += header.len() as u64;
        }
        Ok(())
    }

    /// Rotate: `<path>.k` → `<path>.k+1` (highest first; the oldest
    /// generation that would exceed `keep` is dropped), `<path>` →
    /// `<path>.1`, and the current file is reopened fresh (the header
    /// follows on the next write, since the size resets to 0).
    fn rotate(&self, inner: &mut Inner) -> std::io::Result<()> {
        inner.file.take();
        let parent = self
            .path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let base = self
            .path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or_default();
        // Existing numeric generations `<base>.<n>`, ascending.
        let mut gens: Vec<u64> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&parent) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(n) = name.strip_prefix(&format!("{base}.")) else {
                    continue;
                };
                if let Ok(g) = n.parse::<u64>() {
                    gens.push(g);
                }
            }
        }
        gens.sort_unstable();
        // After the shift, generation g lands at g+1; with a bounded
        // `keep`, the top kept generation is `keep` — drop what would
        // exceed it.
        if self.keep > 0 {
            for g in gens.iter().rev() {
                if *g >= self.keep as u64 {
                    let _ = std::fs::remove_file(parent.join(format!("{base}.{g}")));
                }
            }
        }
        for g in gens.iter().rev() {
            let from = parent.join(format!("{base}.{g}"));
            if from.exists() {
                std::fs::rename(&from, parent.join(format!("{base}.{}", g + 1)))?;
            }
        }
        std::fs::rename(&self.path, parent.join(format!("{base}.1")))?;
        inner.size = 0;
        Ok(())
    }
}

impl RequestLogger for RequestLog {
    fn name(&self) -> &str {
        NAME
    }

    fn prepare(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create dir {}: {e}", parent.display()))?;
        }
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| format!("request log mutex poisoned: {e}"))?;
        self.ensure_open(&mut inner)
            .map_err(|e| format!("open {}: {e}", self.path.display()))
    }

    fn log(&self, record: &RequestLogRecord) -> Result<(), String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|e| format!("request log mutex poisoned: {e}"))?;
        let line = self.render_line(record);
        self.ensure_open(&mut inner)
            .map_err(|e| format!("open {}: {e}", self.path.display()))?;
        let file = inner.file.as_mut().expect("file open after ensure_open");
        file.write_all(line.as_bytes())
            .map_err(|e| format!("write {}: {e}", self.path.display()))?;
        inner.size += line.len() as u64;
        if self.max_bytes > 0 && inner.size > self.max_bytes {
            self.rotate(&mut inner)
                .map_err(|e| format!("rotate {}: {e}", self.path.display()))?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Plugin factory (configurable, dynamically loadable)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RequestLogParams {
    #[serde(default = "default_path")]
    path: String,
    #[serde(default = "default_max_bytes")]
    max_bytes: u64,
    #[serde(default = "default_keep")]
    keep: usize,
    #[serde(default)]
    fields: Option<Vec<String>>,
}

fn default_path() -> String {
    DEFAULT_PATH.to_string()
}

fn default_max_bytes() -> u64 {
    DEFAULT_MAX_BYTES
}

fn default_keep() -> usize {
    DEFAULT_KEEP
}

/// Factory for the `request_log` plugin (registered by
/// [`crate::builtin::register`]).
pub struct RequestLogPluginFactory;

impl PluginFactory for RequestLogPluginFactory {
    fn create_request_logger(
        &self,
        params: &Value,
    ) -> Result<Option<Arc<dyn RequestLogger>>, PluginError> {
        let RequestLogParams {
            path,
            max_bytes,
            keep,
            fields,
        } = RequestLogParams::deserialize(params).map_err(|e| PluginError::BadParams {
            name: NAME.to_string(),
            message: e.to_string(),
        })?;
        if path.trim().is_empty() {
            return Err(PluginError::BadParams {
                name: NAME.to_string(),
                message: "`path` must not be empty".to_string(),
            });
        }
        let fields =
            fields.unwrap_or_else(|| DEFAULT_FIELDS.iter().map(|f| f.to_string()).collect());
        let log =
            RequestLog::new(PathBuf::from(path), max_bytes, keep, fields).map_err(|message| {
                PluginError::BadParams {
                    name: NAME.to_string(),
                    message,
                }
            })?;
        Ok(Some(Arc::new(log)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::format_utc;

    /// `Result::expect` needs `T: Debug`; the request-logger trait objects
    /// are not `Debug`, so these match instead.
    fn ok_of<R, E: std::fmt::Display>(r: Result<R, E>) -> R {
        match r {
            Ok(v) => v,
            Err(e) => panic!("expected Ok: {e}"),
        }
    }

    fn err_of<R, E: std::fmt::Display>(r: Result<R, E>) -> E {
        match r {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        }
    }

    fn record() -> RequestLogRecord {
        RequestLogRecord {
            datetime: Some("2026-01-01T00:00:00.000Z".to_string()),
            method: Some("POST".to_string()),
            uri: Some("/v1/chat/completions".to_string()),
            client_ip: Some("127.0.0.1".to_string()),
            user_agent: Some("test-agent".to_string()),
            request_bytes: Some(42),
            duration_secs: Some(0.123),
            status: Some(200),
            response_bytes: Some(17),
            model: Some("llama.cpp".to_string()),
            stream: Some(false),
            key: Some("abcdef0123456789".to_string()),
            n_words: Some(120),
            is_big: Some(false),
            backend: Some(0),
            slot: Some(3),
            restored: Some(true),
            cache_hit: Some(false),
            coalesced: Some("leader"),
            reason: None,
            prompt_tokens: Some(2423),
            completion_tokens: Some(64),
            cached_tokens: Some(2422),
            prefill_ms: Some(2670.62),
            gen_ms: Some(1104.21),
            prefill_tps: Some(907.6),
            gen_tps: Some(57.9),
            ttft_ms: Some(312.4),
        }
    }

    fn default_fields() -> Vec<String> {
        DEFAULT_FIELDS.iter().map(|f| f.to_string()).collect()
    }

    fn log_in(td: &tempfile::TempDir) -> (RequestLog, PathBuf) {
        let path = td.path().join("request.log");
        (
            RequestLog::new(
                path.clone(),
                DEFAULT_MAX_BYTES,
                DEFAULT_KEEP,
                default_fields(),
            )
            .expect("log"),
            path,
        )
    }

    #[test]
    fn header_and_record_format() {
        let td = tempfile::tempdir().expect("tempdir");
        let (log, path) = log_in(&td);
        log.log(&record()).expect("log");
        let content = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = content.lines().collect();
        assert!(lines[0].starts_with("#Software: lpcache-plugins/"));
        assert_eq!(lines[1], "#Version: 1.0");
        assert_eq!(lines[2], "#Date-format: %Y-%m-%dT%H:%M:%S.%fZ");
        assert_eq!(lines[3], format!("#Fields: {}", DEFAULT_FIELDS.join(" ")));
        assert_eq!(lines.len(), 5);
        let values: Vec<&str> = lines[4].split(' ').collect();
        assert_eq!(values.len(), DEFAULT_FIELDS.len());
        let get = |name: &str| -> &str {
            let i = DEFAULT_FIELDS
                .iter()
                .position(|f| *f == name)
                .expect("field");
            values[i]
        };
        assert_eq!(get("datetime"), "2026-01-01T00:00:00.000Z");
        assert_eq!(get("cs-method"), "POST");
        assert_eq!(get("cs-uri"), "/v1/chat/completions");
        assert_eq!(get("c-ip"), "127.0.0.1");
        assert_eq!(get("cs(User-Agent)"), "test-agent");
        assert_eq!(get("cs-bytes"), "42");
        assert_eq!(get("cs-time"), "0.123");
        assert_eq!(get("sc-status"), "200");
        assert_eq!(get("sc-bytes"), "17");
        assert_eq!(get("lpcache-model"), "llama.cpp");
        assert_eq!(get("lpcache-restored"), "true");
        assert_eq!(get("lpcache-coalesced"), "leader");
        assert_eq!(get("lpcache-reason"), "-"); // None -> not available
        // performance fields (deterministic mock values).
        assert_eq!(get("lpcache-prompt-tokens"), "2423");
        assert_eq!(get("lpcache-completion-tokens"), "64");
        assert_eq!(get("lpcache-cached-tokens"), "2422");
        assert_eq!(get("lpcache-prefill-ms"), "2670.62");
        assert_eq!(get("lpcache-gen-ms"), "1104.21");
        assert_eq!(get("lpcache-prefill-tps"), "907.6");
        assert_eq!(get("lpcache-gen-tps"), "57.9");
        assert_eq!(get("lpcache-ttft-ms"), "312.40");
    }

    #[test]
    fn perf_fields_render_dash_when_backend_reports_none() {
        // A record with no performance data at all (e.g. a failure path,
        // or a backend without `timings`): every perf field is `-` while
        // the other fields are intact.
        let td = tempfile::tempdir().expect("tempdir");
        let (log, path) = log_in(&td);
        let mut r = record();
        r.prompt_tokens = None;
        r.completion_tokens = None;
        r.cached_tokens = None;
        r.prefill_ms = None;
        r.gen_ms = None;
        r.prefill_tps = None;
        r.gen_tps = None;
        r.ttft_ms = None;
        log.log(&r).expect("log");
        let content = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = content.lines().collect();
        let values: Vec<&str> = lines[4].split(' ').collect();
        let get = |name: &str| -> &str {
            let i = DEFAULT_FIELDS
                .iter()
                .position(|f| *f == name)
                .expect("field");
            values[i]
        };
        for f in [
            "lpcache-prompt-tokens",
            "lpcache-completion-tokens",
            "lpcache-cached-tokens",
            "lpcache-prefill-ms",
            "lpcache-gen-ms",
            "lpcache-prefill-tps",
            "lpcache-gen-tps",
            "lpcache-ttft-ms",
        ] {
            assert_eq!(get(f), "-", "{f}");
        }
        // non-perf fields unaffected.
        assert_eq!(get("lpcache-model"), "llama.cpp");
        assert_eq!(get("sc-status"), "200");
    }

    #[test]
    fn missing_fields_render_dash() {
        let td = tempfile::tempdir().expect("tempdir");
        let (log, path) = log_in(&td);
        log.log(&RequestLogRecord::default()).expect("log");
        let content = std::fs::read_to_string(&path).expect("read");
        let record_line = content
            .lines()
            .find(|l| !l.starts_with('#'))
            .expect("record line");
        let values: Vec<&str> = record_line.split(' ').collect();
        assert_eq!(values.len(), DEFAULT_FIELDS.len());
        assert!(values.iter().all(|v| *v == "-"));
    }

    #[test]
    fn escaping_rules() {
        assert_eq!(escape_value("plain"), "plain");
        assert_eq!(escape_value(""), "-");
        // starts with '-' (ambiguous with "not available")
        assert_eq!(escape_value("-leading"), "\"-leading\"");
        // starts with '"'
        assert_eq!(escape_value("\"quoted"), "\"\\\"quoted\"");
        // whitespace / quote / backslash inside
        assert_eq!(escape_value("a b"), "\"a b\"");
        assert_eq!(escape_value("a\"b"), "\"a\\\"b\"");
        assert_eq!(escape_value("a\\b"), "\"a\\\\b\"");
        assert_eq!(escape_value("a\nb"), "\"a\\nb\"");
        assert_eq!(escape_value("a\rb"), "\"a\\rb\"");
        // non-ASCII passes through unquoted
        assert_eq!(escape_value("café"), "café");
    }

    #[test]
    fn custom_fields_subset_and_order() {
        let td = tempfile::tempdir().expect("tempdir");
        let path = td.path().join("sub.log");
        let log = RequestLog::new(
            path.clone(),
            DEFAULT_MAX_BYTES,
            DEFAULT_KEEP,
            vec!["sc-status".to_string(), "cs-method".to_string()],
        )
        .expect("log");
        log.log(&record()).expect("log");
        let content = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines[3], "#Fields: sc-status cs-method");
        assert_eq!(lines[4], "200 POST");
    }

    #[test]
    fn invalid_field_lists_are_rejected() {
        let err = err_of(RequestLog::new(PathBuf::from("x"), 0, 0, vec![]));
        assert!(err.contains("fields"));
        let err = err_of(RequestLog::new(
            PathBuf::from("x"),
            0,
            0,
            vec!["nope".to_string()],
        ));
        assert!(err.contains("nope"));
        let err = err_of(RequestLog::new(
            PathBuf::from("x"),
            0,
            0,
            vec!["sc-status".to_string(), "sc-status".to_string()],
        ));
        assert!(err.contains("duplicate"));
    }

    #[test]
    fn rotation_moves_generations_and_rewrites_header() {
        let td = tempfile::tempdir().expect("tempdir");
        let path = td.path().join("rot.log");
        // Threshold below one record line: every write rotates, so each
        // generation holds exactly one record (plus a fresh header).
        let log = RequestLog::new(path.clone(), 100, 0, default_fields()).expect("log");
        log.prepare().expect("prepare");
        for i in 0..5 {
            let mut r = record();
            r.uri = Some(format!("/v1/chat/completions?i={i}"));
            log.log(&r).expect("log");
        }
        // .1 holds the newest record, .5 the first; keep=0 retains all.
        let gen1 = path.with_extension("log.1");
        let gen2 = path.with_extension("log.2");
        let gen5 = path.with_extension("log.5");
        assert!(gen1.is_file(), "generation 1 exists");
        assert!(gen5.is_file(), "keep=0 retains all generations");
        let gen1_content = std::fs::read_to_string(&gen1).expect("read");
        assert!(gen1_content.starts_with("#Software:"));
        let records: Vec<&str> = gen1_content
            .lines()
            .filter(|l| !l.starts_with('#'))
            .collect();
        assert_eq!(records.len(), 1);
        assert!(records[0].contains("/v1/chat/completions?i=4"));
        // Every rotated file re-gains the XLF header.
        let gen2_content = std::fs::read_to_string(&gen2).expect("read");
        assert!(gen2_content.starts_with("#Software:"));
        assert!(gen2_content.contains("/v1/chat/completions?i=3"));
        // The current file was rotated away; the next write re-creates it
        // (header + record) — and rotates it right away again.
        assert!(!path.is_file());
        log.log(&record()).expect("log");
        assert!(
            !path.is_file(),
            "a single record already exceeds the threshold"
        );
        let gen1_content = std::fs::read_to_string(&gen1).expect("read");
        assert!(gen1_content.starts_with("#Software:"));
        assert!(gen1_content.contains("/v1/chat/completions"));
    }

    #[test]
    fn keep_bounds_retained_generations() {
        let td = tempfile::tempdir().expect("tempdir");
        let path = td.path().join("keep.log");
        let log = RequestLog::new(path.clone(), 100, 2, default_fields()).expect("log");
        for i in 0..120 {
            let mut r = record();
            r.uri = Some(format!("/req/{i:03}"));
            log.log(&r).expect("log");
        }
        // At most generations 1..=2 survive (plus the current file).
        let gens: Vec<u64> = std::fs::read_dir(td.path())
            .expect("read_dir")
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.strip_prefix("keep.log."))
                    .and_then(|n| n.parse::<u64>().ok())
            })
            .collect();
        assert!(gens.iter().all(|g| *g <= 2), "bounded to keep: {gens:?}");
        assert!(gens.contains(&1), "newest generation kept: {gens:?}");
        // With keep=0 (unbounded), generations accumulate.
        let path2 = td.path().join("all.log");
        let log2 = RequestLog::new(path2.clone(), 100, 0, default_fields()).expect("log");
        for i in 0..120 {
            let mut r = record();
            r.uri = Some(format!("/req/{i:03}"));
            log2.log(&r).expect("log");
        }
        let gens2: Vec<u64> = std::fs::read_dir(td.path())
            .expect("read_dir")
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.strip_prefix("all.log."))
                    .and_then(|n| n.parse::<u64>().ok())
            })
            .collect();
        assert!(gens2.len() > 2, "unbounded generations: {gens2:?}");
    }

    #[test]
    fn rotation_disabled_when_max_bytes_zero() {
        let td = tempfile::tempdir().expect("tempdir");
        let path = td.path().join("request.log");
        let log = RequestLog::new(path.clone(), 0, DEFAULT_KEEP, default_fields()).expect("log");
        assert_eq!(log.max_bytes(), 0);
        for i in 0..200 {
            let mut r = record();
            r.uri = Some(format!("/req/{i:03}"));
            log.log(&r).expect("log");
        }
        assert!(!path.with_extension("log.1").exists());
        let content = std::fs::read_to_string(&path).expect("read");
        assert_eq!(content.lines().count(), 4 + 200);
    }

    #[test]
    fn prepare_creates_parent_dirs() {
        let td = tempfile::tempdir().expect("tempdir");
        let path = td.path().join("nested/deep/request.log");
        let log = RequestLog::new(path.clone(), 0, 0, default_fields()).expect("log");
        log.prepare().expect("prepare");
        assert!(path.is_file());
        // prepare on an existing file keeps it (no truncation).
        log.prepare().expect("prepare");
        let content = std::fs::read_to_string(&path).expect("read");
        assert_eq!(content.matches("#Software:").count(), 1);
    }

    #[test]
    fn concurrent_writes_are_not_lost() {
        let td = tempfile::tempdir().expect("tempdir");
        let (log, path) = log_in(&td);
        let log = Arc::new(log);
        let mut handles = Vec::new();
        for t in 0..8 {
            let log = Arc::clone(&log);
            handles.push(std::thread::spawn(move || {
                for i in 0..50 {
                    let mut r = record();
                    r.uri = Some(format!("/t{t}/{i}"));
                    log.log(&r).expect("log");
                }
            }));
        }
        for h in handles {
            h.join().expect("join");
        }
        let content = std::fs::read_to_string(&path).expect("read");
        assert_eq!(content.lines().count(), 4 + 400, "every record landed");
        // No interleaved/corrupt lines: each record line has the right
        // field count.
        for line in content.lines().skip(4) {
            assert_eq!(line.split(' ').count(), DEFAULT_FIELDS.len());
        }
    }

    #[test]
    fn factory_applies_params_and_defaults() {
        let td = tempfile::tempdir().expect("tempdir");
        let path = td.path().join("factory.log");
        let factory = RequestLogPluginFactory;
        let params = serde_json::json!({
            "path": path.to_string_lossy(),
            "max_bytes": 1024,
            "keep": 3
        });
        let Some(log) = ok_of(factory.create_request_logger(&params)) else {
            panic!("expected a logger");
        };
        assert_eq!(log.name(), "request_log");
        // Accessors on the concrete type (the trait object only exposes
        // `name`/`prepare`/`log`).
        let concrete = RequestLog::new(path.clone(), 1024, 3, default_fields()).expect("concrete");
        assert_eq!(concrete.path(), path.as_path());
        assert_eq!(concrete.max_bytes(), 1024);
        assert_eq!(concrete.keep(), 3);
        assert_eq!(concrete.fields().len(), DEFAULT_FIELDS.len());

        // Unknown field in `fields` → BadParams.
        let bad = serde_json::json!({
            "path": path.to_string_lossy(),
            "fields": ["sc-status", "nope"]
        });
        let err = err_of(factory.create_request_logger(&bad));
        assert!(matches!(err, PluginError::BadParams { .. }));
        // Empty path → BadParams.
        let err = err_of(factory.create_request_logger(&serde_json::json!({ "path": "  " })));
        assert!(matches!(err, PluginError::BadParams { .. }));
        // Other factories ignore this spec.
        let pre = crate::builtin::keyword::KeywordPluginFactory;
        assert!(ok_of(pre.create_request_logger(&params)).is_none());
    }

    #[test]
    fn format_utc_millisecond_precision() {
        // 1_700_000_000 = 2023-11-14T22:13:20Z
        let s = format_utc(1_700_000_000, 123_000_000);
        assert!(s.ends_with("Z"), "{s}");
        let secs_part = s.trim_end_matches('Z').split('.').next().expect("secs");
        assert_eq!(secs_part.len(), 19, "YYYY-MM-DDTHH:MM:SS");
        assert!(s.ends_with(".123Z"), "{s}");
        assert_ne!(s, format_utc(1_700_000_000, 0));
        assert_ne!(s, format_utc(1_700_000_000, 124_000_000));
    }
}
