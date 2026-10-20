# 8. Backend client — HTTP behavior — `tests/client.rs` (15)

[← Suite index](README.md)

Part of the [lpcache regression test cases](README.md): every entry maps
1:1 to a test function; the full suite (197 tests) runs with
`cargo test --workspace`.

Direct tests of `LlamaClient` against `MockLlama`; the two auth-header
tests capture the raw request on a one-shot TCP server.

## Module overview

- **Target:** `lpcache` integration tests — the `client` test binary ([`tests/client.rs`](../tests/client.rs)); 15 tests, all async.
- **Fixtures & helpers:**
  - `client_and_mock()` — starts `MockLlama` on a free port and a `LlamaClient` pointed at it.
  - `one_shot_capture_server()` — a one-shot raw-TCP server that accepts a single connection, captures the request head (up to CRLFCRLF), answers `200 {}` and yields the captured request; used to assert the `Authorization` header on the wire.
- **Surface under test:** `LlamaClient::get_model_id` (incl. backend-down degradation), `chat_json` (object / non-JSON / malformed / non-object / HTTP-error outcomes, slot pinning), `chat_stream` (chunk pass-through, error status), `save_slot` / `restore_slot` status mapping, trailing-slash URL normalization.

## How to run

```sh
cd lpcache
cargo test --test client                             # all 15 client integration tests
cargo test --test client get_model_id_from_backend   # one test (exact name)
```

The **Test case** column is the exact filter for that test: pass it
into `cargo test` as shown above.

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 1 | `get_model_id_from_backend` | async | [L18](../tests/client.rs#L18) | GET `/v1/models` | `"mock-model"` |
| 2 | `get_model_id_unknown_when_backend_down` | async | [L24](../tests/client.rs#L24) | connection refused | `"unknown"` |
| 3 | `api_key_sends_authorization_header` | async | [L60](../tests/client.rs#L60) | API key set (raw-TCP capture) | `Authorization: Bearer 777` header present |
| 4 | `no_api_key_sends_no_authorization_header` | async | [L73](../tests/client.rs#L73) | no API key | no `Authorization` header |
| 5 | `chat_json_object_passthrough_and_slot_pinning` | async | [L85](../tests/client.rs#L85) | JSON chat with slot 0 | response object parsed (`chat.completion`); pin in body root/options/query |
| 6 | `chat_json_no_pinning_when_slot_none` | async | [L113](../tests/client.rs#L113) | JSON chat without a slot | no pin fields in body; empty query |
| 7 | `chat_json_non_json_content_type` | async | [L126](../tests/client.rs#L126) | 200 `text/plain` | `NonJson { "provider returned non-JSON", raw }` |
| 8 | `chat_json_malformed_json_body` | async | [L142](../tests/client.rs#L142) | 200 `application/json` body `"{broken"` | `NonJson { "invalid json from provider", raw }` |
| 9 | `chat_json_non_object_json` | async | [L158](../tests/client.rs#L158) | 200 JSON array body | `NonObject` |
| 10 | `chat_json_http_error` | async | [L168](../tests/client.rs#L168) | 503 | `Err(HttpStatus { 503, "mock chat failure" })` |
| 11 | `chat_stream_chunks` | async | [L184](../tests/client.rs#L184) | 200 SSE stream | chunks contain Hello, world, `[DONE]` |
| 12 | `chat_stream_error_status` | async | [L204](../tests/client.rs#L204) | 500 stream | `StreamChat { status: 500 }` + error body passes through |
| 13 | `save_slot_statuses` | async | [L223](../tests/client.rs#L223) | `POST /slot/{id}/save` | 200 → `true`, 500 → `false`, 404 → `Err(HttpStatus)`; calls recorded in order |
| 14 | `restore_slot_statuses` | async | [L248](../tests/client.rs#L248) | `POST /slot/{id}/restore` | 200 → `Restored`, 400 → `Rejected`, 500 → `Failed` (no error); calls recorded |
| 15 | `base_url_trailing_slash_normalized` | async | [L271](../tests/client.rs#L271) | backend URL with a trailing `/` | works — model id resolved |

## Coverage map — mock endpoint → tests

| Endpoint / behavior | Tests |
|---|---|
| `GET /v1/models` | `get_model_id_from_backend`, `get_model_id_unknown_when_backend_down` |
| Auth header on the wire | `api_key_sends_authorization_header`, `no_api_key_sends_no_authorization_header` |
| `POST` chat completion (JSON) | `chat_json_object_passthrough_and_slot_pinning`, `chat_json_no_pinning_when_slot_none`, `chat_json_non_json_content_type`, `chat_json_malformed_json_body`, `chat_json_non_object_json`, `chat_json_http_error` |
| `POST` chat completion (SSE) | `chat_stream_chunks`, `chat_stream_error_status` |
| `POST /slot/{id}/save` | `save_slot_statuses` |
| `POST /slot/{id}/restore` | `restore_slot_statuses` |
| URL normalization | `base_url_trailing_slash_normalized` |
