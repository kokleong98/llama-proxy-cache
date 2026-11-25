# lpcache — Architecture Design Document

Design document for the **lpcache** proxy (a KV-cache-aware,
OpenAI-compatible reverse proxy for one or more llama.cpp servers).
Companion reading: [README](../README.md),
[PARAMETERS.md](../PARAMETERS.md), [PLUGINS.md](../PLUGINS.md),
[Use Cases](use-cases.md),
[Setup Guide](setup-guide.md),
[Configurator Guide](configurator-guide.md),
[test case catalog](../test-cases/README.md).

---

## 1. Overview & design goals

**Problem.** llama.cpp's `llama-server` serializes concurrent requests
through a small pool of slots and recomputes the full KV prefill for every
request. Long prompts that repeat across requests (shared system prompts,
RAG context, multi-turn conversation prefixes) therefore pay the full
prefill cost every time, and clients see queue latency proportional to the
slot count.

**Approach.** lpcache sits in front of one or more llama-servers and
transparently manages their slot KV caches:

1. it addresses each request by a **content-derived KV cache key**
   (hash of model id + message contents);
2. for "big" requests it scans on-disk **meta files** (block-hash
   fingerprints of previously saved KV prefixes), picks the best
   **longest-common-prefix (LCP)** candidate, and restores that KV file
   into a slot **before** dispatch, so the backend only prefills the new
   tail;
3. after completion it **saves** the slot's KV cache and writes/refreshes
   the meta file, keeping the cache population LRU-bounded.

**Design goals.**

- **OpenAI-compatible, drop-in**: clients speak `/v1/chat/completions`
  (JSON or SSE) and `GET /v1/models` only; no client changes.
- **Transparent**: the proxy is a pass-through proxy plus cache
  management — no model files, no GPU, no tokenization.
- **Rust rewrite parity + extensions**: behavior matches the Python
  `proxycache` original (documented per module), plus Rust-only
  features: request coalescing, the plugin system, circuit breaker,
  `key_affinity` routing, per-slot last-KV tracking, LRU pruning,
  model-aware routing, save retries, drain/resume.
- **Never break serving**: every observability/adapter side-channel
  (logging, result store, dashboard, payload log) swallows its own
  failures; a cache miss or restore failure degrades to the un-cached
  path, not an error.

**Non-goals.** No token-level cache alignment (hashing is word-based, see
§11), no multi-tenant isolation, no auth/authn of its own, no shared
cluster-wide cache store, no request-level routing by generation
parameters.

## 2. System context

```
                      OpenAI /v1 API
   clients ─────────────────────────────────────────────────┐
   (curl, SDKs, agents)                                      │
                                                             ▼
        ┌────────────────────────────────────────────────────────────────┐
        │ lpcache (axum, one process)                                   │
        │                                                                │
        │  middleware: payload log · XLF request log · proc sampling     │
        │  prefilter chain (plugins) ── reject / serve (short-circuit)   │
        │  singleflight coalescing                                       │
        │  slot manager: routing policy · breaker · drain · save/restore │
        └──────┬──────────────────────┬─────────────────────────┬────────┘
               │ chat / save / restore│ probe (GET /v1/models)  │ drain/resume
               ▼                      ▼                         ▲ (operator)
        ┌─────────────┐        ┌─────────────┐
        │ llama-server│        │ llama-server│ ... (N backends, each n_slots)
        │ backend 0   │        │ backend 1   │
        └──────┬──────┘        └──────┬──────┘
               │ KV slot files        │
               ▼                      ▼
        {slot_save_path}/{model}_{key}   (KV cache files, per backend)

        on the proxy host:  META_DIR/{key}.meta.json      (block-hash fingerprints)
                            result cache dir, request log, payload dir, dashboard
```

Side channels: the **meta directory** (proxy-local) fingerprints saved KV
prefixes; the **slot-save directories** (per backend, possibly remote)
hold the actual KV files; result cache / logs / dashboard are plugin
side-channels. The dashboard's own page/stats routes are internal polling
and are excluded from the request log and from the dashboard's own
aggregates.

## 3. Workspace layout

Cargo workspace, three crates:

| Crate | Binaries / lib | Responsibility |
|---|---|---|
| `lpcache` (`src/`) | `lpcache` bin + lib | The proxy: HTTP layer, caching pipeline, slot management, config |
| `lpcache-plugins` (`plugins/`) | `lpcache_plugins` lib | Adapter API traits, built-in plugins, name-based registry |
| `lpcache-config` (`configurator/`) | `lpcache-config` bin | CLI wizard + web UI that builds/validates/exports configuration |

`lpcache` modules (the Python original maps 1:1 — see `src/lib.rs`):

| Module | Python | Responsibility |
|---|---|---|
| `config` | `config.py` | CLI + env parsing, precedence, defaults, `BACKENDS`/`PLUGINS` JSON validation |
| `hashing` | `hashing.py` | word tokenization, block hashes, cache key, meta files, LCP candidate scan, LRU pruning |
| `slot_manager` | `slot_manager.py` | global slots, per-slot semaphores, routing policy, circuit breaker, drain, save/restore orchestration |
| `coalesce` | — (Rust-only) | singleflight grouping of concurrent same-key requests |
| `app` | `app.py` | axum router, request pipeline, streaming pump, log/dashboard middleware |
| `main` | `proxycache.py` | entry point: parse config, load plugins from the registry, wire `AppState`, serve |
| `procsample` | — (Rust-only) | `/proc` sampling of the proxy's CPU time and disk I/O per request |
| `mock_backend` | — (test infra) | in-process mock llama-server for integration tests |

`examples/`: `mock_llama` (runnable mock backend), `load_test` (load
generator). `tests/`: `api.rs` (77 E2E) and `client.rs` (15 HTTP-behavior)
integration suites against `MockLlama`.

## 4. Request lifecycle

The `POST /v1/chat/completions` pipeline, in order (the `app` module
implements it; `GET /v1/models` and the `/_control/*` endpoints bypass it):

```
 client POST /v1/chat/completions
   │
   ▼
 ① middleware (axum): capture W3C fields, start proc sample,
   payload_log.log_payload(raw bytes + headers)   [before parsing —
                                            malformed bodies captured as nokey files]
   ▼
 ② body parse; model resolution
      model-aware routing? request.model must match a backend model_id
      (case-insensitive) or 404 "model not found" (lists available ids)
   ▼
 ③ cache-key computation (hashing):
      raw_prefix = message contents (roles stripped) joined "\n\n"
      words = \w+ tokens (lowercased); blocks = sha256 per wpb words
      key = sha256(model_id + "\n" + raw_prefix); is_big = n_words > threshold
   ▼
 ④ prefilter chain (plugins, config order):
      Reject{status,msg} → immediate JSON error (lpcache-reason recorded)
      Serve{status,body} → immediate cached response (lpcache-cache-hit)
      (both short-circuits: no slot, no restore/save, no meta, no group)
   ▼
 ⑤ coalescing (if enabled): same key already in flight?
      follower → wait on the leader's Flight, receive the shared outcome
      (stream followers follow the SSE pump; JSON followers get the
      aggregated result); a follower never touches slots/backends/meta
      leader → continue
   ▼
 ⑥ slot acquisition (SlotManager, up to 300 s):
      candidates: not drained, backend not in cooldown (unless all down →
      fail-open), model match (if routing on), kv flag respected
      policy tiers: key_affinity → idle slot already running the restore
      key · free (never-used, insertion order) · LRU (oldest last_used)
      pick → acquire per-slot Semaphore(1); on connection failure: retry
      excluding the failed backend; on timeout → 503 (slot_timeout)
   ▼
 ⑦ restore (big requests with a candidate, LCP ratio >= LCP_TH):
      skip if the picked slot's last-KV == requested key (zero wire call)
      else backend action=restore:
         Restored → last-KV := key
         Rejected (400) → remove stale meta, forget last-KV
         Failed (other) → transient; chat proceeds un-restored
   ▼
 ⑧ backend call (slot pinned in body/options/query):
      non-stream: read full response
        200 JSON object → respond; store in result_store (if present);
                          big request → save_after + write/touch meta
        200 non-object  → 502
        5xx/other       → error JSON (backend_error / provider_http_…)
      stream: bounded mpsc (STREAM_QUEUE_SIZE) + background reader pumping
        raw SSE to the client; the reader's "finally" ALWAYS runs
        save + meta + slot release; if the client dropped first, the
        reader aborts the upstream request (drops the stream, closing
        TCP — the backend has no cancel endpoint) before cleanup
   ▼
 ⑨ save_after (big requests, kv-enabled backend):
      skip if self-restore succeeded and SKIP_SAVE_ON_SELF_RESTORE (a
      warm "already running" skip only while the key's meta exists; a
      missing meta → save to heal it)
      else backend action=save, retried SAVE_RETRIES × SAVE_RETRY_DELAY_MS
      on success: last-KV := key, touch/write meta, LRU prune (META_MAX)
      which also removes same-named KV files from visible slot-save dirs
   ▼
 ⑩ slot release (RAII SlotGuard drop). When the response body completes
      (or the client went away), the attached PendingRequestLog's Drop
      finalizes: end proc sample → RequestLogRecord → request_log.log +
      dashboard.record (one record per finished request, success or not)
```

Key invariants:

- **Prefilter short-circuit is total** — a Reject/Serve request never
  leads or joins a coalescing group (the check runs before §⑤).
- **Followers are pure consumers** — no slots, backends, or meta files;
  they receive the leader's outcome, converted to their response shape.
- **A flight is removed as soon as the leader finishes** — a later
  same-key request starts a fresh group.
- **The stream "finally" always runs** — save + meta + slot release
  happen on every completion path, including client disconnect and
  backend error.
- **Logging never breaks serving** — the payload write runs on the
  blocking pool; the XLF record is written synchronously from the
  response body's `Drop` (the only hook a disappearing body offers);
  dashboard updates are in-memory. All failures are warned and
  swallowed.

## 5. Core components

### 5.1 `app` — HTTP layer & pipeline

- `AppState` (cloned into every handler) holds: `Arc<Config>`,
  `Vec<Arc<dyn LlamaBackend>>` clients (one per backend),
  `Arc<SlotManager>`, `Arc<SingleFlight>`, and the optional plugin
  instances — `prefilter`, `result_store`, `request_log`, `payload_log`,
  `dashboard` (each `None` = that feature disabled, the default).
- Routes: `GET /v1/models`, `POST /v1/chat/completions`, the operator
  `POST /_control/backends/{index}/drain|resume` (always mounted, 404 on
  out-of-range index), plus the dashboard's routes when loaded.
- A single middleware wraps every request (active when a request logger
  and/or dashboard is loaded): it captures the W3C fields, starts the
  `ProcSample`, and wraps the response body so that when the body
  completes (or the client goes away) the attached `PendingRequestLog`'s
  `Drop` builds the `RequestLogRecord` from the W3C fields plus the
  `PipelineLog` the handlers attached via response extensions (model,
  stream, key, word count, backend, slot, restore outcome/candidate, LCP
  ratio, cache-hit, coalesced flag, outcome reason) and dispatches it to
  the request logger and the dashboard. Dashboard page/stats URIs are
  excluded from this accounting (they are internal polling).
- Streaming design: a background reader task reads the backend SSE
  stream and pushes raw bytes through a bounded `tokio::sync::mpsc`
  channel into the HTTP response; the channel provides backpressure to a
  slow client (`STREAM_QUEUE_SIZE`, default 16). The reader owns the
  cleanup ("finally"): save + meta + slot release run exactly once, and
  on client disconnect it first aborts the in-flight upstream request by
  dropping the stream (closing the TCP connection — the backend has no
  cancel endpoint). Non-streaming requests read the full body before
  responding; a client disconnect there drops the handler future, which
  drops the backend-client future and aborts upstream the same way.

### 5.2 `hashing` — content addressing & meta files

- `raw_prefix(messages)`: message **contents only** (roles stripped,
  empty contents skipped, non-string content as its JSON text) joined by
  `\n\n`.
- `words_from_text`: `\w+` tokens on the lowercased text — Unicode
  letters/digits/underscore, mirroring Python's `\w+`.
- `block_hashes_from_text`: the word list chunked into blocks of
  `WORDS_PER_BLOCK` (default 100); each block SHA256-hashed (words
  space-joined).
- Cache key: `sha256(model_id + "\n" + raw_prefix)` — the model is
  whichever id the cache is addressed by (first backend's reported id, or
  the request's lowercased `model` under model-aware routing).
- KV file basename: `{lowercase(model_id)}_{key}` — the model prefix
  isolates models' caches in a shared slot-save directory, and matches
  the id hashed into the key so a key always addresses the same filename
  it was saved under.
- Meta files (`{key}.meta.json` in `META_DIR`): `key`, `model_id`,
  `prefix_len`, `wpb`, `blocks` (the block-hash list), `timestamp` —
  the fingerprint a future request's LCP scan compares against.
- `find_best_restore_candidate`: scans all metas of the same model and
  `wpb`, computes `LCP ratio = lcp_blocks(req, meta) / min(len(req),
  len(meta))`, and returns the candidate with the best ratio at or above
  `LCP_TH` (default 0.1).
- `prune_meta`: keeps at most `META_MAX` metas (default 10; 0 =
  unlimited), removing the oldest by `timestamp` (ties: mtime, filename;
  unparseable files count as oldest) and returning `(key, model_id)`
  pairs so the caller deletes the same-named KV files from the slot-save
  directories it can see.

### 5.3 `slot_manager` — selection, locking, resilience

State, per **global slot** `GSlot = (backend_id, local_slot_id)`:
`Semaphore(1)` (the slot lock, RAII via `SlotGuard`), a `last_used`
atomic (LRU), and a mutex-guarded `last_kv: Option<String>` — the last KV
key the slot is *known* to be running (set by a successful restore or
save, cleared after a failed restore). State, per **backend**:
`down_until` (circuit-breaker deadline), `failure_streak` (cooldown
escalation), `drained` (operator flag), plus `kv_enabled` and the
optional `model_id`.

- **Selection tiers** (`SLOT_ROUTING_POLICY`, default `key_affinity`):
  1. *key-free* (key_affinity only, restore-keyed requests): an idle
     slot whose `last_kv` equals the requested key — the restore then
     costs zero wire calls;
  2. *free*: never-used slots in insertion order;
  3. *LRU*: the smallest `last_used` (idle slots win the tie-break).

  A slot becomes "used" when any request completes on it — success or
  failure (small requests never save, so the completion itself stamps
  the slot). Excluded from all tiers: slots of drained backends and of
  backends inside cooldown (unless every backend is down — fail-open,
  which still never routes into drained backends); under model-aware
  routing, slots of non-matching backends; restores are only attempted
  on `kv_cache`-enabled backends.
- **`acquire`** waits up to 300 s (`ACQUIRE_TIMEOUT`) for the semaphore;
  on a connection failure during restore/chat the caller re-acquires
  *excluding* the failed backend; on timeout the request is a 503.
  `acquire` performs the restore before returning the `SlotGuard` (which
  reports `restored`, whether a wire call was actually made, and its
  duration).
- **`save_after`** performs the post-request save with `SAVE_RETRIES`
  retries (delay `SAVE_RETRY_DELAY_MS`, default 3 × 1000 ms); a backend
  500 reports a failed save, a transport error after all retries makes
  the request a 500.
- **Circuit breaker**: a connection failure sets
  `down_until = now + 5 s × 2^streak` (capped at 60 s) and bumps the
  streak; any successful request or probe resets both. A 1-second
  background probe loop polls `GET /v1/models` on down backends
  (skipping drained ones); a healthy answer re-joins the backend.
- **Drain/resume**: `drain(be)` sets the flag and parks `down_until` at
  a 10-year sentinel — the backend leaves the pool immediately,
  in-flight requests finish, and nothing (not even a healthy probe)
  re-joins it. `resume(be)` clears the flag and probes inline; if the
  probe fails, the sentinel stays and the background loop keeps trying.
- **Observability hooks**: `slot_snapshot()` (occupancy + LRU age for
  the dashboard's slot grid) and per-backend breaker status.

### 5.4 `coalesce` — singleflight

A `HashMap<key, Arc<Flight>>` under a `std::sync::Mutex` (short
critical sections, no async work inside) plus a per-flight
`tokio::sync::Notify`. The leader registers its `Flight` and runs the
pipeline; concurrent same-key requests join it. A `Flight` carries: the
accumulated raw SSE bytes (appended as the streaming leader pumps), a
`stream_started` flag (lets stream followers commit to a 200 SSE
response, or bail with the leader's pre-start error), and the terminal
`SharedOutcome` — `Json(status, body)`, `Sse(bytes)`, or
`Raw(status, content_type, bytes)` — set exactly once. When the leader
finishes, the flight is removed from the map (the next same-key request
starts a fresh group) and waiters are notified. Grouping deliberately
ignores generation parameters — documented and tested (a follower may
receive the leader's sampling settings, `max_tokens`, and stream mode,
converted to its response shape).

### 5.5 `config` — layered configuration

- Precedence: **CLI flag > env var > built-in default** (every parameter
  has both forms; see PARAMETERS.md). Invalid numeric/boolean values log
  a warning and fall back to the default; unknown flags are errors.
- `BACKENDS` (JSON array) with per-backend `url`, `n_slots` and optional
  `slot_save_path`, `kv_cache` (default true), `model_id`; without it,
  the `LLAMA_URL`/`N_SLOTS` fallback backend. `model_id` is validated
  all-or-nothing at startup (a mixed set names the offending entries and
  exits).
- `PLUGINS` (JSON array of `{name, params}` specs) feeds the plugin
  registry; when unset, legacy `PREFILTER_*` / `REQUEST_LOG*` /
  `REQUEST_PAYLOAD_DIR` values are synthesized into equivalent specs
  (precedence rules in PLUGINS.md §2.2).
- `procsample` (supporting module): reads `/proc/self/stat`,
  `/proc/self/io` and `status` (RSS/HWM) at request start and finish to
  attach the proxy's own CPU-ms and disk-IO deltas to the log record —
  process-wide counter deltas, so approximate under concurrency; values
  the platform cannot sample render as `-`.

## 6. Cache design (save/restore lifecycle)

The KV cache is a three-way contract between the proxy, the meta
directory, and each backend's slot-save directory:

| Artifact | Location | Owner | Content |
|---|---|---|---|
| KV file `{model}_{key}` | backend `slot_save_path` | llama-server (written by `action=save`) | the slot's raw KV state after a request |
| Meta `{key}.meta.json` | proxy `META_DIR` | lpcache | block-hash fingerprint + model id + `wpb` + timestamp |
| In-memory `last_kv` | `SlotManager` | lpcache | the key a slot is known to be running right now |

**Big vs small.** Only "big" requests (`n_words > BIG_THRESHOLD_WORDS`,
default 500) participate: they run with `cache_prompt=true`, may restore
before chat, and save + write meta after. Small requests pass through
with no cache side effects (their completion still stamps the slot for
LRU purposes).

**Restore candidate selection.** For a big request, the proxy computes
its block hashes and scans all metas of the same model + `wpb`; the
candidate with the best LCP ratio at or above `LCP_TH` wins. An exact
repeat yields ratio 1.0 with candidate == own key ("self-restore"); a
multi-turn extension yields ratio 1.0 over the shared leading blocks
against the *previous* key ("closest-prefix restore"). The candidate and
ratio are recorded in the request log
(`lpcache-restore-candidate`, `lpcache-lcp-ratio`).

**The self-restore save skip** (`SKIP_SAVE_ON_SELF_RESTORE`, default on):
when a request successfully restored its *own* key, the post-request
save is skipped — a cold restore's 200 proved the KV file's presence, and
a warm "already running" no-op restore trusts the file while the key's
meta is still on disk. If the meta has since gone (LRU pruning, manual
cleanup), the save re-runs and *heals* the meta/KV pair. A ratio-1.0
restore from a *different* key always still saves (the file holds the
other key's state).

**Stale-meta cleanup.** A restore `Rejected` (backend 400) means the KV
file is useless on that backend (missing, corrupt, or too big for the
slot's context) — the proxy deletes the stale meta so it stops being a
candidate, and clears the slot's `last_kv`. When the slot-save directory
is not visible to the proxy (remote backend), the KV file itself cannot
be checked and only the meta is managed.

**Startup cleanup.** At startup, corrupt `.meta.json` files are always
removed; with `STARTUP_CLEANUP_KV=true` their orphan KV files are removed
too (the model id is unrecoverable from a corrupt meta, so KV names are
matched by key suffix). Orphan KV files of *valid* metas are kept.

**LRU.** After every successful save, `prune_meta` trims `META_MAX`
entries (oldest by meta timestamp) and deletes the corresponding KV
files from every visible slot-save directory. A restore of a surviving
entry touches its meta timestamp, keeping hot entries alive; a pruned
key simply falls back to the un-restored path (its meta is gone, so the
next request re-saves it).

## 7. Plugin architecture

All adapter functionality lives in the `lpcache-plugins` crate; the
binary only *loads* adapter instances from configuration at startup.
Which plugins run is decided entirely by configuration, not compile-time
wiring — a custom plugin crate can register new names and become
loadable from `PLUGINS` like any built-in.

### 7.1 The adapter traits (`lpcache_plugins::api`)

| Trait | Pipeline slot | Contract |
|---|---|---|
| `Prefilter` | ④ | `check(&PrefilterRequest) -> PrefilterDecision` — `Accept` / `Reject {status, message}` / `Serve {status, body}`. Runs before coalescing/slots/backends; `result_store()` optionally exposes the backing store |
| `ResultStore` | ⑧ | `prepare()` (once at startup) + `store(model_id, key, status, body)` after successful non-streaming backend calls; `caches_streaming()` opts in to storing aggregated stream results |
| `PostCleanup` | ④ (post-serve) | `cleanup(key, path)` after a cached result was read into memory (e.g. built-in `RemoveAfterServe` one-shot) |
| `LlamaBackend` | ⑥–⑧ | `chat_stream` / `chat_json`, `save_slot` / `restore_slot`, `get_model_id` — one instance per `BACKENDS` entry |
| `RequestLogger` | ⑩ | `prepare()` + synchronous `log(&RequestLogRecord)` — the record is written from the response body's `Drop` (the only hook a disappearing body offers); failures swallowed |
| `PayloadLogger` | ① | `prepare()` + `log_payload(key, payload, headers)` on raw arrival, before parsing (runs on the blocking pool) |
| `Dashboard` | ⑩ + routes | `record(&RequestLogRecord)` for aggregates + serves its page/`api/stats` (routes excluded from log/stats) |

Shared data types: `PrefilterRequest` (parsed body, messages, model,
backend model id, stream flag, key, word count, is_big, headers),
`RequestLogRecord` (the 36-field W3C-XLF + pipeline record),
`RestoreOutcome` (`Restored` / `Rejected` / `Failed`), `BackendError`,
`SlotStatus`, `BackendStatus`, `ProxyStatus`. All traits are
`Send + Sync` — instances are shared behind `Arc` across requests.

### 7.2 Registry & loading (`lpcache_plugins::registry`)

- A `PluginSpec { name, params: Value }`; the `PLUGINS` value is a JSON
  array of specs. `Registry::register(name, factory)` adds a
  name → `PluginFactory` mapping; `Registry::with_builtins()` / the
  process-global registry preload the eight built-ins: `keyword`,
  `qwen_guard`, `result_cache`, `chain` (prefilters), `llama` (backend
  client), `request_log`, `payload_log`, `dashboard`.
- Loaders: `load_prefilters` (specs in config order; multiple prefilters
  are wrapped in a `chain` — first non-`Accept` wins; a `chain` spec can
  also nest explicitly), `load_request_loggers` /
  `load_payload_loggers` / `load_dashboard` (at most one each).
- **Invalid specs are startup errors**: the proxy prints the offending
  entry and exits (code 2) before binding its port — parameter
  validation rules and error formats are in PLUGINS.md §2.5.
- Legacy translation: with `PLUGINS` unset, non-empty
  `PREFILTER_BLOCKLIST` → a `keyword` spec, `PREFILTER_RESULT_CACHE_DIR`
  → a `result_cache` spec, `REQUEST_LOG=true` → a `request_log` spec
  from the `REQUEST_LOG_*` values, `REQUEST_PAYLOAD_DIR` → a
  `payload_log` spec. With `PLUGINS` set, the specs are used as-is and
  the corresponding legacy variables are ignored (per-slot precedence).

### 7.3 Wiring in `main`

`main` parses `Cli` → `Config::from_cli` (precedence CLI > env >
default), validates backends (all-or-nothing `model_id`), loads the
plugin specs through the registry, constructs one `LlamaBackend` per
`BACKENDS` entry from the standard config (URL, API key, timeout — the
`llama` client is not selected via `PLUGINS`), calls
`ResultStore::prepare`, then builds `AppState` and serves with
`axum::serve`. The `lpcache-config` configurator reuses the same
`Config` and registry (for its plugin schema and validation) to build,
validate, and export configurations without running the proxy.

## 8. Failure & resilience model

Failure classes and their treatment (each has dedicated tests):

| Failure | Treatment |
|---|---|
| Restore rejected (backend 400) | stale meta removed, slot `last_kv` cleared, request proceeds **un-restored** (degrade, not error) |
| Restore transport error | treated as transient (`Failed`); request proceeds un-restored — deliberate deviation from Python, whose unhandled error also leaked the slot lock |
| KV save: backend 500 | retried `SAVE_RETRIES` ×; final 500 → save reported failed, request still succeeded |
| KV save: transport error | retried `SAVE_RETRIES` ×; final `Err` → request 500 |
| Backend connection failure (restore/chat) | circuit breaker cooldown `5 s × 2^streak` (cap 60 s); the in-flight request is retried once on another backend; a failing request also stamps the slot used so traffic moves on |
| All backends down | fail-open: selection falls through, the request surfaces the real backend error (drained backends are never picked even here) |
| Backend recovery | 1 s probe loop (`GET /v1/models`); healthy probe clears cooldown + streak |
| Slot pool exhaustion | acquire waits up to 300 s → 503 (`slot_timeout`) |
| Client disconnect (stream) | reader aborts the upstream request (drops the stream → closes TCP; no backend cancel endpoint), then runs the always-on finally: save + meta + slot release |
| Client disconnect (non-stream) | hyper drops the handler future → backend-client future drops → upstream aborted; slot release via `SlotGuard` RAII |
| Meta/KV corruption | corrupt metas removed at startup (orphan KV files too with `STARTUP_CLEANUP_KV`); rejected restores clean up stale metas at runtime |
| Slot-save dir invisible/unwritable | pruning for that backend skipped; proxy runs normally |
| Logging / result-store / dashboard errors | logged and swallowed — **never** affect the in-flight or subsequent responses |
| Malformed request body | 400 JSON error; the raw payload is still captured by the payload log as a `nokey` file |
| `qwen_guard` call failure | `on_error` policy: `accept` (default, fail-open) or `reject` (fail-closed) |

Design rules: (1) a cache-layer failure degrades to the un-cached path,
never to a proxy error; (2) slot release is RAII, so no failure path can
leak a slot; (3) operator actions (drain) are monotonic until explicitly
reversed; (4) every adapter side-channel is failure-isolated.

## 9. Concurrency model

- **Runtime**: single tokio multi-thread runtime (`#[tokio::main]`);
  the axum router, the 1 s probe loop, and per-request background stream
  readers are tasks on it.
- **Locking discipline**: `std::sync::Mutex` only for short critical
  sections (the singleflight map, per-slot `last_kv`, flight buffers) —
  no async work inside; `tokio::sync::Semaphore(1)` per slot for the
  actual request mutual exclusion (held across awaits, released by RAII);
  atomics (`AtomicU64`/`AtomicU32`/`AtomicBool`) for LRU stamps, breaker
  deadlines, streaks, and drain flags; `tokio::sync::Notify` to wake
  coalescing followers.
- **Blocking work**: only the payload-log write runs on the blocking
  pool (`spawn_blocking`) so a slow disk never stalls the request hot
  path; the other disk work (meta scans/writes/pruning, the XLF append
  from the body's `Drop`) is short synchronous file I/O, and the
  `/proc` CPU/IO sampling is a couple of small reads.
- **Backpressure**: the bounded `STREAM_QUEUE_SIZE` mpsc between the
  backend reader and the HTTP response bounds memory for slow clients.
- **Sharing**: everything behind `Arc` (`Arc<Config>`,
  `Vec<Arc<dyn LlamaBackend>>`, `Arc<SlotManager>`, `Arc<SingleFlight>`,
  plugin instances) — `AppState` is `Clone`-cheap and sent into every
  handler.

## 10. Testing architecture

A four-layer pyramid, all runnable with `cargo test --workspace`
(359 tests incl. 1 doctest, ~4 s — no real `llama-server`, model files,
or network access required; the full catalog with per-test runnable
filters lives in [test-cases/](../test-cases/README.md)):

1. **Pure functions** — `hashing` (tokenization, block hashes, LCP, meta
   read/write/prune/cleanup) and `config` parsing/precedence: fast,
   sync, table-driven.
2. **Components** — `coalesce` (singleflight semantics), the built-in
   plugins against fakes (`TestClient` backend stubs, temp dirs),
   `slot_manager` (36 async tests: selection tiers, locking, breaker,
   drain, save retries), and the plugin registry (specs, loading,
   chaining).
3. **End-to-end** — `tests/api.rs`: the real axum router + real reqwest
   client against an in-process `MockLlama` (the same mock as the
   `mock_llama` example) on free localhost ports: restore/save flows,
   pruning, failover, coalescing, prefilters, result cache, dashboard
   and request-log behavior.
4. **Real-TCP** — `tests/client.rs`: client-disconnect behaviour
   (upstream abort, slot release) and HTTP-level edge cases that only
   show up over real connections.

Quality gates: `cargo clippy --all-targets` (0 warnings expected) and
`cargo fmt --check` are part of the workflow; Rust-only deviations from
the Python original (breaker, coalescing, plugins, pruning, …) each have
dedicated tests, cross-referenced in the catalog.

## 11. Known limitations & design notes

- **Word-based, not token-based hashing.** The cache key and block
  hashes are computed over `\w+` word tokens, not the model's
  tokenizer. Two prompts that differ inside a 100-word block produce
  different block hashes even if they share most *tokens*; a restore
  always aligns to 100-word block boundaries (`WORDS_PER_BLOCK`), so the
  re-prefilled tail is bounded by ~wpb words plus the divergent tail.
- **Coalescing ignores generation parameters.** A follower receives the
  leader's sampling settings, `max_tokens`, and stream mode — fine for
  identical-prompt bursts, wrong for same-prompt-different-temperature
  traffic (leave `COALESCE_REQUESTS` off in that case).
- **Result cache is file-based and per-host** with lazy TTL expiry; it
  stores only non-streaming 200 JSON results by default.
- **KV files are opaque to the proxy.** The proxy cannot validate them
  (it may not even see the directory); correctness relies on the
  backend's `restore` 400 rejection plus the stale-meta cleanup.
- **Proc sampling is approximate under concurrency** (process-wide
  counter deltas), and unavailable fields render as `-` on non-Linux
  platforms.
- **No backend cancel endpoint.** Client disconnects abort upstream by
  dropping the TCP connection; the backend may keep generating until it
  notices the closed stream.
- **Single proxy process.** Slot state, flights, and the breaker are
  in-memory; running two lpcache processes in front of the same
  backends is unsupported (they would both manage the same slots and KV
  files without coordination).

## 12. References

- [README.md](../README.md) — overview, full parameter table, test catalog
- [PARAMETERS.md](../PARAMETERS.md) — every env var/CLI flag, matching
  llama-server flags, precedence & gotchas, runtime backend control
- [PLUGINS.md](../PLUGINS.md) — plugin API, built-ins, custom-plugin how-to,
  configuration recipes, live validation notes
- [docs/setup-guide.md](setup-guide.md) — end-to-end setup (llama-server
  flags, systemd, verification, troubleshooting)
- [docs/configurator-guide.md](configurator-guide.md) — the
  `lpcache-config` CLI wizard and web UI
- [docs/use-cases.md](use-cases.md) — scenario-driven use cases
- [test-cases/README.md](../test-cases/README.md) — 1:1 regression test
  catalog (359 tests) with per-test runnable filters