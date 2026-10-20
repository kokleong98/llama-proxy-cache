# 5. llama backend client plugin — `plugins/src/builtin/llama.rs` (5)

[← Suite index](README.md)

Part of the [lpcache regression test cases](README.md): every entry maps
1:1 to a test function; the full suite (197 tests) runs with
`cargo test --workspace`.

The built-in `llama` plugin's `LlamaClient` — slot-pinning unit tests
plus the plugin factory.

## Module overview

- **Target:** `lpcache-plugins` library — `builtin::llama::tests` in [`llama.rs`](../plugins/src/builtin/llama.rs); 5 tests (4 sync, 1 async).
- **Fixtures & helpers:** `serde_json::json!` chat bodies; the one async test creates a real client against a dead port (`http://127.0.0.1:1`) to exercise the `get_model_id` degradation.
- **Surface under test:** `LlamaClient::with_slot_id` (slot-pin injection into body root, `options` and the query string) and `LlamaPluginFactory::create_backend` (param validation).

## How to run

```sh
cd lpcache
cargo test -p lpcache-plugins llama::tests                               # all 5 plugin tests
cargo test -p lpcache-plugins builtin::llama::tests::slot_pinning_none  # one test, exact path
```

The **Test case** column is the exact filter for that test: pass it
into `cargo test` as shown above.

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 1 | `builtin::llama::tests::slot_pinning_none` | sync | [L345](../plugins/src/builtin/llama.rs#L345) | `slot = None` | body unchanged, no query params |
| 2 | `builtin::llama::tests::slot_pinning_duplicated_in_root_options_query` | sync | [L353](../plugins/src/builtin/llama.rs#L353) | `slot = Some(1)` | `_slot_id`/`slot_id`/`id_slot` in body root **and** `options`, `slot_id`/`id_slot` in query; existing fields preserved |
| 3 | `builtin::llama::tests::slot_pinning_adds_options_when_missing` | sync | [L374](../plugins/src/builtin/llama.rs#L374) | body without an `options` object | `options` object created with both pin fields |
| 4 | `builtin::llama::tests::factory_creates_backend_from_json_params` | async | [L383](../plugins/src/builtin/llama.rs#L383) | `llama` plugin factory with JSON params | backend client created from `url` (+ optional `api_key`) |
| 5 | `builtin::llama::tests::factory_rejects_missing_url` | sync | [L398](../plugins/src/builtin/llama.rs#L398) | spec without a `url` param | factory `Err` |
