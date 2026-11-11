# lpcache — Parameters Reference

All tunable parameters for the `lpcache` proxy (CLI flags **or** env
vars — CLI > env > built-in default), plus the matching
**llama-server** flags the proxy depends on.

The proxy also carries this summary as **embedded help**:

```sh
lpcache --help
```

---

## 1. Llama Proxy Cache (`lpcache`)

Every parameter is settable as an **env var** or a **command-line flag**.
Explicit flags take precedence over environment variables, which take
precedence over the built-in defaults. `-h` / `--help` prints the built-in help.

| Env var | CLI flag | Default | Meaning |
|---|---|---|---|
| `BACKENDS` | `--backends <JSON>` | — | JSON array `[{"url": "...", "n_slots": N, "slot_save_path": "...", "kv_cache": true}]` for multiple backends; `slot_save_path` and `kv_cache` are optional per backend. `kv_cache` (default `true`) gates KV save/restore for that backend — set `false` for a backend without slot save/restore support (e.g. llama-server < 1.05.40): the proxy never sends `restore_cache_slot` to it, and no KV save / meta file is written for a request it serves. A parse failure logs a warning and yields **no** backends. |
| `LLAMA_URL` | `--llama-url <URL>` | `http://127.0.0.1:8000` | Fallback single backend when `BACKENDS` is unset |
| `N_SLOTS` | `--n-slots <N>` | `1` | Slot count for the fallback backend |
| `WORDS_PER_BLOCK` | `--words-per-block <N>` | `100` | Words per SHA256 hash block used for prefix hashing |
| `BIG_THRESHOLD_WORDS` | `--big-threshold-words <N>` | `500` | Word count above which a request is "big": runs with `cache_prompt=true`, KV cache saved to backend afterwards, meta file written |
| `LCP_TH` | `--lcp-th <F>` | `0.1` | Min. shared-block ratio (0..1) for a restore candidate to be restored before dispatch |
| `META_DIR` | `--meta-dir <PATH>` | `kv_meta` | Meta file directory, joined to the **current working directory** |
| `META_MAX` | `--meta-max <N>` | `10` | LRU bound on meta files kept after each save; oldest pruned along with same-named KV files in the slot-save dirs; `0` = unlimited |
| `SLOT_SAVE_PATH` | `--slot-save-path <PATH>` | — | Backend `--slot-save-path` dir (single-backend fallback only; with `BACKENDS` use per-backend `slot_save_path`). Pruned meta files' KV files are removed from here. Missing/unwritable dir → pruning skipped, proxy still runs |
| `REQUEST_TIMEOUT` | `--request-timeout <SECS>` | `600` | Per-request timeout to the backend, seconds |
| `MODEL_ID` | `--model-id <ID>` | `llama.cpp` | Model id advertised by `GET /v1/models` |
| `PORT` | `--port <PORT>` | `8081` | Proxy listen port |
| `LLAMA_API_KEY` | `--api-key <KEY>` | — | Sent to the backend as `Authorization: Bearer <KEY>` (llama-server `--api-key`). Blank/whitespace-only = no header |
| `LOG_LEVEL` | `--log-level <LEVEL>` | `INFO` | `TRACE`..`ERROR`; **`RUST_LOG` env var overrides it when set** |
| `STREAM_QUEUE_SIZE` | `--stream-queue-size <N>` | `16` | Capacity of the per-request bounded channel that buffers streamed SSE bytes between the background reader and the HTTP response. Must be `>= 1` (`0`/invalid fall back to the default with a warning). Smaller values backpressure a slow client faster; the default is 16 |
| `COALESCE_REQUESTS` | `--coalesce-requests <BOOL>` | `false` | Group concurrent requests with the same KV cache key into one backend call (followers receive the leader's result, regardless of generation parameters) |
| `PLUGINS` | `--plugins <JSON>` | — | JSON **array** of plugin specs `[{"name": "...", "params": {...}}]` loaded from the `lpcache-plugins` registry at runtime — built-ins: `keyword`, `qwen_guard`, `result_cache`, `chain`, `llama`, `request_log`, `dashboard`. Full per-plugin parameter reference: see the **Plugins** subsection below. |
| `REQUEST_LOG` | `--request-log <BOOL>` | `false` | When true, one [W3C-XLF](https://www.w3.org/TR/xlf-10/) record per **finished** request (successes, errors and client disconnects — the record is written when the response body completes or is dropped) is appended to `REQUEST_LOG_PATH` — the built-in `request_log` plugin |
| `REQUEST_LOG_PATH` | `--request-log-path <PATH>` | `lpcache-request.log` | Request-log file location; relative paths resolve against the **current working directory**; the parent dir is created at startup |
| `REQUEST_LOG_MAX_BYTES` | `--request-log-max-bytes <N>` | `10485760` (10 MiB) | Rotation threshold in bytes: the current file is renamed to `<path>.1` and older generations shift upward (`<path>.2`, …); the fresh current file starts with a new XLF header. `0` = no rotation |
| `REQUEST_LOG_KEEP` | `--request-log-keep <N>` | `5` | Rotated generations retained (the oldest is dropped); `0` = keep them all |
| `REQUEST_LOG_FIELDS` | `--request-log-fields <LIST>` | all 31 fields | Comma-separated **ordered** subset of the known XLF field names — W3C fields `datetime`, `cs-method`, `cs-uri`, `c-ip`, `cs(User-Agent)`, `cs-bytes`, `cs-time`, `sc-status`, `sc-bytes` plus the lpcache pipeline fields `lpcache-model`, `lpcache-stream`, `lpcache-key`, `lpcache-n-words`, `lpcache-is-big`, `lpcache-backend`, `lpcache-slot`, `lpcache-restored`, `lpcache-cache-hit`, `lpcache-coalesced`, `lpcache-reason` and the lpcache performance fields `lpcache-prompt-tokens`, `lpcache-completion-tokens`, `lpcache-cached-tokens` (backend `usage` — the last one is the prompt tokens served from the backend's KV cache), `lpcache-prefill-ms`, `lpcache-gen-ms`, `lpcache-prefill-tps`, `lpcache-gen-tps` (backend `timings`) and `lpcache-ttft-ms` (proxy-side time to first token, streaming only), and the lpcache resource fields `lpcache-cpu-ms`, `lpcache-disk-read-bytes`, `lpcache-disk-write-bytes` (the proxy process's own CPU time and disk I/O during the request; process-wide counters, so approximate under concurrency; `-` where the platform cannot sample them). Missing values render as `-`; values containing whitespace/quotes are quoted+escaped per XLF. `lpcache-reason` is `-` on success, otherwise the pipeline outcome (`prefilter_reject`, `slot_timeout`, `backend_unreachable`, `backend_error`, `provider_http_<status>`, `provider_non_object`, `coalesce_leader_failed`, `save_failed`). Log I/O failures are swallowed — logging never breaks serving. Caveats: `c-ip` needs a real socket connection (Axum `ConnectInfo`); `cs-bytes` is taken from `Content-Length` (chunked requests without one log `-`) |
| `PREFILTER_BLOCKLIST` | `--prefilter-blocklist <LIST>` | — | **Legacy** (synthesized into a `keyword` plugin when `PLUGINS` is unset). Comma-separated keywords (surrounding whitespace ignored, empty entries dropped). When set, requests whose message contents (string content or text content parts) contain any keyword are **rejected with `400` before any slot/backend work** — no slot is acquired, no KV cache is restored/saved, no meta file is written, and the request never joins a coalescing group. Unset/blank = no prefilter |
| `PREFILTER_CASE_INSENSITIVE` | `--prefilter-case-insensitive <BOOL>` | `true` | **Legacy**: prefilter keyword matching is case-insensitive |
| `PREFILTER_RESULT_CACHE_DIR` | `--prefilter-result-cache-dir <PATH>` | — | **Legacy** (synthesized into a `result_cache` plugin when `PLUGINS` is unset). The **cache-result path** (dir holding one `{key}.json` per request KV cache key). When set, the result-cache prefilter answers fresh (non-expired) same-key requests with a previously cached backend result **before any slot/backend work** — no slot, no restore/save, no meta file, no coalescing group; fresh non-streaming 200 JSON results are stored under the key after the backend call; streaming requests are never served from the cache. Unset/blank = disabled |
| `PREFILTER_RESULT_CACHE_TTL` | `--prefilter-result-cache-ttl <SECS>` | `300` | **Legacy**: per-entry result-cache expiry in seconds; expired entries are removed lazily on read; `0` = entries never expire |
| `SAVE_RETRIES` | `--save-retries <N>` | `3` | Retries after a **failed KV cache save** (backend `500` or network/other error) before giving up; a `save_retry` warning is logged per attempt. `0` = no retry. The final outcome is unchanged: backend `500` after all attempts → save reported as failed; `Err` after all attempts → request `500` |
| `SAVE_RETRY_DELAY_MS` | `--save-retry-delay-ms <MS>` | `1000` | Delay in milliseconds between KV cache save retry attempts |
| — | `-V`, `--version` | — | Print the proxy version (from `Cargo.toml`) and exit; the version is also logged in the `app_start` line at startup |
| — | `-h`, `--help` | — | Show help and exit |

Parsing rules:

- Both `--flag value` and `--flag=value` forms are accepted (the `=` form
  splits on the first `=` only, so values containing `=` survive).
- Unknown flags and flags with a missing value are **errors**.
- Invalid numeric/boolean values log a warning and fall back to the default.
- Booleans accept `1/true/yes/on` and `0/false/no/off` (case-insensitive).

Example:

```sh
lpcache --port 8090 --llama-url http://127.0.0.1:8081 --n-slots 4 \
           --api-key 777 --meta-max 500 --lcp-th 0.5
```

### 1.1 Plugins (`PLUGINS` / `--plugins`)

`PLUGINS` loads one or more plugins from the `lpcache-plugins` registry by
name, at runtime. Each entry is a **plugin spec**:

```json
{ "name": "<plugin name>", "params": { ... } }
```

and the whole value is a JSON **array** of such specs. Which plugins run is
decided entirely by configuration, not by compile-time wiring — so a custom
plugin crate can register new names and become loadable from configuration
like any built-in.

Rules:

- **Prefilter** specs (`keyword`, `qwen_guard`, `result_cache`, `chain`) are
  **chained in list order**; the first non-`Accept` decision (a `Reject` or a
  `Serve`) wins. An empty array (`[]`) is valid and disables the prefilter.
- **`PLUGINS` vs. legacy `PREFILTER_*`:** **`PLUGINS` takes precedence** over
  the legacy `PREFILTER_*` variables below — when `PLUGINS` is **set**, the
  prefilter specs are used as-is and the `PREFILTER_*` settings are ignored;
  when **unset**, they are synthesized into specs — a non-empty
  `PREFILTER_BLOCKLIST` → a `keyword` spec, `PREFILTER_RESULT_CACHE_DIR` → a
  `result_cache` spec.
- **Request logging:** a `request_log` **spec** in `PLUGINS` takes precedence
  over the `REQUEST_LOG*` variables (its params are used as-is, the
  `REQUEST_LOG*` settings are ignored); but when `PLUGINS` has **no**
  `request_log` spec — whether set or unset — `REQUEST_LOG=true` still
  synthesizes a `request_log` spec from the `REQUEST_LOG_*` values and appends
  it.
- The **`llama` backend client is not selected here**: one built-in `llama`
  plugin client is created per backend from `BACKENDS` / `LLAMA_URL`,
  `LLAMA_API_KEY` and `REQUEST_TIMEOUT`. It is listed below for reference and
  for embedded use, where a `llama` spec can be loaded explicitly.
- At most **one** `request_log` and one `dashboard` plugin may be configured.
- Invalid specs are **startup errors** — the proxy prints the offending entry
  and exits with code 2 before binding its port; per-plugin validation rules
  and the exact error-message formats are documented in
  [PLUGINS.md §2.5](PLUGINS.md#25-parameter-validation-and-startup-errors).

#### `keyword` — keyword blocklist prefilter

Rejects a request when the combined message text (string content and text
content parts) contains any blocked keyword (plain substring match).
Rejections are `400` naming the first matching keyword; an empty keyword list
yields no plugin.

| Param | Default | Meaning |
|---|---|---|
| `keywords` (required) | — | Array of keyword strings, or a comma-separated string (`"a, b"`); surrounding whitespace is trimmed and empty entries dropped |
| `case_insensitive` | `true` | `true` matches keywords and text lowercased; `false` is exact case |

#### `qwen_guard` — Qwen3Guard-Gen content-safety prefilter

Runs a **Qwen3Guard-Gen** moderation model (hosted on a separate llama-server
or any OpenAI-compatible endpoint) over the request's combined message text in
prompt-moderation mode and applies the configured allow/reject policy. A
non-2xx guard response (e.g. llama-server's `503` while the model is still
loading) is a *failed check* (follows `on_error`), never an *unparseable* one.
Rejections are `400` before any slot/backend work.

| Param | Default | Meaning |
|---|---|---|
| `url` (required) | — | Base URL of the server hosting the guard model; the plugin POSTs to `{url}/v1/chat/completions` (single `user` message, `max_tokens: 64`, `temperature: 0`) |
| `api_key` | — | Sent as `Authorization: Bearer <key>` |
| `model` | — | Model id in the chat-completions request; when omitted the `model` field is left out and the guard server's default model is used |
| `timeout_secs` | `30` | Per-check timeout |
| `max_input_chars` | `0` | Cap, in **characters** (not bytes), on the flattened text sent to the guard; `0` = no truncation (guard models have small contexts) |
| `reject_levels` | `["Unsafe"]` | Severity levels that reject outright — a JSON array or a comma-separated string of `Safe`/`Controversial`/`Unsafe` (case-insensitive) |
| `reject_categories` | `[]` | Categories that reject when present in the verdict (independent of the level) — a JSON array or a comma-separated string; any of the nine Qwen3Guard categories below (case-insensitive; `PII` also matches "Personally Identifiable Information") |
| `on_error` | `"accept"` | `"accept"` or `"reject"` (aliases `"allow"`/`"block"`) — what to do when the guard call fails (transport/HTTP/timeout) |
| `on_unparseable` | `"accept"` | `"accept"` or `"reject"` (aliases `"allow"`/`"block"`) — what to do when the guard responds but its output cannot be parsed |
| `cache_max` | `0` | Bounded LRU of verdicts (accepts and rejects) keyed by the request's KV cache key; `0` = off (repeated same-key prompts skip the guard call) |

The nine Qwen3Guard categories: `Violent`, `Non-violent Illegal Acts`,
`Sexual Content or Sexual Acts`, `PII`, `Suicide & Self-Harm`, `Unethical
Acts`, `Politically Sensitive Topics`, `Copyright Violation`, `Jailbreak`
(input only).

#### `result_cache` — file-based result cache + cached-result prefilter

Stores completed non-streaming backend results on disk (one `{key}.json` per
request KV cache key) and answers fresh (non-expired) same-key requests with a
previously cached result **before any slot/backend work** — no slot, no
restore/save, no meta file, no coalescing group. The proxy stores fresh
non-streaming 200 JSON results under the key after the backend call; streaming
requests are never served from the cache unless `cache_streaming` is enabled.

| Param | Default | Meaning |
|---|---|---|
| `dir` (required) | — | Cache-result path (dir holding the `{key}.json` entries); created on first write |
| `ttl_secs` | `300` | Per-entry expiry in seconds; `0` = entries never expire |
| `cleanup` | — | `"remove_after_serve"` attaches the built-in one-shot cleanup (the entry file is deleted after it is served); omitted/`null` = no cleanup; any other value is a startup error |
| `cache_streaming` | `false` | When true, completed streaming results are aggregated into a non-streaming completion and cached, and cached entries are served to streaming clients as SSE |
| `cache_control` | `false` | When true, requests whose `Cache-Control` header carries the `no-cache` directive (case-insensitive, e.g. `no-cache` or `no-cache, no-store`) are forwarded to the backend instead of being answered from the cache |

#### `chain` — run nested prefilters in order

Runs several prefilter plugins in order; the first non-`Accept` decision wins.
The proxy synthesizes a chain automatically when several prefilter specs are
configured, but a `chain` spec also lets you nest arbitrary (custom) plugins
explicitly.

| Param | Default | Meaning |
|---|---|---|
| `filters` (required) | — | Array of nested plugin specs `[{"name": "...", "params": {...}}, ...]`, resolved against the same registry (in evaluation order) |

Example:

```json
{ "name": "chain",
  "params": { "filters": [
    { "name": "keyword", "params": { "keywords": ["a"] } },
    { "name": "result_cache", "params": { "dir": "/var/rc" } }
  ] } }
```

#### `llama` — llama.cpp backend client

The reqwest-based HTTP client for one llama.cpp backend (chat stream/JSON,
slot save/restore, model id). **Not selected via `PLUGINS`** — the proxy
creates one per backend automatically; a `llama` spec is only used in embedded
setups that load a backend explicitly.

| Param | Default | Meaning |
|---|---|---|
| `url` (required) | — | The backend base URL |
| `api_key` | — | Sent as `Authorization: Bearer <key>` (llama-server's `--api-key`) |
| `timeout_secs` | `600` | Per-request timeout |

#### `request_log` — W3C-XLF request logger

One [W3C Extended Log Format (XLF)](https://www.w3.org/TR/xlf-10/) record per
finished request (success, error, rejection or disconnect) to a size-rotated
log file. A `request_log` spec in `PLUGINS` takes precedence over the
`REQUEST_LOG*` variables — when one is present, the `REQUEST_LOG*` settings are
ignored. See the `REQUEST_LOG*` rows above for the full field list.

| Param | Default | Meaning |
|---|---|---|
| `path` | `lpcache-request.log` | Log file location; relative paths resolve against the **current working directory**; the parent dir is created |
| `max_bytes` | `10485760` (10 MiB) | Rotation threshold in bytes; `0` = no rotation |
| `keep` | `5` | Rotated generations retained (the oldest is dropped); `0` = keep them all |
| `fields` | all 31 fields | Ordered list of field names to emit — any subset of the known XLF fields (see `REQUEST_LOG_FIELDS`); omitted = all 31. An empty list, an unknown name, or a duplicate is a startup error |

#### `dashboard` — real-time statistics web dashboard

Serves a self-contained HTML page (inline CSS + JS, no external assets) plus a
JSON stats endpoint, fed by one record per finished request (the same records
the request logger receives). The dashboard's own routes are excluded from the
aggregates and the request log. At most one `dashboard` plugin. Unlike the
other plugins it **rejects unknown params** — a typo'd param name is a startup
error.

| Param | Default | Meaning |
|---|---|---|
| `path` | `/dashboard` | Page URL path (must start with `/`); the stats API is mounted at `{path}/api/stats` |
| `refresh_ms` | `2000` | The page's polling interval (100..=3600000 ms; out-of-range = startup error) |
| `window_secs` | `300` | Trailing window for a stats poll without `?since` (10..=86400 s) |
| `bucket_secs` | `5` | Time-series bucket size (1..=3600 s) |
| `max_recent` | `100` | Recent-requests ring buffer size (0..=10000; `0` = no recent list) |

Example:

```sh
lpcache --plugins '[
  {"name": "keyword",      "params": {"keywords": ["forbidden"]}},
  {"name": "result_cache", "params": {"dir": "./result_cache", "ttl_secs": 600}},
  {"name": "request_log",  "params": {"path": "./lpcache.log", "max_bytes": 5242880}},
  {"name": "dashboard",    "params": {"path": "/dashboard"}}
]'
```

---

## 2. Matching llama-server flags

The proxy only works correctly when the backend is started with the
corresponding flags:

| llama-server flag | Must match proxy | Why |
|---|---|---|
| `--slot-save-path <dir>` | `SLOT_SAVE_PATH` (or per-backend `slot_save_path` in `BACKENDS`) | The proxy restores/saves slot KV files and prunes old ones from this dir |
| `-np` / `--parallel N` | `N_SLOTS` (or per-backend `n_slots` in `BACKENDS`) | Number of slots the proxy will pin to |
| `--api-key <key>` | `LLAMA_API_KEY` | Backend rejects unauthenticated requests otherwise |
| `--port` / `--host` | `LLAMA_URL` | Where the proxy sends chats/saves/restores |
| (slot save/restore support) | per-backend `kv_cache` in `BACKENDS` (default `true`) | Set `false` for a backend that cannot save/restore slot KV files — the proxy then skips the restore lookup targeting it and the post-completion KV save + meta write for requests it serves |

Example:

```sh
llama-server -m ./model.gguf -np 4 --api-key 777 \
  --slot-save-path "$PWD/kvcache" --host 0.0.0.0 --port 8081
```

---

## 3. Precedence & gotchas

- CLI flag > env var > built-in default.
- `META_DIR` and relative `SLOT_SAVE_PATH` are resolved against the
  **current working directory**.
- `META_MAX=0` disables pruning; a later restore of a pruned key simply fails
  and falls back to a full prefill.
- With `BACKENDS` set, `LLAMA_URL`, `N_SLOTS`, and `SLOT_SAVE_PATH` env vars
  are **ignored** — per-backend values in the JSON array win.
- Blank `LLAMA_API_KEY` means "no auth".
- `RUST_LOG` overrides `LOG_LEVEL` when set.
- Request log: a `request_log` spec in `PLUGINS` takes precedence over the `REQUEST_LOG*`
  variables — with `PLUGINS` set, the spec synthesized from the `REQUEST_LOG*` settings is
  not appended. After a rotation the current file is missing until the next record is
  written (the rotated file holds the records up to the rotation).
- Slot acquire timeout (300 s) and circuit-breaker cooldowns (5 s → 60 s,
  probe recovery) are built-in behaviour, not configurable.

