# 10. Plugin registry — `plugins/src/registry.rs` (9)

[← Suite index](README.md)

Part of the [lpcache regression test cases](README.md): every entry maps
1:1 to a test function; the full suite (197 tests) runs with
`cargo test --workspace`.

The runtime registry: plugin factories registered by name, `PluginSpec`
parsing from `PLUGINS` JSON, `load_prefilters` (zero/one/many specs
→ no prefilter / one prefilter / a `chain`), and `load_request_loggers`
(the single request logger, if any).

## Module overview

- **Target:** `lpcache-plugins` library — `registry::tests` in [`registry.rs`](../plugins/src/registry.rs); 9 tests, all synchronous.
- **Fixtures & helpers:** `keyword_spec(kw)` builds a `PluginSpec`; `logger_of()` unwraps `Result<Option<Arc<dyn RequestLogger>>>` by matching (the trait object is not `Debug`).
- **Surface under test:** `Registry::with_builtins` (builtin set + names), `global`/`ensure_builtin` idempotency, runtime registration of custom factories, `PluginSpec` parsing (`from_value` defaulting `params`), `load_prefilters` (zero/one/many → `None`/single/`chain`, error propagation), `load_request_loggers` (kind filtering, single-logger bound), `PluginError` variants (`UnknownPlugin`, `UnsupportedKind`, `BadParams`).

## How to run

```sh
cd lpcache
cargo test -p lpcache-plugins registry::tests                         # all 9 registry tests
cargo test -p lpcache-plugins registry::tests::load_prefilters_zero_one_many   # one, exact path
```

The **Test case** column is the exact filter for that test: pass it
into `cargo test` as shown above.

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 1 | `registry::tests::with_builtins_registers_all_builtin_plugins` | sync | [L367](../plugins/src/registry.rs#L367) | `Registry::with_builtins()` | `chain`, `keyword`, `llama`, `request_log`, `result_cache` registered; unknown names not |
| 2 | `registry::tests::global_registry_and_ensure_builtin_are_idempotent` | sync | [L545](../plugins/src/registry.rs#L545) | `global()` + repeated `ensure_builtin()` | same registry; built-ins registered exactly once |
| 3 | `registry::tests::register_custom_plugin_and_create_it` | sync | [L425](../plugins/src/registry.rs#L425) | custom factory registered at runtime | created by name; `name()` = custom plugin |
| 4 | `registry::tests::unknown_plugin_is_an_error` | sync | [L466](../plugins/src/registry.rs#L466) | spec naming an unregistered plugin | `PluginError::UnknownPlugin` naming it |
| 5 | `registry::tests::wrong_kind_is_an_error` | sync | [L480](../plugins/src/registry.rs#L480) | prefilter spec for a backend-only plugin (`llama`) and backend spec for a prefilter-only plugin (`keyword`) | `PluginError::UnsupportedKind` in both directions |
| 6 | `registry::tests::plugin_spec_from_value_defaults_empty_params` | sync | [L535](../plugins/src/registry.rs#L535) | JSON object with/without `params` | name kept; missing `params` → empty object |
| 7 | `registry::tests::load_prefilters_zero_one_many` | sync | [L502](../plugins/src/registry.rs#L502) | empty list / 1 spec / 2 specs | `None` / the single prefilter (unwrapped) / a `chain` prefilter |
| 8 | `registry::tests::load_prefilters_propagates_errors` | sync | [L519](../plugins/src/registry.rs#L519) | a spec whose factory fails | the factory's error propagates |
| 9 | `registry::tests::loaders_skip_specs_of_other_adapter_kinds` | sync | [L384](../plugins/src/registry.rs#L384) | mixed specs (`keyword` + `request_log`) | `load_prefilters` skips the logger spec (returns the `keyword` prefilter); `load_request_loggers` skips the prefilter spec (returns the `request_log` logger); a known non-logger plugin → no logger; unknown names are `UnknownPlugin` errors for both loaders; a second request-logger spec is `BadParams` |
