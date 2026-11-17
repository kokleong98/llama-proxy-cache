# 6. Slot management — `src/slot_manager.rs` (30)

[← Suite index](README.md)

Part of the [lpcache regression test cases](README.md): every entry maps
1:1 to a test function; the full suite (290 tests) runs with
`cargo test --workspace`.

In-memory `TestClient` backends; verifies selection (incl. the
`SLOT_ROUTING_POLICY` routing policy), per-slot locking,
save/restore semantics, per-slot last-KV tracking (a restore is skipped
when the slot already runs the requested key), the backend circuit breaker
and the per-backend `kv_cache` flag.

## Module overview

- **Target:** `lpcache` library — the `mod tests` at the bottom of [`src/slot_manager.rs`](../src/slot_manager.rs); 30 tests, all async (`#[tokio::test]`).
- **Fixtures & helpers:**
  - `TestClient` — an in-memory `LlamaBackend` that records every `save_slot`/`restore_slot` call and is steered with `save_mode` (Ok(true) / Ok(false) / Err), `save_fail_first_n` (N transient failures) and `restore_ok`.
  - `make(n_backends, n_slots)` builds a `SlotManager` over `TestClient`s with the production-default retry count but a 1 ms retry delay, so retry-driven tests stay fast; `make_kv(n_backends, n_slots, kv_disabled)` is the same with per-backend `kv_cache` flags; `make_policy(n_backends, n_slots, policy)` is the same with an explicit `SLOT_ROUTING_POLICY`.
- **Surface under test:** `SlotManager::acquire` (free-first → LRU ordering, per-slot `Semaphore(1)` locking, acquire timeout, restore gating incl. the per-backend `kv_cache` flag, idle-aware pick, the `SLOT_ROUTING_POLICY` routing policy (`lru` vs `key_affinity` — the latter adds a key-free tier: an idle slot already running the requested key is picked before the free/LRU tiers), circuit-breaker skipping, fail-open), per-slot last-KV tracking (restore skipped when the slot already runs the key; tracking updated by successful restores/saves and cleared by failed restores), `save_after` (mark-used semantics incl. 500 vs other errors, auto-retry), `report_failure`/`report_success` (escalating backoff), `probe_down_backends`, `acquire_excluding`.

## How to run

```sh
cargo test --lib slot_manager::tests                                       # all 30 slot manager tests
cargo test --lib slot_manager::tests::free_slot_picked_first_in_order      # one test, exact path
```

The **Test case** column is the exact filter for that test: pass it
into `cargo test` as shown above.

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 1 | `slot_manager::tests::free_slot_picked_first_in_order` | async | [L654](../src/slot_manager.rs#L654) | fresh pool, repeated acquire | never-used slot 0 picked both times (release does not mark used) |
| 2 | `slot_manager::tests::used_slots_are_skipped_then_oldest_wins` | async | [L666](../src/slot_manager.rs#L666) | free-first, then LRU | free slot 2 beats used 0/1; all used → oldest (slot 0) |
| 3 | `slot_manager::tests::multi_backend_slot_ordering` | async | [L683](../src/slot_manager.rs#L683) | free-first across backends | backend 0 all used → backend 1 slot 0 |
| 4 | `slot_manager::tests::restore_called_only_when_key_given` | async | [L693](../src/slot_manager.rs#L693) | restore gating in `acquire` | key → 1 restore call, `restored = Some(true)`; `None` → no call; backend fail → `Some(false)` |
| 5 | `slot_manager::tests::save_after_saves_and_marks_used` | async | [L771](../src/slot_manager.rs#L771) | save OK path | save recorded; slot marked used → next free pick is slot 1 |
| 6 | `slot_manager::tests::save_after_backend_500_returns_false_but_still_marks_used` | async | [L783](../src/slot_manager.rs#L783) | backend 500 on save | `Ok(false)`; slot still marked used |
| 7 | `slot_manager::tests::save_after_other_error_propagates_and_keeps_slot_free` | async | [L795](../src/slot_manager.rs#L795) | other save error | `Err` propagates; `last_used` untouched |
| 8 | `slot_manager::tests::second_acquire_waits_until_release` | async | [L883](../src/slot_manager.rs#L883) | per-slot `Semaphore(1)` guard | 2nd acquire blocks until 1st releases, then gets slot 0 |
| 9 | `slot_manager::tests::acquire_times_out_when_slot_held` | async | [L897](../src/slot_manager.rs#L897) | acquire timeout (100 ms in test) | `Err(AcquireTimeout)` |
| 10 | `slot_manager::tests::failed_backend_is_skipped_while_others_available` | async | [L916](../src/slot_manager.rs#L916) | circuit breaker in selection | cooling backend skipped by both free-first and LRU paths |
| 11 | `slot_manager::tests::all_backends_down_fails_open` | async | [L932](../src/slot_manager.rs#L932) | all backends cooling | still picks the first slot in order (fail-open) |
| 12 | `slot_manager::tests::pick_skips_held_slot_in_favor_of_idle` | async | [L943](../src/slot_manager.rs#L943) | idle-aware selection | held oldest slot skipped while an idle free slot exists; LRU prefers idle slots |
| 13 | `slot_manager::tests::backend_cooldown_expires` | async | [L961](../src/slot_manager.rs#L961) | cooldown expiry (50 ms) | backend eligible again after the cooldown |
| 14 | `slot_manager::tests::report_failure_escalates_backoff` | async | [L1006](../src/slot_manager.rs#L1006) | escalating backoff ladder | 5 → 10 → 20 → 40 s, capped at 60 s; streak 6 |
| 15 | `slot_manager::tests::report_success_resets_cooldown_and_streak` | async | [L1026](../src/slot_manager.rs#L1026) | reset on success | cooldown 0 + streak 0; next failure restarts at 5 s |
| 16 | `slot_manager::tests::probe_down_backends_recovers_up_backend` | async | [L1041](../src/slot_manager.rs#L1041) | probe recovery | one probe round recovers a live backend; healthy backends not counted |
| 17 | `slot_manager::tests::acquire_excluding_picks_other_backend` | async | [L1055](../src/slot_manager.rs#L1055) | retry on a different backend | exclude be0 → be1 slot 0; single backend → fast error (no 300 s wait) |
| 18 | `slot_manager::tests::save_after_retries_until_success` | async | [L808](../src/slot_manager.rs#L808) | save auto-retry (Rust-only feature) | 2 transient failures then success → `Ok(true)`, exactly 3 save calls, slot marked used |
| 19 | `slot_manager::tests::save_after_retries_exhausted_propagates_error` | async | [L832](../src/slot_manager.rs#L832) | always-failing save | `Err` after 1 initial + `SAVE_RETRIES` attempts; `last_used` untouched (slot stays free) |
| 20 | `slot_manager::tests::save_after_retries_exhausted_500_returns_false` | async | [L847](../src/slot_manager.rs#L847) | always-500 save | `Ok(false)` after 1 + `SAVE_RETRIES` attempts; slot still marked used |
| 21 | `slot_manager::tests::save_after_no_retry_when_retries_disabled` | async | [L861](../src/slot_manager.rs#L861) | `SAVE_RETRIES=0` | exactly one save attempt, `Err` propagated |
| 22 | `slot_manager::tests::acquire_excluding_respects_kv_flag` | async | [L747](../src/slot_manager.rs#L747) | `acquire_excluding` with a `kv_cache` flag | both kv-enabled: the excluded backend is skipped and the restore lands on the other; with the target kv-disabled: the exclusion still works but the restore is dropped (`restored = None`, no error) |
| 23 | `slot_manager::tests::restore_prefers_kv_enabled_backend` | async | [L718](../src/slot_manager.rs#L718) | backend 0 kv-disabled with "older" slots | the restore lands on the kv-enabled backend 1 even though backend 0's slots are older |
| 24 | `slot_manager::tests::restore_skipped_when_no_kv_enabled_backend` | async | [L736](../src/slot_manager.rs#L736) | all backends kv-disabled | a slot is still acquired (full prefill) but no restore is issued to any backend |
| 25 | `slot_manager::tests::restore_skipped_when_slot_already_running_key` | async | [L784](../src/slot_manager.rs#L784) | repeat acquire with the same key, then a different key | a successful restore records the key as running in the slot: a second restore of the same key is skipped (no backend call) yet reported `Restored`; a different key is still restored |
| 26 | `slot_manager::tests::save_updates_last_kv_tracking` | async | [L813](../src/slot_manager.rs#L813) | `save_after` then acquire with the same / a different key | a successful save records the key as running in the slot: the same-key restore is skipped, the different-key restore is issued |
| 27 | `slot_manager::tests::failed_restore_forgets_last_kv_tracking` | async | [L837](../src/slot_manager.rs#L837) | failed restore, then a retry with the same key | a failed restore clears the slot's tracking (KV state unknown): a later same-key restore is re-issued, not wrongly skipped |
| 28 | `slot_manager::tests::key_affinity_picks_idle_slot_running_key` | async | [L878](../src/slot_manager.rs#L878) | `key_affinity`; slot 0 runs "k" (newer), slot 1 runs "other" (older/LRU pick) | a repeat of "k" is picked on the idle slot running "k" (not the LRU slot), the post-pick skip counts as `restored = Some(Restored)`, and no redundant restore wire call is issued |
| 29 | `slot_manager::tests::lru_policy_ignores_key_on_pick` | async | [L897](../src/slot_manager.rs#L897) | default `lru`; same setup as 28 | the key never influences the pick: the oldest slot (running "other") is picked and the restore is actually issued there |
| 30 | `slot_manager::tests::key_affinity_without_key_matches_lru_pick` | async | [L915](../src/slot_manager.rs#L915) | `key_affinity`, acquire without a restore key | the policy has no effect: free-first ordering, no restore calls |

## Coverage map — surface → tests

| Surface | Tests |
|---|---|
| Slot selection ordering | `free_slot_picked_first_in_order`, `used_slots_are_skipped_then_oldest_wins`, `multi_backend_slot_ordering`, `pick_skips_held_slot_in_favor_of_idle` |
| Routing policy (`SLOT_ROUTING_POLICY`) | `key_affinity_picks_idle_slot_running_key`, `lru_policy_ignores_key_on_pick`, `key_affinity_without_key_matches_lru_pick` |
| Restore gating in `acquire` | `restore_called_only_when_key_given`, `restore_prefers_kv_enabled_backend`, `restore_skipped_when_no_kv_enabled_backend` |
| Per-slot last-KV tracking (skip redundant restores) | `restore_skipped_when_slot_already_running_key`, `save_updates_last_kv_tracking`, `failed_restore_forgets_last_kv_tracking` |
| `save_after` semantics | `save_after_saves_and_marks_used`, `save_after_backend_500_returns_false_but_still_marks_used`, `save_after_other_error_propagates_and_keeps_slot_free` |
| Save auto-retry | `save_after_retries_until_success`, `save_after_retries_exhausted_propagates_error`, `save_after_retries_exhausted_500_returns_false`, `save_after_no_retry_when_retries_disabled` |
| Per-slot locking + timeout | `second_acquire_waits_until_release`, `acquire_times_out_when_slot_held` |
| Circuit breaker | `failed_backend_is_skipped_while_others_available`, `all_backends_down_fails_open`, `backend_cooldown_expires`, `report_failure_escalates_backoff`, `report_success_resets_cooldown_and_streak` |
| Probe recovery | `probe_down_backends_recovers_up_backend` |
| Retry on another backend | `acquire_excluding_picks_other_backend`, `acquire_excluding_respects_kv_flag` |
