# 7. API end-to-end — `tests/api.rs` (58)

[← Suite index](README.md)

Part of the [lpcache regression test cases](README.md): every entry maps
1:1 to a test function; the full suite (244 tests) runs with
`cargo test --workspace`.

The real proxy router (axum) in front of `MockLlama`, which records every
chat body/query, restore and save. Default setup: 1 backend, 2 slots,
wpb 100, big threshold 500 words, LCP 0.6, `META_MAX=0`. Tests marked
*real TCP* serve the proxy on a real local port (client-disconnect
behaviour requires a real connection).

## Module overview

- **Target:** `lpcache` integration tests — the `api` test binary ([`tests/api.rs`](../tests/api.rs)); 58 tests, all async.
- **Fixtures & helpers:**
  - `make_state*` family — builds `AppState` in front of `MockLlama`: `make_state` / `make_app` (default: 1 backend, 2 slots, wpb 100, big threshold 500 words, LCP 0.6, meta in a `tempfile` dir, `META_MAX=0`), `make_state_coalesce*` (coalescing on), `make_state*stream_queue*` (custom `STREAM_QUEUE_SIZE`), `make_state_prefilter*` (built-in keyword blocklist), `make_state_result_cache*` (`result_cache` plugin specs, incl. `cache_streaming` and `cache_control` variants), `make_state_with_save_dir` (multi-backend with KV save dirs, per-backend `kv_cache` flags), `make_state_request_log` (logger via `Config::with_request_log`) and `make_state_dashboard` (a `dashboard` spec from JSON params).
  - `load_prefilter` — loads the prefilter + result store through the production `load_prefilters` path from the config's plugin specs.
  - `ServedProxy` — serves the router on a real local TCP port (`axum::serve`) for the client-disconnect tests; `Drop` aborts the task.
  - Request helpers: `post_json` / `get_req` / `read_body`, `words(n)` (n distinct prompt words), `expected_key(prefix)` (recomputes the exact cache key the proxy computes), `free_port()`.
  - The `request_log_*` tests build state the way `main.rs` does (`Config::with_request_log` → `load_request_loggers` → `prepare`).
- **Surface under test:** `router`/`AppState` end-to-end: `GET /v1/models`, `POST /v1/chat/completions` (non-stream JSON and SSE streaming), KV save/restore orchestration (incl. the per-backend `kv_cache` flag), failover/retry on dead or failing backends, meta+KV pruning, request coalescing, prefilters, the result-cache plugin, the request-log plugin and the dashboard plugin (page + stats API).

## How to run

```sh
cargo test --test api                                       # all 58 E2E tests
cargo test --test api models_endpoint_returns_configured_id # one test (exact name)
```

The **Test case** column is the exact filter for that test: pass it
into `cargo test` as shown above.

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 1 | `models_endpoint_returns_configured_id` | async | [L232](../tests/api.rs#L232) | GET `/v1/models` | 200 `{"data":[{"id":"llama.cpp"}]}` |
| 2 | `nonstream_small_request_no_save_no_meta` | async | [L241](../tests/api.rs#L241) | small non-stream chat | 200 + mock reply; model forwarded; slot-0 pin in body root/options/query; no save, no restore, no meta file |
| 3 | `nonstream_big_request_saves_and_writes_meta` | async | [L279](../tests/api.rs#L279) | big (600-word) non-stream chat | `cache_prompt=true`; KV saved; meta file with key, model, wpb 100, 6 blocks, prefix_len, timestamp |
| 4 | `second_big_request_restores_saved_cache` | async | [L312](../tests/api.rs#L312) | 2nd request sharing the prefix | req 1 saves key_a; req 2 restores key_a into slot 1 and saves new key_b |
| 5 | `stream_request_passes_sse_and_saves_afterwards` | async | [L358](../tests/api.rs#L358) | stream chat | `text/event-stream`; Hello/world/`[DONE]`; save + meta written after the stream ends |
| 6 | `nonstream_client_cancel_aborts_upstream` | async | [L406](../tests/api.rs#L406) | *real TCP*: client cancels an in-flight JSON request | upstream request aborted (0 bodies produced); slot released → follow-up request 200 |
| 7 | `stream_client_cancel_aborts_upstream_over_tcp` | async | [L461](../tests/api.rs#L461) | *real TCP*: client disconnects after 1 chunk | upstream aborted (chunk count stays 1, not merely paused); reader cleanup still saves KV + writes meta |
| 8 | `stream_provider_error_is_passed_through` | async | [L526](../tests/api.rs#L526) | backend 500 on a stream request | 500 + provider error body; no save |
| 9 | `nonstream_provider_error_becomes_500` | async | [L549](../tests/api.rs#L549) | backend 500 on a non-stream request | 500 + error mentioning 500; no save |
| 10 | `nonstream_json_array_provider_returns_502` | async | [L567](../tests/api.rs#L567) | JSON array provider body | 502 `"provider non-JSON body"`; no save |
| 11 | `invalid_json_body_rejected` | async | [L585](../tests/api.rs#L585) | malformed request JSON | 422 |
| 12 | `model_defaults_to_configured_id_when_missing` | async | [L598](../tests/api.rs#L598) | request without `model` | default `llama.cpp` forwarded to the backend |
| 13 | `dead_backend_traffic_fails_over_to_live_backend` | async | [L614](../tests/api.rs#L614) | be0 live / be1 closed port, 4 big requests | all 200 (req 2 transparently retried on live); be1 cooldown routes req 3/4 to live; 4 saves + 3 restores on live |
| 14 | `connection_failure_retries_on_other_backend` | async | [L693](../tests/api.rs#L693) | be0 dead / be1 live, 1 small request | 200 via one retry; exactly 1 chat reaches the live backend |
| 15 | `probe_recovers_cooled_down_backend` | async | [L763](../tests/api.rs#L763) | be0 port freed, server later restarts on it | 3 requests served by live while be0 down; probe round: 0 recoveries; after restart: 1 probe recovers; LRU routes the next request back to be0 |
| 16 | `failing_backend_slots_do_not_pin_all_traffic` | async | [L852](../tests/api.rs#L852) | be1 answers provider 500 to every chat | `[200,200,500,500,200]`; failing slots marked used → traffic rotates; 3 saves on be0, 0 on be1 |
| 17 | `prune_removes_oldest_meta_and_kv_files` | async | [L931](../tests/api.rs#L931) | `META_MAX=2`, 3 big saves | oldest meta **and** KV file pruned; the 2 newer survive in both dirs |
| 18 | `coalesce_concurrent_identical_requests_single_backend_call` | async | [L1487](../tests/api.rs#L1487) | 5 concurrent identical non-stream requests | 1 backend call; all 5 get the identical leader response |
| 19 | `coalesce_different_generation_params_still_merge` | async | [L1523](../tests/api.rs#L1523) | same content, different `temperature`/`max_tokens` | 1 backend call (grouping is by cache key, not params) |
| 20 | `coalesce_mixed_stream_and_json_share_one_backend_call` | async | [L1562](../tests/api.rs#L1562) | 1 stream + 1 JSON request, same content | 1 backend call; JSON caller gets a completion, stream caller gets the full SSE ending `[DONE]` |
| 21 | `coalesce_stream_groups_share_one_backend_stream` | async | [L1610](../tests/api.rs#L1610) | 3 concurrent stream requests | 1 backend stream; every follower receives the identical byte stream |
| 22 | `coalesce_flag_off_preserves_per_request_backend_calls` | async | [L1646](../tests/api.rs#L1646) | `COALESCE_REQUESTS` off | 5 requests → 5 backend calls |
| 23 | `coalesce_new_group_after_completion` | async | [L1671](../tests/api.rs#L1671) | sequential identical requests | 2 backend calls (a new group starts after the leader finishes) |
| 24 | `stream_queue_size_one_delivers_full_stream` | async | [L1689](../tests/api.rs#L1689) | stream chat with `STREAM_QUEUE_SIZE=1` (maximal backpressure) | full SSE delivered (Hello/world/`[DONE]`); 3 chunks produced exactly once; reader cleanup still saves KV + writes meta |
| 25 | `coalesce_stream_queue_size_one_follower_receives_full_stream` | async | [L1738](../tests/api.rs#L1738) | 3 concurrent stream requests, coalescing on, queue size 1 | 1 backend stream; every follower receives the identical full stream ending `[DONE]` |
| 26 | `prefilter_rejects_keyword_request_before_backend` | async | [L1782](../tests/api.rs#L1782) | `PREFILTER_BLOCKLIST` keyword in a non-stream request | 400 JSON error naming the keyword; **zero** chat/save/restore calls; no meta file |
| 27 | `prefilter_accepts_clean_request` | async | [L1808](../tests/api.rs#L1808) | no blocked keyword | 200 mock reply; exactly 1 chat call reaches the backend |
| 28 | `prefilter_is_case_insensitive_by_default` | async | [L1824](../tests/api.rs#L1824) | keyword `ForBIDDEN` vs request text `FORBIDDEN` | 400; no backend call |
| 29 | `prefilter_inspects_content_part_arrays` | async | [L1837](../tests/api.rs#L1837) | keyword inside an OpenAI content-part array | 400; no backend call |
| 30 | `prefilter_rejects_stream_request_with_json_error` | async | [L1857](../tests/api.rs#L1857) | `stream: true` request containing a keyword | 400 `application/json` (not SSE); no backend call |
| 31 | `prefilter_rejects_big_request_without_save_or_meta` | async | [L1886](../tests/api.rs#L1886) | 600-word prompt containing a keyword | 400; no save, no restore, no meta file |
| 32 | `prefilter_rejects_before_coalescing` | async | [L1906](../tests/api.rs#L1906) | coalescing on; 2 concurrent same-key blocked requests | both 400 immediately; no group forms; no backend call |
| 33 | `custom_prefilter_adapter_accepts_and_rejects` | async | [L1960](../tests/api.rs#L1960) | custom `Prefilter` trait impl (rejects >5-word prompts with 413) | short prompt 200 (1 backend call); long prompt 413 with the adapter's own status/message |
| 34 | `restore_works_after_prune_and_pruned_key_falls_back` | async | [L1260](../tests/api.rs#L1260) | `META_MAX=2`; 3 saves (prunes A); then B+tail; then A again | B (a surviving entry) is still restored after the prune (ratio 5/6); pruned A gets **no** restore (full prefill), still 200, and is re-saved; bound stays at 2 |
| 35 | `restore_touches_lru_timestamp_so_prune_keeps_hot_entry` | async | [L1383](../tests/api.rs#L1383) | `META_MAX=2`; saves A, B, C (prunes A); then B+tail restores B and saves B' | the successful restore bumps B's LRU timestamp → B's own post-save prune evicts C (the true oldest), not the just-restored B |
| 36 | `restore_rejected_cleans_up_stale_meta` | async | [L2536](../tests/api.rs#L2536) | save A (KV created); delete A's KV; mock restore → 400; A+tail; A+tail again; delete KV; A+tail2 | 400 with the KV absent from every slot-save dir → stale meta removed (`stale_meta_removed`); 400 with the KV still present → meta **kept** (may be transient); final request saves a fresh entry |
| 37 | `restore_rejected_keeps_meta_when_save_dir_unvisible` | async | [L2611](../tests/api.rs#L2611) | configured slot-save dir does not exist on the proxy's host; mock restore → 400 | absence of the KV file can't be verified (remote backend) → meta kept (status quo) |
| 38 | `result_cache_serves_cached_result_without_backend` | async | [L2034](../tests/api.rs#L2034) | 1st request stores the result in the cache dir; 2nd same-key request | 2nd served from cache (identical body, 200) with **no** backend call, no save/restore, no meta file; a different key misses → backend |
| 39 | `result_cache_expires_after_ttl` | async | [L2079](../tests/api.rs#L2079) | TTL 0.2 s; fresh hit, then 300 ms sleep | fresh entry served from cache; after the TTL the entry is expired + lazily removed and the request goes to the backend (new entry stored) |
| 40 | `result_cache_stream_requests_bypass_cache` | async | [L2108](../tests/api.rs#L2108) | same key, `stream: true` after a cached non-stream result | SSE (`text/event-stream`) from the backend — streaming requests are never served from the cache |
| 41 | `result_cache_remove_after_serve_cleanup` | async | [L2379](../tests/api.rs#L2379) | `RemoveAfterServe` post-cleanup attached; 3 same-key requests | 1st → backend (entry stored); 2nd → served from cache, then entry file removed; 3rd → backend again (one-shot cache) |
| 42 | `keyword_blocklist_rejects_before_result_cache_serves` | async | [L2424](../tests/api.rs#L2424) | blocklist + result cache chained; blocked prompt with a pre-populated cache entry | `400` from the blocklist — the reject beats the cached serve; backend never called |
| 43 | `request_log_writes_xlf_record` | async | [L2784](../tests/api.rs#L2784) | non-stream chat with the built-in `request_log` plugin enabled (via `Config::with_request_log` → `Registry::with_builtins` + `load_request_loggers` + `prepare`, the `main.rs` path) | the file starts with the 4 XLF header lines and holds 1 record: W3C fields (`POST`, URI, user agent, 24-char `…Z` datetime, `cs-time` ≥ 0, `cs-bytes` = `Content-Length`, 200, `sc-bytes` > 0; `c-ip` = `-` — `oneshot` carries no `ConnectInfo`) + lpcache fields (model, `stream` false, key = `expected_key(..)[..16]`, `n-words` 2, `is-big` false, backend 0, slot 0/1, `restored`/`cache-hit`/`coalesced`/`reason` = `-`) + performance fields (mock `usage`/`timings`: prompt/completion/cached tokens `10`/`5`/`8`, `prefill-ms` `25.00`, `gen-ms` `15.00`, `prefill-tps` `400.0`, `gen-tps` `333.3`, `ttft-ms` = `-` — non-streaming) |
| 44 | `request_log_stream_request_counts_delivered_bytes` | async | [L2892](../tests/api.rs#L2892) | `stream: true` chat, logger enabled | `lpcache-stream` = `true`, 200, and `sc-bytes` equals exactly the number of SSE bytes delivered to the client (byte counting via `Body::into_data_stream`) |
| 45 | `request_log_logs_failed_request_without_pipeline` | async | [L2923](../tests/api.rs#L2923) | malformed JSON body — the handler fails before any pipeline view exists | 422 **and** one log record: `sc-status` 422, method/URI filled, every lpcache field `-` |
| 46 | `request_log_rotates_and_bounds_keep` | async | [L2950](../tests/api.rs#L2950) | `max_bytes=2048` (between one and a few records, given the 28-field header), `keep=2`, repeated requests | ≥ 2 rotations: `.1` and `.2` exist, `.3` does not; every generation parses (fresh XLF header); the rotated generations hold the overflowed records; the current file is recreated after a rotation |
| 47 | `request_log_stream_writes_perf_fields` | async | [L2855](../tests/api.rs#L2855) | `stream: true` chat, logger enabled | `lpcache-ttft-ms` is a proxy-measured value ≥ 0 (time to the first SSE chunk); the backend `usage`/`timings` carried in the stream's final chunk are logged (prompt/completion/cached tokens `10`/`5`/`8`, `prefill-ms` `25.00`, `gen-ms` `15.00`, `prefill-tps` `400.0`, `gen-tps` `333.3`) |
| 48 | `result_cache_no_cache_header_bypasses_the_cache` | async | [L2334](../tests/api.rs#L2334) | `result_cache` plugin with `cache_control: true` (via a `PLUGINS` spec); cache warmed; repeat same-key requests with `Cache-Control: no-cache`, `no-cache, no-store`, `max-age=60`, and no header | `no-cache` (alone or in a directive list) bypasses the cache → backend (call count grows); `max-age=60` and no header are still served from the cache (no new backend call) |
| 49 | `result_cache_caches_streaming_result_and_serves_streaming_client` | async | [L2196](../tests/api.rs#L2196) | `result_cache` plugin with `cache_streaming: true`; a streaming request, then a repeat streaming request for the same key | the first stream reaches the backend and, once complete, is aggregated into a non-streaming completion and stored; the repeat request is served from the cache as SSE (`text/event-stream` + `[DONE]`) with no new backend call |
| 50 | `kv_cache_disabled_backend_never_saves_or_restores` | async | [L1005](../tests/api.rs#L1005) | single backend with `kv_cache: false` (but a KV save dir), big non-stream request | the chat succeeds; the mock records **no** save and no restore call and no `.meta.json` is written (the backend is a plain chat provider) |
| 51 | `kv_cache_disabled_backend_skips_stream_save` | async | [L1077](../tests/api.rs#L1077) | single backend with `kv_cache: false`, big **streaming** request | the stream passes through; the reader's cleanup issues no save call and writes no meta file (regression guard — the stream path historically saved unconditionally) |
| 52 | `kv_restore_prefers_kv_enabled_backend` | async | [L1170](../tests/api.rs#L1170) | two backends (be0 `kv_cache: true`, be1 `kv_cache: false`), big request twice | the KV save and the second request's restore both land on the kv-enabled backend 0; backend 1's save dir stays empty |
| 53 | `dashboard_page_and_stats_reachable` | async | [L3064](../tests/api.rs#L3064) | a `dashboard` spec with default params; `GET /dashboard` + `GET /dashboard/api/stats` | stats API: 200 JSON with `version`/`uptime_secs`/`model`/`refresh_ms`, the `proxy` snapshot (meta files, total slots, in-flight, active groups, backends, slots) and empty aggregates; the page: 200 `text/html; charset=utf-8` self-contained HTML |
| 54 | `dashboard_custom_path` | async | [L3433](../tests/api.rs#L3433) | a `dashboard` spec with `path: "/stats"` | the page is served at `/stats` (referencing `/stats/api/stats`), stats at `/stats/api/stats`; the default `/dashboard` path is **not** mounted |
| 55 | `dashboard_counters_update_after_requests` | async | [L3179](../tests/api.rs#L3179) | a `dashboard` spec; one successful chat + one 404 route | the aggregates count both (total 2, success 1); the recent-requests ring holds both entries |
| 56 | `dashboard_routes_excluded_from_request_log_and_aggregates` | async | [L3329](../tests/api.rs#L3329) | both the request log and the dashboard enabled; page + stats polls interleaved with a real chat | the dashboard's own routes are absent from the request log and from the aggregates (only the chat request is counted), so polling never inflates the statistics |
| 57 | `dashboard_stats_since_scopes_the_range` | async | [L3257](../tests/api.rs#L3257) | a `dashboard` spec; two finished requests (1 ok + 1 client error); stats with `?since=0`, a future `since`, and an invalid `since` | `since=0` (all time): `range.counters` holds both requests and the series buckets sum to the range; a future `since`: empty range with the lifetime counters and recent ring unaffected; `since=abc` → `400` |
| 58 | `dashboard_page_charts_draw_bar_figures_and_baseline` | async | [L3136](../tests/api.rs#L3136) | a `dashboard` spec; `GET /dashboard` | the HTML carries one canvas per throughput series (`chart-req`, `chart-tok`, `chart-cached`) and the per-bar figure drawing with adaptive stride + hover |

## Coverage map — feature → tests

| Feature | Tests |
|---|---|
| Endpoints & happy path | `models_endpoint_returns_configured_id`, `nonstream_small_request_no_save_no_meta`, `nonstream_big_request_saves_and_writes_meta`, `second_big_request_restores_saved_cache`, `stream_request_passes_sse_and_saves_afterwards` |
| Client cancel / disconnect (*real TCP*) | `nonstream_client_cancel_aborts_upstream`, `stream_client_cancel_aborts_upstream_over_tcp` |
| Provider error mapping & validation | `stream_provider_error_is_passed_through`, `nonstream_provider_error_becomes_500`, `nonstream_json_array_provider_returns_502`, `invalid_json_body_rejected`, `model_defaults_to_configured_id_when_missing` |
| Failover & circuit breaker | `dead_backend_traffic_fails_over_to_live_backend`, `connection_failure_retries_on_other_backend`, `probe_recovers_cooled_down_backend`, `failing_backend_slots_do_not_pin_all_traffic` |
| Pruning (`META_MAX`) + restore LRU touch | `prune_removes_oldest_meta_and_kv_files`, `restore_works_after_prune_and_pruned_key_falls_back`, `restore_touches_lru_timestamp_so_prune_keeps_hot_entry` |
| Stale-meta cleanup on rejected restore | `restore_rejected_cleans_up_stale_meta`, `restore_rejected_keeps_meta_when_save_dir_unvisible` |
| Request coalescing | `coalesce_concurrent_identical_requests_single_backend_call`, `coalesce_different_generation_params_still_merge`, `coalesce_mixed_stream_and_json_share_one_backend_call`, `coalesce_stream_groups_share_one_backend_stream`, `coalesce_flag_off_preserves_per_request_backend_calls`, `coalesce_new_group_after_completion` |
| `STREAM_QUEUE_SIZE=1` backpressure | `stream_queue_size_one_delivers_full_stream`, `coalesce_stream_queue_size_one_follower_receives_full_stream` |
| Prefilter plugins | `prefilter_rejects_keyword_request_before_backend`, `prefilter_accepts_clean_request`, `prefilter_is_case_insensitive_by_default`, `prefilter_inspects_content_part_arrays`, `prefilter_rejects_stream_request_with_json_error`, `prefilter_rejects_big_request_without_save_or_meta`, `prefilter_rejects_before_coalescing`, `custom_prefilter_adapter_accepts_and_rejects` |
| Result-cache plugin | `result_cache_serves_cached_result_without_backend`, `result_cache_expires_after_ttl`, `result_cache_stream_requests_bypass_cache`, `result_cache_remove_after_serve_cleanup`, `keyword_blocklist_rejects_before_result_cache_serves`, `result_cache_no_cache_header_bypasses_the_cache`, `result_cache_caches_streaming_result_and_serves_streaming_client` |
| Per-backend `kv_cache` flag | `kv_cache_disabled_backend_never_saves_or_restores`, `kv_cache_disabled_backend_skips_stream_save`, `kv_restore_prefers_kv_enabled_backend` |
| Dashboard plugin | `dashboard_page_and_stats_reachable`, `dashboard_custom_path`, `dashboard_counters_update_after_requests`, `dashboard_routes_excluded_from_request_log_and_aggregates`, `dashboard_stats_since_scopes_the_range`, `dashboard_page_charts_draw_bar_figures_and_baseline` |
| Request-log plugin | `request_log_writes_xlf_record`, `request_log_stream_request_counts_delivered_bytes`, `request_log_logs_failed_request_without_pipeline`, `request_log_rotates_and_bounds_keep`, `request_log_stream_writes_perf_fields` |
