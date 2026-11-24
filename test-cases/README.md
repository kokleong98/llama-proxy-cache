# lpcache — Regression Test Cases

Complete catalog of the **lpcache** regression suite (KV-cache-aware,
OpenAI-compatible proxy for llama.cpp). Every entry maps 1:1 to a test
function; the full suite runs with `cargo test --workspace`. The catalog
is split across the module pages below — one per suite. Each module page
documents every test with its exact runnable filter, sync/async kind and
source line, plus the module's fixtures/helpers, how to run just that
module, and a coverage map of the surface it exercises.

**Total: 317 test cases** — 116 lpcache unit + 113 lpcache-plugins
unit + 73 API integration + 15 client integration.

| # | Suite | Location | Tests | Kinds |
|---|-------|----------|-------|-------|
| 1 | [Configuration (env vars, CLI, precedence, `PLUGINS` specs)](01-configuration.md) | `src/config.rs` | 44 | 44 sync |
| 2 | [Prefilter plugins (keyword, chain)](02-prefilter-plugins.md) | `plugins/src/builtin/keyword.rs`, `chain.rs` | 14 | 10 async + 4 sync |
| 3 | [Hashing, meta files, pruning](03-hashing.md) | `src/hashing.rs` | 24 | 24 sync |
| 4 | [Request coalescing (`SingleFlight`)](04-coalescing.md) | `src/coalesce.rs` | 5 | 3 sync + 2 async |
| 5 | [llama backend client plugin](05-llama-backend-plugin.md) | `plugins/src/builtin/llama.rs` | 5 | 4 sync + 1 async |
| 6 | [Slot management (selection, locks, breaker, drain)](06-slot-management.md) | `src/slot_manager.rs` | 36 | 36 async |
| 7 | [API end-to-end (proxy ↔ mock llama.cpp)](07-api-e2e.md) | `tests/api.rs` | 77 | 77 async |
| 8 | [Backend client — HTTP behavior](08-backend-client.md) | `tests/client.rs` | 15 | 15 async |
| 9 | [Result cache plugin](09-result-cache-plugin.md) | `plugins/src/builtin/result_cache.rs` | 23 | 12 sync + 11 async |
| 10 | [Plugin registry (specs, loading, chaining)](10-plugin-registry.md) | `plugins/src/registry.rs` | 10 | 10 sync |
| 11 | [Request log plugin (W3C-XLF format, rotation)](11-request-log-plugin.md) | `plugins/src/builtin/request_log.rs` | 14 | 14 sync |
| 12 | [Qwen guard prefilter (Qwen3Guard-Gen safety check)](12-qwen-guard-plugin.md) | `plugins/src/builtin/qwen_guard.rs` | 17 | 8 sync + 9 async |

The module pages catalog 284 of the 358 tests; the remaining 74 (the
22 `dashboard` + 8 `payload_log` plugin unit tests, the 7 `procsample`
and 6 `mock_backend` unit tests, and 1 registry unit test — none with a
dedicated module page — plus the 29 `lpcache-config` configurator unit
tests and 1 doctest) run in the same suites and pass under
`cargo test --workspace`.

## How to run

```sh
cargo test --workspace        # full regression suite (359 tests incl. 1 doctest, ~4 s)
cargo test --lib              # 123 lpcache unit tests only
cargo test -p lpcache-plugins # 114 plugin library unit tests
cargo test --test api         # 77 API integration tests
cargo test --test client      # 15 client integration tests
cargo test -p lpcache-config  # 29 configurator unit tests
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
| Startup corrupt-meta scan (`STARTUP_CLEANUP_KV`) | `clean_corrupt_meta_*` (unit), `startup_scan_*` (E2E) |
| Concurrent-request coalescing (Rust-only feature) | 5 unit + 6 E2E coalesce tests |
| Prefilter plugins (Rust-only feature) | 14 plugin unit tests (keyword 11, chain 3) + 8 E2E prefilter tests |
| Result cache plugin (Rust-only feature) | 20 plugin unit + 8 E2E tests (7 result-cache, incl. `cache_streaming` and `no-cache` header bypass + 1 blocklist-over-cache chain) |
| Plugin system (`PLUGINS` env/flag, `lpcache-plugins` registry) | 6 `PLUGINS` config tests + 10 registry tests + 8 plugin `factory_*` tests; E2E state is built via `load_prefilters` / `load_request_loggers` |
| Per-backend `kv_cache` flag (Rust-only feature) | `backends_kv_cache_flag` (config), `acquire_excluding_respects_kv_flag` / `restore_prefers_kv_enabled_backend` / `restore_skipped_when_no_kv_enabled_backend` (unit), `kv_cache_disabled_backend_never_saves_or_restores` / `kv_cache_disabled_backend_skips_stream_save` / `kv_restore_prefers_kv_enabled_backend` (E2E) |
| Skip redundant restores — per-slot last-KV tracking (Rust-only feature) | `restore_skipped_when_slot_already_running_key` / `save_updates_last_kv_tracking` / `failed_restore_forgets_last_kv_tracking` (unit) + the E2E assertions in `dead_backend_traffic_fails_over_to_live_backend`, `kv_restore_prefers_kv_enabled_backend`, `restore_works_after_prune_and_pruned_key_falls_back`, `restore_touches_lru_timestamp_so_prune_keeps_hot_entry` and `restore_rejected_cleans_up_stale_meta` |
| Request log (`REQUEST_LOG*` env/flags or a `request_log` spec in `PLUGINS`, Rust-only feature) | 3 config tests + 14 plugin unit tests (section 11) + 5 E2E tests (`request_log_writes_xlf_record` — incl. the performance fields, `request_log_stream_request_counts_delivered_bytes`, `request_log_logs_failed_request_without_pipeline`, `request_log_rotates_and_bounds_keep`, `request_log_stream_writes_perf_fields`) |
| Dashboard plugin (`dashboard` spec in `PLUGINS`, Rust-only feature) | 22 plugin unit tests (counters — incl. the process resource metrics (CPU ms / disk bytes / peak RAM), the `?backend` filter (index scopes + the `cached`, `coalesced` and `coalesced_groups` scopes), per-scope recent rings, the series percentiles (per-request and overall p50/p95, incl. the coarsening merge), `?since` range scoping, bucket pruning/coarsening, chart rendering, the backends table's drain flag, self-contained HTML) + 7 E2E tests |
| KV cache save auto-retry (`SAVE_RETRIES` / `SAVE_RETRY_DELAY_MS`, Rust-only feature) | `save_after_retries_until_success`, `save_after_retries_exhausted_propagates_error`, `save_after_retries_exhausted_500_returns_false`, `save_after_no_retry_when_retries_disabled` (unit), `save_retry_env_and_cli` (config) |
| Upstream abort on client cancel/disconnect | `nonstream_client_cancel_aborts_upstream`, `stream_client_cancel_aborts_upstream_over_tcp` |
**Suite health (2026-11-24).** 359/359 passing, clippy clean, ~4 s wall
clock. Timing-sensitive tests use explicit delays with bounded 5 s waits.
On 2026-11-24 the runtime backend drain control landed (+11 cases):
`SlotManager` gained the operator drain state (a drained backend is
excluded from slot selection *unconditionally* — the all-down fail-open
path never routes into it, in-flight requests finish normally, and only
an explicit resume re-joins it, via one inline `GET /v1/models` probe
that falls back to the 1 s background probe loop while the backend is
unhealthy). New cases: 6 `slot_manager` unit tests (drained skipping on
free-first/LRU, drain never fail-opens, all-drained acquire fails fast,
success/failure no-ops on a drained backend, probe rounds skipping
drained backends, resume re-joins when healthy / waits for the probe
when unhealthy) and 4 E2E tests (`backend_drain_resume_keeps_in_flight_and_reroutes`
— in-flight request completes on the drained backend while new traffic
reroutes, resume restores it; `drain_all_backends_503s_fast_then_resume_recovers`
— fast 503 with every backend drained; `resume_of_down_backend_stays_out_until_the_probe_sees_it`;
`control_endpoints_reject_bad_index_and_resume_is_idempotent`), plus 1
`dashboard` unit test (the stats payload carries the per-backend
`drained` flag the Backends tab renders as a `DRAINED` badge + Drain /
Resume button). The mock backend gained a `set_models_down` hook (its
`GET /v1/models` answers 500). The endpoints `POST
/_control/backends/<i>/drain|resume` are always mounted (no config),
404 on an out-of-range index, and are excluded from the request log and
dashboard aggregates (operator traffic, not proxied traffic).
On 2026-11-24 the `SLOT_ROUTING_POLICY` default flipped from `lru` to
`key_affinity` (no new cases): `RoutingPolicy::default()`, the invalid-value
fallback (and its warning), the `--help` text, the configurator template /
wizard / UI defaults and the `PARAMETERS.md` / `README.md` tables now say
`key_affinity`; `slot_routing_policy_env_and_cli` asserts the new default
(unset and invalid → `key_affinity`). Seven E2E cases whose scenarios
depend on the free-first/LRU pick and its restore wire calls were pinned
to an explicit `RoutingPolicy::Lru` (a new `make_app_lru` builder; the
`make_state_request_log` / `make_state_dashboard` builders gained a
`policy` param; `make_state_with_save_dir` and two inline `SlotManager`
constructions), so both policies stay covered exactly as before.
On 2026-11-19 the dashboard charts gained the percentile overlays, the
toggleable legend, the `?backend=` deep link and the window totals
(+10 cases): each throughput/resource bar overlays its per-request
p50/p95 ticks, the charts draw two full-width line pairs (the
window-wide per-request p50/p95 and the p50/p95 of the plotted
per-bucket aggregates — "bars"), a two-row legend on each chart (per-request
p50/p95 on row 1, drawn-bars p50/p95 on row 2) toggles its lines (hidden
lines are dropped from the scale), the page
URL accepts the same `?backend=` values as the filter, and the additive
resource charts (cpu ms, disk read, disk write) draw the window total
next to the max (kept on hover redraws). The recent-requests table's
backend column became a composite `backend / slot` — the backend index
and slot the request actually ran on (`-` where no slot was acquired:
prefilter rejections, coalesced followers, non-chat routes). New cases:
7 `dashboard` unit
tests (`pct_hist_quantiles_are_bounded_and_merge`,
`point_agg_percentiles_nearest_rank`,
`series_buckets_track_per_request_percentiles`,
`series_overall_percentiles_cover_the_window`,
`series_scoped_percentiles_track_each_backend`,
`series_coarsening_merges_percentiles`,
`backend_filter_coalesced_scopes_select_group_records`) and E2E
`dashboard_coalesced_scopes_track_group_records` (`?backend=coalesced`
= the followers only, `?backend=coalesced_groups` = the whole group);
`coalesce::tests::early_finish_does_not_lose_the_wakeup` (the wakeup
race when `finish` lands before the waiter registers) and
`small_requests_move_on_to_the_next_free_slot` (a successful small
request marks its slot used, so the free tier moves on instead of
pinning all traffic to slot 0).
On 2026-11-13 the `SLOT_ROUTING_POLICY` setting (`lru` default,
`key_affinity` opt-in) made the slot pick key-aware (+5 cases): under
`key_affinity`, a request that carries a KV restore key is picked on an
**idle** slot already running that key (a new key-free tier before the
free/LRU tiers), turning the existing post-pick restore skip into a
zero-wire-call no-op; the default `lru` policy is byte-identical to the
previous behaviour. The pick core is `pick_filtered(extra, key_free)`
returning the winning tier (`PickReason::KeyFree` / `Free` / `Lru`).
Cases: `slot_routing_policy_env_and_cli` (config: parse,
case-insensitivity, the `key-affinity` alias, invalid → default with a
warning, CLI > env), `key_affinity_picks_idle_slot_running_key` /
`lru_policy_ignores_key_on_pick` /
`key_affinity_without_key_matches_lru_pick` (slot manager: the
key-affinity pick prefers the idle slot running the key and issues no
redundant restore; `lru` picks the oldest slot and issues the restore;
keyless acquires are unaffected) and
`key_affinity_avoids_redundant_restore` (E2E: A, B, A on 3 slots — 1
restore wire call under `lru`, 0 under `key_affinity`). The dashboard's
`proxy` status now exposes the active policy
(`slot_routing_policy`).
On 2026-11-13 the slot manager gained per-slot last-KV tracking
(+3 cases): it records the last KV cache key each slot is known to be
running (set by a successful restore or save, cleared by a failed
restore) and skips the backend restore call when the picked slot already
runs the requested key (`restore_skipped ... reason=already_running` —
the acquire still counts as restored, so the meta LRU touch and the
request-log flag still apply); llama.cpp keeps a slot's KV across
requests, so the restore would be redundant. Unit tests:
`restore_skipped_when_slot_already_running_key`,
`save_updates_last_kv_tracking`, `failed_restore_forgets_last_kv_tracking`.
Five E2E assertions updated where slots already ran the key:
`dead_backend_traffic_fails_over_to_live_backend` (live-backend restores
now 0), `kv_restore_prefers_kv_enabled_backend` (be0 now has 2 slots so
the restore lands on a fresh slot), `restore_works_after_prune_and_pruned_key_falls_back`
(gained an exact-B step that forces a real restore on the other slot),
`restore_touches_lru_timestamp_so_prune_keeps_hot_entry` (the B+tail
restore is skipped; the LRU touch still fires) and
`restore_rejected_cleans_up_stale_meta` (the final step first rotates
both slots off key2 with two unrelated big prompts, since their saves
would otherwise skip the rejected restore the cleanup needs).
On 2026-11-05 the result cache gained two params and the backends a
per-backend `kv_cache` flag (+15 cases).
`cache_streaming` (aggregate a completed streaming result into a
non-streaming completion and cache it; serve cached entries to streaming
clients as SSE): plugin unit `factory_cache_streaming_param` /
`prefilter_serves_streaming_when_enabled` and E2E
`result_cache_caches_streaming_result_and_serves_streaming_client`.
`cache_control` (a `Cache-Control: no-cache` request bypasses the cache):
plugin unit `wants_no_cache_parses_directives` /
`cache_control_no_cache_bypasses_the_cache` /
`cache_control_disabled_ignores_the_header` /
`factory_cache_control_param_bypasses` and E2E
`result_cache_no_cache_header_bypasses_the_cache`. The same day the
per-backend `kv_cache` flag (a backend without slot save/restore support
is skipped by KV save/restore): config `backends_kv_cache_flag`, slot
unit `restore_prefers_kv_enabled_backend` /
`restore_skipped_when_no_kv_enabled_backend` /
`acquire_excluding_respects_kv_flag`, and E2E
`kv_cache_disabled_backend_never_saves_or_restores` /
`kv_cache_disabled_backend_skips_stream_save` /
`kv_restore_prefers_kv_enabled_backend`.
On 2026-11-11 the request log gained 5 cross-platform resource XLF
fields (`lpcache-cpu-ms`, `lpcache-disk-read-bytes`,
`lpcache-disk-write-bytes` — the proxy process's own CPU time and disk
I/O during the request, from process-wide counters sampled at request
start and completion — plus `lpcache-ram-bytes`,
`lpcache-peak-ram-bytes`, its resident memory and peak resident memory
when the request completed; the CPU/disk values are per-request deltas,
the memory values point-in-time gauges; the `src/procsample` module
uses `CLOCK_PROCESS_CPUTIME_ID` on Unix, `/proc/self/io` and
`/proc/self/status` (`VmRSS`/`VmHWM`) on Linux, `proc_pid_rusage`
(`rusage_info_v4`) on macOS and `GetProcessTimes`/
`GetProcessIoCounters`/`GetProcessMemoryInfo` on Windows): 6
`procsample` unit tests, 1 plugin unit test
(`resource_fields_render_dash_when_sampling_unavailable`);
`request_log_writes_xlf_record` now asserts the resource fields too.
On 2026-11-02 the Qwen3Guard-Gen safety prefilter (built-in
`qwen_guard` plugin, selected via `PLUGINS`; guard call per request key
with a bounded verdict LRU, fail-open/fail-closed) added 17 plugin unit
tests (new section 12); this feature is covered by unit tests only.
On 2026-10-22 the dashboard feature (built-in `dashboard` plugin: an
HTML status page + `/api/stats` JSON, served under a configurable path)
added 8 plugin unit tests (`plugins/src/builtin/dashboard.rs`), the
registry's `load_dashboard` + `load_dashboard_zero_one_many`, and 6 E2E
tests; the dashboard's own routes are excluded from the request log and
the aggregates so polling never inflates the statistics.
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

