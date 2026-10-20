# 6. Slot management — `src/slot_manager.rs` (21)

[← Suite index](README.md)

Part of the [lpcache regression test cases](README.md): every entry maps
1:1 to a test function; the full suite (197 tests) runs with
`cargo test --workspace`.

In-memory `TestClient` backends; verifies selection, per-slot locking,
save/restore semantics and the backend circuit breaker.

## Module overview

- **Target:** `lpcache` library — the `mod tests` at the bottom of [`src/slot_manager.rs`](../src/slot_manager.rs); 21 tests, all async (`#[tokio::test]`).
- **Fixtures & helpers:**
  - `TestClient` — an in-memory `LlamaBackend` that records every `save_slot`/`restore_slot` call and is steered with `save_mode` (Ok(true) / Ok(false) / Err), `save_fail_first_n` (N transient failures) and `restore_ok`.
  - `make(n_backends, n_slots)` builds a `SlotManager` over `TestClient`s with the production-default retry count but a 1 ms retry delay, so retry-driven tests stay fast.
- **Surface under test:** `SlotManager::acquire` (free-first → LRU ordering, per-slot `Semaphore(1)` locking, acquire timeout, restore gating, idle-aware pick, circuit-breaker skipping, fail-open), `save_after` (mark-used semantics incl. 500 vs other errors, auto-retry), `report_failure`/`report_success` (escalating backoff), `probe_down_backends`, `acquire_excluding`.

## How to run

```sh
cd lpcache
cargo test --lib slot_manager::tests                                       # all 21 slot manager tests
cargo test --lib slot_manager::tests::free_slot_picked_first_in_order      # one test, exact path
```

The **Test case** column is the exact filter for that test: pass it
into `cargo test` as shown above.

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 1 | `slot_manager::tests::free_slot_picked_first_in_order` | async | [L556](../src/slot_manager.rs#L556) | fresh pool, repeated acquire | never-used slot 0 picked both times (release does not mark used) |
| 2 | `slot_manager::tests::used_slots_are_skipped_then_oldest_wins` | async | [L568](../src/slot_manager.rs#L568) | free-first, then LRU | free slot 2 beats used 0/1; all used → oldest (slot 0) |
| 3 | `slot_manager::tests::multi_backend_slot_ordering` | async | [L585](../src/slot_manager.rs#L585) | free-first across backends | backend 0 all used → backend 1 slot 0 |
| 4 | `slot_manager::tests::restore_called_only_when_key_given` | async | [L595](../src/slot_manager.rs#L595) | restore gating in `acquire` | key → 1 restore call, `restored = Some(true)`; `None` → no call; backend fail → `Some(false)` |
| 5 | `slot_manager::tests::save_after_saves_and_marks_used` | async | [L620](../src/slot_manager.rs#L620) | save OK path | save recorded; slot marked used → next free pick is slot 1 |
| 6 | `slot_manager::tests::save_after_backend_500_returns_false_but_still_marks_used` | async | [L632](../src/slot_manager.rs#L632) | backend 500 on save | `Ok(false)`; slot still marked used |
| 7 | `slot_manager::tests::save_after_other_error_propagates_and_keeps_slot_free` | async | [L644](../src/slot_manager.rs#L644) | other save error | `Err` propagates; `last_used` untouched |
| 8 | `slot_manager::tests::second_acquire_waits_until_release` | async | [L731](../src/slot_manager.rs#L731) | per-slot `Semaphore(1)` guard | 2nd acquire blocks until 1st releases, then gets slot 0 |
| 9 | `slot_manager::tests::acquire_times_out_when_slot_held` | async | [L745](../src/slot_manager.rs#L745) | acquire timeout (100 ms in test) | `Err(AcquireTimeout)` |
| 10 | `slot_manager::tests::failed_backend_is_skipped_while_others_available` | async | [L763](../src/slot_manager.rs#L763) | circuit breaker in selection | cooling backend skipped by both free-first and LRU paths |
| 11 | `slot_manager::tests::all_backends_down_fails_open` | async | [L779](../src/slot_manager.rs#L779) | all backends cooling | still picks the first slot in order (fail-open) |
| 12 | `slot_manager::tests::pick_skips_held_slot_in_favor_of_idle` | async | [L790](../src/slot_manager.rs#L790) | idle-aware selection | held oldest slot skipped while an idle free slot exists; LRU prefers idle slots |
| 13 | `slot_manager::tests::backend_cooldown_expires` | async | [L808](../src/slot_manager.rs#L808) | cooldown expiry (50 ms) | backend eligible again after the cooldown |
| 14 | `slot_manager::tests::report_failure_escalates_backoff` | async | [L851](../src/slot_manager.rs#L851) | escalating backoff ladder | 5 → 10 → 20 → 40 s, capped at 60 s; streak 6 |
| 15 | `slot_manager::tests::report_success_resets_cooldown_and_streak` | async | [L871](../src/slot_manager.rs#L871) | reset on success | cooldown 0 + streak 0; next failure restarts at 5 s |
| 16 | `slot_manager::tests::probe_down_backends_recovers_up_backend` | async | [L886](../src/slot_manager.rs#L886) | probe recovery | one probe round recovers a live backend; healthy backends not counted |
| 17 | `slot_manager::tests::acquire_excluding_picks_other_backend` | async | [L900](../src/slot_manager.rs#L900) | retry on a different backend | exclude be0 → be1 slot 0; single backend → fast error (no 300 s wait) |
| 18 | `slot_manager::tests::save_after_retries_until_success` | async | [L657](../src/slot_manager.rs#L657) | save auto-retry (Rust-only feature) | 2 transient failures then success → `Ok(true)`, exactly 3 save calls, slot marked used |
| 19 | `slot_manager::tests::save_after_retries_exhausted_propagates_error` | async | [L681](../src/slot_manager.rs#L681) | always-failing save | `Err` after 1 initial + `SAVE_RETRIES` attempts; `last_used` untouched (slot stays free) |
| 20 | `slot_manager::tests::save_after_retries_exhausted_500_returns_false` | async | [L696](../src/slot_manager.rs#L696) | always-500 save | `Ok(false)` after 1 + `SAVE_RETRIES` attempts; slot still marked used |
| 21 | `slot_manager::tests::save_after_no_retry_when_retries_disabled` | async | [L710](../src/slot_manager.rs#L710) | `SAVE_RETRIES=0` | exactly one save attempt, `Err` propagated |

## Coverage map — surface → tests

| Surface | Tests |
|---|---|
| Slot selection ordering | `free_slot_picked_first_in_order`, `used_slots_are_skipped_then_oldest_wins`, `multi_backend_slot_ordering`, `pick_skips_held_slot_in_favor_of_idle` |
| Restore gating in `acquire` | `restore_called_only_when_key_given` |
| `save_after` semantics | `save_after_saves_and_marks_used`, `save_after_backend_500_returns_false_but_still_marks_used`, `save_after_other_error_propagates_and_keeps_slot_free` |
| Save auto-retry | `save_after_retries_until_success`, `save_after_retries_exhausted_propagates_error`, `save_after_retries_exhausted_500_returns_false`, `save_after_no_retry_when_retries_disabled` |
| Per-slot locking + timeout | `second_acquire_waits_until_release`, `acquire_times_out_when_slot_held` |
| Circuit breaker | `failed_backend_is_skipped_while_others_available`, `all_backends_down_fails_open`, `backend_cooldown_expires`, `report_failure_escalates_backoff`, `report_success_resets_cooldown_and_streak` |
| Probe recovery | `probe_down_backends_recovers_up_backend` |
| Retry on another backend | `acquire_excluding_picks_other_backend` |
