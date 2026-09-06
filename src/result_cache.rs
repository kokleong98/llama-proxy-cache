//! Result cache + cached-result prefilter adapter.
//!
//! [`ResultCache`] stores completed non-streaming backend results (a JSON
//! response body plus its status code) on disk, one file per request KV
//! cache key: `{dir}/{key}.json` with a `stored_at` timestamp. Entries
//! expire after the configured TTL; expired entries are removed lazily on
//! read (there is no background sweeper). Writes are atomic (temp file +
//! rename), so a concurrent reader never sees a torn entry.
//!
//! [`CachedResultPrefilter`] is the prefilter adapter wired into
//! `AppState::prefilter`: on a fresh (non-expired) cache hit for the
//! request's hash key it answers the client with the cached backend result
//! (`PrefilterDecision::Serve`) — **no slot is acquired, no backend is
//! called, no KV cache is restored/saved, no meta file is written, and the
//! request never leads or joins a coalescing group**. Streaming requests
//! always pass through (`Accept`): only non-streaming JSON results are
//! cached, so a `stream: true` request can never be answered from the
//! cache.
//!
//! Post-serve cleanup: an optional [`PostCleanup`] adapter runs after a
//! cached result has been read for the client (before the response is
//! sent). The built-in [`RemoveAfterServe`] deletes the entry file — a
//! one-shot cache where each result is served at most once.
//!
//! Enabled via `PREFILTER_RESULT_CACHE_DIR` / `--prefilter-result-cache-dir`
//! (the cache-result path); the per-entry expiry is
//! `PREFILTER_RESULT_CACHE_TTL` / `--prefilter-result-cache-ttl` (seconds,
//! `0` = entries never expire). When both the keyword blocklist and the
//! result cache are enabled, the `config` module chains them
//! (blocklist first: a blocked request is rejected with `400` before a
//! cache hit could serve it).

use crate::prefilter::{Prefilter, PrefilterDecision, PrefilterRequest};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One cached backend result: the response status code, the full JSON
/// response body, and the storage time (unix seconds) used for TTL expiry.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    status: u16,
    body: Value,
    stored_at: f64,
}

/// Current unix time in seconds (`0.0` if the clock is pre-epoch).
fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Monotonic suffix for per-put temp files (unique within this process).
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// File-based cache of non-streaming backend results, keyed by the
/// request's KV cache key (sha256 hex).
pub struct ResultCache {
    dir: PathBuf,
    /// Per-entry expiry; zero = entries never expire.
    ttl: Duration,
}

impl ResultCache {
    /// Build a cache over `dir` with per-entry expiry `ttl`
    /// (zero = never expires). The directory is created on first write.
    pub fn new(dir: PathBuf, ttl: Duration) -> Self {
        Self { dir, ttl }
    }

    /// The cache-result path (directory holding the `{key}.json` entries).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The configured per-entry expiry (zero = never expires).
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// The file path of the entry for `key`.
    pub fn path_for(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    /// Fetch the entry for `key`: `Some((status, body))` when the file
    /// exists and is fresh. Expired entries are removed lazily (no
    /// background sweeper); corrupt/unreadable entries are removed too
    /// (they can never be served) and reported as a miss.
    pub fn get(&self, key: &str) -> Option<(u16, Value)> {
        let path = self.path_for(key);
        let raw = std::fs::read(&path).ok()?;
        let entry: Entry = match serde_json::from_slice(&raw) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    "result_cache_corrupt key={} path={}: {e}; removing",
                    key,
                    path.display()
                );
                let _ = std::fs::remove_file(&path);
                return None;
            }
        };
        if !self.ttl.is_zero() && now_secs() - entry.stored_at > self.ttl.as_secs_f64() {
            tracing::debug!(
                "result_cache_expired key={} age_s={:.1} ttl_s={}",
                key,
                now_secs() - entry.stored_at,
                self.ttl.as_secs_f64()
            );
            let _ = std::fs::remove_file(&path);
            return None;
        }
        Some((entry.status, entry.body))
    }

    /// Store (or refresh) the entry for `key`. The write is atomic: the
    /// JSON is written to a temp file in the same directory and renamed
    /// over `{key}.json`, so a concurrent reader sees either the old or
    /// the new file, never a torn one.
    pub fn put(&self, key: &str, status: u16, body: &Value) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let entry = Entry {
            status,
            body: body.clone(),
            stored_at: now_secs(),
        };
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .dir
            .join(format!(".{key}.tmp.{}-{}", std::process::id(), seq));
        let result = std::fs::write(
            &tmp,
            serde_json::to_vec(&entry)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
        );
        if let Err(e) = result.and_then(|_| std::fs::rename(&tmp, self.path_for(key))) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(())
    }
}

/// Post-serve cleanup adapter: invoked after the proxy has read a cached
/// result to answer a request (the entry bytes are already in memory, so
/// cleanup may safely remove the file before the response reaches the
/// client). Implementations must be `Send + Sync` — they are shared across
/// all requests behind an `Arc`.
///
/// ```ignore
/// #[async_trait::async_trait]
/// impl PostCleanup for MyCleanup {
///     fn name(&self) -> &str { "my_cleanup" }
///     async fn cleanup(&self, key: &str, path: &Path) { /* ... */ }
/// }
/// ```
#[async_trait]
pub trait PostCleanup: Send + Sync {
    /// Short identifier used in log lines.
    fn name(&self) -> &str;

    /// Run after a cached result for `key` was served from `path`.
    /// Failures are logged by the adapter; they never affect the response.
    async fn cleanup(&self, key: &str, path: &Path);
}

/// Built-in cleanup: deletes the cached entry file after it has been
/// served (a one-shot cache — each cached result is served at most once,
/// the next request for the same key goes to the backend).
#[derive(Debug, Clone)]
pub struct RemoveAfterServe;

#[async_trait]
impl PostCleanup for RemoveAfterServe {
    fn name(&self) -> &str {
        "remove_after_serve"
    }

    async fn cleanup(&self, _key: &str, path: &Path) {
        match std::fs::remove_file(path) {
            Ok(()) => tracing::info!("post_cleanup_removed path={}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A concurrent request already served (and removed) it.
                tracing::debug!("post_cleanup_missing path={}", path.display());
            }
            Err(e) => tracing::warn!("post_cleanup_remove_fail path={}: {e}", path.display()),
        }
    }
}

/// Prefilter adapter that answers requests from the [`ResultCache`]: when
/// the request's KV cache key has a fresh (non-expired) cached backend
/// result, the client receives that result directly
/// ([`PrefilterDecision::Serve`]) before any slot/backend work.
///
/// - Streaming requests always return [`PrefilterDecision::Accept`]: only
///   non-streaming JSON results are cached.
/// - A cache miss returns [`PrefilterDecision::Accept`]; the normal
///   pipeline runs and stores a fresh result (see the `app` module).
/// - An optional [`PostCleanup`] adapter runs after a hit is read.
#[derive(Clone)]
pub struct CachedResultPrefilter {
    cache: Arc<ResultCache>,
    cleanup: Option<Arc<dyn PostCleanup>>,
}

impl std::fmt::Debug for CachedResultPrefilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedResultPrefilter")
            .field("cache_dir", &self.cache.dir())
            .field("ttl", &self.cache.ttl())
            .field("cleanup", &self.cleanup.as_ref().map(|c| c.name()))
            .finish()
    }
}

impl CachedResultPrefilter {
    /// Build the adapter over `cache` (no post-serve cleanup).
    pub fn new(cache: Arc<ResultCache>) -> Self {
        Self {
            cache,
            cleanup: None,
        }
    }

    /// Attach the post-serve cleanup adapter (runs after a cached result
    /// is read for the client).
    pub fn with_cleanup(mut self, cleanup: Arc<dyn PostCleanup>) -> Self {
        self.cleanup = Some(cleanup);
        self
    }

    /// The backing cache (the `app` module stores fresh non-streaming
    /// results here after a successful backend call).
    pub fn cache(&self) -> &Arc<ResultCache> {
        &self.cache
    }
}

#[async_trait]
impl Prefilter for CachedResultPrefilter {
    fn name(&self) -> &str {
        "result_cache"
    }

    async fn check(&self, req: &PrefilterRequest) -> PrefilterDecision {
        // Streaming requests are never served from the cache: a cached
        // JSON answer would not match the client's SSE expectation.
        if req.stream {
            return PrefilterDecision::Accept;
        }
        let Some((status, body)) = self.cache.get(&req.key) else {
            return PrefilterDecision::Accept;
        };
        tracing::info!(
            "result_cache_hit key={} status={status} cleanup={}",
            req.key.chars().take(16).collect::<String>(),
            self.cleanup.as_ref().map(|c| c.name()).unwrap_or("none")
        );
        if let Some(c) = &self.cleanup {
            c.cleanup(&req.key, &self.cache.path_for(&req.key)).await;
        }
        PrefilterDecision::Serve { status, body }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn preq(key: &str, stream: bool) -> PrefilterRequest {
        PrefilterRequest {
            body: json!({ "messages": [] }),
            messages: json!([]),
            model: "llama.cpp".to_string(),
            backend_model_id: "backend-model".to_string(),
            stream,
            key: key.to_string(),
            n_words: 1,
            is_big: false,
        }
    }

    #[test]
    fn put_get_roundtrip() {
        let td = tempfile::tempdir().unwrap();
        let cache = ResultCache::new(td.path().to_path_buf(), Duration::from_secs(60));
        assert!(cache.get("nope").is_none());
        cache
            .put("k1", 200, &json!({ "object": "chat.completion" }))
            .unwrap();
        assert_eq!(
            cache.get("k1"),
            Some((200, json!({ "object": "chat.completion" })))
        );
        // put overwrites (refreshes) an existing entry
        cache.put("k1", 200, &json!({ "n": 2 })).unwrap();
        assert_eq!(cache.get("k1"), Some((200, json!({ "n": 2 }))));
        // no temp files left behind
        let leftovers: Vec<_> = std::fs::read_dir(td.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn expired_entry_misses_and_is_removed() {
        let td = tempfile::tempdir().unwrap();
        let cache = ResultCache::new(td.path().to_path_buf(), Duration::from_millis(50));
        cache.put("k", 200, &json!({})).unwrap();
        assert!(cache.get("k").is_some());
        std::thread::sleep(Duration::from_millis(60));
        assert!(cache.get("k").is_none());
        assert!(!cache.path_for("k").exists());
    }

    #[test]
    fn zero_ttl_never_expires() {
        let td = tempfile::tempdir().unwrap();
        let cache = ResultCache::new(td.path().to_path_buf(), Duration::ZERO);
        cache.put("k", 200, &json!({ "x": 1 })).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(cache.get("k"), Some((200, json!({ "x": 1 }))));
    }

    #[test]
    fn corrupt_entry_misses_and_is_removed() {
        let td = tempfile::tempdir().unwrap();
        let cache = ResultCache::new(td.path().to_path_buf(), Duration::from_secs(60));
        std::fs::create_dir_all(td.path()).unwrap();
        std::fs::write(cache.path_for("bad"), "not json").unwrap();
        assert!(cache.get("bad").is_none());
        assert!(!cache.path_for("bad").exists());
    }

    #[tokio::test]
    async fn prefilter_serves_fresh_hit() {
        let td = tempfile::tempdir().unwrap();
        let cache = Arc::new(ResultCache::new(
            td.path().to_path_buf(),
            Duration::from_secs(60),
        ));
        let pf = CachedResultPrefilter::new(Arc::clone(&cache));
        assert_eq!(pf.name(), "result_cache");
        // miss -> Accept
        assert_eq!(
            pf.check(&preq("k1", false)).await,
            PrefilterDecision::Accept
        );
        // store a result, then hit -> Serve with status + body
        cache
            .put("k1", 200, &json!({ "object": "chat.completion" }))
            .unwrap();
        assert_eq!(
            pf.check(&preq("k1", false)).await,
            PrefilterDecision::Serve {
                status: 200,
                body: json!({ "object": "chat.completion" })
            }
        );
        // the adapter exposes the backing cache for the write path
        assert_eq!(pf.cache().dir(), td.path());
    }

    #[tokio::test]
    async fn prefilter_ignores_stream_requests() {
        let td = tempfile::tempdir().unwrap();
        let cache = Arc::new(ResultCache::new(
            td.path().to_path_buf(),
            Duration::from_secs(60),
        ));
        cache.put("k1", 200, &json!({})).unwrap();
        let pf = CachedResultPrefilter::new(cache);
        // stream=true even with a fresh hit -> Accept (never served)
        assert_eq!(pf.check(&preq("k1", true)).await, PrefilterDecision::Accept);
    }

    #[tokio::test]
    async fn prefilter_ignores_expired_entries() {
        let td = tempfile::tempdir().unwrap();
        let cache = Arc::new(ResultCache::new(
            td.path().to_path_buf(),
            Duration::from_millis(50),
        ));
        cache.put("k1", 200, &json!({})).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        let pf = CachedResultPrefilter::new(cache);
        assert_eq!(
            pf.check(&preq("k1", false)).await,
            PrefilterDecision::Accept
        );
    }
    #[tokio::test]
    async fn remove_after_serve_deletes_the_entry() {
        let td = tempfile::tempdir().unwrap();
        let cache = Arc::new(ResultCache::new(
            td.path().to_path_buf(),
            Duration::from_secs(60),
        ));
        cache
            .put("k1", 200, &json!({ "object": "chat.completion" }))
            .unwrap();
        let pf =
            CachedResultPrefilter::new(Arc::clone(&cache)).with_cleanup(Arc::new(RemoveAfterServe));
        let d = pf.check(&preq("k1", false)).await;
        assert!(matches!(d, PrefilterDecision::Serve { status: 200, .. }));
        // served once: the file is gone, so the next request misses
        assert!(!cache.path_for("k1").exists());
        assert_eq!(
            pf.check(&preq("k1", false)).await,
            PrefilterDecision::Accept
        );
    }

    #[tokio::test]
    async fn remove_after_serve_missing_file_is_noop() {
        let c = RemoveAfterServe;
        assert_eq!(c.name(), "remove_after_serve");
        c.cleanup("k", Path::new("/definitely/missing/file.json"))
            .await;
    }

    #[tokio::test]
    async fn trait_object_dispatch() {
        let td = tempfile::tempdir().unwrap();
        let pf: Arc<dyn Prefilter> = Arc::new(CachedResultPrefilter::new(Arc::new(
            ResultCache::new(td.path().to_path_buf(), Duration::from_secs(60)),
        )));
        assert_eq!(pf.name(), "result_cache");
        assert_eq!(pf.check(&preq("k", false)).await, PrefilterDecision::Accept);
    }
}
