//! `payload_log` plugin — dumps each chat request's raw payload to a
//! unique file in a configured directory.
//!
//! The proxy feeds the payload as it arrives (before JSON parsing,
//! prefiltering, or any slot/backend work), so every chat request is
//! captured — including malformed bodies that the proxy rejects with a
//! `422` and requests a prefilter later rejects. One payload file plus a
//! paired headers file per request:
//!
//! ```text
//! {dir}/{seq:06}-{timestamp}-{key16}.json     (valid UTF-8 payloads)
//! {dir}/{seq:06}-{timestamp}-{key16}.bin      (anything else)
//! {dir}/{seq:06}-{timestamp}-{key16}.headers  (the request's HTTP headers)
//! ```
//!
//! where `seq` is a per-process 1-based request counter, `timestamp` is
//! the UTC time the payload was received (`%Y-%m-%dT%H-%M-%S.%fZ` —
//! colons replaced by dashes so the name is portable to every
//! filesystem), and `key16` is the first 16 hex chars of the request's
//! KV cache key (`nokey` when the payload could not be parsed into a
//! chat request, so the key was never computed). The payload and headers
//! files always share the same base name (one `seq` per request), so
//! they pair up and never interleave with another request.
//!
//! The `.headers` file holds the request's HTTP headers as received —
//! one `name: value` line per header, names lowercased, in wire order
//! (duplicate names kept as separate lines); non-UTF-8 header values are
//! dropped, so the file is always valid UTF-8.
//!
//! Payloads and headers may contain sensitive data (messages, system
//! prompts, credentials echoed in tool calls, `Authorization` values):
//! treat the directory like a log file and prune it as needed — the
//! plugin never deletes or rotates on its own.
//!
//! Params:
//! - `dir` (required): the directory the payload files are written to
//!   (relative paths resolve against the proxy's working directory; the
//!   directory is created at startup).

use crate::api::PayloadLogger;
use crate::registry::{PluginError, PluginFactory};
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Plugin name (registry key).
pub const NAME: &str = "payload_log";

/// File state behind the write mutex: the per-process request counter
/// (drives the `{seq}` file-name prefix — unique names mean concurrent
/// payloads never overwrite each other).
#[derive(Default)]
struct Inner {
    seq: u64,
}

/// Built-in payload logger: one payload file plus a paired `.headers`
/// file per request in `dir` (see the module docs for the file-name
/// format).
///
/// All file operations run under one mutex, so concurrent payloads get
/// distinct names and never interleave.
pub struct PayloadLog {
    dir: PathBuf,
    inner: Mutex<Inner>,
}

impl PayloadLog {
    /// Build a payload logger writing into `dir` (created by
    /// [`PayloadLogger::prepare`]).
    ///
    /// `Err` when `dir` is empty.
    pub fn new(dir: PathBuf) -> Result<Self, String> {
        if dir.as_os_str().is_empty() {
            return Err("`dir` must not be empty".to_string());
        }
        Ok(Self {
            dir,
            inner: Mutex::new(Inner::default()),
        })
    }

    /// The directory the payload files are written to.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The UTC time `secs`/`nanos` as a filesystem-safe timestamp
    /// (`%Y-%m-%dT%H-%M-%S.%fZ`; the colons of the request-log datetime
    /// replaced by dashes).
    fn file_timestamp(secs: u64, nanos: u32) -> String {
        crate::api::format_utc(secs, nanos).replace(':', "-")
    }

    /// Allocate the next payload's file-name base (without extension):
    /// `{seq:06}-{timestamp}-{key16}`. The `seq` is assigned under the
    /// mutex so concurrent payloads always get distinct names.
    fn next_file_base(&self, key: Option<&str>, ts: &str) -> (String, u64) {
        let mut inner = self.inner.lock().expect("payload log mutex poisoned");
        inner.seq += 1;
        let seq = inner.seq;
        let key16 = match key {
            Some(k) if !k.trim().is_empty() => k.chars().take(16).collect::<String>(),
            _ => "nokey".to_string(),
        };
        (format!("{seq:06}-{ts}-{key16}"), seq)
    }
}

impl PayloadLogger for PayloadLog {
    fn name(&self) -> &str {
        NAME
    }

    fn prepare(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("create dir {}: {e}", self.dir.display()))
    }

    fn log_payload(
        &self,
        key: Option<&str>,
        payload: &[u8],
        headers: &[(String, String)],
    ) -> Result<(), String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let ts = Self::file_timestamp(now.as_secs(), now.subsec_nanos());
        // Valid UTF-8 payloads are written as `.json`, everything else
        // as `.bin`.
        let ext = if std::str::from_utf8(payload).is_ok() {
            "json"
        } else {
            "bin"
        };
        let (base, seq) = self.next_file_base(key, &ts);
        let path = self.dir.join(format!("{base}.{ext}"));
        std::fs::write(&path, payload)
            .map_err(|e| format!("write payload {} (seq {seq}): {e}", path.display()))?;
        // The paired headers file: same base name, `.headers`
        // extension — one `name: value` line per header, in wire order.
        let mut text = String::new();
        for (name, value) in headers {
            text.push_str(name);
            text.push_str(": ");
            text.push_str(value);
            text.push('\n');
        }
        let headers_path = self.dir.join(format!("{base}.headers"));
        std::fs::write(&headers_path, text)
            .map_err(|e| format!("write headers {} (seq {seq}): {e}", headers_path.display()))
    }
}

// ---------------------------------------------------------------------------
// Plugin factory (configurable, dynamically loadable)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct PayloadLogParams {
    dir: Option<String>,
}

/// Factory for the `payload_log` plugin (registered by
/// [`crate::builtin::register`]).
pub struct PayloadLogPluginFactory;

impl PluginFactory for PayloadLogPluginFactory {
    fn create_payload_logger(
        &self,
        params: &Value,
    ) -> Result<Option<Arc<dyn PayloadLogger>>, PluginError> {
        let PayloadLogParams { dir } =
            PayloadLogParams::deserialize(params).map_err(|e| PluginError::BadParams {
                name: NAME.to_string(),
                message: e.to_string(),
            })?;
        let Some(dir) = dir.filter(|d| !d.trim().is_empty()) else {
            return Err(PluginError::BadParams {
                name: NAME.to_string(),
                message: "`dir` is required and must not be empty".to_string(),
            });
        };
        let log =
            PayloadLog::new(PathBuf::from(dir)).map_err(|message| PluginError::BadParams {
                name: NAME.to_string(),
                message,
            })?;
        Ok(Some(Arc::new(log)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_of<R, E: std::fmt::Display>(r: Result<R, E>) -> E {
        match r {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        }
    }

    fn names_in(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn prepare_creates_nested_dirs() {
        let td = tempfile::tempdir().expect("tempdir");
        let dir = td.path().join("nested/deep/payloads");
        let log = PayloadLog::new(dir.clone()).expect("log");
        log.prepare().expect("prepare");
        assert!(dir.is_dir());
        // prepare on an existing dir keeps it (no error).
        log.prepare().expect("prepare again");
    }

    #[test]
    fn log_payload_writes_exact_bytes() {
        let td = tempfile::tempdir().expect("tempdir");
        let log = PayloadLog::new(td.path().to_path_buf()).expect("log");
        log.prepare().expect("prepare");
        let payload = br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        log.log_payload(Some("abcdef0123456789abcdef"), payload, &[])
            .expect("log");
        let files = names_in(td.path());
        assert_eq!(files.len(), 2, "payload + headers file: {files:?}");
        let name = files
            .into_iter()
            .find(|n| n.ends_with(".json"))
            .expect("payload file");
        // {seq:06}-{timestamp}-{key16}.json
        assert!(name.starts_with("000001-"), "{name}");
        assert!(name.ends_with("-abcdef0123456789.json"), "{name}");
        let ts = &name["000001-".len()..name.len() - "-abcdef0123456789.json".len()];
        assert!(
            ts.ends_with('Z') && ts.len() == 24 && ts.contains('T'),
            "timestamp part: {ts}"
        );
        assert_eq!(std::fs::read(td.path().join(&name)).expect("read"), payload);
    }

    #[test]
    fn log_payload_unique_names_per_request() {
        let td = tempfile::tempdir().expect("tempdir");
        let log = PayloadLog::new(td.path().to_path_buf()).expect("log");
        log.prepare().expect("prepare");
        // Same key, same (likely) timestamp — the seq still keeps the
        // names unique.
        for _ in 0..5 {
            log.log_payload(Some("abcdef0123456789"), b"{}", &[])
                .expect("log");
        }
        let files = names_in(td.path());
        assert_eq!(files.len(), 10, "every payload got its own pair: {files:?}");
        assert_eq!(
            files.iter().collect::<std::collections::HashSet<_>>().len(),
            10
        );
    }

    #[test]
    fn log_payload_writes_headers_companion() {
        let td = tempfile::tempdir().expect("tempdir");
        let log = PayloadLog::new(td.path().to_path_buf()).expect("log");
        log.prepare().expect("prepare");
        let headers = vec![
            ("host".to_string(), "localhost:9090".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
            // Duplicate names are kept as separate lines, in wire order.
            ("x-twice".to_string(), "a".to_string()),
            ("x-twice".to_string(), "b".to_string()),
        ];
        log.log_payload(Some("abcdef0123456789"), b"{}", &headers)
            .expect("log");
        let files = names_in(td.path());
        assert_eq!(files.len(), 2, "payload + headers file: {files:?}");
        let payload_name = files
            .iter()
            .find(|n| n.ends_with(".json"))
            .expect("payload file");
        let headers_name = files
            .iter()
            .find(|n| n.ends_with(".headers"))
            .expect("headers file");
        // Same base name (the headers name is the payload name with the
        // extension swapped), so the pair can never mix with another
        // request.
        assert!(
            headers_name.starts_with(&payload_name[..payload_name.len() - ".json".len()]),
            "paired base names: {payload_name} / {headers_name}"
        );
        assert_eq!(
            std::fs::read_to_string(td.path().join(headers_name)).expect("read"),
            "host: localhost:9090\ncontent-type: application/json\nx-twice: a\nx-twice: b\n"
        );
        // No headers sent → an empty (but present) headers file.
        log.log_payload(None, b"{}2", &[]).expect("log again");
        let files = names_in(td.path());
        let empty = files.iter().filter(|n| n.ends_with(".headers")).count();
        assert_eq!(empty, 2, "every request got a headers file: {files:?}");
    }

    #[test]
    fn log_payload_no_key_uses_nokey() {
        let td = tempfile::tempdir().expect("tempdir");
        let log = PayloadLog::new(td.path().to_path_buf()).expect("log");
        log.prepare().expect("prepare");
        log.log_payload(None, b"not json", &[]).expect("log");
        let files = names_in(td.path());
        // Payload + headers companion, both under `nokey`.
        let json = files
            .iter()
            .find(|n| n.ends_with("-nokey.json"))
            .expect("nokey payload file");
        let headers = files
            .iter()
            .find(|n| n.ends_with("-nokey.headers"))
            .expect("nokey headers file");
        // Same base name: the headers file is the payload name with the
        // extension swapped.
        assert_eq!(
            headers.as_str(),
            &format!("{}.headers", &json[..json.len() - ".json".len()]),
            "{files:?}"
        );
    }

    #[test]
    fn log_payload_non_utf8_gets_bin_extension() {
        let td = tempfile::tempdir().expect("tempdir");
        let log = PayloadLog::new(td.path().to_path_buf()).expect("log");
        log.prepare().expect("prepare");
        let payload: Vec<u8> = vec![0xff, 0xfe, 0x00, 0x01];
        log.log_payload(None, &payload, &[]).expect("log");
        let files = names_in(td.path());
        // The `.bin` payload file (plus its `.headers` companion).
        let bin = files
            .iter()
            .find(|n| n.ends_with(".bin"))
            .expect("bin file");
        assert_eq!(std::fs::read(td.path().join(bin)).expect("read"), payload);
    }

    #[test]
    fn concurrent_writes_get_distinct_files() {
        let td = tempfile::tempdir().expect("tempdir");
        let log = Arc::new(PayloadLog::new(td.path().to_path_buf()).expect("log"));
        log.prepare().expect("prepare");
        let mut handles = Vec::new();
        for t in 0..8 {
            let log = Arc::clone(&log);
            handles.push(std::thread::spawn(move || {
                for i in 0..25 {
                    log.log_payload(Some("k"), &format!("t{t} i{i}").into_bytes(), &[])
                        .expect("log");
                }
            }));
        }
        for h in handles {
            h.join().expect("join");
        }
        let files = names_in(td.path());
        assert_eq!(
            files.len(),
            400,
            "every payload + headers landed: {} files",
            files.len()
        );
    }

    #[test]
    fn factory_requires_dir() {
        let factory = PayloadLogPluginFactory;
        let Some(log) = factory
            .create_payload_logger(&serde_json::json!({ "dir": "/tmp/x" }))
            .expect("ok")
        else {
            panic!("expected a payload logger");
        };
        assert_eq!(log.name(), "payload_log");
        // missing dir → BadParams,
        let err = err_of(factory.create_payload_logger(&serde_json::json!({})));
        assert!(
            matches!(err, PluginError::BadParams { ref message, .. } if message.contains("dir"))
        );
        // blank dir → BadParams,
        let err = err_of(factory.create_payload_logger(&serde_json::json!({ "dir": "   " })));
        assert!(matches!(err, PluginError::BadParams { .. }));
        // other factories ignore this spec.
        let pre = crate::builtin::keyword::KeywordPluginFactory;
        assert!(
            pre.create_payload_logger(&serde_json::json!({ "dir": "/tmp/x" }))
                .expect("ok")
                .is_none()
        );
    }
}
