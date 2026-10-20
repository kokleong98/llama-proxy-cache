# 7. API end-to-end — `tests/api.rs` (47)

[← Suite index](README.md)

Part of the [lpcache regression test cases](README.md): every entry maps
1:1 to a test function; the full suite (197 tests) runs with
`cargo test --workspace`.

The real proxy router (axum) in front of `MockLlama`, which records every
chat body/query, restore and save. Default setup: 1 backend, 2 slots,
wpb 100, big threshold 500 words, LCP 0.6, `META_MAX=0`. Tests marked
*real TCP* serve the proxy on a real local port (client-disconnect
behaviour requires a real connection).

## Module overview

- **Target:** `lpcache` integration tests — the `api` test binary ([`tests/api.rs`](../tests/api.rs)); 47 tests, all async.
- **Fixtures & helpers:**
  - `make_state*` family — builds `AppState` in front of `MockLlama`: `make_state` / `make_app` (default: 1 backend, 2 slots, wpb 100, big threshold 500 words, LCP 0.6, meta in a `tempfile` dir, `META_MAX=0`), `make_state_coalesce*` (coalescing on), `make_state*stream_queue*` (custom `STREAM_QUEUE_SIZE`), `make_state_prefilter*` (built-in keyword blocklist).
  - `load_prefilter` — loads the prefilter + result store through the production `load_prefilters` path from the config's plugin specs.
  - `ServedProxy` — serves the router on a real local TCP port (`axum::serve`) for the client-disconnect tests; `Drop` aborts the task.
  - Request helpers: `post_json` / `get_req` / `read_body`, `words(n)` (n distinct prompt words), `expected_key(prefix)` (recomputes the exact cache key the proxy computes), `free_port()`.
  - The `request_log_*` tests build state the way `main.rs` does (`Config::with_request_log` → `load_request_loggers` → `prepare`).
- **Surface under test:** `router`/`AppState` end-to-end: `GET /v1/models`, `POST /v1/chat/completions` (non-stream JSON and SSE streaming), KV save/restore orchestration, failover/retry on dead or failing backends, meta+KV pruning, request coalescing, prefilters, the result-cache plugin, and the request-log plugin.

## How to run

```sh
cd lpcache
cargo test --test api                                       # all 47 E2E tests
cargo test --test api models_endpoint_returns_configured_id # one test (exact name)
```

The **Test case** column is the exact filter for that test: pass it
into `cargo test` as shown above.

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 1 | `models_endpoint_returns_configured_id` | async | [L230](../tests/api.rs#L230) | GET `/v1/models` | 200 `{"data":[{"id":"llama.cpp"}]}` |
| 2 | `nonstream_small_request_no_save_no_meta` | async | [L239](../tests/api.rs#L239) | small non-stream chat | 200 + mock reply; model forwarded; slot-0 pin in body root/options/query; no save, no restore, no meta file |
| 3 | `nonstream_big_request_saves_and_writes_meta` | async | [L277](../tests/api.rs#L277) | big (600-word) non-stream chat | `cache_prompt=true`; KV saved; meta file with key, model, wpb 100, 6 blocks, prefix_len, timestamp |
| 4 | `second_big_request_restores_saved_cache` | async | [L310](../tests/api.rs#L310) | 2nd request sharing the prefix | req 1 saves key_a; req 2 restores key_a into slot 1 and saves new key_b |
| 5 | `stream_request_passes_sse_and_saves_afterwards` | async | [L356](../tests/api.rs#L356) | stream chat | `text/event-stream`; Hello/world/`[DONE]`; save + meta written after the stream ends |
| 6 | `nonstream_client_cancel_aborts_upstream` | async | [L404](../tests/api.rs#L404) | *real TCP*: client cancels an in-flight JSON request | upstream request aborted (0 bodies produced); slot released → follow-up request 200 |
| 7 | `stream_client_cancel_aborts_upstream_over_tcp` | async | [L459](../tests/api.rs#L459) | *real TCP*: client disconnects after 1 chunk | upstream aborted (chunk count stays 1, not merely paused); reader cleanup still saves KV + writes meta |
| 8 | `stream_provider_error_is_passed_through` | async | [L524](../tests/api.rs#L524) | backend 500 on a stream request | 500 + provider error body; no save |
| 9 | `nonstream_provider_error_becomes_500` | async | [L547](../tests/api.rs#L547) | backend 500 on a non-stream request | 500 + error mentioning 500; no save |
| 10 | `nonstream_json_array_provider_returns_502` | async | [L565](../tests/api.rs#L565) | JSON array provider body | 502 `"provider non-JSON body"`; no save |
| 11 | `invalid_json_body_rejected` | async | [L583](../tests/api.rs#L583) | malformed request JSON | 422 |
| 12 | `model_defaults_to_configured_id_when_missing` | async | [L596](../tests/api.rs#L596) | request without `model` | default `llama.cpp` forwarded to the backend |
| 13 | `dead_backend_traffic_fails_over_to_live_backend` | async | [L612](../tests/api.rs#L612) | be0 live / be1 closed port, 4 big requests | all 200 (req 2 transparently retried on live); be1 cooldown routes req 3/4 to live; 4 saves + 3 restores on live |
| 14 | `connection_failure_retries_on_other_backend` | async | [L688](../tests/api.rs#L688) | be0 dead / be1 live, 1 small request | 200 via one retry; exactly 1 chat reaches the live backend |
| 15 | `probe_recovers_cooled_down_backend` | async | [L755](../tests/api.rs#L755) | be0 port freed, server later restarts on it | 3 requests served by live while be0 down; probe round: 0 recoveries; after restart: 1 probe recovers; LRU routes the next request back to be0 |
| 16 | `failing_backend_slots_do_not_pin_all_traffic` | async | [L841](../tests/api.rs#L841) | be1 answers provider 500 to every chat | `[200,200,500,500,200]`; failing slots marked used → traffic rotates; 3 saves on be0, 0 on be1 |
| 17 | `prune_removes_oldest_meta_and_kv_files` | async | [L917](../tests/api.rs#L917) | `META_MAX=2`, 3 big saves | oldest meta **and** KV file pruned; the 2 newer survive in both dirs |
| 18 | `coalesce_concurrent_identical_requests_single_backend_call` | async | [L1212](../tests/api.rs#L1212) | 5 concurrent identical non-stream requests | 1 backend call; all 5 get the identical leader response |
| 19 | `coalesce_different_generation_params_still_merge` | async | [L1248](../tests/api.rs#L1248) | same content, different `temperature`/`max_tokens` | 1 backend call (grouping is by cache key, not params) |
| 20 | `coalesce_mixed_stream_and_json_share_one_backend_call` | async | [L1287](../tests/api.rs#L1287) | 1 stream + 1 JSON request, same content | 1 backend call; JSON caller gets a completion, stream caller gets the full SSE ending `[DONE]` |
| 21 | `coalesce_stream_groups_share_one_backend_stream` | async | [L1335](../tests/api.rs#L1335) | 3 concurrent stream requests | 1 backend stream; every follower receives the identical byte stream |
| 22 | `coalesce_flag_off_preserves_per_request_backend_calls` | async | [L1371](../tests/api.rs#L1371) | `COALESCE_REQUESTS` off | 5 requests → 5 backend calls |
| 23 | `coalesce_new_group_after_completion` | async | [L1396](../tests/api.rs#L1396) | sequential identical requests | 2 backend calls (a new group starts after the leader finishes) |
| 24 | `stream_queue_size_one_delivers_full_stream` | async | [L1414](../tests/api.rs#L1414) | stream chat with `STREAM_QUEUE_SIZE=1` (maximal backpressure) | full SSE delivered (Hello/world/`[DONE]`); 3 chunks produced exactly once; reader cleanup still saves KV + writes meta |
| 25 | `coalesce_stream_queue_size_one_follower_receives_full_stream` | async | [L1463](../tests/api.rs#L1463) | 3 concurrent stream requests, coalescing on, queue size 1 | 1 backend stream; every follower receives the identical full stream ending `[DONE]` |
| 26 | `prefilter_rejects_keyword_request_before_backend` | async | [L1507](../tests/api.rs#L1507) | `PREFILTER_BLOCKLIST` keyword in a non-stream request | 400 JSON error naming the keyword; **zero** chat/save/restore calls; no meta file |
| 27 | `prefilter_accepts_clean_request` | async | [L1533](../tests/api.rs#L1533) | no blocked keyword | 200 mock reply; exactly 1 chat call reaches the backend |
| 28 | `prefilter_is_case_insensitive_by_default` | async | [L1549](../tests/api.rs#L1549) | keyword `ForBIDDEN` vs request text `FORBIDDEN` | 400; no backend call |
| 29 | `prefilter_inspects_content_part_arrays` | async | [L1562](../tests/api.rs#L1562) | keyword inside an OpenAI content-part array | 400; no backend call |
| 30 | `prefilter_rejects_stream_request_with_json_error` | async | [L1582](../tests/api.rs#L1582) | `stream: true` request containing a keyword | 400 `application/json` (not SSE); no backend call |
| 31 | `prefilter_rejects_big_request_without_save_or_meta` | async | [L1611](../tests/api.rs#L1611) | 600-word prompt containing a keyword | 400; no save, no restore, no meta file |
| 32 | `prefilter_rejects_before_coalescing` | async | [L1631](../tests/api.rs#L1631) | coalescing on; 2 concurrent same-key blocked requests | both 400 immediately; no group forms; no backend call |
| 33 | `custom_prefilter_adapter_accepts_and_rejects` | async | [L1685](../tests/api.rs#L1685) | custom `Prefilter` trait impl (rejects >5-word prompts with 413) | short prompt 200 (1 backend call); long prompt 413 with the adapter's own status/message |
| 34 | `restore_works_after_prune_and_pruned_key_falls_back` | async | [L989](../tests/api.rs#L989) | `META_MAX=2`; 3 saves (prunes A); then B+tail; then A again | B (a surviving entry) is still restored after the prune (ratio 5/6); pruned A gets **no** restore (full prefill), still 200, and is re-saved; bound stays at 2 |
| 35 | `restore_touches_lru_timestamp_so_prune_keeps_hot_entry` | async | [L1110](../tests/api.rs#L1110) | `META_MAX=2`; saves A, B, C (prunes A); then B+tail restores B and saves B' | the successful restore bumps B's LRU timestamp → B's own post-save prune evicts C (the true oldest), not the just-restored B |
| 36 | `restore_rejected_cleans_up_stale_meta` | async | [L2019](../tests/api.rs#L2019) | save A (KV created); delete A's KV; mock restore → 400; A+tail; A+tail again; delete KV; A+tail2 | 400 with the KV absent from every slot-save dir → stale meta removed (`stale_meta_removed`); 400 with the KV still present → meta **kept** (may be transient); final request saves a fresh entry |
| 37 | `restore_rejected_keeps_meta_when_save_dir_unvisible` | async | [L2094](../tests/api.rs#L2094) | configured slot-save dir does not exist on the proxy's host; mock restore → 400 | absence of the KV file can't be verified (remote backend) → meta kept (status quo) |
| 38 | `result_cache_serves_cached_result_without_backend` | async | [L1757](../tests/api.rs#L1757) | 1st request stores the result in the cache dir; 2nd same-key request | 2nd served from cache (identical body, 200) with **no** backend call, no save/restore, no meta file; a different key misses → backend |
| 39 | `result_cache_expires_after_ttl` | async | [L1802](../tests/api.rs#L1802) | TTL 0.2 s; fresh hit, then 300 ms sleep | fresh entry served from cache; after the TTL the entry is expired + lazily removed and the request goes to the backend (new entry stored) |
| 40 | `result_cache_stream_requests_bypass_cache` | async | [L1831](../tests/api.rs#L1831) | same key, `stream: true` after a cached non-stream result | SSE (`text/event-stream`) from the backend — streaming requests are never served from the cache |
| 41 | `result_cache_remove_after_serve_cleanup` | async | [L1866](../tests/api.rs#L1866) | `RemoveAfterServe` post-cleanup attached; 3 same-key requests | 1st → backend (entry stored); 2nd → served from cache, then entry file removed; 3rd → backend again (one-shot cache) |
| 42 | `keyword_blocklist_rejects_before_result_cache_serves` | async | [L1911](../tests/api.rs#L1911) | blocklist + result cache chained; blocked prompt with a pre-populated cache entry | `400` from the blocklist — the reject beats the cached serve; backend never called |
| 43 | `request_log_writes_xlf_record` | async | [L2263](../tests/api.rs#L2263) | non-stream chat with the built-in `request_log` plugin enabled (via `Config::with_request_log` → `Registry::with_builtins` + `load_request_loggers` + `prepare`, the `main.rs` path) | the file starts with the 4 XLF header lines and holds 1 record: W3C fields (`POST`, URI, user agent, 24-char `…Z` datetime, `cs-time` ≥ 0, `cs-bytes` = `Content-Length`, 200, `sc-bytes` > 0; `c-ip` = `-` — `oneshot` carries no `ConnectInfo`) + lpcache fields (model, `stream` false, key = `expected_key(..)[..16]`, `n-words` 2, `is-big` false, backend 0, slot 0/1, `restored`/`cache-hit`/`coalesced`/`reason` = `-`) + performance fields (mock `usage`/`timings`: prompt/completion/cached tokens `10`/`5`/`8`, `prefill-ms` `25.00`, `gen-ms` `15.00`, `prefill-tps` `400.0`, `gen-tps` `333.3`, `ttft-ms` = `-` — non-streaming) |
| 44 | `request_log_stream_request_counts_delivered_bytes` | async | [L2371](../tests/api.rs#L2371) | `stream: true` chat, logger enabled | `lpcache-stream` = `true`, 200, and `sc-bytes` equals exactly the number of SSE bytes delivered to the client (byte counting via `Body::into_data_stream`) |
| 45 | `request_log_logs_failed_request_without_pipeline` | async | [L2402](../tests/api.rs#L2402) | malformed JSON body — the handler fails before any pipeline view exists | 422 **and** one log record: `sc-status` 422, method/URI filled, every lpcache field `-` |
| 46 | `request_log_rotates_and_bounds_keep` | async | [L2429](../tests/api.rs#L2429) | `max_bytes=2048` (between one and a few records, given the 28-field header), `keep=2`, repeated requests | ≥ 2 rotations: `.1` and `.2` exist, `.3` does not; every generation parses (fresh XLF header); the rotated generations hold the overflowed records; the current file is recreated after a rotation |
| 47 | `request_log_stream_writes_perf_fields` | async | [L2334](../tests/api.rs#L2334) | `stream: true` chat, logger enabled | `lpcache-ttft-ms` is a proxy-measured value ≥ 0 (time to the first SSE chunk); the backend `usage`/`timings` carried in the stream's final chunk are logged (prompt/completion/cached tokens `10`/`5`/`8`, `prefill-ms` `25.00`, `gen-ms` `15.00`, `prefill-tps` `400.0`, `gen-tps` `333.3`) |

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
| Result-cache plugin | `result_cache_serves_cached_result_without_backend`, `result_cache_expires_after_ttl`, `result_cache_stream_requests_bypass_cache`, `result_cache_remove_after_serve_cleanup`, `keyword_blocklist_rejects_before_result_cache_serves` |
| Request-log plugin | `request_log_writes_xlf_record`, `request_log_stream_request_counts_delivered_bytes`, `request_log_logs_failed_request_without_pipeline`, `request_log_rotates_and_bounds_keep`, `request_log_stream_writes_perf_fields` |
