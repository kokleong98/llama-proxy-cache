# Llama Server Proxy Cache
![Llama Server Proxy Cache Logo](llama-proxy-cache.png)
A KV-cache-aware, OpenAI-compatible proxy in front of one or more
**llama.cpp** servers — a from-scratch Rust rewrite of the Python
[`proxycache`](https://github.com/airnsk/proxycache) project.

It speaks OpenAI's `/v1/chat/completions` API and transparently manages
llama.cpp's slot KV caches so that long, similar prompts don't pay the
full prefill cost every time.

## How it works

1. **Prefix hashing** — the concatenated message contents (roles
   stripped) are split into words (`\w+`, lowercased), chunked into blocks
   of `WORDS_PER_BLOCK` (default 100) words, and each block is SHA256-hashed.
   The cache key is `sha256(backend_model_id + "\n" + raw_prefix)`.
2. **Big/small requests** — requests with more than `BIG_THRESHOLD_WORDS`
   (default 500) words are "big": they run with `cache_prompt=true` and
   their KV cache is saved to the backend after completion, with a
   `<key>.meta.json` file (block hashes, model id, wpb, timestamp) written
   to `META_DIR`.
3. **Restore candidates** — before a big request, the proxy scans the meta
   files and looks for one with the same model and `wpb` whose block list
   shares the longest common prefix; if the LCP ratio is at least `LCP_TH`
   (default 0.1), that cache is restored into the chosen slot *before* the
   chat is dispatched.
4. **Slot management** — every llama.cpp slot is guarded by a lock
   The proxy picks a never-used slot first, otherwise the
   least recently used one, waits up to 300 s for it (`503 all slots busy`
   on timeout), and releases it when the response is fully handled.
5. **Streaming** — SSE responses are passed through byte-for-byte via a
   background reader task; the reader's cleanup step always saves the KV
   cache and writes the meta file, then releases the slot.
6. **Bounded cache** — after each save the proxy keeps at most `META_MAX`
   (default 10) meta files: the oldest ones (by last save/restore
   timestamp) are deleted, together with the KV files of the same name in
   the configured `--slot-save-path` directories. `META_MAX=0` disables
   pruning. A later restore of a pruned key simply fails and falls back to
   a full prefill.

## Stack

Rust 2024, `axum` 0.8 + `tokio` + `reqwest` (rustls) + `serde` + `sha2` +
`tracing`. The slot pin is duplicated in the request body root, `options`
and the query string ((llama.cpp accepts it in several places depending on the build).

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
| `LOG_LEVEL`         | `INFO`              | log level (`RUST_LOG` overrides it)            |
| `STREAM_QUEUE_SIZE` | `16`                | per-request SSE channel capacity (must be >= 1) |
| `COALESCE_REQUESTS` | `false`             | group concurrent same-cache-key requests into one backend call (Rust-only) |

Every variable is also available as a command-line flag (see
`./target/release/lpcache --help`): `--backends`, `--llama-url`,
`--n-slots`, `--words-per-block`, `--big-threshold-words`, `--lcp-th`,
`--meta-dir`, `--meta-max`, `--slot-save-path`, `--request-timeout`,
`--model-id`, `--port`, `--api-key` (→ `LLAMA_API_KEY`), `--log-level`,
`--stream-queue-size`, `--coalesce-requests`.
`-h`/`--help` shows the full help; `-V`/`--version` prints the proxy version
(also logged in the `app_start` line at startup).
Explicit flags take precedence over environment variables, which take
precedence over the built-in defaults:

```bash
./target/release/lpcache --llama-url http://127.0.0.1:8000 --n-slots 1 \
  --port 8081 --meta-dir ./kv_meta --log-level INFO
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
cargo test
```

- **Unit tests** (67): config parsing/defaults (incl. `META_MAX` /
  `SLOT_SAVE_PATH` / `STREAM_QUEUE_SIZE`, CLI-over-env precedence), raw-prefix
  construction, word tokenization, block hashing, LCP, SHA256 vectors, meta
  file round-trips, candidate filtering/thresholds, LRU pruning, slot
  selection (free/oldest ordering, idle-aware, multi-backend, circuit
  breaker, escalating backoff, success/probe recovery, retry slot
  exclusion), per-slot locking, acquire timeout, save semantics.
- **Integration tests** (40): `LlamaClient` against the mock backend
  (slot pinning in body/options/query, save/restore status codes, JSON vs
  non-JSON provider answers, streaming), and end-to-end proxy behaviour
  (small vs big requests, meta file creation, restore on the second big
  request, SSE passthrough, provider error mapping, 422 on bad JSON, dead
  backend failover, transparent retry on connection failure, probe recovery
  of a cooled-down backend, failing-slot non-pinning, LRU meta+KV pruning,
  streaming with a minimal `STREAM_QUEUE_SIZE` on the leader and
  coalesced-follower paths).

`cargo clippy --all-targets` and `cargo fmt --check` are clean.

