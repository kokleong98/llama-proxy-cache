# lpcache — Use Cases

Companion to the [README](../README.md) (what the proxy does),
[PARAMETERS.md](../PARAMETERS.md) (every tunable),
[PLUGINS.md](../PLUGINS.md) (plugin reference), the
[Setup Guide](setup-guide.md) (how to stand one up), and the
[Architecture](architecture.md) design document (how it works inside).

This document walks through the workloads lpcache is built for. Each use
case lists the scenario, a minimal configuration, what the proxy does for
you, and the caveats to know. All configurations below are env vars —
every one is also a CLI flag (see `lpcache --help`).

---

## 1. Reusing KV caches for long, repetitive prompts

The core value proposition: llama.cpp serializes requests through its
slots and pays the full prefill cost on every request. lpcache addresses
each request by a content-derived KV cache key, restores a saved KV prefix
into a slot before dispatch, and saves the slot's KV cache afterwards — so
a second request sharing the first request's long prefix pays only for the
new tail.

### 1.1 Document-QA assistant (RAG-style)

**Scenario.** A team chatbot answers questions over one large knowledge
document (policy manual, codebase digest, long spec). Every user question
carries the same ~5,000-word context with only the question itself
differing.

**Setup** (one llama-server, four slots, shared KV directory):

```sh
BACKENDS='[{"url":"http://127.0.0.1:8000","n_slots":4,"slot_save_path":"/srv/llama/kv"}]'
META_DIR=/srv/llama/kv_meta
META_MAX=100      # keep a bigger KV history (default is 10)
LCP_TH=0.5        # restore a candidate sharing >=50% of blocks (default 0.1)
```

**What happens.**

1. First question: cold prefill. The request is "big" (well over the
   default `BIG_THRESHOLD_WORDS=500`), so it runs with
   `cache_prompt=true`; after completion lpcache saves the slot's KV cache
   (`/srv/llama/kv/{model}_{key}`) and writes
   `/srv/llama/kv_meta/{key}.meta.json` (the block-hash fingerprint).
2. Second question (same document, different question): the proxy computes
   the same block hashes for the shared document portion, scans the meta
   files, finds the candidate with the best longest-common-prefix ratio,
   restores that KV file into the slot, and the backend only prefills the
   new question. The request log shows
   `lpcache-restore-candidate` / `lpcache-lcp-ratio` and the log lines
   `restore_candidate ... ratio=0.9xx` / `restore_before_chat ... ok=true`.

**Caveats.**

- Only "big" requests (more than `BIG_THRESHOLD_WORDS` words of message
  content) are cache-eligible — small requests never restore or save.
- The cache key hashes *contents only* (roles are stripped), so the same
  text addressed by different clients still hits the same cache entry.
- Restores are per model: the KV file and cache key both carry the model
  id, so a different model never touches another model's KV files.

### 1.2 Multi-turn conversation / agent sessions

**Scenario.** A coding agent or chat app keeps a growing conversation with
a fixed system prompt; each turn appends a few messages to the previous
prefix.

**Setup.** The defaults are already tuned for this — the only requirement
is that the conversation outgrows 500 words:

```sh
BACKENDS='[{"url":"http://127.0.0.1:8000","n_slots":4,"slot_save_path":"/srv/llama/kv"}]'
```

**What happens.** Each turn's message content is an *extension* of the
previous turn's, so its block-hash fingerprint matches the previous turn's
fingerprint with LCP ratio 1.0 over the shared blocks — the previous
turn's KV cache is restored and only the new messages are prefilled. Two
further optimizations kick in automatically:

- **`key_affinity` slot routing** (default `SLOT_ROUTING_POLICY`): if an
  *idle* slot is already running this turn's key (e.g. the client's
  previous turn just finished on it), the proxy picks that slot and the
  restore becomes a zero-wire-call skip — no backend `action=restore`
  round-trip at all.
- **`SKIP_SAVE_ON_SELF_RESTORE`** (default `true`): when a request
  successfully restores *its own* key, the post-request KV save is
  skipped — the saved file already holds that key's state. (A ratio-1.0
  match against a *different* key, i.e. a genuine multi-turn extension,
  still saves.)

**Caveats.** The saved prefix is the full conversation up to the save
point; if the agent branches (edits earlier messages), the fingerprints
diverge and the new branch re-prefills from the last common block boundary
(100 words per block by default — `WORDS_PER_BLOCK`).

---

## 2. Absorbing bursts of identical concurrent requests (coalescing)

**Scenario.** An evaluation harness, a batch classifier, or many users
hitting the same long prompt at once (e.g. right after a release note)
send N concurrent requests with the same content. Without coalescing,
N requests queue on the slots and each pays a full prefill/generation.

**Setup.**

```sh
COALESCE_REQUESTS=true
```

**What happens.** Concurrent requests with the same KV cache key form one
group regardless of generation parameters: the first request ("leader")
runs the normal pipeline (slot acquire → restore → backend call → save),
and every other concurrent request ("follower") waits for the leader and
receives its result. Followers never touch slots, backends, or meta files
— the N-way burst costs exactly one backend call. A mixed group works too:
a streaming leader's full SSE output is converted to JSON for non-streaming
followers and vice versa. A request arriving *after* the leader finishes
starts a fresh group (it may still hit the restored KV cache, though).

**Caveats.** Grouping ignores generation parameters: a follower receives a
result generated under the *leader's* `temperature`/`top_p`/`seed`,
`max_tokens`, and stream mode. Only enable coalescing for workloads where
that is acceptable (identical prompts, near-deterministic settings).

---

## 3. Caching fully repeated queries (result cache)

**Scenario.** Some endpoints are effectively deterministic (temperature
0, fixed seed): identical requests should get identical answers. For those,
even a KV restore is wasted work — the whole answer can be replayed from a
stored completion, skipping prefill *and* generation, with zero slot
occupation.

**Setup** (`PLUGINS` — the modern way to configure plugins):

```sh
PLUGINS='[{"name":"result_cache","params":{"dir":"/var/lpcache/results","ttl_secs":3600}}]'
```

**What happens.** After every successful non-streaming backend call, the
200 JSON result is stored at `/var/lpcache/results/{model}_{key}.json`. A
later request with the same cache key that is still within the TTL is
answered straight from that file: no slot acquired, no KV restore/save, no
meta file, no coalescing group. Optional parameters:

- `cache_streaming: true` — also aggregate completed *streaming* results
  into a non-streaming completion and cache it; cached entries are then
  served to streaming clients as SSE.
- `cache_control: true` — a request whose `Cache-Control` header carries
  `no-cache` bypasses the cache and is forwarded to the backend.
- `cleanup: "remove_after_serve"` — one-shot entries: the file is deleted
  after it is served (useful for expensive one-off computations).

The legacy env vars `PREFILTER_RESULT_CACHE_DIR` /
`PREFILTER_RESULT_CACHE_TTL` do the same when `PLUGINS` is unset.

**Caveats.** By default only non-streaming 200 JSON results are stored;
entries are files, so the cache is per-host (no shared store); expiry is
TTL-based and lazy.

---

## 4. Multi-model fleet behind one endpoint (model-aware routing)

**Scenario.** A single OpenAI-compatible endpoint should serve several
models — e.g. a small fast model for triage/classification and a large
model for synthesis — each pinned to its own backend.

**Setup.** `model_id` on backends is all-or-nothing (every backend or
none — a mixed set is a startup error):

```sh
BACKENDS='[
  {"url":"http://127.0.0.1:8000","n_slots":2,"model_id":"triage"},
  {"url":"http://127.0.0.1:8010","n_slots":1,"model_id":"synthesis"}
]'
```

**What happens.**

- A request's `model` must match a configured id (case-insensitive) or the
  proxy answers `404` with the list of available ids — no backend call is
  made for unknown models.
- Each model's traffic only runs on its own backend; `GET /v1/models`
  lists the configured ids.
- KV caches are isolated per model: cache keys hash the model id in and
  KV files are named `{lowercase(model_id)}_{key}`, so models never
  restore each other's KV state.

**Caveats.** When model-aware routing is off (no `model_id` anywhere), all
traffic is addressed to the first backend's reported model id and any
request `model` value is accepted — the classic single-model setup.

---

## 5. High-availability backend pool & rolling upgrades

**Scenario.** Several GPU nodes run llama-server behind one proxy. A node
dies, a network blips, or an operator wants to restart a node / swap its
model while clients keep streaming.

**Setup.** All nodes share the proxy's KV layout (each backend's
`slot_save_path`; for cross-node restores a shared directory such as a
network mount):

```sh
BACKENDS='[
  {"url":"http://gpu-a:8000","n_slots":4,"slot_save_path":"/shared/kv"},
  {"url":"http://gpu-b:8000","n_slots":4,"slot_save_path":"/shared/kv"}
]'
```

**What the proxy does automatically.**

- **Circuit breaker:** a connection failure to a backend parks it for a
  cooldown that escalates with the failure streak (5 s → 10 s → 20 s →
  40 s, capped at 60 s); the in-flight request is transparently retried
  on another backend.
- **Probe loop:** every second the proxy probes cooled-down backends with
  `GET /v1/models`; the first healthy answer clears the breaker and the
  backend re-joins the slot pool.
- **Fail-open:** when *all* backends are down, selection falls through
  rather than queueing forever (requests then surface the real backend
  error).

**Rolling upgrade / maintenance.** Drain a backend out of rotation at
runtime without touching its in-flight requests:

```sh
curl -X POST http://127.0.0.1:8081/_control/backends/0/drain    # stop new traffic
# ... restart / update the node ...
curl -X POST http://127.0.0.1:8081/_control/backends/0/resume   # re-join (one health probe)
```

A drained backend is excluded even from the all-down fail-open path;
`resume` clears the drain and probes immediately, falling back to the
background probe loop if the node isn't up yet. These operator endpoints
are excluded from the request log and dashboard aggregates.

**Caveats.** If the proxy cannot see a backend's slot-save directory
(remote, unwritable), KV-file pruning for that backend is skipped (the
proxy still runs); cross-node restore requires the KV files to actually
be shared.

---

## 6. Safety & compliance gate in front of the GPUs

**Scenario.** A public-facing endpoint must refuse certain content *before*
spending any GPU time — cheap keyword rules plus an LLM-based moderation
verdict.

**Setup** (chained prefilters — evaluated in list order, first non-accept
wins; the blocklist runs first because it is free):

```sh
PLUGINS='[
  {"name":"keyword","params":{"keywords":["confidential","internal-only"]}},
  {"name":"qwen_guard","params":{
     "url":"http://127.0.0.1:8020",
     "reject_levels":["Unsafe"],
     "reject_categories":["PII","Jailbreak"],
     "on_error":"reject"
  }}
]'
```

**What happens.** Every request is checked right after body parsing and
cache-key computation, before coalescing, slot acquisition, or backend
dispatch:

- `keyword` rejects (HTTP 400, naming the first matched keyword) when the
  combined message text contains a blocked substring (case-insensitive by
  default).
- `qwen_guard` classifies the combined message text with a separate
  **Qwen3Guard-Gen** server (OpenAI-compatible) and rejects with 400 when
  the verdict hits `reject_levels` (default `["Unsafe"]`) or
  `reject_categories`. `on_error: "reject"` makes a failed guard call
  fail-closed; the default `"accept"` is fail-open. `cache_max` keeps a
  bounded LRU of verdicts so repeated same-key prompts skip the guard call.

Rejected requests never reach llama-server: no slot, no KV restore/save,
no meta file, no coalescing group — and with `REQUEST_LOG=true` they are
still recorded (`lpcache-reason=prefilter_reject`), which makes this a
usable audit trail.

---

## 7. Audit trail & operational observability

**Scenario.** Compliance needs a per-request audit record (who asked what,
what happened, why), and ops needs to see whether the cache is actually
earning its keep.

**Setup.**

```sh
REQUEST_LOG=true
REQUEST_LOG_PATH=/var/log/lpcache/request.log
REQUEST_LOG_MAX_BYTES=52428800   # rotate at 50 MiB
PLUGINS='[{"name":"dashboard","params":{"path":"/stats"}}]'
# optionally capture raw payloads for forensic replay:
REQUEST_PAYLOAD_DIR=/var/log/lpcache/payloads
```

**What happens.**

- **Request log (W3C-XLF):** one record per *finished* request —
  successes, errors, and client disconnects — with 36 selectable fields:
  the W3C transaction fields (method, URI, client IP, user agent, bytes,
  duration, status) plus the pipeline's view (`lpcache-key`,
  `lpcache-is-big`, `lpcache-backend`, `lpcache-slot`,
  `lpcache-restore-candidate`, `lpcache-lcp-ratio`, `lpcache-cache-hit`,
  `lpcache-coalesced`, `lpcache-reason`), backend performance (`usage`
  tokens incl. `lpcache-cached-tokens`, prefill/generation ms and tps,
  `lpcache-ttft-ms`) and the proxy's own resource usage
  (`lpcache-cpu-ms`, disk I/O, resident/peak memory). The file
  size-rotates with a configurable retention count; logging failures are
  swallowed — logging never breaks request serving.
- **Payload log:** each chat request's exact raw body (plus a paired
  `.headers` file) is written as it arrives — *before* parsing,
  prefiltering, or dispatch — so malformed and rejected requests are
  captured too. Payloads may contain sensitive data and are never pruned
  by the proxy; point the directory at storage you manage.
- **Dashboard:** a self-contained statistics page (charts with percentile
  lines, slot grid, backend circuit-breaker state, counters) with a JSON
  `api/stats` endpoint, scoppable by time window (`?since=`) and backend
  (`?backend=`, plus the `cached` / `coalesced` / `coalesced_groups`
  scopes) — e.g. to prove restore hit rates and coalescing savings.

---

## 8. Development & testing without GPUs

**Scenario.** A developer wants to exercise the proxy, the tests, or a
load test without a model file, a GPU, or a real llama-server.

**Workflow.**

```sh
# 1. guided configuration (probes the backends, exports env/JSON):
cargo run -p lpcache-config -- configure          # interactive wizard
cargo run -p lpcache-config -- serve              # web UI

# 2. smoke test against the mock llama.cpp backend:
cargo run --example mock_llama -- 8100 mock-model
BACKENDS='[{"url":"http://127.0.0.1:8100","n_slots":2}]' \
  PORT=8091 META_DIR=/tmp/kv_meta LOG_LEVEL=DEBUG cargo run
# -> send a >500-word prompt, then the same prompt + a few words:
#    the second request logs restore_candidate ratio=1.000

# 3. load test (3 shared long prompts, status breakdown + p50/p90/p99):
cargo run --release --example load_test -- http://127.0.0.1:8091 16 30 1200

# 4. the full regression suite (no real backend needed — in-process mock):
cargo test --workspace
```

The same `mock_llama` example backs the 92 integration tests, so the
behaviors you smoke-test by hand are the behaviors the suite covers
(catalog in [test-cases/](../test-cases/README.md)).

---

## Summary map

| Use case | Key features | Main knobs |
|---|---|---|
| Long repetitive prompts (§1.1) | prefix hashing, LCP restore candidate scan, KV save/restore, meta files | `BIG_THRESHOLD_WORDS`, `LCP_TH`, `META_MAX`, `WORDS_PER_BLOCK` |
| Multi-turn sessions (§1.2) | ratio-1.0 extension, `key_affinity` routing, self-restore save skip | `SLOT_ROUTING_POLICY`, `SKIP_SAVE_ON_SELF_RESTORE` |
| Identical concurrent bursts (§2) | request coalescing (singleflight) | `COALESCE_REQUESTS` |
| Deterministic repeated queries (§3) | result cache prefilter + result store | `PLUGINS: result_cache` (`ttl_secs`, `cache_streaming`, `cache_control`, `cleanup`) |
| Multi-model endpoint (§4) | model-aware routing, per-model KV isolation | `BACKENDS[*].model_id` |
| HA pool & rolling upgrades (§5) | circuit breaker, 1 s probe loop, fail-open, drain/resume | `BACKENDS`, `/_control/backends/{i}/drain|resume` |
| Safety gate (§6) | prefilter chain: `keyword`, `qwen_guard` | `PLUGINS` (chain order), `on_error`, `reject_*` |
| Audit & observability (§7) | W3C-XLF request log, payload log, dashboard | `REQUEST_LOG*`, `REQUEST_PAYLOAD_DIR`, `PLUGINS: dashboard` |
| Dev/test without GPUs (§8) | `mock_llama`, `load_test`, configurator, mock-based test suite | — |
