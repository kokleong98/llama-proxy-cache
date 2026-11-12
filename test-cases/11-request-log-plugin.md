# 11. Request log plugin — `plugins/src/builtin/request_log.rs` (14)

[← Suite index](README.md)

Part of the [lpcache regression test cases](README.md): every entry maps
1:1 to a test function; the full suite (249 tests) runs with
`cargo test --workspace`.

The built-in `request_log` plugin: a W3C Extended Log Format (XLF)
request-log writer (`RequestLog` — lazy open, XLF escaping,
size-based rotation — and its `PluginFactory`), plus the shared
`format_utc` XLF datetime formatter in `plugins/src/api.rs`.

## Module overview

- **Target:** `lpcache-plugins` library — `builtin::request_log::tests` in [`request_log.rs`](../plugins/src/builtin/request_log.rs); 14 tests, all synchronous.
- **Fixtures & helpers:**
  - `record()` — a fully populated `RequestLogRecord` (all 33 fields, incl. the 8 performance fields and the 5 resource fields).
  - `default_fields()` — the `DEFAULT_FIELDS` list in declaration order.
  - `log_in(td)` — a `RequestLog` at `<tempdir>/request.log` with the default max bytes/keep.
  - `ok_of`/`err_of` — `Result` unwrapping helpers (trait objects are not `Debug`).
- **Surface under test:** `RequestLog` — lazy open, 4-line XLF header, field rendering (missing → `-`), escaping rules, custom field subset/order, size-based rotation with `keep` bounds, rotation disabled at `max_bytes=0`, `prepare()` (parent dirs, idempotency), concurrent writers, `RequestLogFactory` param validation — plus the shared `format_utc` formatter (defined in [`plugins/src/api.rs`](../plugins/src/api.rs), tested from this module).

## How to run

```sh
cargo test -p lpcache-plugins request_log::tests                          # all 14 plugin tests
cargo test -p lpcache-plugins builtin::request_log::tests::header_and_record_format   # one, exact path
```

The **Test case** column is the exact filter for that test: pass it
into `cargo test` as shown above.

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 1 | `builtin::request_log::tests::header_and_record_format` | sync | [L570](../plugins/src/builtin/request_log.rs#L570) | one record into a fresh file | 4 XLF header lines (`#Software: lpcache-plugins/…`, `#Version: 1.0`, `#Date-format: %Y-%m-%dT%H:%M:%S.%fZ`, `#Fields: …` in the declared order) followed by one space-separated record line in `#Fields` order |
| 2 | `builtin::request_log::tests::missing_fields_render_dash` | sync | [L703](../plugins/src/builtin/request_log.rs#L703) | a record with `None` fields | `-` at each missing position |
| 3 | `builtin::request_log::tests::escaping_rules` | sync | [L718](../plugins/src/builtin/request_log.rs#L718) | values with whitespace/quote/backslash; values starting with `-`/`"` | quoted, with `\`, `"`, CR and LF escaped; plain values unquoted |
| 4 | `builtin::request_log::tests::custom_fields_subset_and_order` | sync | [L736](../plugins/src/builtin/request_log.rs#L736) | a `fields` subset in non-default order | `#Fields` and the record lines contain exactly those fields in that order |
| 5 | `builtin::request_log::tests::invalid_field_lists_are_rejected` | sync | [L754](../plugins/src/builtin/request_log.rs#L754) | factory `fields` with an unknown name | factory `Err` |
| 6 | `builtin::request_log::tests::rotation_moves_generations_and_rewrites_header` | sync | [L774](../plugins/src/builtin/request_log.rs#L774) | small `max_bytes`, many records | current file → `.1`, `.1` → `.2` (shifted); the fresh current file carries a new XLF header + the new record; the old records live in the generations |
| 7 | `builtin::request_log::tests::keep_bounds_retained_generations` | sync | [L818](../plugins/src/builtin/request_log.rs#L818) | `keep` bound vs `keep=0` | generations beyond `keep` are dropped on rotation; `keep=0` keeps them all |
| 8 | `builtin::request_log::tests::rotation_disabled_when_max_bytes_zero` | sync | [L862](../plugins/src/builtin/request_log.rs#L862) | `max_bytes=0`, 200 records | no `.1` file; one file with header + 200 record lines |
| 9 | `builtin::request_log::tests::prepare_creates_parent_dirs` | sync | [L878](../plugins/src/builtin/request_log.rs#L878) | `prepare()` on a nested path | parent dirs + file created; re-running `prepare` keeps the existing file (no truncation, single header) |
| 10 | `builtin::request_log::tests::concurrent_writes_are_not_lost` | sync | [L891](../plugins/src/builtin/request_log.rs#L891) | 8 threads × 50 records | all 400 records present; every record line has exactly the `#Fields` field count (no interleaved/corrupt lines) |
| 11 | `builtin::request_log::tests::factory_applies_params_and_defaults` | sync | [L919](../plugins/src/builtin/request_log.rs#L919) | factory with JSON params (explicit / missing / invalid) | `path`/`max_bytes`/`keep` applied or defaulted (10 MiB / 5); unknown `fields` entry or blank `path` → `BadParams`; other adapters' factories return `None` for this spec |
| 12 | `builtin::request_log::tests::format_utc_millisecond_precision` | sync | [L956](../plugins/src/builtin/request_log.rs#L956) | `format_utc` on epoch values | UTC `%Y-%m-%dT%H:%M:%S.%fZ` with millisecond precision (calendar boundaries exact) |
| 13 | `builtin::request_log::tests::perf_fields_render_dash_when_backend_reports_none` | sync | [L621](../plugins/src/builtin/request_log.rs#L621) | a record with every performance field `None` (failure path / backend without `timings`) | all 8 `lpcache-*` performance fields render `-`; the other fields stay intact |
| 14 | `builtin::request_log::tests::resource_fields_render_dash_when_sampling_unavailable` | sync | [L665](../plugins/src/builtin/request_log.rs#L665) | a record with every resource field `None` (platform without CPU/disk sampling) | all 3 `lpcache-*` resource fields render `-`; the other fields stay intact |
