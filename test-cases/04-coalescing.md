# 4. Request coalescing — `src/coalesce.rs` (4)

[← Suite index](README.md)

Part of the [lpcache regression test cases](README.md): every entry maps
1:1 to a test function; the full suite (197 tests) runs with
`cargo test --workspace`.

## Module overview

- **Target:** `lpcache` library — the `mod tests` at the bottom of [`src/coalesce.rs`](../src/coalesce.rs); 4 tests (3 sync, 1 async).
- **Fixtures & helpers:** `SingleFlight::new()` / `Flight::new()` used directly; the async test spawns a waiter task that polls `notified()` until the leader finishes.
- **Surface under test:** `SingleFlight::enter` (leader/follower grouping by key), `forget`, `finish` (waiter wakeup with the shared outcome) and the ordered incremental SSE drain (`Flight::append`/`drain`).

## How to run

```sh
cd lpcache
cargo test --lib coalesce::tests                                           # all 4 coalescing tests
cargo test --lib coalesce::tests::enter_first_is_leader_second_is_follower # one test, exact path
```

The **Test case** column is the exact filter for that test: pass it
into `cargo test` as shown above.

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 1 | `coalesce::tests::enter_first_is_leader_second_is_follower` | sync | [L163](../src/coalesce.rs#L163) | group entry by key | 1st caller = leader, 2nd = follower (same flight); different key → new group |
| 2 | `coalesce::tests::forget_removes_the_group` | sync | [L177](../src/coalesce.rs#L177) | group removal | re-entering a forgotten key starts a new leader |
| 3 | `coalesce::tests::finish_wakes_waiters_with_outcome` | async | [L187](../src/coalesce.rs#L187) | waiter wakeup on leader finish | follower receives the leader's `SharedOutcome` |
| 4 | `coalesce::tests::sse_append_and_drain_is_ordered` | sync | [L208](../src/coalesce.rs#L208) | ordered incremental SSE drain | `"a"+"bc"` drains as `"abc"`, then `"d"`; full buffer `"abcd"` |
