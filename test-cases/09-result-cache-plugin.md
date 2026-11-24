# 9. Result cache plugin — `plugins/src/builtin/result_cache.rs` (23)

[← Suite index](README.md)

Part of the [lpcache regression test cases](README.md): every entry maps
1:1 to a test function; the full suite (359 tests) runs with
`cargo test --workspace`.

The `result_cache` plugin: file-based cache of non-streaming backend
results keyed by the request KV cache key (one
`{lowercase(model_id)}_{key}.json` per key — the KV cache file naming
convention, with `status`/`body`/`stored_at`; legacy bare `{key}.json`
entries are still read and migrated on first access), a `Prefilter`
(`PrefilterDecision::Serve` on a fresh hit), a `ResultStore` for the
post-backend write path, and the `PostCleanup` post-serve cleanup
adapter.

## Module overview

- **Target:** `lpcache-plugins` library — `builtin::result_cache::tests` in [`result_cache.rs`](../plugins/src/builtin/result_cache.rs); 23 tests (12 sync file-level, 11 async — the `Prefilter`/`ResultStore`/cleanup paths are async trait methods).
- **Fixtures & helpers:** `tempfile::tempdir()` cache dirs; `preq(key, stream)` builds a `PrefilterRequest` with `backend_model_id = "backend-model"` (the model used for the entry filename in the prefilter-backed tests) (`preq_cc(key, stream, headers)` adds HTTP headers); `ResultCache::new(dir, ttl)` with short TTLs (50 ms) plus real sleeps for the expiry tests.
- **Surface under test:** `entry_name` (the `{lowercase(model_id)}_{key}.json` convention), `ResultCache` put/get/overwrite + `path_for` (incl. the legacy bare-name migration), lazy TTL expiry, corrupt-entry self-healing, cross-model entry isolation, the `Prefilter` impl (serve on fresh hit; skip stream/expired; `cache_control` no-cache bypass), `RemoveAfterServe` cleanup, `ResultStore` trait wrapping, `Arc<dyn Prefilter>` dispatch, factory param validation.

## How to run

```sh
cargo test -p lpcache-plugins result_cache::tests                        # all 23 plugin tests
cargo test -p lpcache-plugins builtin::result_cache::tests::put_get_roundtrip   # one, exact path
```

The **Test case** column is the exact filter for that test: pass it
into `cargo test` as shown above.

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 1 | `builtin::result_cache::tests::put_get_roundtrip` | sync | [L519](../plugins/src/builtin/result_cache.rs#L519) | store/fetch/overwrite an entry | roundtrip of `(status, body)`; missing key → `None`; entry file is `{lowercase(model_id)}_{key}.json`; no temp files left |
| 2 | `builtin::result_cache::tests::expired_entry_misses_and_is_removed` | sync | [L556](../plugins/src/builtin/result_cache.rs#L556) | TTL 50 ms, read after 60 ms | miss + the entry file is removed (lazy expiry) |
| 3 | `builtin::result_cache::tests::zero_ttl_never_expires` | sync | [L567](../plugins/src/builtin/result_cache.rs#L567) | TTL `0` | entry stays fresh |
| 4 | `builtin::result_cache::tests::corrupt_entry_misses_and_is_removed` | sync | [L576](../plugins/src/builtin/result_cache.rs#L576) | unreadable JSON entry | miss + removed (self-healing) |
| 5 | `builtin::result_cache::tests::entry_name_follows_the_kv_convention` | sync | [L586](../plugins/src/builtin/result_cache.rs#L586) | `entry_name` naming scheme | `{lowercase(model_id)}_{key}.json` (the KV cache file convention + `.json`); model id lowercased; different models, same key → distinct files |
| 6 | `builtin::result_cache::tests::legacy_bare_entry_is_served_and_migrated` | sync | [L601](../plugins/src/builtin/result_cache.rs#L601) | a pre-rename bare `{key}.json` entry | served from the legacy name and migrated (renamed) to `{lowercase(model_id)}_{key}.json`; still a hit afterwards |
| 7 | `builtin::result_cache::tests::different_models_do_not_share_entries` | sync | [L619](../plugins/src/builtin/result_cache.rs#L619) | same key stored under two different models | lookup under the other model → miss (a different file); the original entry is untouched |
| 8 | `builtin::result_cache::tests::prefilter_serves_fresh_hit` | async | [L633](../plugins/src/builtin/result_cache.rs#L633) | miss → `Accept`; after `put`, hit | `Serve { status, body }`; the prefilter exposes the backing cache |
| 9 | `builtin::result_cache::tests::prefilter_ignores_stream_requests` | async | [L787](../plugins/src/builtin/result_cache.rs#L787) | `stream: true` with a fresh hit | `Accept` (never served) |
| 10 | `builtin::result_cache::tests::prefilter_ignores_expired_entries` | async | [L847](../plugins/src/builtin/result_cache.rs#L847) | hit after TTL elapsed | `Accept` |
| 11 | `builtin::result_cache::tests::remove_after_serve_deletes_the_entry` | async | [L862](../plugins/src/builtin/result_cache.rs#L862) | `RemoveAfterServe` cleanup on a hit | `Serve`, entry file removed, next check misses |
| 12 | `builtin::result_cache::tests::remove_after_serve_missing_file_is_noop` | async | [L884](../plugins/src/builtin/result_cache.rs#L884) | cleanup on an absent file | no error (concurrent-serve race is safe) |
| 13 | `builtin::result_cache::tests::trait_object_dispatch` | async | [L892](../plugins/src/builtin/result_cache.rs#L892) | prefilter behind `Arc<dyn Prefilter>` | `name()` = `result_cache`; dispatch works |
| 14 | `builtin::result_cache::tests::factory_creates_plugin_from_json_params` | sync | [L902](../plugins/src/builtin/result_cache.rs#L902) | plugin factory with JSON params | prefilter + result store from `dir` / `ttl_secs` / `cleanup` |
| 15 | `builtin::result_cache::tests::factory_requires_dir_and_validates_cleanup` | sync | [L921](../plugins/src/builtin/result_cache.rs#L921) | missing `dir`; unknown `cleanup` value | factory `Err` |
| 16 | `builtin::result_cache::tests::store_trait_wraps_put` | sync | [L545](../plugins/src/builtin/result_cache.rs#L545) | `ResultStore::store(model_id, key, …)` through the trait object | entry file written via the trait |
| 17 | `builtin::result_cache::tests::prefilter_result_store_exposes_the_same_cache` | async | [L765](../plugins/src/builtin/result_cache.rs#L765) | the plugin's prefilter + result store together | both share the same cache (a store is visible to the prefilter) |
| 18 | `builtin::result_cache::tests::wants_no_cache_parses_directives` | sync | [L662](../plugins/src/builtin/result_cache.rs#L662) | `Cache-Control` directive parsing (`no-cache`, `NO-CACHE`, `no-cache, no-store`, `max-age=60`, no header, similarly named header) | `no-cache` matched case-insensitively in any list position; other directives/headers → no match |
| 19 | `builtin::result_cache::tests::cache_control_no_cache_bypasses_the_cache` | async | [L679](../plugins/src/builtin/result_cache.rs#L679) | `cache_control: true`; fresh hit with `no-cache`, `max-age=60`, and no header | `no-cache` → `Accept` (forward to the backend); other directives / no header → `Serve` |
| 20 | `builtin::result_cache::tests::cache_control_disabled_ignores_the_header` | async | [L715](../plugins/src/builtin/result_cache.rs#L715) | flag off (default); fresh hit with `no-cache` | `Serve` — the header is ignored |
| 21 | `builtin::result_cache::tests::factory_cache_control_param_bypasses` | async | [L736](../plugins/src/builtin/result_cache.rs#L736) | factory with `cache_control: true` param | flag wired through: `no-cache` → `Accept`, plain request → `Serve` |
| 22 | `builtin::result_cache::tests::factory_cache_streaming_param` | sync | [L826](../plugins/src/builtin/result_cache.rs#L826) | factory with / without the `cache_streaming` param | omitted → the result store reports `caches_streaming() == false`; explicit `true` → `true` |
| 23 | `builtin::result_cache::tests::prefilter_serves_streaming_when_enabled` | async | [L800](../plugins/src/builtin/result_cache.rs#L800) | `cache_streaming: true`; `stream: true` request with a fresh hit | `Serve { status, body }` — cached entries are served to streaming clients (rendered as SSE by the proxy) |
