# 10. Plugin registry — `plugins/src/registry.rs` (10)

[← Suite index](README.md)

Part of the [lpcache regression test cases](README.md): every entry maps
1:1 to a test function; the full suite (244 tests) runs with
`cargo test --workspace`.

The runtime registry: plugin factories registered by name, `PluginSpec`
parsing from `PLUGINS` JSON, `load_prefilters` (zero/one/many specs
→ no prefilter / one prefilter / a `chain`), `load_dashboard` (the
single dashboard, if any) and `load_request_loggers`
(the single request logger, if any).

## Module overview

- **Target:** `lpcache-plugins` library — `registry::tests` in [`registry.rs`](../plugins/src/registry.rs); 10 tests, all synchronous.
- **Fixtures & helpers:** `keyword_spec(kw)` builds a `PluginSpec`; `logger_of()` unwraps `Result<Option<Arc<dyn RequestLogger>>>` by matching (the trait object is not `Debug`).
- **Surface under test:** `Registry::with_builtins` (builtin set + names), `global`/`ensure_builtin` idempotency, runtime registration of custom factories, `PluginSpec` parsing (`from_value` defaulting `params`), `load_prefilters` (zero/one/many → `None`/single/`chain`, error propagation), `load_dashboard` (zero/one → `None`/adapter, kind filtering, single-dashboard bound) and `load_request_loggers` (kind filtering, single-logger bound), `PluginError` variants (`UnknownPlugin`, `UnsupportedKind`, `BadParams`).

## How to run

```sh
cargo test -p lpcache-plugins registry::tests                         # all 10 registry tests
cargo test -p lpcache-plugins registry::tests::load_prefilters_zero_one_many   # one, exact path
```

The **Test case** column is the exact filter for that test: pass it
into `cargo test` as shown above.

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 1 | `registry::tests::with_builtins_registers_all_builtin_plugins` | sync | [L424](../plugins/src/registry.rs#L424) | `Registry::with_builtins()` | `chain`, `dashboard`, `keyword`, `llama`, `qwen_guard`, `request_log`, `result_cache` registered; unknown names not |
| 2 | `registry::tests::global_registry_and_ensure_builtin_are_idempotent` | sync | [L604](../plugins/src/registry.rs#L604) | `global()` + repeated `ensure_builtin()` | same registry; built-ins registered exactly once |
| 3 | `registry::tests::register_custom_plugin_and_create_it` | sync | [L484](../plugins/src/registry.rs#L484) | custom factory registered at runtime | created by name; `name()` = custom plugin |
| 4 | `registry::tests::unknown_plugin_is_an_error` | sync | [L525](../plugins/src/registry.rs#L525) | spec naming an unregistered plugin | `PluginError::UnknownPlugin` naming it |
| 5 | `registry::tests::wrong_kind_is_an_error` | sync | [L539](../plugins/src/registry.rs#L539) | prefilter spec for a backend-only plugin (`llama`) and backend spec for a prefilter-only plugin (`keyword`) | `PluginError::UnsupportedKind` in both directions |
| 6 | `registry::tests::plugin_spec_from_value_defaults_empty_params` | sync | [L594](../plugins/src/registry.rs#L594) | JSON object with/without `params` | name kept; missing `params` → empty object |
| 7 | `registry::tests::load_prefilters_zero_one_many` | sync | [L561](../plugins/src/registry.rs#L561) | empty list / 1 spec / 2 specs | `None` / the single prefilter (unwrapped) / a `chain` prefilter |
| 8 | `registry::tests::load_prefilters_propagates_errors` | sync | [L578](../plugins/src/registry.rs#L578) | a spec whose factory fails | the factory's error propagates |
| 9 | `registry::tests::loaders_skip_specs_of_other_adapter_kinds` | sync | [L443](../plugins/src/registry.rs#L443) | mixed specs (`keyword` + `request_log`) | `load_prefilters` skips the logger spec (returns the `keyword` prefilter); `load_request_loggers` skips the prefilter spec (returns the `request_log` logger); a known non-logger plugin → no logger; unknown names are `UnknownPlugin` errors for both loaders; a second request-logger spec is `BadParams` |
| 10 | `registry::tests::load_dashboard_zero_one_many` | sync | [L618](../plugins/src/registry.rs#L618) | zero specs / one `dashboard` spec among other plugins / two `dashboard` specs / an unknown name | `None` / that adapter (prefilter specs in the list are skipped; its configured `path` is exposed) / `BadParams` naming `dashboard` / `UnknownPlugin` |
