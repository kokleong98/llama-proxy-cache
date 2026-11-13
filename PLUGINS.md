# lpcache Plugins

Comprehensive guide to the `lpcache` **adapter-plugin system** — what each built-in plugin does, how it behaves at runtime, how to configure it, and worked examples. Every behavior in [§6 Live validation](#6-live-validation-llama-32-1b) was exercised against a real **Llama-3.2-1B-Instruct** backend run with `llama-server`.

> **Scope.** This doc explains the plugin architecture, runtime behavior and edge cases, and how to use each plugin. The terse parameter reference (every env var / CLI flag / default, including the legacy `PREFILTER_*`/`REQUEST_LOG*` variables) is in [PARAMETERS.md](PARAMETERS.md). Per-plugin behavioral test suites live in [test-cases/](test-cases/).

## Table of contents

1. [What a plugin is](#1-what-a-plugin-is)
2. [Loading plugins](#2-loading-plugins)
3. [Built-in plugins](#3-built-in-plugins) — `keyword`, `result_cache`, `qwen_guard`, `chain`, `llama`, `request_log`, `dashboard`
4. [Writing a custom plugin](#4-writing-a-custom-plugin)
5. [Configuration recipes](#5-configuration-recipes)
6. [Live validation (Llama-3.2-1B)](#6-live-validation-llama-32-1b)

---

## 1. What a plugin is

The proxy core — KV-cache-aware routing, slot management, concurrent-request coalescing, and backend failover (the `lpcache` crate) — is **backend- and policy-agnostic**. All "adapter" behavior lives in a separate crate, **`lpcache-plugins`**, behind six small, object-safe traits. The `lpcache` binary hard-codes **none** of these adapters: it **instantiates them from JSON configuration at startup** through a name-based registry. Which plugins run is decided entirely by configuration, not by compile-time wiring — so a custom plugin crate can register a new name and become loadable from configuration like any built-in.

The six adapter kinds (all used behind `Arc<dyn …>` in the proxy's `AppState`):

| Adapter trait | Role | Consulted when | Built-in plugin(s) |
|---|---|---|---|
| `Prefilter` | Request prefilter short-circuit | **Before** slot acquisition, KV restore/save, backend dispatch, and coalescing | `keyword`, `qwen_guard`, `result_cache`, `chain` |
| `ResultStore` | Persist fresh non-streaming backend results | After every successful non-streaming backend call | `result_cache` (its backing cache) |
| `PostCleanup` | Post-serve cleanup of a cache entry | After a cached result has been read | `result_cache` (`remove_after_serve`) |
| `LlamaBackend` | One llama.cpp backend client (chat, slot save/restore, model id) | During slot acquisition and dispatch | `llama` (auto, one per backend) |
| `RequestLogger` | One W3C-XLF record per finished request | When each response completes (success/error/reject/disconnect) | `request_log` |
| `Dashboard` | Real-time statistics web page + JSON stats | Page/stats route polls; one record fed per finished request | `dashboard` |

### The request pipeline (where plugins hook in)

A `POST /v1/chat/completions` flows like this; the numbered **hooks** are where a loaded plugin can act:

```text
client ─▶ (H) request-log + dashboard middleware            (record on completion)
       ─▶ parse body, compute KV cache key
       ─▶ (1) PREFILTER chain   ← keyword / qwen_guard / result_cache / chain
             Accept → continue
             Reject → JSON error; STOP (never touches a backend)
             Serve  → cached result; STOP (never touches a backend)
       ─▶ coalescing (COALESCE_REQUESTS): leader / follower
       ─▶ slot acquisition (circuit breaker + cross-backend retry)
       ─▶ KV restore (big requests only)
       ─▶ (2) LlamaBackend chat (stream or JSON)
       ─▶ (3) ResultStore.store(key, 200, body)   ← result_cache
       ─▶ KV save + meta write (big requests)
       ─▶ response
       ─▶ (4) RequestLogger.log(record) + Dashboard.record(record)
```

The prefilter is the most powerful hook. Because it runs **before** coalescing, a `Reject` or `Serve` means the request **never acquires a slot, never restores/saves KV cache, never writes a meta file, and never leads or joins a coalescing group** — it is short-circuited entirely (confirmed live in [§6](#6-live-validation-llama-32-1b): a rejected request produced no backend call).

---

## 2. Loading plugins

### 2.1 The plugin spec

Every plugin is described by a **spec** — a JSON object with a `name` (a registry key) and a `params` object:

```json
{ "name": "<plugin name>", "params": { ... } }
```

`PLUGINS` (env var) / `--plugins` (CLI flag) is a JSON **array** of such specs:

```json
[
  { "name": "keyword",      "params": { "keywords": ["forbidden"], "case_insensitive": true } },
  { "name": "result_cache", "params": { "dir": "./result_cache", "ttl_secs": 300 } }
]
```

`params` is optional — a spec without one (or with a `null` value) is treated
as `{}` (all of the plugin's default parameters). The whole `PLUGINS` value
must be a JSON **array**; anything else (including a bare object) is a
startup error.

### 2.2 `PLUGINS` vs. legacy variables

- **Prefilter specs:**
  - **`PLUGINS` set** → the prefilter specs are used **as-is**; the legacy `PREFILTER_*` variables are **ignored**.
  - **`PLUGINS` unset** → prefilter specs are **synthesized** from the legacy settings: a non-empty `PREFILTER_BLOCKLIST` → a `keyword` spec (placed first, so a blocked request is rejected with `400` before a cache hit could serve it), and `PREFILTER_RESULT_CACHE_DIR` set → a `result_cache` spec.
- **Request logging:** a `request_log` **spec in `PLUGINS` takes precedence** — its params are used as-is, the `REQUEST_LOG*` settings are ignored, and the spec enables logging on its own (even with `REQUEST_LOG` unset). When `PLUGINS` has **no** `request_log` spec (whether `PLUGINS` is set without one or unset at all) and `REQUEST_LOG=true`, a `request_log` spec is synthesized from the `REQUEST_LOG_*` values and appended to the list — so `REQUEST_LOG=true` keeps enabling logging alongside `PLUGINS` unless `PLUGINS` carries its own `request_log` spec.

An empty array `[]` is valid and disables the prefilter.

### 2.3 The registry and loaders

The `lpcache-plugins` crate provides:

- **`Registry`** — a `name → PluginFactory` map. `Registry::with_builtins()` registers the built-ins; `global()` / `ensure_builtin()` register them into the process-wide registry the `lpcache` binary uses.
- **`PluginFactory`** — a trait with one method per adapter kind (`create_prefilter`, `create_backend`, `create_request_logger`, `create_dashboard`); a factory implements only the kind(s) it provides and returns `Ok(None)` for the rest.
- **Loaders:** `load_prefilters(registry, specs)`, `load_request_loggers(registry, specs)`, `load_dashboard(registry, specs)`.

Loading rules (a bad spec is a **startup error** that names the offending entry):

| Adapter | Count | Rules |
|---|---|---|
| prefilter | 0..n | Chained **in list order**; the first non-`Accept` decision wins. Specs for other kinds are skipped; an unknown name is an error. |
| request logger | 0..1 | First spec providing a logger; a second is an error. Other kinds skipped. |
| dashboard | 0..1 | At most one; a second is an error. Other kinds skipped. |
| `llama` backend | 1 per backend | **Not selected via `PLUGINS`.** One is created per `BACKENDS`/`LLAMA_URL` entry from the backend URL + `LLAMA_API_KEY` + `REQUEST_TIMEOUT`; it exists in the registry for embedded use. |

### 2.4 The prefilter contract

`Prefilter::check(&PrefilterRequest) -> PrefilterDecision`. The proxy hands the plugin a snapshot taken *after* body parsing and cache-key computation, *before* any slot/backend work:

| `PrefilterRequest` field | Meaning |
|---|---|
| `body` | The full parsed request body (JSON object) |
| `messages` | The `messages` array (`[]` when absent) |
| `model` | The model the client asked for (the proxy `MODEL_ID` when absent) |
| `backend_model_id` | The backend model id used in the cache key |
| `stream` | `true` when the client asked for streaming |
| `key` | The request's KV cache key (sha256 hex) |
| `n_words` | Total word count of the concatenated message contents |
| `is_big` | `true` when `n_words > BIG_THRESHOLD_WORDS` (cache-eligible) |
| `headers` | `(lowercased name, value)` pairs (empty when none shared) |

The decision is one of:

- **`Accept`** — forward to the normal pipeline.
- **`Reject { status, message }`** — return a JSON error (`{"error": message}`) with `status` (built-ins use `400`; `qwen_guard` uses `503` on a failed check). The request never reaches a backend.
- **`Serve { status, body }`** — return a previously-cached backend result (a full JSON body + its original status). The request never reaches a backend.

### 2.5 Parameter validation and startup errors

Plugins are instantiated **at startup**, before the proxy binds its port: any
invalid spec prints `lpcache: failed to load <prefilter|request-log|dashboard>
plugin: <message>` (or `lpcache: failed to prepare request log: …` for an
unopenable log path) and exits with **code 2**. `lpcache-config validate`
reports these same conditions pre-startup (see
[the configurator guide](docs/configurator-guide.md#what-validate-checks)). The message names the
offending plugin — and, for a nested `chain` filter, its index:

| Condition | Message |
|---|---|
| `PLUGINS` is not a JSON array | `PLUGINS is not valid JSON: …` |
| an entry is not a spec object (missing / non-string `name`) | `PLUGINS[i]: spec must be an object with a `name` string` |
| name not registered | `no plugin registered under name "x"` |
| missing / malformed / out-of-range params | `invalid params for plugin "x": <detail>` |
| a second `request_log` / `dashboard` spec | `invalid params for plugin "x": only one request logger plugin is supported` / `…only one dashboard plugin is supported` |

Per-plugin validation (what makes a spec invalid at startup):

| plugin | fails when |
|---|---|
| `keyword` | `keywords` is missing, not an array-of-strings / comma-separated string, or empty after trimming |
| `result_cache` | `dir` is missing or empty; `cleanup` is anything other than `"remove_after_serve"` (or omitted/`null`) |
| `qwen_guard` | `url` is missing or empty; a `reject_levels` entry is not `Safe`/`Controversial`/`Unsafe`; a `reject_categories` entry is not one of the nine categories; `on_error`/`on_unparseable` is not `accept`/`reject` (each error names the valid values) |
| `chain` | `filters` is missing, not an array, or empty; any nested spec is invalid (reported as `filters[i]: …`, so a nested unknown name or bad params is pinpointed) |
| `llama` | `url` is missing or empty |
| `request_log` | `path` is empty; `fields` is an empty list, names an unknown field, or repeats a field (the error lists the known fields) |
| `dashboard` | `path` does not start with `/`; **any unknown param** (see below); a number outside its range (`refresh_ms` 100..=3600000, `window_secs` 10..=86400, `bucket_secs` 1..=3600, `max_recent` 0..=10000) |

Two asymmetries worth knowing:

- **Unknown params are silently ignored** by every built-in **except
  `dashboard`**, which rejects them — so a typo'd param name in another
  plugin's spec does *not* fail startup (the plugin just runs with its default
  for that param); `dashboard` is the only built-in that protects against such
  typos.
- **Negative numerics:** `result_cache.ttl_secs` (→ `0` = never expire) and
  `llama.timeout_secs` (→ `0` = **no per-request timeout**) are silently
  clamped to zero, while `qwen_guard`'s `timeout_secs`/`max_input_chars`/
  `cache_max` and every `dashboard` number are unsigned types, so a negative
  value there is a startup error. `dashboard` additionally range-validates its
  numbers — `refresh_ms` 100..=3600000, `window_secs` 10..=86400,
  `bucket_secs` 1..=3600, `max_recent` 0..=10000 — and an out-of-range value
  is a startup error (the other built-ins clamp or take whatever fits their
  types).

---

## 3. Built-in plugins

Each of the seven built-ins is a module in `plugins/src/builtin/` implementing one or more adapter traits plus a `PluginFactory`. Summary first, then behavior + worked examples:

| name | adapters | one-liner |
|---|---|---|
| `keyword` | prefilter | Reject requests whose message text contains a blocked keyword |
| `result_cache` | prefilter + result store (+ optional post-cleanup) | Serve fresh same-key requests from a disk cache; store fresh non-streaming results |
| `qwen_guard` | prefilter | Content-safety check via a Qwen3Guard-Gen model on a separate server |
| `chain` | prefilter | Run nested prefilter plugins in order |
| `llama` | backend | The reqwest-based llama.cpp backend client (auto per backend) |
| `request_log` | request logger | W3C-XLF request log to a size-rotated file |
| `dashboard` | dashboard | Real-time statistics web page + JSON stats endpoint |

### 3.1 `keyword` — keyword blocklist prefilter

**What it does.** Rejects a request when the combined message text contains any blocked keyword (plain substring match).

**Behavior.**
- The message text is flattened by `message_text`: it handles both a plain-string `content` and OpenAI content-part arrays (`[{"type":"text","text":"…"}]`); parts without a text payload (images, tool calls) and messages without `content` are skipped; pieces are joined with newlines.
- `case_insensitive` (default `true`): keywords and text are matched lowercased; `false` is exact case.
- Empty/whitespace keywords are dropped at construction. In a `PLUGINS` spec a `keywords` list that ends up empty is a **startup error** (`"keywords must be a non-empty list"`); with the legacy `PREFILTER_BLOCKLIST`, an empty/whitespace-only blocklist instead simply synthesizes **no** `keyword` spec.
- A match rejects with **`400`** and a message naming the **first** matching keyword.

**Params.**

| Param | Default | Meaning |
|---|---|---|
| `keywords` (required) | — | Array of keyword strings, or a comma-separated string (`"a, b"`); whitespace trimmed, empty entries dropped |
| `case_insensitive` | `true` | Case-insensitive matching when true |

**Example.**

```json
{ "name": "keyword", "params": { "keywords": ["forbidden", "internal"], "case_insensitive": true } }
```

Live result ([§6](#6-live-validation-llama-32-1b)): a request containing `secretpass` → `400 {"error":"request blocked by keyword prefilter: \"secretpass\""}`; `FORBIDDEN` (uppercase) also matches → `400 …"forbidden"`.

### 3.2 `result_cache` — file-based result cache + cached-result prefilter

**What it does.** Stores completed non-streaming backend results on disk (one `{key}.json` per request KV cache key, each `{status, body, stored_at}`) and serves fresh (non-expired) same-key requests from that cache before any slot/backend work.

**Behavior.**
- The plugin is **both** a prefilter and a `ResultStore`. The proxy takes the store from `Prefilter::result_store` and stores fresh **non-streaming 200 JSON** results under the request key after the backend call.
- On a fresh (non-expired) cache hit the prefilter answers with `Serve { status, body }` — **no slot, no backend call, no KV restore/save, no meta file, no coalescing group**.
- **Streaming:** by default (`cache_streaming=false`) a streaming request always `Accept`s — only non-streaming JSON results are cached, so `stream:true` is never served from cache. With `cache_streaming=true` the proxy (a) aggregates a completed SSE stream into a non-streaming completion before storing it, and (b) converts a cached completion back to SSE when serving a streaming client.
- **TTL:** entries expire after `ttl_secs` (`0` = never); expired entries are removed **lazily on read** (no background sweeper).
- **Atomicity:** writes are atomic (temp file + rename), so a concurrent reader never sees a torn entry.
- **Post-serve cleanup:** `cleanup: "remove_after_serve"` attaches the built-in one-shot cleanup — the entry file is deleted right after it is read, so each result is served **at most once**.
- **`Cache-Control`:** with `cache_control=true`, a request whose `Cache-Control` header carries the `no-cache` directive (case-insensitive) is forwarded to the backend instead of being served from the cache.
- **Ordering note:** the store runs right after a request's response is prepared. A same-key request that arrives *before* the previous response's store lands will miss (see the timing discussion in [§6.4](#64-result_cache-miss-then-hit)).

**Params.**

| Param | Default | Meaning |
|---|---|---|
| `dir` (required) | — | Cache-result dir (holds the `{key}.json` entries); created on first write |
| `ttl_secs` | `300` | Per-entry expiry in seconds; `0` = never expire |
| `cleanup` | — | `"remove_after_serve"` for one-shot cleanup; omitted/`null` = no cleanup |
| `cache_streaming` | `false` | Also cache streaming results (aggregated) and serve cached entries to streaming clients |
| `cache_control` | `false` | Honor a `Cache-Control: no-cache` header (bypass the cache) |

**Example.**

```json
{ "name": "result_cache", "params": { "dir": "./result_cache", "ttl_secs": 600, "cleanup": "remove_after_serve" } }
```

Live result ([§6.4](#64-result_cache-miss-then-hit)): first non-streaming request → `200` in ~0.16 s (backend, entry written to `{key}.json`); an identical request → served from cache in ~0.00 s with a **byte-identical body (same `id`)**.

### 3.3 `qwen_guard` — Qwen3Guard-Gen content-safety prefilter

**What it does.** Runs a **Qwen3Guard-Gen** moderation model (Alibaba's safety series, hosted on a *separate* llama-server or any OpenAI-compatible endpoint) over the request's combined message text in prompt-moderation mode, and applies a configurable allow/reject policy.

**Behavior.**
- Qwen3Guard-Gen is a generative classifier: it answers `Safety: <Level>` / `Categories: <c1, c2>` with one of three severity levels — `Safe`, `Controversial`, `Unsafe` — and zero or more harm categories.
- The plugin sends the flattened message text as a single `user` message to `{url}/v1/chat/completions` (with `max_tokens: 64`, `temperature: 0`, `stream: false`; the `model` field is sent **only when the `model` param is set** — otherwise the guard server's default model is used), parses the verdict, and **rejects** (`400`) when the level is in `reject_levels` or any category is in `reject_categories`; otherwise it **accepts**.
- **Empty text** (no message content) skips the guard call and is accepted.
- **Failure handling** (fail-open vs. fail-closed):
  - **`on_error`** (default `"accept"`): action when the guard call fails (transport / HTTP / timeout). A **non-2xx** guard response (e.g. llama-server's `503 {"error":{"message":"Loading model"}}` while the model loads) is a *failed check* → `on_error`, **never** `on_unparseable` (so a fail-closed deployment cannot fail open just because the error body happened to be valid JSON without a `choices` array). A failed check with `on_error:"reject"` returns **`503`**.
  - **`on_unparseable`** (default `"accept"`): action when the guard responds but its output cannot be parsed into a level/categories; `on_unparseable:"reject"` returns **`400`**.
  - Both accept `"accept"` or `"reject"` (aliases `"allow"`/`"block"`).
- **Verdict cache:** `cache_max` (default `0` = off) — a bounded LRU of verdicts keyed by the request's KV cache key; repeated same-key prompts skip the guard call. The LRU memoizes **every** decision — accepts *and* rejects — so a blocked prompt is not re-checked either.
- A rejected request never reaches the chat backend (no slot, no KV work, no meta file, no coalescing group).

**The nine Qwen3Guard categories** (case-insensitive; `PII` also matches "Personally Identifiable Information"): `Violent`, `Non-violent Illegal Acts`, `Sexual Content or Sexual Acts`, `PII`, `Suicide & Self-Harm`, `Unethical Acts`, `Politically Sensitive Topics`, `Copyright Violation`, `Jailbreak` (input only).

**Params.**

| Param | Default | Meaning |
|---|---|---|
| `url` (required) | — | Base URL of the server hosting the guard model; POSTs to `{url}/v1/chat/completions` |
| `api_key` | — | Sent as `Authorization: Bearer <key>` |
| `model` | — | Model id in the chat-completions request; when omitted the `model` field is left out and the guard server's default model is used |
| `timeout_secs` | `30` | Per-check timeout |
| `max_input_chars` | `0` | Cap, in **characters** (not bytes), on the flattened text sent to the guard (`0` = no truncation); guard models have small contexts |
| `reject_levels` | `["Unsafe"]` | Severity levels that reject outright — a JSON array or a comma-separated string of `Safe`/`Controversial`/`Unsafe` (case-insensitive) |
| `reject_categories` | `[]` | Categories that reject when present (independent of the level) — a JSON array or a comma-separated string; any of the nine categories below (case-insensitive) |
| `on_error` | `"accept"` | Action when the guard call fails |
| `on_unparseable` | `"accept"` | Action when the verdict is unparseable |
| `cache_max` | `0` | Bounded LRU of verdicts keyed by KV cache key; `0` = off |

**Examples.** Fail-open (default):

```json
{ "name": "qwen_guard", "params": { "url": "http://127.0.0.1:8010", "api_key": "guard-key", "model": "qwen3guard", "reject_levels": ["Unsafe", "Controversial"], "timeout_secs": 10 } }
```

Fail-closed:

```json
{ "name": "qwen_guard", "params": { "url": "http://127.0.0.1:8010", "reject_levels": ["Unsafe"], "on_error": "reject", "on_unparseable": "reject" } }
```

Live results ([§6.6](#66-qwen_guard-fail-open--fail-closed)): with the guard down, `on_error:"accept"` → `200` (forwarded to the chat model); `on_error:"reject"` → `503`; with a live (non-guard) Llama-3.2 as the guard and `on_unparseable:"reject"` → `400`.

### 3.4 `chain` — run nested prefilters in order

**What it does.** Runs several prefilter plugins in order; the first non-`Accept` decision (a `Reject` or `Serve`) wins.

**Behavior.**
- The proxy synthesizes a chain automatically whenever more than one prefilter spec is configured — the specs run in **list order** (the startup log names it `filter=chain`).
- A `chain` spec also lets you nest arbitrary (custom) plugins **explicitly**, resolved against the same registry.
- The chain's `result_store` is the store of the first member that provides one.

**Params.**

| Param | Default | Meaning |
|---|---|---|
| `filters` (required) | — | Array of nested plugin specs, in evaluation order |

**Example.**

```json
{ "name": "chain",
  "params": { "filters": [
    { "name": "keyword",      "params": { "keywords": ["banned"] } },
    { "name": "result_cache", "params": { "dir": "./rc" } }
  ] } }
```

### 3.5 `llama` — llama.cpp backend client

**What it does.** The reqwest-based HTTP client for one llama.cpp backend: `chat_stream`/`chat_json` (POST `/v1/chat/completions`), `save_slot`/`restore_slot` (POST `/slots/{id}?action=save|restore` with the file basename in the body), and `get_model_id` (GET `/v1/models`).

**Behavior.**
- **Not selected via `PLUGINS`** — the proxy creates one per `BACKENDS`/`LLAMA_URL` backend automatically, from the backend URL + `LLAMA_API_KEY` + `REQUEST_TIMEOUT`. It is registered for **embedded** setups that load a backend explicitly.
- **Slot pinning:** the pinned slot id is duplicated in the body root (`_slot_id`, `slot_id`, `id_slot`), in `options`, and in the query params.
- **Cancel-safe:** dropping the returned future (request or response stream) aborts the in-flight HTTP request to llama-server — the TCP connection is closed and the backend frees its slot when it notices. This is how the proxy cancels a backend call when its own client goes away (llama-server has no cancel endpoint).
- **`api_key`** is optional: when set, every request carries `Authorization: Bearer <key>` (llama-server `--api-key`).

**Params.**

| Param | Default | Meaning |
|---|---|---|
| `url` (required) | — | The backend base URL |
| `api_key` | — | Sent as `Authorization: Bearer <key>` |
| `timeout_secs` | `600` | Per-request timeout |

### 3.6 `request_log` — W3C-XLF request logger

**What it does.** One [W3C Extended Log Format (XLF)](https://www.w3.org/TR/xlf-10/) record per finished request (success, error, rejection or disconnect) to a size-rotated log file.

**Behavior.**
- The file starts with the XLF header:
  ```text
  #Software: lpcache-plugins/0.1.0
  #Version: 1.0
  #Date-format: %Y-%m-%dT%H:%M:%S.%fZ
  #Fields: datetime cs-method cs-uri ... lpcache-reason
  ```
- **Fields:** W3C-standard (`datetime`, `cs-method`, `cs-uri`, `c-ip`, `cs(User-Agent)`, `cs-bytes`, `cs-time`, `sc-status`, `sc-bytes`) plus the lpcache pipeline fields (`lpcache-model`, `lpcache-stream`, `lpcache-key`, `lpcache-n-words`, `lpcache-is-big`, `lpcache-backend`, `lpcache-slot`, `lpcache-restored`, `lpcache-cache-hit`, `lpcache-coalesced`, `lpcache-reason`), the performance fields (`lpcache-prompt-tokens`, `lpcache-completion-tokens`, `lpcache-cached-tokens`, `lpcache-prefill-ms`, `lpcache-gen-ms`, `lpcache-prefill-tps`, `lpcache-gen-tps`, `lpcache-ttft-ms`) and the resource fields (`lpcache-cpu-ms`, `lpcache-disk-read-bytes`, `lpcache-disk-write-bytes`, `lpcache-ram-bytes`, `lpcache-peak-ram-bytes` — the proxy process's own CPU and disk-I/O consumption during the request, plus its resident memory and peak resident memory when the request completed; the first three are counter deltas, the last two point-in-time gauges; `-` where the platform cannot sample them).
- **Values:** space-separated; a missing value is `-`; a value with whitespace/quote/backslash, or starting with `-`/`"`, is double-quoted with `\"`, `\\`, CR and LF escaped (XLF escaping rules).
- **Concurrency:** writes are serialized; concurrent requests never lose or corrupt a line.
- **Rotation:** when the current file exceeds `max_bytes` it is renamed to `<path>.1`, the existing `<path>.1` → `<path>.2`, …; at most `keep` rotated generations are retained (`0` = keep all). The fresh current file restarts with the XLF header.
- **`datetime`** uses UTC with millisecond precision.
- **Never breaks serving:** logging failures are logged and swallowed.

**Params.**

| Param | Default | Meaning |
|---|---|---|
| `path` | `lpcache-request.log` | Log file location (relative → cwd; parent dir created) |
| `max_bytes` | `10485760` (10 MiB) | Rotation threshold; `0` = no rotation |
| `keep` | `5` | Rotated generations kept; `0` = keep all |
| `fields` | all 33 | Ordered subset of the known XLF fields; omitted = all 33 in the default order. An empty list, an unknown field name, or a repeated field is a startup error (the error lists the known fields) |

**Example.**

```json
{ "name": "request_log", "params": { "path": "./logs/lpcache.log", "max_bytes": 5242880, "keep": 3 } }
```

Live result ([§6.5](#65-request_log-xlf-request-log)): every request — including the `400` rejections (`lpcache-reason=prefilter_reject`) and the cache hit (`lpcache-cache-hit=true`) — appends one XLF line with the full field set.

### 3.7 `dashboard` — real-time statistics web dashboard

**What it does.** Serves a self-contained HTML page (inline CSS + JS, no external assets) plus a JSON stats endpoint, fed by one record per finished request (the same records the request logger receives).

**Behavior.**
- When loaded, the proxy mounts two routes:
  - `GET {path}` — the page;
  - `GET {path}/api/stats` — the JSON payload: the proxy's live state (a `ProxyStatus` snapshot — version, uptime, model id, port, coalescing, thresholds, meta files, in-flight slots, per-backend and per-slot status, circuit-breaker state) **merged** with the plugin's aggregates.
- The stats endpoint accepts `?since=<unix seconds>` to scope the aggregates to `[since, now]` and `?backend=<index>` to restrict them to the records that ran on one backend, or `?backend=cached` to restrict them to the records served from the result cache (they never touch a backend), `?backend=coalesced` to the coalesced followers (they joined an in-flight group and never touch a backend) or `?backend=coalesced_groups` to every request of the coalesced groups (the leaders and their followers — the leaders also count in their backend's scope: the scopes are views, not a partition); absent = all backends (no filtering, the default); without `?since` the payload is the lifetime counters plus the trailing `window_secs` series.
- The plugin keeps in memory:
  - **counters** — requests by status class, cache hits / hit-rate, KV restores, big/streamed requests, coalesced followers, token totals, process resource totals (CPU ms, disk bytes read/written, peak resident memory — the proxy's own per-request deltas / gauges), average duration / TTFT / generation rate — kept for all backends, per backend index and for the `cached` / `coalesced` / `coalesced_groups` scopes (for the `?backend` filter);
  - **time series** — one bucket of `bucket_secs` (full counters per bucket, plus per-backend and per-scope sub-counters) covering the process lifetime, bounded by a hard cap (oldest dropped first); a long range is coarsened to a bounded number of points;
  - **recent requests** — a ring buffer of the last `max_recent` finished requests (incl. the backend index of each); each backend scope (all, each index, `cached`, `coalesced`, `coalesced_groups`) keeps its own ring, so a selected filter keeps showing that scope's recent rows even when they have rotated out of the all-backends ring.
- The page polls every `refresh_ms` and renders stat cards, a throughput panel (requests / completion tokens / cached tokens charts), a resources panel (CPU ms / disk read / disk write / peak RAM charts), the backend table, a per-slot occupancy grid and the recent-requests table (with the backend index); a time-filter dropdown (presets + custom/all-time) and a backend-filter dropdown (default: `all` — no filtering; an index, or the `cached`, `coalesced` or `coalesced_groups` option for the result-cache, coalesced-follower and coalesced-group records) scope the cards/charts/table via `?since` and `?backend`.
- **Its own routes are excluded:** the page and stats requests are neither logged nor fed into the aggregates, so polling never inflates the statistics.
- **Nothing is persisted:** all aggregates reset on proxy restart. **At most one** dashboard plugin may be configured.
- **Strict params:** the only built-in that rejects **unknown params** — a typo'd param name is a startup error, so a bad dashboard config fails loudly instead of silently falling back to defaults (see [§2.5](#25-parameter-validation-and-startup-errors)).

**Params.**

| Param | Default | Meaning |
|---|---|---|
| `path` | `/dashboard` | Page URL path (must start with `/`); stats at `{path}/api/stats` |
| `refresh_ms` | `2000` | Page polling interval (100..=3600000 ms) |
| `window_secs` | `300` | Trailing window for a stats poll without `?since` (10..=86400 s) |
| `bucket_secs` | `5` | Time-series bucket size (1..=3600 s) |
| `max_recent` | `100` | Recent-requests ring size per backend scope (0..=10000; `0` = no recent list) |

**Example.**

```json
{ "name": "dashboard", "params": { "path": "/dashboard" } }
```

Live result ([§6.7](#67-dashboard)): `GET /dashboard` returns an ~85 KB self-contained page titled "lpcache dashboard" (headings: throughput / resources / backends / slots / recent requests; filter bar with time presets + a backend filter that defaults to "all"); `GET /dashboard/api/stats` returns JSON with a `counters` object (`ok`, `client_errors`, `cache_hits`, `cache_hit_rate`, `tokens`, `resources` — the process CPU ms, disk bytes read/written and peak RAM, …) plus the live `proxy` snapshot.

---

## 4. Writing a custom plugin

A custom plugin is a separate crate that depends on `lpcache-plugins`, implements the relevant adapter trait(s) and a `PluginFactory`, and registers itself under a name. In the `lpcache` binary, register it into the **global** registry before the proxy loads plugins; for embedded use, register it into a private `Registry`.

```rust
use lpcache_plugins::registry::{PluginError, PluginFactory, ensure_builtin, global};
use lpcache_plugins::{Prefilter, PrefilterDecision, PrefilterRequest};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

struct TooLongPrefilter { max_words: usize }

#[async_trait]
impl Prefilter for TooLongPrefilter {
    fn name(&self) -> &str { "too_long" }
    async fn check(&self, req: &PrefilterRequest) -> PrefilterDecision {
        if req.n_words > self.max_words {
            return PrefilterDecision::Reject {
                status: 413,
                message: format!("prompt too long ({} words)", req.n_words),
            };
        }
        PrefilterDecision::Accept
    }
}

struct TooLongFactory;
impl PluginFactory for TooLongFactory {
    fn create_prefilter(&self, params: &Value) -> Result<Option<Arc<dyn Prefilter>>, PluginError> {
        let max_words = params.get("max_words")
            .and_then(Value::as_u64)
            .unwrap_or(1000) as usize;
        Ok(Some(Arc::new(TooLongPrefilter { max_words })))
    }
    // create_backend / create_request_logger / create_dashboard: keep the defaults (Ok(None))
}

fn main() {
    ensure_builtin();                                  // register the built-ins
    global().register("too_long", Arc::new(TooLongFactory));
    // ... the proxy now loads PLUGINS from this global registry.
}
```

The plugin is now loadable from configuration like any built-in:

```json
[ { "name": "too_long", "params": { "max_words": 2000 } } ]
```

Custom plugins can also be **nested inside a `chain`** spec, because the `chain` factory resolves its nested specs against the same registry. A custom plugin may implement several adapter kinds at once (e.g. a `Prefilter` whose `result_store()` exposes a `ResultStore`), exactly like the built-in `result_cache`.

---

## 5. Configuration recipes

Each of the following is a complete `PLUGINS` value (set it as `PLUGINS='…'` or `--plugins '…'`).

- **Keyword blocklist only:**
  ```json
  [ { "name": "keyword", "params": { "keywords": ["forbidden"] } } ]
  ```
- **Blocklist + result cache (check the cheap `keyword` first, then cache):**
  ```json
  [
    { "name": "keyword",      "params": { "keywords": ["forbidden"] } },
    { "name": "result_cache", "params": { "dir": "./rc", "ttl_secs": 600 } }
  ]
  ```
- **Full observability + caching** (the configuration used for the live run in [§6](#6-live-validation-llama-32-1b)):
  ```json
  [
    { "name": "keyword",      "params": { "keywords": ["forbidden", "secretpass"], "case_insensitive": true } },
    { "name": "result_cache", "params": { "dir": "./rc", "ttl_secs": 300 } },
    { "name": "request_log",  "params": { "path": "./lpcache-request.log" } },
    { "name": "dashboard",    "params": { "path": "/dashboard" } }
  ]
  ```
- **Safety gate (fail-open, the default):**
  ```json
  [ { "name": "qwen_guard", "params": { "url": "http://127.0.0.1:8010", "reject_levels": ["Unsafe"] } } ]
  ```
- **Order matters.** Because prefilters run in list order and the first non-`Accept` wins, put cheap deterministic checks (`keyword`) before expensive or fallible ones (`qwen_guard`, `result_cache`).

---

## 6. Live validation (Llama-3.2-1B)

This section records a real end-to-end run. The backend was a local **Llama-3.2-1B-Instruct** (Q4_K_M) served by `llama-server`; the proxy was `lpcache 0.8.0`. All commands and outputs below are from that run.

### 6.1 Setup

Backend (`llama-server`, port 8099, 2 slots, CPU, with slot-save for the KV-cache features):

```sh
llama-server -m Llama-3.2-1B-Instruct-Q4_K_M.gguf \
  --port 8099 --host 127.0.0.1 -np 2 -ngl 0 -c 4096 -t 8 \
  --api-key 777 --slot-save-path ./kvcache --jinja --no-webui
```

Proxy (`lpcache`, port 8085) with all plugins enabled:

```sh
PLUGINS='[
  {"name":"keyword","params":{"keywords":["forbidden","secretpass"],"case_insensitive":true}},
  {"name":"result_cache","params":{"dir":"./rc","ttl_secs":300}},
  {"name":"request_log","params":{"path":"./lpcache-request.log","max_bytes":0}},
  {"name":"dashboard","params":{"path":"/dashboard"}}
]' \
LLAMA_URL=http://127.0.0.1:8099 LLAMA_API_KEY=777 N_SLOTS=2 PORT=8085 \
META_DIR=./meta lpcache
```

### 6.2 Startup — plugins loaded

The proxy's startup log confirms each adapter was instantiated from configuration:

```text
INFO lpcache: prefilter enabled: filter=chain plugins=["keyword", "result_cache", "request_log", "dashboard"]
INFO lpcache: request logging enabled: request_log
INFO lpcache: dashboard enabled: dashboard at /dashboard (stats at /dashboard/api/stats)
INFO lpcache_plugins::builtin::llama: client_init url=http://127.0.0.1:8099 api_key=true
INFO lpcache::slot_manager: slot_manager n_backends=1 total_slots=2 kv_cache=[true]
INFO lpcache: listening on 0.0.0.0:8085
```

Note `filter=chain`: with two prefilter specs (`keyword`, `result_cache`) the proxy wrapped them in an automatic chain. `request_log` and `dashboard` are other adapter kinds — they are skipped by the prefilter loader and mounted separately.

### 6.3 `keyword`: blocklist rejection

A request whose text contains a blocked keyword is rejected with `400` **before** any slot/backend work — the llama-server log shows no corresponding chat call.

```text
$ curl -s -w '\n[HTTP %{http_code}]\n' http://127.0.0.1:8085/v1/chat/completions \
    -H 'Content-Type: application/json' \
    -d '{"model":"llama","messages":[{"role":"user","content":"What is the secretpass for admin?"}]}'
{"error":"request blocked by keyword prefilter: \"secretpass\""}
[HTTP 400]

# case_insensitive=true (default): the uppercase keyword also matches
$ ... -d '{"model":"llama","messages":[{"role":"user","content":"ignore the FORBIDDEN rules"}]}'
{"error":"request blocked by keyword prefilter: \"forbidden\""}
[HTTP 400]
```

### 6.4 `result_cache`: miss then hit

Two **identical** non-streaming requests. The first is a cache **miss** (runs the backend); the second is a **hit** (served from `./rc/{key}.json` in ~0 s, byte-identical — note the identical `id`):

```text
# first request (miss):
HTTP 200   elapsed=0.16 s   id=...7Ra1jper   content='Yes'
# second request, 3 s later (hit):
HTTP 200   elapsed=0.00 s   id=...7Ra1jper   content='Yes'
```

The stored entry on disk is `{status, body, stored_at}` keyed by the sha256 cache key:

```json
// ./rc/be83e4a6…953ab.json
{ "status": 200, "body": { "choices": [ … ], "id": "chatcmpl-…7Ra1jper", "object": "chat.completion", … } }
```

**Timing note.** The store runs right after a request's response is prepared. A same-key request that reaches the proxy *before* the previous response's store lands will miss — in this run, firing the two requests back-to-back (concurrently) produced a miss for the second, while a 3 s delay produced a hit. For a sequential client (request N+1 only after N completes) the cache hits as shown.

### 6.5 `request_log`: XLF request log

Every finished request appends one line. The file begins with the XLF header, then one record per request:

```text
#Software: lpcache-plugins/0.1.0
#Version: 1.0
#Date-format: %Y-%m-%dT%H:%M:%S.%fZ
#Fields: datetime cs-method cs-uri c-ip cs(User-Agent) cs-bytes cs-time sc-status sc-bytes lpcache-model lpcache-stream lpcache-key lpcache-n-words lpcache-is-big lpcache-backend lpcache-slot lpcache-restored lpcache-cache-hit lpcache-coalesced lpcache-reason lpcache-prompt-tokens lpcache-completion-tokens lpcache-cached-tokens lpcache-prefill-ms lpcache-gen-ms lpcache-prefill-tps lpcache-gen-tps lpcache-ttft-ms lpcache-cpu-ms lpcache-disk-read-bytes lpcache-disk-write-bytes lpcache-ram-bytes lpcache-peak-ram-bytes
```

Selected fields from three requests — a `keyword` rejection, a `result_cache` miss, and a `result_cache` hit (miss + hit share key `2466479e…`):

| datetime | sc-status | lpcache-key | lpcache-slot | lpcache-cache-hit | lpcache-reason |
|---|---|---|---|---|---|
| `…:21.687Z` | 400 | `36a10489…` | `-` | false | `prefilter_reject` |
| `…:41.415Z` | 200 | `2466479e…` | 0 | false | `-` |
| `…:43.432Z` | 200 | `2466479e…` | `-` | **true** | `-` |

The hit row shows the short-circuit: `lpcache-slot=-` (no slot was acquired) and `lpcache-cache-hit=true`. The rejection row shows `lpcache-reason=prefilter_reject`. A successful backend row also carries the performance fields (`lpcache-prompt-tokens`, `lpcache-gen-ms`, `lpcache-gen-tps`, …) taken from llama-server's `usage`/`timings`, and the resource fields (`lpcache-cpu-ms`, `lpcache-disk-read-bytes`, `lpcache-disk-write-bytes`, `lpcache-ram-bytes`, `lpcache-peak-ram-bytes`) sampled from the proxy process itself.

### 6.6 `qwen_guard`: fail-open & fail-closed

Three short-lived proxy instances, same backend, different `qwen_guard` configs (no Qwen3Guard model is bundled, so the guard `url` here is a dead port or the Llama-3.2 chat model itself):

```text
# B: guard down (dead port), on_error=accept (default) → fail open, chat proceeds
$ curl -s -w ' [HTTP %{http_code}]\n' http://127.0.0.1:8086/v1/chat/completions \
    -H 'Content-Type: application/json' -d '{"model":"llama","messages":[{"role":"user","content":"hello there"}],"max_tokens":6}'
{"choices":[{"message":{"content":"Hello! How can I help",…}}], …} [HTTP 200]

# C: guard down (dead port), on_error=reject → fail closed (503)
$ … http://127.0.0.1:8087/v1/chat/completions …
{"error":"content check unavailable: error sending request for url (http://127.0.0.1:9999/v1/chat/completions)"} [HTTP 503]

# D: live Llama-3.2 as the "guard", on_unparseable=reject → its output is not a
#    safety verdict, so the check is unparseable → fail closed (400)
$ … http://127.0.0.1:8088/v1/chat/completions …
{"error":"content check failed: guard output could not be parsed"} [HTTP 400]
```

The configs:

```json
// B (8086) — fail open
[{"name":"qwen_guard","params":{"url":"http://127.0.0.1:9999","reject_levels":["Unsafe"],"on_error":"accept"}}]
// C (8087) — fail closed on error
[{"name":"qwen_guard","params":{"url":"http://127.0.0.1:9999","reject_levels":["Unsafe"],"on_error":"reject"}}]
// D (8088) — unparseable output rejected (guard = the chat model itself)
[{"name":"qwen_guard","params":{"url":"http://127.0.0.1:8099","api_key":"777","reject_levels":["Unsafe"],"on_unparseable":"reject"}}]
```

This validates the plugin's wiring and both failure paths. A full safety-classification run additionally requires a real **Qwen3Guard-Gen** model hosted on the guard `url`.

### 6.7 `dashboard`

```text
$ curl -s http://127.0.0.1:8085/dashboard | wc -c
85468                       # self-contained HTML page, titled "lpcache dashboard"

$ curl -s 'http://127.0.0.1:8085/dashboard/api/stats' | python3 -c 'import sys,json; print(list(json.load(sys.stdin).keys()))'
['counters', 'model', 'proxy', 'recent', 'refresh_ms', 'series', 'uptime_secs', 'version', 'window_secs']
```

The `counters` object reflects the run's traffic (after the tests above):

```json
{ "ok": 7, "client_errors": 3, "cache_hits": 2, "cache_hit_rate": 0.2,
  "total": 10, "success_rate": 0.7, "restored": 0, "streamed": 0, "coalesced_followers": 0,
  "avg_duration_secs": 0.085, "avg_prefill_tps": 319.97, "avg_gen_tps": 119.95,
  "tokens": { "prompt": 252, "completion": 12, "cached": 0 },
  "resources": { "cpu_ms": 41.2, "avg_cpu_ms": 4.12,
                 "disk_read_bytes": 32768, "disk_write_bytes": 65536 } }
```

`client_errors` counts the `400` keyword rejections; `cache_hits` counts the `result_cache` serves. The `resources` object is the proxy's **own** per-request process deltas summed (CPU ms and disk bytes read/written — the backend's KV I/O is not included). The `proxy` key carries the live `ProxyStatus` (backends, slots, circuit breakers, meta files, in-flight). The dashboard's own page/stats requests are excluded, so polling never inflates these numbers.

---

*Validated against `lpcache 0.8.0` + Llama-3.2-1B-Instruct (Q4_K_M) on `llama-server`, 2026-11-09.*
