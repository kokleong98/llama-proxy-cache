# 2. Prefilter plugins — `plugins/src/builtin/keyword.rs` + `chain.rs` (14)

[← Suite index](README.md)

Part of the [lpcache regression test cases](README.md): every entry maps
1:1 to a test function; the full suite (197 tests) runs with
`cargo test --workspace`.

The `Prefilter` trait implementations from the `lpcache-plugins`
library — loaded by the proxy from `PLUGINS` specs (or specs synthesized
from the legacy `PREFILTER_*` variables) and consulted before any
slot/backend work.

## Module overview

- **Target:** `lpcache-plugins` library — `builtin::keyword::tests` (11) + `builtin::chain::tests` (3) in [`keyword.rs`](../plugins/src/builtin/keyword.rs) / [`chain.rs`](../plugins/src/builtin/chain.rs); 10 async (`#[tokio::test]` — `check()` is an async trait method) + 4 sync (pure helpers, factories).
- **Fixtures & helpers:**
  - `preq(messages, n_words)` (defined in both test modules) builds a `PrefilterRequest` with a fixed model/backend-model/key.
  - `ServeFilter` (chain module) — a fixture `Prefilter` that always answers `Serve { 200, body }`.
- **Surface under test:** `KeywordPrefilter` (`new`, `check`, case sensitivity, multi-message scan, content-part arrays), `message_text` flattening, `from_comma_list` parsing, the `keyword`/`chain` `PluginFactory`s, `ChainPrefilter` ordering semantics, and `Arc<dyn Prefilter>` trait-object dispatch (the shape `AppState` holds).

## How to run

```sh
cd lpcache
cargo test -p lpcache-plugins keyword::tests   # 11 keyword plugin tests
cargo test -p lpcache-plugins chain::tests     # 3 chain plugin tests
cargo test -p lpcache-plugins builtin::keyword::tests::keyword_rejects_on_match   # one, exact path
```

The **Test case** column is the exact filter for that test: pass it
into `cargo test` as shown above.

Keyword plugin (11 tests):

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 1 | `builtin::keyword::tests::keyword_rejects_on_match` | async | [L244](../plugins/src/builtin/keyword.rs#L244) | request containing a blocked keyword | `Reject { 400, message naming the keyword }` |
| 2 | `builtin::keyword::tests::keyword_accepts_clean_request` | async | [L260](../plugins/src/builtin/keyword.rs#L260) | request with no blocked keyword | `Accept` |
| 3 | `builtin::keyword::tests::keyword_case_insensitive_by_default` | async | [L267](../plugins/src/builtin/keyword.rs#L267) | keyword `ForBIDDEN` vs text `FORBIDDEN` | case-insensitive matches; case-sensitive mode requires exact case |
| 4 | `builtin::keyword::tests::keyword_scans_all_messages` | async | [L288](../plugins/src/builtin/keyword.rs#L288) | keyword in a later message of a multi-turn conversation | `Reject` |
| 5 | `builtin::keyword::tests::keyword_inspects_content_part_arrays` | async | [L306](../plugins/src/builtin/keyword.rs#L306) | OpenAI content-part array (`text` parts + non-text parts) | text parts inspected, non-text skipped → `Reject` |
| 6 | `builtin::keyword::tests::keyword_empty_messages_accepts` | async | [L326](../plugins/src/builtin/keyword.rs#L326) | no messages / message without `content` | `Accept` |
| 7 | `builtin::keyword::tests::message_text_flattens_content` | sync | [L340](../plugins/src/builtin/keyword.rs#L340) | string + array contents flattened | newline-joined text; empty/defensive inputs → `""` |
| 8 | `builtin::keyword::tests::from_comma_list_trims_and_drops_empty` | sync | [L357](../plugins/src/builtin/keyword.rs#L357) | `PREFILTER_BLOCKLIST`-style parsing | trimmed list; whitespace-only/empty → `None` (disabled) |
| 9 | `builtin::keyword::tests::trait_object_dispatch` | async | [L369](../plugins/src/builtin/keyword.rs#L369) | adapter behind `Arc<dyn Prefilter>` (the `AppState` shape) | `name()` + `check()` dispatch correctly |
| 10 | `builtin::keyword::tests::factory_creates_prefilter_from_json_params` | async | [L381](../plugins/src/builtin/keyword.rs#L381) | `keyword` plugin factory with JSON params | prefilter built from `keywords` + `case_insensitive` |
| 11 | `builtin::keyword::tests::factory_rejects_empty_or_missing_keywords` | sync | [L405](../plugins/src/builtin/keyword.rs#L405) | no `keywords` / empty list / wrong type | factory `Err` |

Chain plugin (3 tests):

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 12 | `builtin::chain::tests::chain_first_non_accept_wins` | async | [L164](../plugins/src/builtin/chain.rs#L164) | `ChainPrefilter` (keyword → serve-fixture) | clean request → `Serve` from the 2nd adapter; blocked request → `Reject { 400 }` from the 1st beats the later serve |
| 13 | `builtin::chain::tests::chain_all_accepts_accepts` | async | [L193](../plugins/src/builtin/chain.rs#L193) | all adapters return `Accept` | chain returns `Accept` |
| 14 | `builtin::chain::tests::factory_builds_nested_chain_from_specs` | sync | [L203](../plugins/src/builtin/chain.rs#L203) | `chain` factory with a nested `prefilters` spec list | nested chain constructed and dispatching |
