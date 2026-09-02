# Llama Proxy Cache (lpcache) — Setup Guide

Step-by-step guide to deploying the Llama Proxy Cache `lpcache` proxy in front of one
or more **llama.cpp** servers, or smoke-testing it with the built-in mock
backend (no model or GPU required).

```
client (OpenAI API)                llama.cpp backend(s)
        │                                ▲
        ▼                                │ slot pin in body/options/query
┌────────────────────────────────────────────────────────┐
│ lpcache (this project)                              │
│  • prefix hashing  • big/small  • LCP restore lookup   │
│  • slot locks (free-first, then LRU)  • SSE passthrough│
│  • prefilter accept/reject (optional, PREFILTER_BLOCKLIST) │
│  • saves KV cache + writes <key>.meta.json            │
└────────────────────────────────────────────────────────┘
```

---

## 1. Prerequisites

| Requirement            | Notes                                                                 |
|------------------------|-----------------------------------------------------------------------|
| Rust (stable)          | `rustup` toolchain; `cargo` in `PATH`. Recent stable recommended.    |
| llama.cpp server       | A build with **slot save/restore** support (the `--slot-save-path` flag and `POST /slots/{id}?action=save\|restore` endpoint). A recent release build is fine. |
| A GGUF model           | e.g. `model.gguf` in a directory the server can read.               |
| Ports                  | One port per llama.cpp server (default `8000`), one for the proxy (default `8081`). |

Check your llama.cpp build supports slot save/restore:

```bash
llama-server --help | grep -i slot-save
# expect: --slot-save-path <path>
```

If the flag is missing, rebuild from a recent llama.cpp release.

---

## 2. Build the proxy

```bash
git clone https://github.com/kokleong98/llama-proxy-cache.git lpcache-rs
cd lpcache-rs
cargo build --release
# binary: target/release/lpcache
```

Optional quality gate (CI-style):

```bash
cargo fmt --check
cargo clippy --all-targets
cargo test
```

---

## 3. Start llama.cpp with slot save/restore enabled

```bash
mkdir -p /var/kvcache

llama-server \
  -m /models/model.gguf \
  -np 4 \
  --slot-save-path /var/kvcache \
  --host 0.0.0.0 \
  --port 8000
```

| Flag                 | Meaning                                                          |
|----------------------|------------------------------------------------------------------|
| `-np 4`              | slot pool size — **must equal `n_slots` in the proxy config**    |
| `--slot-save-path`   | directory where the real KV `.bin` files are written/read by basename |
| `--host`/`--port`    | where the proxy's `BACKENDS` URL points at                      |

> **Note:** without `--slot-save-path`, save/restore calls fail (500) and the
> proxy logs `save_slot_500` — the proxy itself keeps working, but caches are
> never persisted and restore never happens.

For multi-GPU/multi-machine deployments, start one `llama-server` per backend
(on different ports or hosts).

---

## 4. Run the proxy

### 4.1 Single backend (env-var fallback)

```bash
LLAMA_URL=http://127.0.0.1:8000 \
N_SLOTS=1 \
PORT=8081 \
META_DIR=/var/kv_meta \
LOG_LEVEL=INFO \
./target/release/lpcache
```

The same settings can be passed as command-line flags instead of
environment variables (flags win when both are given):

```bash
./target/release/lpcache \
  --llama-url http://127.0.0.1:8000 \
  --n-slots 1 \
  --port 8081 \
  --meta-dir /var/kv_meta \
  --log-level INFO
```

### 4.2 Multiple backends (`BACKENDS` JSON)

```bash
BACKENDS='[
  {"url":"http://127.0.0.1:8000","n_slots":1,"slot_save_path":"/var/kvcache"},
  {"url":"http://127.0.0.1:8001","n_slots":1,"slot_save_path":"/var/kvcache"}
]' \
PORT=8081 \
META_DIR=/var/kv_meta \
./target/release/lpcache
```

When `BACKENDS` is set, `LLAMA_URL`/`N_SLOTS`/`SLOT_SAVE_PATH` are ignored.

The optional per-backend `slot_save_path` (same value as that
`llama-server`'s `--slot-save-path` flag) tells the proxy where the KV
files live, so it can remove them when it prunes old cache entries (see
`META_MAX`). Without it, pruning only removes the small `.meta.json`
index files. A configured directory that is missing at startup logs a
`slot_save_path … does not exist` warning — KV-file pruning is skipped
there until the directory exists.

**Multi-backend safety:** give every `llama-server` its **own**
`--slot-save-path` directory (e.g. `/var/kv/be0`, `/var/kv/be1`). Several
llama.cpp processes sharing one directory write/rewrite the same KV
files non-atomically; a save concurrent with a restore of the same key
can corrupt the file or even crash the server (llama.cpp-side issue).
The proxy copes with a dead backend — a connection failure trips a
circuit breaker (5 s, escalating 10 s / 20 s / 40 s / 60 s on repeated
failures), the failing request is retried once on another backend, and
the proxy keeps serving on the remaining backends. Cooling backends are
probed every second, so a restarted backend rejoins within about a second
of coming back up. Per-instance directories
are the safe setup. The meta index (`META_DIR`) is shared, so a restore
candidate may point at a KV file that only exists in another backend's
directory; in that case the restore fails and the request proceeds with
a full prefill (no data loss, just a slower request).

Expected startup logs:

```
client_init url=http://127.0.0.1:8000 api_key=false
client_init url=http://127.0.0.1:8001 api_key=false
slot_manager n_backends=2 total_slots=6
app_start version=0.2.0 n_backends=2 port=8081 meta_max=10 stream_queue=16
listening on 0.0.0.0:8081
```

### 4.3 Configuration reference

| Variable              | Default                    | Meaning                                             |
|-----------------------|----------------------------|-----------------------------------------------------|
| `BACKENDS`            | —                          | JSON array `[{"url":"...","n_slots":N}, ...]`      |
| `LLAMA_URL`           | `http://127.0.0.1:8000`    | single-backend fallback when `BACKENDS` is unset    |
| `N_SLOTS`             | `1`                        | slot count for the fallback backend                 |
| `WORDS_PER_BLOCK`     | `100`                      | words per hash block (must be stable — it is part of the restore filter) |
| `BIG_THRESHOLD_WORDS` | `500`                      | requests above this word count are "big" (saved)    |
| `LCP_TH`              | `0.1`                      | min. shared-block ratio (0..1) to restore a cache   |
| `META_DIR`            | `./kv_meta` (relative to CWD) | where `<key>.meta.json` files live — use an absolute path |
| `META_MAX`            | `10`                        | max meta files kept; oldest are pruned (with their KV files) after each save — 0 disables pruning |
| `SLOT_SAVE_PATH`      | —                            | the backend's `--slot-save-path` dir (single-backend fallback); where pruned KV files are removed from |
| `REQUEST_TIMEOUT`     | `600` (s)                  | per-request timeout to the backend                  |
| `MODEL_ID`            | `llama.cpp`                | id advertised by `/v1/models` and used as the default `model` field |
| `PORT`                | `8081`                     | proxy listen port (binds `0.0.0.0`)                 |
| `LOG_LEVEL`           | `INFO`                     | `TRACE..ERROR`; `RUST_LOG` overrides it             |
| `STREAM_QUEUE_SIZE`   | `16`                       | capacity of the per-request SSE channel that buffers streamed bytes between the background reader and the HTTP response (must be >= 1; smaller values backpressure the backend faster) |
| `COALESCE_REQUESTS`   | `false`                    | group concurrent same-cache-key requests into one backend call (Rust-only) |

Every variable also has a command-line flag; explicit flags take
precedence over environment variables, which take precedence over the
built-in defaults. `--help` prints the full list with defaults:

| Flag                          | Env var             |
|-------------------------------|---------------------|
| `--backends <JSON>`           | `BACKENDS`          |
| `--llama-url <URL>`           | `LLAMA_URL`         |
| `--n-slots <N>`               | `N_SLOTS`           |
| `--words-per-block <N>`       | `WORDS_PER_BLOCK`   |
| `--big-threshold-words <N>`   | `BIG_THRESHOLD_WORDS` |
| `--lcp-th <F>`                | `LCP_TH`            |
| `--meta-dir <PATH>`           | `META_DIR`          |
| `--meta-max <N>`              | `META_MAX`          |
| `--slot-save-path <PATH>`     | `SLOT_SAVE_PATH`    |
| `--request-timeout <SECS>`    | `REQUEST_TIMEOUT`   |
| `--model-id <ID>`             | `MODEL_ID`          |
| `--port <PORT>`               | `PORT`              |
| `--api-key <KEY>`             | `LLAMA_API_KEY`     |
| `--log-level <LEVEL>`         | `LOG_LEVEL`         |
| `--stream-queue-size <N>`     | `STREAM_QUEUE_SIZE` |
| `--coalesce-requests <BOOL>`  | `COALESCE_REQUESTS` |

The cache key is `sha256(backend_model_id + "\n" + prefix)`, where
`backend_model_id` is the model id reported by the **first** configured
backend — so in a multi-backend setup all backends should run the same
model. The same `META_DIR` can be shared by all backends.

---

## 5. Point your client at the proxy

Clients keep using the OpenAI chat-completions API — just change the base URL
to the proxy:

```bash
curl http://127.0.0.1:8081/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"llama.cpp","stream":true,"messages":[{"role":"user","content":"Hello!"}]}'
```

IDE example (OpenAI-compatible provider in Cline/Roo/Continue/etc.):

- Base URL: `http://<proxy-host>:8081/v1`
- API key: any non-empty string (ignored)
- Model: `llama.cpp` (or whatever you set `MODEL_ID` to)

---

## 6. Verify caching actually works

Send the **same long prompt twice** (each with > `BIG_THRESHOLD_WORDS` words —
e.g. paste a long document, then the document + one extra question).

Watch the proxy log for:

```
# first request (no cache yet):
before_acquire is_big=true restore_key=
json_done g=(0, 0) key=... saved=true is_big=true dur_ms=...

# second request (shared prefix):
restore_candidate basename=... ratio=1.000
after_acquire g=(0, 0) restored=Some(Restored)
json_done g=(0, 0) key=... saved=true is_big=true dur_ms=...
```

Check the artifacts on disk:

```bash
# proxy-side descriptors (one per distinct big prompt):
ls /var/kv_meta/*.meta.json | head

# real KV cache files (owned by llama.cpp):
ls -lh /var/kvcache
```

A successful restore logs `restore_before_chat ... ok=true`; the second
request should finish noticeably faster (KV restore takes seconds instead of
minutes of full prefill for large contexts).

---

## 7. Smoke test without a model (mock backend)

The project ships a mock llama.cpp server implementing `/v1/models`,
`/v1/chat/completions` (JSON + SSE) and `/slots/{id}?action=save|restore`:

```bash
# terminal 1 — mock backend on :8100 (optional 3rd arg: KV save dir)
cargo run --example mock_llama -- 8100 mock-model

# terminal 2 — the proxy in front of it
BACKENDS='[{"url":"http://127.0.0.1:8100","n_slots":2}]' \
PORT=8091 META_DIR=/tmp/kv_meta LOG_LEVEL=DEBUG cargo run
```

Then, with a >500-word prompt in the messages, send the prompt twice
(second time with a few extra words appended):

```bash
# first:  expect "json_done ... saved=true"   and a new file in /tmp/kv_meta
# second: expect "restore_candidate ... ratio=1.000"
#         and    "restore_before_chat ... ok=true"
```

This is the same flow the test-suite validates; the identical flows are
covered by `cargo test` (125 tests) against the in-process mock.

---

## 8. Run as a systemd service (optional)

`/etc/systemd/system/lpcache.service`:

```ini
[Unit]
Description=lpcache — KV-cache-aware llama.cpp proxy
After=network.target

[Service]
Type=simple
User=llama
WorkingDirectory=/opt/lpcache
Environment=BACKENDS=[{"url":"http://127.0.0.1:8000","n_slots":4}]
Environment=PORT=8081
Environment=META_DIR=/var/kv_meta
Environment=LOG_LEVEL=INFO
ExecStart=/opt/lpcache/lpcache
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
```

```bash
systemctl daemon-reload
systemctl enable --now lpcache
journalctl -u lpcache -f
```

---

## 9. Tuning tips

- **`BIG_THRESHOLD_WORDS`** — raise it (e.g. 2000) if most prompts are short
  but you still want the truly long ones saved; lower it to persist more.
- **`LCP_TH`** — `0.1` is the default. Raise toward `0.8`+ if you see weak
  matches being restored (wastes the restore); lower toward `0.05` if good
  candidates are being skipped.
- **`n_slots`** — keep it equal to the backend's `-np`. Over-stating it makes
  the proxy pin slots that don't exist; under-stating it wastes slots.
- **Cache size** — the proxy keeps at most `META_MAX` meta files (default
  10) and prunes the oldest after each save; set it lower to bound disk
  use (each KV file can be hundreds of MB). To also prune the KV files,
  set each backend's `slot_save_path` / `SLOT_SAVE_PATH`. Candidate scan
  cost is O(meta files), so a smaller bound is also faster.
- **`WORDS_PER_BLOCK`** — changing it orphans existing caches (the restore
  filter skips mismatching `wpb`); only change it before caches are needed.
- **`PREFILTER_BLOCKLIST`** — optional comma-separated keyword blocklist;
  matching requests are rejected with `400` before any slot/backend work
  (no KV cost at all). Matching is a plain case-insensitive substring by
  default; set `PREFILTER_CASE_INSENSITIVE=false` for exact-case matches.
  Unset/blank disables the prefilter entirely.

## 10. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `503 all slots busy, please retry later` | All slots locked for > 300 s — a client is stuck mid-stream, or `n_slots` exceeds the real `-np`. Check the `llama-server` logs. |
| `backend_down be=N` / `chat_error` warnings (usually no client-visible errors) | Backend N became unreachable (crash, OOM, restart). The proxy skips it for 5 s (circuit breaker; 10/20/40/60 s on repeated failures), transparently retries the failing request once on another backend, and probes it every second — a recovered backend rejoins within ~1 s (`backend_up be=N`). A 500 is only returned when no other backend could serve the request. Check that backend's log / restart it. |
| `save_slot_500` warnings, restore never happens | llama.cpp started without `--slot-save-path`, or an old build without the slot save/restore API. |
| `restore_before_chat ... ok=false` | The KV `.bin` for that key is missing from `--slot-save-path` (deleted, or a different server/build). The request proceeds without the restore. When the backend *rejects* the restore (HTTP 400) and the KV file is absent from **every** configured slot-save dir, the proxy also removes the stale meta entry (`stale_meta_removed` in the proxy log) so later requests stop retrying the doomed restore; if the file still exists somewhere (or the dir isn't visible from the proxy's host), the meta is kept. |
| No `restore_candidate` on repeated prompts | Prompt below `BIG_THRESHOLD_WORDS` words, `LCP_TH` too high, different model, or different `WORDS_PER_BLOCK` than when the cache was saved. |
| `422` from the proxy | Malformed JSON body. |
| `400` `request blocked by keyword prefilter: "..."` | `PREFILTER_BLOCKLIST` matched the request's message contents; the request never reached the backend. Adjust/remove the keyword, or unset `PREFILTER_BLOCKLIST` to disable the prefilter. |
| `502` `provider non-JSON body` | The backend answered 200 but with a JSON body that is not an object (e.g. an array); the response is unusable. |
| 200 response with `{"object":"error", ...}` payload | The backend answered 200 with a non-JSON body; the raw snippet is included. |
| Logs too quiet | `LOG_LEVEL=DEBUG` (or `RUST_LOG=lpcache=debug`). Key lines: `before_acquire`, `after_acquire`, `dispatch`, `restore_candidate`, `restore_before_chat`, `json_done`, `stream_reader_done`. With `COALESCE_REQUESTS` on, also `coalesce_lead`/`coalesce_join`. |


