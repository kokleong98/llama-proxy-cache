# Llama Server Proxy Cache
![Llama Server Proxy Cache Logo](llama-proxy-cache.png)
A KV-cache-aware, OpenAI-compatible proxy in front of one or more
**llama.cpp** servers — a from-scratch Rust rewrite of the Python
[`proxycache`](https://github.com/airnsk/proxycache) project.

It speaks OpenAI's `/v1/chat/completions` API and transparently manages
llama.cpp's slot KV caches so that long, similar prompts don't pay the
full prefill cost every time.

## How it works

1. **Request prefilter** (optional) — when a prefilter plugin is
   configured, every chat request is checked by a prefilter plugin right
   after body parsing and cache-key computation and **before**
   coalescing, slot acquisition, or backend dispatch. Prefilters come
   from the `lpcache-plugins` library and are selected by name through
   the `PLUGINS` JSON config (see below); the legacy `PREFILTER_*`
   variables keep working and are translated into the equivalent plugin
   specs when `PLUGINS` is unset. The llama.cpp backend client is
   likewise a plugin (the built-in `llama`) and is instantiated per
   `BACKENDS` entry with its URL, API key and timeout from the standard
   config.
   Two built-in prefilter plugins: the **keyword blocklist** (`keyword`)
   rejects requests whose message contents contain a blocked keyword
   (plain substring match, case-insensitive by default) with a `400`
   JSON error, and the **result cache** (`result_cache`) answers fresh
   (non-expired) same-key requests with a previously cached backend
   result stored at `{dir}/{key}.json` — fresh non-streaming results are
   stored there after the backend call; streaming requests are never
   served from the cache. In both cases the request never reaches
   llama-server — no slot is acquired, no KV cache is restored or saved,
   no meta file is written, and the request never leads or joins a
   coalescing group (several plugins are chained in config order, so the
   blocklist can run first). Custom accept/reject/serve logic plugs in
   through the `Prefilter` trait from `lpcache_plugins` (register the
   factory at startup); result-cache hits can trigger a pluggable
   post-serve cleanup adapter (`PostCleanup`, e.g. the built-in
   `RemoveAfterServe` one-shot cleanup).
2. **Prefix hashing** — the concatenated message contents (roles
   stripped) are split into words (`\w+`, lowercased), chunked into blocks
   of `WORDS_PER_BLOCK` (default 100) words, and each block is SHA256-hashed.
   The cache key is `sha256(backend_model_id + "\n" + raw_prefix)`.
3. **Big/small requests** — requests with more than `BIG_THRESHOLD_WORDS`
   (default 500) words are "big": they run with `cache_prompt=true` and
   their KV cache is saved to the backend after completion, with a
   `<key>.meta.json` file (block hashes, model id, wpb, timestamp) written
   to `META_DIR`.
4. **Restore candidates** — before a big request, the proxy scans the meta
   files and looks for one with the same model and `wpb` whose block list
   shares the longest common prefix; if the LCP ratio is at least `LCP_TH`
   (default 0.1), that cache is restored into the chosen slot *before* the
   chat is dispatched.
5. **Slot management** — every llama.cpp slot is guarded by a lock.
   The proxy picks a never-used slot first, otherwise the
   least recently used one, waits up to 300 s for it (`503 all slots busy`
   on timeout), and releases it when the response is fully handled.
6. **Streaming** — SSE responses are passed through byte-for-byte via a
   background reader task; the reader's cleanup step always saves the KV
   cache and writes the meta file, then releases the slot.
7. **Bounded cache** — after each save the proxy keeps at most `META_MAX`
   (default 10) meta files: the oldest ones (by last save/restore
   timestamp) are deleted, together with the KV files of the same name in
   the configured `--slot-save-path` directories. `META_MAX=0` disables
   pruning. A later restore of a pruned key simply fails and falls back to
   a full prefill.
8. **Request log** (optional) — when `REQUEST_LOG` is enabled, every
   finished request (including failed ones) becomes one W3C Extended Log
   Format (XLF) record appended to a size-rotated file — see [Request
   log](#request-log).

## Stack

Rust 2024, `axum` 0.8 + `tokio` + `reqwest` (rustls) + `serde` + `sha2` +
`tracing`. The slot pin is duplicated in the request body root, `options`
and the query string (llama.cpp accepts it in several places depending on the build).

## Configuration

| Variable            | Default             | Meaning                                        |
|---------------------|---------------------|------------------------------------------------|
| `BACKENDS`          | —                   | JSON array `[{"url": "...", "n_slots": N}, ...]` |
| `LLAMA_URL`         | `http://127.0.0.1:8000` | fallback single backend when `BACKENDS` unset |
| `N_SLOTS`           | `1`                 | fallback slot count per backend                |
| `WORDS_PER_BLOCK`   | `100`               | words per hash block                           |
| `BIG_THRESHOLD_WORDS` | `500`             | word count above which a request is "big"      |
| `LCP_TH`            | `0.1`               | minimum LCP block ratio to restore a cache     |
| `META_DIR`          | `./kv_meta`         | where `<key>.meta.json` files live             |
| `META_MAX`          | `10`                | max meta files kept (LRU); 0 = unlimited       |
| `SLOT_SAVE_PATH`    | —                   | backend's `--slot-save-path` dir (single-backend fallback); pruned KV files are removed from it |
| `REQUEST_TIMEOUT`   | `600` (s)           | per-request timeout to the backend             |
| `MODEL_ID`          | `llama.cpp`         | model id advertised by `/v1/models` and used as default |
| `PORT`              | `8081`              | proxy listen port (0.0.0.0)                    |
| `LLAMA_API_KEY`     | —                   | sent to the backend as `Authorization: Bearer <KEY>` (blank = no auth) |
| `LOG_LEVEL`         | `INFO`              | log level (`RUST_LOG` overrides it)            |
| `STREAM_QUEUE_SIZE` | `16`                | per-request SSE channel capacity (must be >= 1) |
| `COALESCE_REQUESTS` | `false`             | group concurrent same-cache-key requests into one backend call (Rust-only) |
| `PLUGINS` | — | JSON **array** of plugin specs `[{"name": "...", "params": {...}}]` — built-in names: `keyword`, `result_cache`, `chain` (prefilters, chained in list order; empty array = no prefilter) and `request_log` (the request logger — a spec here takes precedence over the `REQUEST_LOG*` variables below); when unset, the legacy `PREFILTER_*` variables below are translated into the equivalent prefilter specs (when set, `PLUGINS` takes precedence over them) |
| `REQUEST_LOG` | `false` | when true, one W3C-XLF record per finished request is appended to the request-log file (the built-in `request_log` plugin) |
| `REQUEST_LOG_PATH` | `lpcache-request.log` | request-log file location; relative paths resolve against the CWD (the parent dir is created) |
| `REQUEST_LOG_MAX_BYTES` | `10485760` (10 MiB) | rotation threshold in bytes; the file is rotated when exceeded (`<path>.1`, `<path>.2`, …); `0` = no rotation |
| `REQUEST_LOG_KEEP` | `5` | rotated generations retained (the oldest is dropped); `0` = keep them all |
| `REQUEST_LOG_FIELDS` | all 28 fields | comma-separated ordered subset of the known XLF field names (W3C fields + `lpcache-*` pipeline/performance fields — see [Request log](#request-log)) |
| `PREFILTER_BLOCKLIST` | —                 | **legacy** (synthesized into a `keyword` plugin when `PLUGINS` is unset): comma-separated keywords; requests whose message contents contain any are rejected with `400` before the backend (default: no prefilter) |
| `PREFILTER_CASE_INSENSITIVE` | `true`     | **legacy**: keyword matching is case-insensitive           |
| `PREFILTER_RESULT_CACHE_DIR` | —          | **legacy** (synthesized into a `result_cache` plugin when `PLUGINS` is unset): cache-result path (`{key}.json` per cache key); fresh same-key requests are answered from the cached backend result before the backend (default: disabled) |
| `PREFILTER_RESULT_CACHE_TTL` | `300` (s)    | **legacy**: per-entry result-cache expiry; `0` = never expires |
| `SAVE_RETRIES` | `3`                 | retries after a failed KV cache save (backend 500 or other error); `0` = no retry |
| `SAVE_RETRY_DELAY_MS` | `1000` (ms)   | delay between KV cache save retry attempts          |

Every variable is also available as a command-line flag (see
`./target/release/lpcache --help`): `--backends`, `--llama-url`,
`--n-slots`, `--words-per-block`, `--big-threshold-words`, `--lcp-th`,
`--meta-dir`, `--meta-max`, `--slot-save-path`, `--request-timeout`,
`--model-id`, `--port`, `--api-key` (→ `LLAMA_API_KEY`), `--log-level`,
`--stream-queue-size`,
`--coalesce-requests`, `--plugins` (→ `PLUGINS`),
`--request-log` (→ `REQUEST_LOG`),
`--request-log-path` (→ `REQUEST_LOG_PATH`),
`--request-log-max-bytes` (→ `REQUEST_LOG_MAX_BYTES`),
`--request-log-keep` (→ `REQUEST_LOG_KEEP`),
`--request-log-fields` (→ `REQUEST_LOG_FIELDS`),
`--prefilter-blocklist` (→ `PREFILTER_BLOCKLIST`, legacy),
`--prefilter-case-insensitive` (→ `PREFILTER_CASE_INSENSITIVE`, legacy),
`--prefilter-result-cache-dir` (→ `PREFILTER_RESULT_CACHE_DIR`, legacy),
`--prefilter-result-cache-ttl` (→ `PREFILTER_RESULT_CACHE_TTL`, legacy),
`--save-retries` (→ `SAVE_RETRIES`),
`--save-retry-delay-ms` (→ `SAVE_RETRY_DELAY_MS`).
`-h`/`--help` shows the full help; `-V`/`--version` prints the proxy version
(also logged in the `app_start` line at startup).
Explicit flags take precedence over environment variables, which take
precedence over the built-in defaults:

```bash
./target/release/lpcache --llama-url http://127.0.0.1:8000 --n-slots 1 \
  --port 8081 --meta-dir ./kv_meta --log-level INFO
```

## Request log

When `REQUEST_LOG=true` — or a `request_log` spec is listed in `PLUGINS`
(which takes precedence) — the built-in `request_log` plugin appends one
[W3C Extended Log Format (XLF)](https://www.w3.org/TR/xlf-10/) record per
**finished** request to `REQUEST_LOG_PATH`: successes, errors, and client
disconnects (the record is written when the response body completes or is
dropped). The file starts with:

```text
#Software: lpcache-plugins/<version>
#Version: 1.0
#Date-format: %Y-%m-%dT%H:%M:%S.%fZ
#Fields: datetime cs-method cs-uri ... lpcache-reason
```

Default fields (W3C fields first, then lpcache pipeline fields, then
lpcache performance fields — `REQUEST_LOG_FIELDS` selects an ordered
subset): `datetime`, `cs-method`, `cs-uri`, `c-ip`, `cs(User-Agent)`,
`cs-bytes`, `cs-time`, `sc-status`, `sc-bytes`, `lpcache-model`,
`lpcache-stream`, `lpcache-key`, `lpcache-n-words`, `lpcache-is-big`,
`lpcache-backend`, `lpcache-slot`, `lpcache-restored`,
`lpcache-cache-hit`, `lpcache-coalesced`, `lpcache-reason`,
`lpcache-prompt-tokens`, `lpcache-completion-tokens`,
`lpcache-cached-tokens`, `lpcache-prefill-ms`, `lpcache-gen-ms`,
`lpcache-prefill-tps`, `lpcache-gen-tps`, `lpcache-ttft-ms`.

- Missing values render as `-` (XLF "not available"); values containing
  whitespace, a quote, or a backslash — or starting with `-`/`"` — are
  quoted and escaped per XLF.
- **Performance fields**: `lpcache-prompt-tokens` /
  `lpcache-completion-tokens` / `lpcache-cached-tokens` come from the
  backend's `usage` (the last one is the prompt tokens served from the
  backend's KV cache — the per-request effect of a restore);
  `lpcache-prefill-ms`, `lpcache-gen-ms`, `lpcache-prefill-tps` and
  `lpcache-gen-tps` come from the `timings` object llama.cpp appends to
  every chat completion (backend-side prefill/generation time and
  throughput). `lpcache-ttft-ms` is measured by the proxy (streaming
  only): time from stream start to the first SSE chunk. All of them
  render `-` when the backend did not report them (failure paths,
  non-chat requests, backends without `timings`).
- **Rotation**: when the file grows beyond `REQUEST_LOG_MAX_BYTES` it is
  renamed to `<path>.1`, older generations shift upward (`<path>.2`,
  …), and at most `REQUEST_LOG_KEEP` generations are retained (the oldest
  is dropped). `max_bytes=0` disables rotation; `keep=0` keeps all
  generations. The fresh current file starts with a new XLF header.
- Request-log I/O failures are logged and swallowed — logging never breaks
  request serving.
- `lpcache-reason` is `-` on success, otherwise a short pipeline outcome:
  `prefilter_reject`, `slot_timeout`, `backend_unreachable`,
  `backend_error`, `provider_http_<status>`, `provider_non_object`,
  `coalesce_leader_failed`, `save_failed`.
- Caveats: `c-ip` is only populated when the proxy is served over a real
  socket (Axum `ConnectInfo`); `cs-bytes` is taken from the request's
  `Content-Length` header, so chunked requests without one log `-`.

```bash
BACKENDS='[{"url":"http://127.0.0.1:8000","n_slots":4}]' \
  REQUEST_LOG=true REQUEST_LOG_PATH=/var/log/lpcache/req.log \
  REQUEST_LOG_MAX_BYTES=10485760 REQUEST_LOG_KEEP=5 ./target/release/lpcache
```

## Build & run

> 📖 **New here?** Start with the full [Setup Guide](docs/setup-guide.md) —
> llama.cpp flags, single/multi-backend configuration, client setup,
> verification, systemd, and troubleshooting.

```bash
cargo build --release
BACKENDS='[{"url":"http://127.0.0.1:8000","n_slots":4}]' \
  PORT=8081 META_DIR=./kv_meta ./target/release/lpcache
```

Example client:

```bash
curl http://127.0.0.1:8081/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"llama.cpp","stream":true,"messages":[{"role":"user","content":"Hello!"}]}'
```

## Smoke test without a real model

A mock llama.cpp server is included (`/v1/models`, `/v1/chat/completions`
with JSON or SSE, and `/slots/:id?action=save|restore`):

```bash
# terminal 1
cargo run --example mock_llama -- 8100 mock-model

# terminal 2
BACKENDS='[{"url":"http://127.0.0.1:8100","n_slots":2}]' \
  PORT=8091 META_DIR=/tmp/kv_meta LOG_LEVEL=DEBUG cargo run
```

Send a prompt with >500 words, then send the same prompt with a few extra
words appended — the second request logs
`restore_candidate ... ratio=1.000` / `restore_before_chat ... ok=true`.

## Tests

```bash
cargo test --workspace
```

- **Unit tests** (80): config parsing/defaults (incl. `META_MAX` /
  `SLOT_SAVE_PATH` / `PREFILTER_*` / `REQUEST_LOG*` / `PLUGINS` spec
  synthesis and precedence), raw-prefix construction, word
  tokenization, block hashing, LCP, SHA256 vectors, meta file round-trips,
  candidate filtering/thresholds, LRU pruning, slot selection (free/oldest
  ordering, idle-aware, multi-backend, circuit breaker, escalating
  backoff, success/probe recovery, retry slot exclusion), per-slot locking,
  acquire timeout, save semantics (incl. save auto-retry).
- **Plugin library tests** (55 + 3 doctests, 2 ignored): the
  `lpcache-plugins` trait objects and built-ins — keyword matching, case
  sensitivity, content-part arrays, result-cache put/get/expire,
  `RemoveAfterServe` one-shot cleanup, store-trait dispatch,
  `LlamaClient` against a mock server (slot pin in body/options/query,
  JSON/non-JSON answers), the `request_log` plugin (XLF header, field
  rendering/escaping, field selection, size rotation with `keep`), and the
  plugin registry (`PluginSpec` parsing, builtin registration, unknown
  plugin error, zero/one/many `load_prefilters` chaining, loader kind
  filtering).
- **Integration tests** (62): the built-in `llama` plugin's `LlamaClient`
  against the mock backend
  (slot pinning in body/options/query, save/restore status codes, JSON vs
  non-JSON provider answers, streaming), and end-to-end proxy behaviour
  (small vs big requests, meta file creation, restore on the second big
  request, SSE passthrough, provider error mapping, 422 on bad JSON, dead
  backend failover, transparent retry on connection failure, probe recovery
  of a cooled-down backend, failing-slot non-pinning, LRU meta+KV pruning,
  restore of surviving entries after a prune with graceful fallback for
  pruned keys, LRU timestamp bump on successful restore, stale-meta
  cleanup after a rejected restore (KV file gone from all slot-save
  dirs), concurrent-request coalescing (identical merges, generation-param
  independence, shared stream/JSON groups, full stream for followers,
  off-flag per-request calls, fresh group after completion), stream queue
  backpressure at size 1, prefilter
  accept/reject before the backend — keyword reject with zero
  backend calls, case-insensitive matching, content-part inspection,
  stream/big-request rejection, reject-before-coalescing, custom
  `Prefilter` adapter, result-cache serve/expire/one-shot cleanup driven
  through plugin specs, legacy `PREFILTER_*` env behaviour, W3C-XLF
  request log (record per finished request, delivered stream bytes,
  failed requests without pipeline fields, size rotation + `keep` bound,
  backend performance fields)).

`cargo clippy --all-targets` and `cargo fmt --check` are clean.

# Want to buy me a coffee?
[![Buy Me A Coffee](https://www.buymeacoffee.com/assets/img/custom_images/orange_img.png)](https://buymeacoffee.com/kokleong98)
