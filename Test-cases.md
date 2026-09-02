# lpcache — Regression Test Cases

Complete catalog of the **lpcache** regression suite (KV-cache-aware,
OpenAI-compatible proxy for llama.cpp). Every entry maps 1:1 to a test
function; the full suite runs with `cargo test`.

**Total: 129 test cases** — 77 unit + 37 API integration + 15 client
integration.

| # | Suite | Location | Tests |
|---|-------|----------|-------|
| 1 | Configuration (env vars, CLI, precedence) | `src/config.rs` | 25 |
| 2 | Request prefilter adapter | `src/prefilter.rs` | 9 |
| 3 | Hashing, meta files, pruning | `src/hashing.rs` | 19 |
| 4 | Request coalescing (`SingleFlight`) | `src/coalesce.rs` | 4 |
| 5 | Backend client — slot pinning | `src/llama_client.rs` | 3 |
| 6 | Slot management (selection, locks, breaker) | `src/slot_manager.rs` | 17 |
| 7 | API end-to-end (proxy ↔ mock llama.cpp) | `tests/api.rs` | 33 |
| 8 | Backend client — HTTP behavior | `tests/client.rs` | 15 |

## How to run

```sh
cd lpcache
cargo test                    # full regression suite (129 tests, ~4 s)
cargo test --lib              # 77 unit tests only
cargo test --test api         # 37 API integration tests
cargo test --test client      # 15 client integration tests
cargo test <name-substring>   # run a single test by name filter
cargo clippy --all-targets    # lint gate (expected: 0 warnings)
```

Integration tests start `MockLlama` — an in-process mock of the llama.cpp
server (`src/mock_backend.rs`) on free localhost ports — so the suite needs
no real `llama-server`, model files, or network access.

## 1. Configuration — `src/config.rs` (25)

| # | Test case | What it verifies | Expected result |
|---|-----------|------------------|-----------------|
| 1 | `defaults_when_empty` | all built-in defaults with an empty env | backend `http://127.0.0.1:8000` / 1 slot; wpb 100; big 500 words; LCP 0.1; meta dir `kv_meta`; 600 s timeout; model `llama.cpp`; port 8081; log INFO |
| 2 | `backends_json_parsed` | `BACKENDS` JSON array with 2 entries | both backends' `url` + `n_slots` parsed |
| 3 | `backends_invalid_json_falls_back_to_empty` | malformed `BACKENDS` value | empty backend list, no crash |
| 4 | `llama_url_and_n_slots_fallback` | single-backend fallback via `LLAMA_URL` + `N_SLOTS` | url `http://x:9`, 4 slots |
| 5 | `invalid_numbers_use_defaults` | invalid `WORDS_PER_BLOCK`/`LCP_TH`, valid `PORT` | wpb + LCP fall back to defaults; port 80 applied |
| 6 | `meta_dir_custom` | `META_DIR` override | meta dir `my_meta` |
| 7 | `meta_max_default_and_env` | `META_MAX` default / value / `0` / invalid | default; 7; 0 (pruning off); default |
| 8 | `slot_save_path_fallback_env` | `SLOT_SAVE_PATH` fallback; ignored when `BACKENDS` set | `/var/kv` in `slot_save_dirs()`; empty when `BACKENDS` set |
| 9 | `backends_slot_save_path_json` | per-backend `slot_save_path` field in `BACKENDS` | only backend 0's path in the save dirs |
| 10 | `api_key_env` | `LLAMA_API_KEY` set / blank | `Some("777")`; blank → `None` (no auth header sent) |
| 11 | `cli_parses_space_form` | `--flag value` form | port, llama-url, n-slots, log-level parsed |
| 12 | `cli_parses_equals_form` | `--flag=value` form | port + `BACKENDS` JSON parsed |
| 13 | `cli_equals_form_keeps_extra_equals_in_value` | value containing `=` | `http://a:1?x=1` kept intact |
| 14 | `cli_flag_twice_last_wins` | same flag given twice | last value wins |
| 15 | `cli_unknown_flag_errors` | unknown `--nope` flag | error message names the flag |
| 16 | `cli_missing_value_errors` | `--port` without a value | error message names the flag |
| 17 | `cli_help_flags` | `--help` / `-h`, also among other options | `help` flag set |
| 18 | `cli_version_flags` | `--version` / `-V`, also among other options | `version` flag set |
| 19 | `version_string_is_crate_name_and_version` | `version_string()` output | starts with `lpcache `, ends with the crate version |
| 20 | `cli_usage_lists_all_flags_and_env_vars` | completeness of `--help` text | all 20 flags and 18 env vars listed |
| 21 | `coalesce_requests_flag` | `COALESCE_REQUESTS` env + CLI | off by default; `true`/`1` on; `0`/invalid off; CLI overrides env |
| 22 | `stream_queue_size_env_and_cli` | `STREAM_QUEUE_SIZE` env + CLI | default 16; env `64` applied; `0` / invalid → default; CLI `--stream-queue-size 8` overrides env; builder clamps below 1 to 1 |
| 23 | `prefilter_env_and_cli` | `PREFILTER_BLOCKLIST` / `PREFILTER_CASE_INSENSITIVE` env + CLI | default disabled (empty list, case-insensitive); comma list trimmed with empty entries dropped; blank list = disabled; invalid bool → default; CLI wins over env |
| 24 | `config_precedence_cli_over_env_over_defaults` | CLI > env > default (merged env map **and** final Config) | CLI overrides; env applies to unset flags; unset flags add nothing; default otherwise |
| 25 | `config_from_cli_overrides_process_env` | `Config::from_cli` vs real process env | CLI value wins; empty CLI reproduces the process-env config |

## 2. Request prefilter adapter — `src/prefilter.rs` (9)

The accept/reject adapter consulted before any slot/backend work (see the
`prefilter` module docs).

| # | Test case | What it verifies | Expected result |
|---|-----------|------------------|-----------------|
| 1 | `keyword_rejects_on_match` | request containing a blocked keyword | `Reject { 400, message naming the keyword }` |
| 2 | `keyword_accepts_clean_request` | request with no blocked keyword | `Accept` |
| 3 | `keyword_case_insensitive_by_default` | keyword `ForBIDDEN` vs text `FORBIDDEN` | case-insensitive matches; case-sensitive mode requires exact case |
| 4 | `keyword_scans_all_messages` | keyword in a later message of a multi-turn conversation | `Reject` |
| 5 | `keyword_inspects_content_part_arrays` | OpenAI content-part array (`text` parts + non-text parts) | text parts inspected, non-text skipped → `Reject` |
| 6 | `keyword_empty_messages_accepts` | no messages / message without `content` | `Accept` |
| 7 | `message_text_flattens_content` | string + array contents flattened | newline-joined text; empty/defensive inputs → `""` |
| 8 | `from_comma_list_trims_and_drops_empty` | `PREFILTER_BLOCKLIST`-style parsing | trimmed list; whitespace-only/empty → `None` (disabled) |
| 9 | `trait_object_dispatch` | adapter behind `Arc<dyn Prefilter>` (the `AppState` shape) | `name()` + `check()` dispatch correctly |

## 3. Hashing, meta files, pruning — `src/hashing.rs` (19)

| # | Test case | What it verifies | Expected result |
|---|-----------|------------------|-----------------|
| 1 | `raw_prefix_joins_and_strips` | message contents joined, trimmed, empties skipped | `"hello\n\nworld"` |
| 2 | `raw_prefix_empty_and_missing_content` | empty list / missing `content` / `null` content | `""` |
| 3 | `raw_prefix_non_string_content` | non-string content hashed from its JSON text | `42` → `"42"` |
| 4 | `words_basic_lowercase_and_split` | `\w+` tokenization on lowercased text | `["hello","world","hello"]` |
| 5 | `words_underscore_and_digits` | underscore/digits kept, `-` splits | `["a_b","c9","d","e"]` |
| 6 | `words_no_word_chars` | whitespace / punctuation only | empty word list |
| 7 | `block_hashes_chunking` | 5 words chunked at wpb=2 | 3 blocks = sha256 of `"a b"`, `"c d"`, `"e"` |
| 8 | `block_hashes_empty_text` | empty / punctuation-only text | no blocks |
| 9 | `lcp_cases` | longest common block prefix | 2 / 4 / 0 / 1 / 0 for the test pairs |
| 10 | `sha256_known_vectors` | SHA-256 correctness | known digests of `""` and `"abc"` |
| 11 | `meta_roundtrip` | `write_meta` → `scan_all_meta` | key, model, prefix_len, wpb, blocks, timestamp>0 all preserved |
| 12 | `scan_skips_corrupt_and_wrong_extension` | corrupt `.meta.json` + non-meta file | scan returns empty |
| 13 | `scan_missing_dir_returns_empty` | non-existent META_DIR | empty, no error |
| 14 | `find_best_restore_candidate_filters_and_thresholds` | model + wpb filters, full match, below threshold, threshold boundary, tie | `k_full` at ratio 1.0; none below 0.6; partial 0.5 qualifies at th 0.5 (tie → first scanned wins) |
| 15 | `find_best_prefers_highest_ratio` | two candidates both clear the threshold | higher ratio (1.0) beats 0.5 at th 0.4 |
| 16 | `touch_meta_updates_timestamp` | `touch_meta` on existing / missing key | timestamp bumped, other fields kept; missing file → no-op |
| 17 | `prune_meta_removes_oldest_beyond_limit` | 5 metas, max 3, shuffled timestamps | 2 oldest (by ts, not name) removed, oldest first; second prune is a no-op |
| 18 | `prune_meta_noop_under_limit_and_zero_max` | under limit / `max=0` / missing dir | nothing pruned, no error |
| 19 | `prune_meta_corrupt_file_pruned_first` | unparseable meta among healthy ones | treated as oldest, removed first |

## 4. Request coalescing — `src/coalesce.rs` (4)

| # | Test case | What it verifies | Expected result |
|---|-----------|------------------|-----------------|
| 1 | `enter_first_is_leader_second_is_follower` | group entry by key | 1st caller = leader, 2nd = follower (same flight); different key → new group |
| 2 | `forget_removes_the_group` | group removal | re-entering a forgotten key starts a new leader |
| 3 | `finish_wakes_waiters_with_outcome` | waiter wakeup on leader finish | follower receives the leader's `SharedOutcome` |
| 4 | `sse_append_and_drain_is_ordered` | ordered incremental SSE drain | `"a"+"bc"` drains as `"abc"`, then `"d"`; full buffer `"abcd"` |

## 5. Backend client — slot pinning — `src/llama_client.rs` (3)

| # | Test case | What it verifies | Expected result |
|---|-----------|------------------|-----------------|
| 1 | `slot_pinning_none` | `slot = None` | body unchanged, no query params |
| 2 | `slot_pinning_duplicated_in_root_options_query` | `slot = Some(1)` | `_slot_id`/`slot_id`/`id_slot` in body root **and** `options`, `slot_id`/`id_slot` in query; existing fields preserved |
| 3 | `slot_pinning_adds_options_when_missing` | body without an `options` object | `options` object created with both pin fields |

## 6. Slot management — `src/slot_manager.rs` (17)

In-memory `TestClient` backends; verifies selection, per-slot locking,
save/restore semantics and the backend circuit breaker.

| # | Test case | What it verifies | Expected result |
|---|-----------|------------------|-----------------|
| 1 | `free_slot_picked_first_in_order` | fresh pool, repeated acquire | never-used slot 0 picked both times (release does not mark used) |
| 2 | `used_slots_are_skipped_then_oldest_wins` | free-first, then LRU | free slot 2 beats used 0/1; all used → oldest (slot 0) |
| 3 | `multi_backend_slot_ordering` | free-first across backends | backend 0 all used → backend 1 slot 0 |
| 4 | `restore_called_only_when_key_given` | restore gating in `acquire` | key → 1 restore call, `restored = Some(true)`; `None` → no call; backend fail → `Some(false)` |
| 5 | `save_after_saves_and_marks_used` | save OK path | save recorded; slot marked used → next free pick is slot 1 |
| 6 | `save_after_backend_500_returns_false_but_still_marks_used` | backend 500 on save | `Ok(false)`; slot still marked used |
| 7 | `save_after_other_error_propagates_and_keeps_slot_free` | other save error | `Err` propagates; `last_used` untouched |
| 8 | `second_acquire_waits_until_release` | per-slot `Semaphore(1)` guard | 2nd acquire blocks until 1st releases, then gets slot 0 |
| 9 | `acquire_times_out_when_slot_held` | acquire timeout (100 ms in test) | `Err(AcquireTimeout)` |
| 10 | `failed_backend_is_skipped_while_others_available` | circuit breaker in selection | cooling backend skipped by both free-first and LRU paths |
| 11 | `all_backends_down_fails_open` | all backends cooling | still picks the first slot in order (fail-open) |
| 12 | `pick_skips_held_slot_in_favor_of_idle` | idle-aware selection | held oldest slot skipped while an idle free slot exists; LRU prefers idle slots |
| 13 | `backend_cooldown_expires` | cooldown expiry (50 ms) | backend eligible again after the cooldown |
| 14 | `report_failure_escalates_backoff` | escalating backoff ladder | 5 → 10 → 20 → 40 s, capped at 60 s; streak 6 |
| 15 | `report_success_resets_cooldown_and_streak` | reset on success | cooldown 0 + streak 0; next failure restarts at 5 s |
| 16 | `probe_down_backends_recovers_up_backend` | probe recovery | one probe round recovers a live backend; healthy backends not counted |
| 17 | `acquire_excluding_picks_other_backend` | retry on a different backend | exclude be0 → be1 slot 0; single backend → fast error (no 300 s wait) |

## 7. API end-to-end — `tests/api.rs` (37)

The real proxy router (axum) in front of `MockLlama`, which records every
chat body/query, restore and save. Default setup: 1 backend, 2 slots,
wpb 100, big threshold 500 words, LCP 0.6, `META_MAX=0`. Tests marked
*real TCP* serve the proxy on a real local port (client-disconnect
behaviour requires a real connection).

| # | Test case | What it verifies | Expected result |
|---|-----------|------------------|-----------------|
| 1 | `models_endpoint_returns_configured_id` | GET `/v1/models` | 200 `{"data":[{"id":"llama.cpp"}]}` |
| 2 | `nonstream_small_request_no_save_no_meta` | small non-stream chat | 200 + mock reply; model forwarded; slot-0 pin in body root/options/query; no save, no restore, no meta file |
| 3 | `nonstream_big_request_saves_and_writes_meta` | big (600-word) non-stream chat | `cache_prompt=true`; KV saved; meta file with key, model, wpb 100, 6 blocks, prefix_len, timestamp |
| 4 | `second_big_request_restores_saved_cache` | 2nd request sharing the prefix | req 1 saves key_a; req 2 restores key_a into slot 1 and saves new key_b |
| 5 | `stream_request_passes_sse_and_saves_afterwards` | stream chat | `text/event-stream`; Hello/world/`[DONE]`; save + meta written after the stream ends |
| 6 | `nonstream_client_cancel_aborts_upstream` | *real TCP*: client cancels an in-flight JSON request | upstream request aborted (0 bodies produced); slot released → follow-up request 200 |
| 7 | `stream_client_cancel_aborts_upstream_over_tcp` | *real TCP*: client disconnects after 1 chunk | upstream aborted (chunk count stays 1, not merely paused); reader cleanup still saves KV + writes meta |
| 8 | `stream_provider_error_is_passed_through` | backend 500 on a stream request | 500 + provider error body; no save |
| 9 | `nonstream_provider_error_becomes_500` | backend 500 on a non-stream request | 500 + error mentioning 500; no save |
| 10 | `nonstream_json_array_provider_returns_502` | JSON array provider body | 502 `"provider non-JSON body"`; no save |
| 11 | `invalid_json_body_rejected` | malformed request JSON | 422 |
| 12 | `model_defaults_to_configured_id_when_missing` | request without `model` | default `llama.cpp` forwarded to the backend |
| 13 | `dead_backend_traffic_fails_over_to_live_backend` | be0 live / be1 closed port, 4 big requests | all 200 (req 2 transparently retried on live); be1 cooldown routes req 3/4 to live; 4 saves + 3 restores on live |
| 14 | `connection_failure_retries_on_other_backend` | be0 dead / be1 live, 1 small request | 200 via one retry; exactly 1 chat reaches the live backend |
| 15 | `probe_recovers_cooled_down_backend` | be0 port freed, server later restarts on it | 3 requests served by live while be0 down; probe round: 0 recoveries; after restart: 1 probe recovers; LRU routes the next request back to be0 |
| 16 | `failing_backend_slots_do_not_pin_all_traffic` | be1 answers provider 500 to every chat | `[200,200,500,500,200]`; failing slots marked used → traffic rotates; 3 saves on be0, 0 on be1 |
| 17 | `prune_removes_oldest_meta_and_kv_files` | `META_MAX=2`, 3 big saves | oldest meta **and** KV file pruned; the 2 newer survive in both dirs |
| 18 | `coalesce_concurrent_identical_requests_single_backend_call` | 5 concurrent identical non-stream requests | 1 backend call; all 5 get the identical leader response |
| 19 | `coalesce_different_generation_params_still_merge` | same content, different `temperature`/`max_tokens` | 1 backend call (grouping is by cache key, not params) |
| 20 | `coalesce_mixed_stream_and_json_share_one_backend_call` | 1 stream + 1 JSON request, same content | 1 backend call; JSON caller gets a completion, stream caller gets the full SSE ending `[DONE]` |
| 21 | `coalesce_stream_groups_share_one_backend_stream` | 3 concurrent stream requests | 1 backend stream; every follower receives the identical byte stream |
| 22 | `coalesce_flag_off_preserves_per_request_backend_calls` | `COALESCE_REQUESTS` off | 5 requests → 5 backend calls |
| 23 | `coalesce_new_group_after_completion` | sequential identical requests | 2 backend calls (a new group starts after the leader finishes) |
| 24 | `stream_queue_size_one_delivers_full_stream` | stream chat with `STREAM_QUEUE_SIZE=1` (maximal backpressure) | full SSE delivered (Hello/world/`[DONE]`); 3 chunks produced exactly once; reader cleanup still saves KV + writes meta |
| 25 | `coalesce_stream_queue_size_one_follower_receives_full_stream` | 3 concurrent stream requests, coalescing on, queue size 1 | 1 backend stream; every follower receives the identical full stream ending `[DONE]` |
| 26 | `prefilter_rejects_keyword_request_before_backend` | `PREFILTER_BLOCKLIST` keyword in a non-stream request | 400 JSON error naming the keyword; **zero** chat/save/restore calls; no meta file |
| 27 | `prefilter_accepts_clean_request` | no blocked keyword | 200 mock reply; exactly 1 chat call reaches the backend |
| 28 | `prefilter_is_case_insensitive_by_default` | keyword `ForBIDDEN` vs request text `FORBIDDEN` | 400; no backend call |
| 29 | `prefilter_inspects_content_part_arrays` | keyword inside an OpenAI content-part array | 400; no backend call |
| 30 | `prefilter_rejects_stream_request_with_json_error` | `stream: true` request containing a keyword | 400 `application/json` (not SSE); no backend call |
| 31 | `prefilter_rejects_big_request_without_save_or_meta` | 600-word prompt containing a keyword | 400; no save, no restore, no meta file |
| 32 | `prefilter_rejects_before_coalescing` | coalescing on; 2 concurrent same-key blocked requests | both 400 immediately; no group forms; no backend call |
| 33 | `custom_prefilter_adapter_accepts_and_rejects` | custom `Prefilter` trait impl (rejects >5-word prompts with 413) | short prompt 200 (1 backend call); long prompt 413 with the adapter's own status/message |
| 34 | `restore_works_after_prune_and_pruned_key_falls_back` | `META_MAX=2`; 3 saves (prunes A); then B+tail; then A again | B (a surviving entry) is still restored after the prune (ratio 5/6); pruned A gets **no** restore (full prefill), still 200, and is re-saved; bound stays at 2 |
| 35 | `restore_touches_lru_timestamp_so_prune_keeps_hot_entry` | `META_MAX=2`; saves A, B, C (prunes A); then B+tail restores B and saves B' | the successful restore bumps B's LRU timestamp → B's own post-save prune evicts C (the true oldest), not the just-restored B |
| 36 | `restore_rejected_cleans_up_stale_meta` | save A (KV created); delete A's KV; mock restore → 400; A+tail; A+tail again; delete KV; A+tail2 | 400 with the KV absent from every slot-save dir → stale meta removed (`stale_meta_removed`); 400 with the KV still present → meta **kept** (may be transient); final request saves a fresh entry |
| 37 | `restore_rejected_keeps_meta_when_save_dir_unvisible` | configured slot-save dir does not exist on the proxy's host; mock restore → 400 | absence of the KV file can't be verified (remote backend) → meta kept (status quo) |

## 8. Backend client — HTTP behavior — `tests/client.rs` (15)

Direct tests of `LlamaClient` against `MockLlama`; the two auth-header
tests capture the raw request on a one-shot TCP server.

| # | Test case | What it verifies | Expected result |
|---|-----------|------------------|-----------------|
| 1 | `get_model_id_from_backend` | GET `/v1/models` | `"mock-model"` |
| 2 | `get_model_id_unknown_when_backend_down` | connection refused | `"unknown"` |
| 3 | `api_key_sends_authorization_header` | API key set (raw-TCP capture) | `Authorization: Bearer 777` header present |
| 4 | `no_api_key_sends_no_authorization_header` | no API key | no `Authorization` header |
| 5 | `chat_json_object_passthrough_and_slot_pinning` | JSON chat with slot 0 | response object parsed (`chat.completion`); pin in body root/options/query |
| 6 | `chat_json_no_pinning_when_slot_none` | JSON chat without a slot | no pin fields in body; empty query |
| 7 | `chat_json_non_json_content_type` | 200 `text/plain` | `NonJson { "provider returned non-JSON", raw }` |
| 8 | `chat_json_malformed_json_body` | 200 `application/json` body `"{broken"` | `NonJson { "invalid json from provider", raw }` |
| 9 | `chat_json_non_object_json` | 200 JSON array body | `NonObject` |
| 10 | `chat_json_http_error` | 503 | `Err(HttpStatus { 503, "mock chat failure" })` |
| 11 | `chat_stream_chunks` | 200 SSE stream | chunks contain Hello, world, `[DONE]` |
| 12 | `chat_stream_error_status` | 500 stream | `StreamChat { status: 500 }` + error body passes through |
| 13 | `save_slot_statuses` | `POST /slot/{id}/save` | 200 → `true`, 500 → `false`, 404 → `Err(HttpStatus)`; calls recorded in order |
| 14 | `restore_slot_statuses` | `POST /slot/{id}/restore` | 200 → `Restored`, 400 → `Rejected`, 500 → `Failed` (no error); calls recorded |
| 15 | `base_url_trailing_slash_normalized` | backend URL with a trailing `/` | works — model id resolved |

## Coverage notes

**Layering.** Pure functions (hashing) → components (config, coalescing,
client, slot manager) → end-to-end flows (api). Unit tests use in-memory
fakes (`TestClient`); integration tests run the real proxy router /
`reqwest` client stack against `MockLlama`, with *real TCP* tests for
client-disconnect behaviour.

**Rust deviations from the Python original — each has dedicated tests:**

| Behavior | Tests |
|----------|-------|
| Backend circuit breaker (escalating 5→60 s cooldown) | `report_failure_escalates_backoff`, `report_success_resets_cooldown_and_streak`, `failed_backend_is_skipped_while_others_available`, `all_backends_down_fails_open`, `backend_cooldown_expires` |
| Probe-based recovery of cooled backends | `probe_down_backends_recovers_up_backend` (unit), `probe_recovers_cooled_down_backend` (E2E) |
| Transparent retry on another backend | `acquire_excluding_picks_other_backend` (unit), `connection_failure_retries_on_other_backend` + `dead_backend_traffic_fails_over_to_live_backend` (E2E) |
| Failed requests mark the slot used | `failing_backend_slots_do_not_pin_all_traffic` (E2E; the selection effect is also asserted by the `save_after_*` unit tests) |
| Idle-aware slot selection | `pick_skips_held_slot_in_favor_of_idle` |
| Bounded cache pruning (`META_MAX`) | `prune_meta_*` (unit), `prune_removes_oldest_meta_and_kv_files` (E2E) |
| Concurrent-request coalescing (Rust-only feature) | 4 unit + 6 E2E coalesce tests |
| Request prefilter adapter (Rust-only feature) | 9 unit + 8 E2E prefilter tests |
| Upstream abort on client cancel/disconnect | `nonstream_client_cancel_aborts_upstream`, `stream_client_cancel_aborts_upstream_over_tcp` |

**Suite health (2026-09-02).** 129/129 passing, clippy clean, ~4 s wall
clock. Timing-sensitive tests use explicit delays with bounded 5 s waits.
On 2026-08-31 the configurable `STREAM_QUEUE_SIZE` (env var +
`--stream-queue-size` flag) added three cases: `stream_queue_size_env_and_cli`
(config) and `stream_queue_size_one_delivers_full_stream` /
`coalesce_stream_queue_size_one_follower_receives_full_stream` (E2E,
both with `STREAM_QUEUE_SIZE=1` for maximal channel backpressure).
The 2026-08-31 prefilter adapter feature added 18 tests (+10 unit, +8 E2E)
to the 2026-08-26 baseline of 104.
De-duplicated from 109 tests on 2026-08-26: removed
`stream_client_disconnect_aborts_upstream` (merged into the TCP variant),
`failed_slot_mark_used_is_skipped_by_free_first` (subsumed by three other
slot tests + the E2E test above) and
`cli_merged_env_overrides_base_only_for_set_flags` (merged into
`config_precedence_cli_over_env_over_defaults`); fixed
`find_best_prefers_highest_ratio` to exercise multi-candidate selection
(threshold 0.6 → 0.4). Also removed the two Python-parity E2E tests
(`nonstream_non_json_provider_returns_error_payload`,
`nonstream_non_json_provider_big_request_still_saves`) as part of the
Python-proxy cleanup; the client-level `chat_json_non_json_content_type`
still covers the non-JSON provider variant.