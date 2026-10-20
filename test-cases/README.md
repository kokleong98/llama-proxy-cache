# lpcache — Regression Test Cases

Complete catalog of the **lpcache** regression suite (KV-cache-aware,
OpenAI-compatible proxy for llama.cpp). Every entry maps 1:1 to a test
function; the full suite runs with `cargo test --workspace`. The catalog
is split across the module pages below — one per suite. Each module page
documents every test with its exact runnable filter, sync/async kind and
source line, plus the module's fixtures/helpers, how to run just that
module, and a coverage map of the surface it exercises.

**Total: 197 test cases** — 80 lpcache unit + 55 lpcache-plugins
unit + 47 API integration + 15 client integration.

| # | Suite | Location | Tests | Kinds |
|---|-------|----------|-------|-------|
| 1 | [Configuration (env vars, CLI, precedence, `PLUGINS` specs)](01-configuration.md) | `src/config.rs` | 36 | 36 sync |
| 2 | [Prefilter plugins (keyword, chain)](02-prefilter-plugins.md) | `plugins/src/builtin/keyword.rs`, `chain.rs` | 14 | 10 async + 4 sync |
| 3 | [Hashing, meta files, pruning](03-hashing.md) | `src/hashing.rs` | 19 | 19 sync |
| 4 | [Request coalescing (`SingleFlight`)](04-coalescing.md) | `src/coalesce.rs` | 4 | 3 sync + 1 async |
| 5 | [llama backend client plugin](05-llama-backend-plugin.md) | `plugins/src/builtin/llama.rs` | 5 | 4 sync + 1 async |
| 6 | [Slot management (selection, locks, breaker)](06-slot-management.md) | `src/slot_manager.rs` | 21 | 21 async |
| 7 | [API end-to-end (proxy ↔ mock llama.cpp)](07-api-e2e.md) | `tests/api.rs` | 47 | 47 async |
| 8 | [Backend client — HTTP behavior](08-backend-client.md) | `tests/client.rs` | 15 | 15 async |
| 9 | [Result cache plugin](09-result-cache-plugin.md) | `plugins/src/builtin/result_cache.rs` | 14 | 7 sync + 7 async |
| 10 | [Plugin registry (specs, loading, chaining)](10-plugin-registry.md) | `plugins/src/registry.rs` | 9 | 9 sync |
| 11 | [Request log plugin (W3C-XLF format, rotation)](11-request-log-plugin.md) | `plugins/src/builtin/request_log.rs` | 13 | 13 sync |

## How to run

```sh
cd lpcache
cargo test --workspace        # full regression suite (197 tests + 3 doctests, ~4 s)
cargo test --lib              # 80 lpcache unit tests only
cargo test -p lpcache-plugins # 55 plugin library unit tests
cargo test --test api         # 47 API integration tests
cargo test --test client      # 15 client integration tests
cargo test <name-substring>   # run a single test by name filter
cargo clippy --all-targets    # lint gate (expected: 0 warnings)
```

Each module page also lists the command that runs just that module's
tests (e.g. `cargo test --lib config::tests`).

Integration tests start `MockLlama` — an in-process mock of the llama.cpp
server (`src/mock_backend.rs`) on free localhost ports — so the suite needs
no real `llama-server`, model files, or network access.

## Coverage notes

**Layering.** Pure functions (hashing) → components (config, coalescing,
client, slot manager) → end-to-end flows (api). Unit tests use in-memory
fakes (`TestClient`); integration tests run the real proxy router /
`reqwest` client stack against `MockLlama`, with *real TCP* tests for
client-disconnect behaviour.

**Rust deviations from the Python original — each has dedicated tests:**

| Behavior | Tests |
|----------|-------|
| Backend circuit breaker (escalating 5→60 s cooldown) | `report_failure_escalates_backoff`, `report_success_resets_cooldown_and_streak` |
| Probe-based recovery of cooled backends | `probe_down_backends_recovers_up_backend` (unit), `probe_recovers_cooled_down_backend` (E2E) |
| Transparent retry on another backend | `acquire_excluding_picks_other_backend` (unit), `connection_failure_retries_on_other_backend` + `dead_backend_traffic_fails_over_to_live_backend` (E2E) |
| Failed requests mark the slot used | `failing_backend_slots_do_not_pin_all_traffic` (E2E; the selection effect is also asserted by the `save_after_*` unit tests) |
| Idle-aware slot selection | `pick_skips_held_slot_in_favor_of_idle` |
| Bounded cache pruning (`META_MAX`) | `prune_meta_*` (unit), `prune_removes_oldest_meta_and_kv_files` (E2E) |
| Concurrent-request coalescing (Rust-only feature) | 4 unit + 6 E2E coalesce tests |
| Prefilter plugins (Rust-only feature) | 14 plugin unit tests (keyword 11, chain 3) + 8 E2E prefilter tests |
| Result cache plugin (Rust-only feature) | 14 plugin unit + 5 E2E tests (4 result-cache + 1 blocklist-over-cache chain) |
| Plugin system (`PLUGINS` env/flag, `lpcache-plugins` registry) | 5 `PLUGINS` config tests + 9 registry tests + 8 plugin `factory_*` tests; E2E state is built via `load_prefilters` / `load_request_loggers` |
| Request log (`REQUEST_LOG*` env/flags or a `request_log` spec in `PLUGINS`, Rust-only feature) | 3 config tests + 13 plugin unit tests (section 11) + 5 E2E tests (`request_log_writes_xlf_record` — incl. the performance fields, `request_log_stream_request_counts_delivered_bytes`, `request_log_logs_failed_request_without_pipeline`, `request_log_rotates_and_bounds_keep`, `request_log_stream_writes_perf_fields`) |
| KV cache save auto-retry (`SAVE_RETRIES` / `SAVE_RETRY_DELAY_MS`, Rust-only feature) | `save_after_retries_until_success`, `save_after_retries_exhausted_propagates_error`, `save_after_retries_exhausted_500_returns_false`, `save_after_no_retry_when_retries_disabled` (unit), `save_retry_env_and_cli` (config) |
| Upstream abort on client cancel/disconnect | `nonstream_client_cancel_aborts_upstream`, `stream_client_cancel_aborts_upstream_over_tcp` |
**Suite health (2026-10-19).** 197/197 passing, clippy clean, ~4 s wall
clock. Timing-sensitive tests use explicit delays with bounded 5 s waits.
On 2026-10-19 the request log gained 8 performance XLF fields
(`lpcache-prompt-tokens`, `lpcache-completion-tokens`,
`lpcache-cached-tokens`, `lpcache-prefill-ms`, `lpcache-gen-ms`,
`lpcache-prefill-tps`, `lpcache-gen-tps`, `lpcache-ttft-ms` — the
backend's `usage`/`timings` plus the proxy-side time to first token): 1
plugin unit test (`perf_fields_render_dash_when_backend_reports_none`)
and 1 E2E test (`request_log_stream_writes_perf_fields`);
`request_log_writes_xlf_record` now asserts the non-streaming perf
fields too. The mock backend reports realistic `usage`/`timings` in its
completions (its SSE stream gained a final chunk carrying them, so its
streams have 4 chunks). On 2026-10-19 the configurable request log
(the built-in `request_log`
plugin: `REQUEST_LOG*` env vars + `--request-log*` flags, or a
`request_log` spec in `PLUGINS`, which takes precedence) added 20 cases:
3 config tests (`request_log_env_settings`,
`request_log_spec_synthesized_when_enabled`,
`request_log_spec_in_plugins_wins`), 1 registry loader test
(`loaders_skip_specs_of_other_adapter_kinds`), 12 `request_log` plugin
unit tests (new section 11) and 4 E2E tests
(`request_log_writes_xlf_record`,
`request_log_stream_request_counts_delivered_bytes`,
`request_log_logs_failed_request_without_pipeline`,
`request_log_rotates_and_bounds_keep`). The E2E request-log tests build
state the same way `main.rs` does (`Config::with_request_log` →
`load_request_loggers` → `prepare`); because they drive the router with
`tower::Service::oneshot`, `c-ip` is `-` (no `ConnectInfo`) and
in-process requests set an explicit `Content-Length` for `cs-bytes`.
On 2026-10-16 the adapter modules (`src/prefilter.rs`,
`src/result_cache.rs`, `src/llama_client.rs`) moved into the
`lpcache-plugins` workspace library behind the `Prefilter` /
`ResultStore` / `LlamaBackend` traits and the plugin registry; `PLUGINS`
(env var + `--plugins` flag) selects plugins by name, and the legacy
`PREFILTER_*` variables are translated into equivalent plugin specs when
`PLUGINS` is unset. All previous tests were kept (the 24 adapter unit
tests moved to the plugin crate; the 42 API tests now build state via
`load_prefilters`), and 22 unit cases were added (5 `PLUGINS` config
tests, 8 registry tests, 9 plugin factory tests).
On 2026-09-02 the suite stood at 129/129 passing.
On 2026-08-31 the configurable `STREAM_QUEUE_SIZE` (env var +
`--stream-queue-size` flag) added three cases: `stream_queue_size_env_and_cli`
(config) and `stream_queue_size_one_delivers_full_stream` /
`coalesce_stream_queue_size_one_follower_receives_full_stream` (E2E,
both with `STREAM_QUEUE_SIZE=1` for maximal channel backpressure).
On 2026-09-17 the KV cache save auto-retry (configurable
`SAVE_RETRIES` + `SAVE_RETRY_DELAY_MS`, env vars + CLI flags) added five
cases: `save_retry_env_and_cli` (config) and four `save_after_*` unit
tests (153 tests total across all sections, clippy clean). The slot
manager unit tests use a 1 ms retry delay so retry-driven tests stay fast
while exercising the production-default retry count.
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

