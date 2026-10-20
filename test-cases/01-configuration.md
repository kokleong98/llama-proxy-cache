# 1. Configuration — `src/config.rs` (36)

[← Suite index](README.md)

Part of the [lpcache regression test cases](README.md): every entry maps
1:1 to a test function; the full suite (197 tests) runs with
`cargo test --workspace`.

## Module overview

- **Target:** `lpcache` library — the `mod tests` at the bottom of [`src/config.rs`](../src/config.rs); 36 tests, all synchronous `#[test]`.
- **Fixtures & helpers:**
  - `vars()` builds a plain `HashMap<String, String>` env map — tests call `Config::from_env_map` on it, so no process env is ever mutated.
  - `base_cfg()` builds a minimal all-default `Config` for exercising the builder methods.
  - One test (`config_from_cli_overrides_process_env`) deliberately reads the real process env to pin the CLI-vs-env contract.
- **Surface under test:** `Config::from_env_map` and `Config::from_cli` (flag parsing: space/equal forms, repeats, unknown flags, missing values, `--help`/`--version`, `--help` text completeness), `Config::new` + the `with_*` builders (clamps/defaults), `Config::plugin_specs()` (legacy `PREFILTER_*` synthesis, `PLUGINS` parsing and precedence), `slot_save_dirs()`, `version_string()`.

## How to run

```sh
cd lpcache
cargo test --lib config::tests                         # all 36 configuration tests
cargo test --lib config::tests::defaults_when_empty    # one test, exact path
cargo test --lib defaults_when_empty                   # one test, name substring
```

The **Test case** column is the exact filter for that test: pass it
into `cargo test` as shown above.

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 1 | `config::tests::defaults_when_empty` | sync | [L920](../src/config.rs#L920) | all built-in defaults with an empty env | backend `http://127.0.0.1:8000` / 1 slot; wpb 100; big 500 words; LCP 0.1; meta dir `kv_meta`; 600 s timeout; model `llama.cpp`; port 8081; log INFO |
| 2 | `config::tests::backends_json_parsed` | sync | [L949](../src/config.rs#L949) | `BACKENDS` JSON array with 2 entries | both backends' `url` + `n_slots` parsed |
| 3 | `config::tests::backends_invalid_json_falls_back_to_empty` | sync | [L962](../src/config.rs#L962) | malformed `BACKENDS` value | empty backend list, no crash |
| 4 | `config::tests::llama_url_and_n_slots_fallback` | sync | [L968](../src/config.rs#L968) | single-backend fallback via `LLAMA_URL` + `N_SLOTS` | url `http://x:9`, 4 slots |
| 5 | `config::tests::invalid_numbers_use_defaults` | sync | [L975](../src/config.rs#L975) | invalid `WORDS_PER_BLOCK`/`LCP_TH`, valid `PORT` | wpb + LCP fall back to defaults; port 80 applied |
| 6 | `config::tests::meta_dir_custom` | sync | [L987](../src/config.rs#L987) | `META_DIR` override | meta dir `my_meta` |
| 7 | `config::tests::meta_max_default_and_env` | sync | [L996](../src/config.rs#L996) | `META_MAX` default / value / `0` / invalid | default; 7; 0 (pruning off); default |
| 8 | `config::tests::slot_save_path_fallback_env` | sync | [L1017](../src/config.rs#L1017) | `SLOT_SAVE_PATH` fallback; ignored when `BACKENDS` set | `/var/kv` in `slot_save_dirs()`; empty when `BACKENDS` set |
| 9 | `config::tests::backends_slot_save_path_json` | sync | [L1034](../src/config.rs#L1034) | per-backend `slot_save_path` field in `BACKENDS` | only backend 0's path in the save dirs |
| 10 | `config::tests::api_key_env` | sync | [L1045](../src/config.rs#L1045) | `LLAMA_API_KEY` set / blank | `Some("777")`; blank → `None` (no auth header sent) |
| 11 | `config::tests::cli_parses_space_form` | sync | [L1059](../src/config.rs#L1059) | `--flag value` form | port, llama-url, n-slots, log-level parsed |
| 12 | `config::tests::cli_parses_equals_form` | sync | [L1080](../src/config.rs#L1080) | `--flag=value` form | port + `BACKENDS` JSON parsed |
| 13 | `config::tests::cli_equals_form_keeps_extra_equals_in_value` | sync | [L1094](../src/config.rs#L1094) | value containing `=` | `http://a:1?x=1` kept intact |
| 14 | `config::tests::cli_flag_twice_last_wins` | sync | [L1100](../src/config.rs#L1100) | same flag given twice | last value wins |
| 15 | `config::tests::cli_unknown_flag_errors` | sync | [L1112](../src/config.rs#L1112) | unknown `--nope` flag | error message names the flag |
| 16 | `config::tests::cli_missing_value_errors` | sync | [L1118](../src/config.rs#L1118) | `--port` without a value | error message names the flag |
| 17 | `config::tests::cli_help_flags` | sync | [L1124](../src/config.rs#L1124) | `--help` / `-h`, also among other options | `help` flag set |
| 18 | `config::tests::cli_version_flags` | sync | [L1140](../src/config.rs#L1140) | `--version` / `-V`, also among other options | `version` flag set |
| 19 | `config::tests::version_string_is_crate_name_and_version` | sync | [L1157](../src/config.rs#L1157) | `version_string()` output | starts with `lpcache `, ends with the crate version |
| 20 | `config::tests::cli_usage_lists_all_flags_and_env_vars` | sync | [L1164](../src/config.rs#L1164) | completeness of `--help` text | all 22 flags and 20 env vars listed |
| 21 | `config::tests::coalesce_requests_flag` | sync | [L1225](../src/config.rs#L1225) | `COALESCE_REQUESTS` env + CLI | off by default; `true`/`1` on; `0`/invalid off; CLI overrides env |
| 22 | `config::tests::stream_queue_size_env_and_cli` | sync | [L1366](../src/config.rs#L1366) | `STREAM_QUEUE_SIZE` env + CLI | default 16; env `64` applied; `0` / invalid → default; CLI `--stream-queue-size 8` overrides env; builder clamps below 1 to 1 |
| 23 | `config::tests::prefilter_env_and_cli` | sync | [L1436](../src/config.rs#L1436) | `PREFILTER_BLOCKLIST` / `PREFILTER_CASE_INSENSITIVE` env + CLI | default disabled (empty list, case-insensitive); comma list trimmed with empty entries dropped; blank list = disabled; invalid bool → default; CLI wins over env; synthesized `keyword` spec params (`keywords`, `case_insensitive`) |
| 24 | `config::tests::config_precedence_cli_over_env_over_defaults` | sync | [L1613](../src/config.rs#L1613) | CLI > env > default (merged env map **and** final Config) | CLI overrides; env applies to unset flags; unset flags add nothing; default otherwise |
| 25 | `config::tests::config_from_cli_overrides_process_env` | sync | [L1643](../src/config.rs#L1643) | `Config::from_cli` vs real process env | CLI value wins; empty CLI reproduces the process-env config |
| 26 | `config::tests::prefilter_result_cache_env_and_cli` | sync | [L1494](../src/config.rs#L1494) | `PREFILTER_RESULT_CACHE_DIR` / `PREFILTER_RESULT_CACHE_TTL` env + CLI | default disabled (no dir, 300 s TTL); dir + TTL applied; blank dir = disabled; `0` = never expire; invalid/negative TTL → default; CLI wins over env; builder clamps negative TTL to 0; synthesized `result_cache` spec params (`dir`, `ttl_secs`) |
| 27 | `config::tests::save_retry_env_and_cli` | sync | [L1394](../src/config.rs#L1394) | `SAVE_RETRIES` / `SAVE_RETRY_DELAY_MS` env + CLI | default 3 retries / 1000 ms; env values applied; `0` disables retries; invalid → defaults; CLI wins over env |
| 28 | `config::tests::plugin_specs_combine_keyword_and_result_cache` | sync | [L1586](../src/config.rs#L1586) | which specs `Config::plugin_specs()` synthesizes from legacy env | blocklist only → one `keyword` spec; result cache only → one `result_cache` spec; both → two specs in order (`keyword` first) |
| 29 | `config::tests::plugin_specs_parsed_from_plugins_json` | sync | [L1278](../src/config.rs#L1278) | `PLUGINS` JSON array (a `keyword` + a `result_cache` spec) | spec names/params parsed exactly (incl. an empty `params` object) |
| 30 | `config::tests::plugins_env_and_cli_stored_raw` | sync | [L1241](../src/config.rs#L1241) | `PLUGINS` env + `--plugins` CLI | raw string kept (surrounding whitespace); CLI wins over env; unset → `None` |
| 31 | `config::tests::plugin_specs_plugins_take_precedence_over_legacy` | sync | [L1294](../src/config.rs#L1294) | `PLUGINS` set **and** legacy `PREFILTER_*` set | `PLUGINS` used verbatim; legacy settings ignored |
| 32 | `config::tests::plugin_specs_synthesized_from_legacy_settings` | sync | [L1333](../src/config.rs#L1333) | legacy `PREFILTER_*` only (no `PLUGINS`) | `keyword` spec (keywords, case_insensitive) + `result_cache` spec (dir, ttl_secs) in order; both unset → no specs |
| 33 | `config::tests::plugin_specs_invalid_json_errors` | sync | [L1316](../src/config.rs#L1316) | malformed `PLUGINS` JSON | `Err` mentioning `PLUGINS` |
| 34 | `config::tests::request_log_env_settings` | sync | [L1651](../src/config.rs#L1651) | `REQUEST_LOG*` env vars (enabled + `PATH`/`MAX_BYTES`/`KEEP`/`FIELDS` with surrounding whitespace and empty entries) | all applied (trimmed, empty entries dropped); invalid `REQUEST_LOG`/numbers fall back to the defaults (disabled, 10 MiB, keep 5, all fields) |
| 35 | `config::tests::request_log_spec_synthesized_when_enabled` | sync | [L1684](../src/config.rs#L1684) | `Config::with_request_log` + a legacy blocklist; `plugin_specs()` | two specs: `keyword` first, then a `request_log` spec carrying `path`/`max_bytes`/`keep`/`fields` params; disabled → no `request_log` spec at all |
| 36 | `config::tests::request_log_spec_in_plugins_wins` | sync | [L1707](../src/config.rs#L1707) | a `request_log` spec in `PLUGINS` **and** enabled `REQUEST_LOG*` settings | the explicit spec is used verbatim — the synthesized one is not appended; the spec alone enables logging even with `REQUEST_LOG` unset |

## Coverage map — configuration surface → tests

| Surface | Tests |
|---|---|
| Built-in defaults | `defaults_when_empty` |
| `BACKENDS` JSON (multi-backend) | `backends_json_parsed`, `backends_invalid_json_falls_back_to_empty`, `backends_slot_save_path_json` |
| `LLAMA_URL` + `N_SLOTS` fallback | `llama_url_and_n_slots_fallback` |
| Invalid numbers (`WORDS_PER_BLOCK`, `LCP_TH`, `PORT`) | `invalid_numbers_use_defaults` |
| `META_DIR` / `META_MAX` | `meta_dir_custom`, `meta_max_default_and_env` |
| `SLOT_SAVE_PATH` | `slot_save_path_fallback_env` |
| `LLAMA_API_KEY` | `api_key_env` |
| CLI mechanics | `cli_parses_space_form`, `cli_parses_equals_form`, `cli_equals_form_keeps_extra_equals_in_value`, `cli_flag_twice_last_wins`, `cli_unknown_flag_errors`, `cli_missing_value_errors`, `cli_help_flags`, `cli_version_flags`, `version_string_is_crate_name_and_version`, `cli_usage_lists_all_flags_and_env_vars` |
| `COALESCE_REQUESTS` | `coalesce_requests_flag` |
| `STREAM_QUEUE_SIZE` | `stream_queue_size_env_and_cli` |
| `PREFILTER_BLOCKLIST` / `PREFILTER_CASE_INSENSITIVE` | `prefilter_env_and_cli` |
| `PREFILTER_RESULT_CACHE_DIR` / `PREFILTER_RESULT_CACHE_TTL` | `prefilter_result_cache_env_and_cli` |
| `SAVE_RETRIES` / `SAVE_RETRY_DELAY_MS` | `save_retry_env_and_cli` |
| `PLUGINS` env / `--plugins` | `plugins_env_and_cli_stored_raw`, `plugin_specs_parsed_from_plugins_json`, `plugin_specs_plugins_take_precedence_over_legacy`, `plugin_specs_synthesized_from_legacy_settings`, `plugin_specs_invalid_json_errors`, `plugin_specs_combine_keyword_and_result_cache` |
| `REQUEST_LOG*` | `request_log_env_settings`, `request_log_spec_synthesized_when_enabled`, `request_log_spec_in_plugins_wins` |
| Precedence (CLI > env > default) | `config_precedence_cli_over_env_over_defaults`, `config_from_cli_overrides_process_env` |
