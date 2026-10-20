# 9. Result cache plugin — `plugins/src/builtin/result_cache.rs` (14)

[← Suite index](README.md)

Part of the [lpcache regression test cases](README.md): every entry maps
1:1 to a test function; the full suite (197 tests) runs with
`cargo test --workspace`.

The `result_cache` plugin: file-based cache of non-streaming backend
results keyed by the request KV cache key (`{dir}/{key}.json` with
`status`/`body`/`stored_at`), a `Prefilter` (`PrefilterDecision::Serve`
on a fresh hit), a `ResultStore` for the post-backend write path, and
the `PostCleanup` post-serve cleanup adapter.

## Module overview

- **Target:** `lpcache-plugins` library — `builtin::result_cache::tests` in [`result_cache.rs`](../plugins/src/builtin/result_cache.rs); 14 tests (7 sync file-level, 7 async — the `Prefilter`/`ResultStore`/cleanup paths are async trait methods).
- **Fixtures & helpers:** `tempfile::tempdir()` cache dirs; `preq(key, stream)` builds a `PrefilterRequest`; `ResultCache::new(dir, ttl)` with short TTLs (50 ms) plus real sleeps for the expiry tests.
- **Surface under test:** `ResultCache` put/get/overwrite + `path_for`, lazy TTL expiry, corrupt-entry self-healing, the `Prefilter` impl (serve on fresh hit; skip stream/expired), `RemoveAfterServe` cleanup, `ResultStore` trait wrapping, `Arc<dyn Prefilter>` dispatch, factory param validation.

## How to run

```sh
cd lpcache
cargo test -p lpcache-plugins result_cache::tests                        # all 14 plugin tests
cargo test -p lpcache-plugins builtin::result_cache::tests::put_get_roundtrip   # one, exact path
```

The **Test case** column is the exact filter for that test: pass it
into `cargo test` as shown above.

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 1 | `builtin::result_cache::tests::put_get_roundtrip` | sync | [L350](../plugins/src/builtin/result_cache.rs#L350) | store/fetch/overwrite an entry | roundtrip of `(status, body)`; missing key → `None`; no temp files left |
| 2 | `builtin::result_cache::tests::expired_entry_misses_and_is_removed` | sync | [L385](../plugins/src/builtin/result_cache.rs#L385) | TTL 50 ms, read after 60 ms | miss + the entry file is removed (lazy expiry) |
| 3 | `builtin::result_cache::tests::zero_ttl_never_expires` | sync | [L396](../plugins/src/builtin/result_cache.rs#L396) | TTL `0` | entry stays fresh |
| 4 | `builtin::result_cache::tests::corrupt_entry_misses_and_is_removed` | sync | [L405](../plugins/src/builtin/result_cache.rs#L405) | unreadable JSON entry | miss + removed (self-healing) |
| 5 | `builtin::result_cache::tests::prefilter_serves_fresh_hit` | async | [L415](../plugins/src/builtin/result_cache.rs#L415) | miss → `Accept`; after `put`, hit | `Serve { status, body }`; the prefilter exposes the backing cache |
| 6 | `builtin::result_cache::tests::prefilter_ignores_stream_requests` | async | [L466](../plugins/src/builtin/result_cache.rs#L466) | `stream: true` with a fresh hit | `Accept` (never served) |
| 7 | `builtin::result_cache::tests::prefilter_ignores_expired_entries` | async | [L479](../plugins/src/builtin/result_cache.rs#L479) | hit after TTL elapsed | `Accept` |
| 8 | `builtin::result_cache::tests::remove_after_serve_deletes_the_entry` | async | [L494](../plugins/src/builtin/result_cache.rs#L494) | `RemoveAfterServe` cleanup on a hit | `Serve`, entry file removed, next check misses |
| 9 | `builtin::result_cache::tests::remove_after_serve_missing_file_is_noop` | async | [L516](../plugins/src/builtin/result_cache.rs#L516) | cleanup on an absent file | no error (concurrent-serve race is safe) |
| 10 | `builtin::result_cache::tests::trait_object_dispatch` | async | [L524](../plugins/src/builtin/result_cache.rs#L524) | prefilter behind `Arc<dyn Prefilter>` | `name()` = `result_cache`; dispatch works |
| 11 | `builtin::result_cache::tests::factory_creates_plugin_from_json_params` | sync | [L534](../plugins/src/builtin/result_cache.rs#L534) | plugin factory with JSON params | prefilter + result store from `dir` / `ttl_secs` / `cleanup` |
| 12 | `builtin::result_cache::tests::factory_requires_dir_and_validates_cleanup` | sync | [L550](../plugins/src/builtin/result_cache.rs#L550) | missing `dir`; unknown `cleanup` value | factory `Err` |
| 13 | `builtin::result_cache::tests::store_trait_wraps_put` | sync | [L374](../plugins/src/builtin/result_cache.rs#L374) | `ResultStore::store` through the trait object | entry file written via the trait |
| 14 | `builtin::result_cache::tests::prefilter_result_store_exposes_the_same_cache` | async | [L444](../plugins/src/builtin/result_cache.rs#L444) | the plugin's prefilter + result store together | both share the same cache (a store is visible to the prefilter) |
