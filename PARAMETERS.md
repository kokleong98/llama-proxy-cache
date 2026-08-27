# proxycache — Parameters Help Summary

All tunable parameters for the proxycache stack, in two layers:

1. **`run-proxycache.sh` launcher** (env vars only)
2. **Llama Proxy Cache** `lpcache/target/release/lpcache` (CLI flags **or** env vars — CLI > env > built-in default)

Plus the matching **llama-server** flags the proxy depends on.

The proxy also carries this summary as **embedded help**:

```sh
lpcache --help                  # Rust binary
```

---

## 1. `run-proxycache.sh` launcher

| Variable | Default | Meaning |
|---|---|---|
| `PROXY_PORT` | `8090` | Proxy listen port (exported to the proxy as `PORT`) |
| `LLAMA_URL` | `http://127.0.0.1:8081` | llama-server base URL |
| `LLAMA_API_KEY` | `777` | llama-server `--api-key`; **empty string = no auth** |
| `N_SLOTS` | `1` | **Must match** llama-server `-np`/`--parallel` |
| `MODEL_ID` | `qwen3.6` | Model id advertised by `GET /v1/models` |
| `META_DIR` | `<script dir>/kv_meta` | Directory for `<key>.meta.json` files |
| `META_MAX` | `2` | LRU bound on meta files (hardcoded in the script, not overridable) |
| `SLOT_SAVE_PATH` | `/data/llama.cpp/slots/8081` | Must match llama-server `--slot-save-path`; auto-unset (and KV-file pruning skipped) if the dir is missing/unwritable |
| `IMPL` | `auto` | `auto` \| `rust` — both launch the Llama Proxy Cache (kept for compatibility; `auto` = prebuilt binary if present, else build Rust) |

Examples:

```sh
./run-proxycache.sh
N_SLOTS=4 ./run-proxycache.sh
PROXY_PORT=9000 LLAMA_URL=http://10.0.0.5:8081 LLAMA_API_KEY= ./run-proxycache.sh
```

The script exports `PORT LLAMA_URL N_SLOTS MODEL_ID META_DIR META_MAX` and
(optionally) `LLAMA_API_KEY` / `SLOT_SAVE_PATH`.

---

## 2. Llama Proxy Cache (`lpcache`)

Every parameter is settable as an **env var** or a **command-line flag**.
Explicit flags take precedence over environment variables, which take
precedence over the built-in defaults. `-h` / `--help` prints the built-in help.

| Env var | CLI flag | Default | Meaning |
|---|---|---|---|
| `BACKENDS` | `--backends <JSON>` | — | JSON array `[{"url": "...", "n_slots": N, "slot_save_path": "..."}]` for multiple backends; `slot_save_path` is optional per backend. A parse failure logs a warning and yields **no** backends. |
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
| `COALESCE_REQUESTS` | `--coalesce-requests <BOOL>` | `false` | Group concurrent requests with the same KV cache key into one backend call (followers receive the leader's result, regardless of generation parameters) |
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

---

## 3. Matching llama-server flags

The proxy only works correctly when the backend is started with the
corresponding flags:

| llama-server flag | Must match proxy | Why |
|---|---|---|
| `--slot-save-path <dir>` | `SLOT_SAVE_PATH` (or per-backend `slot_save_path` in `BACKENDS`) | The proxy restores/saves slot KV files and prunes old ones from this dir |
| `-np` / `--parallel N` | `N_SLOTS` (or per-backend `n_slots` in `BACKENDS`) | Number of slots the proxy will pin to |
| `--api-key <key>` | `LLAMA_API_KEY` | Backend rejects unauthenticated requests otherwise |
| `--port` / `--host` | `LLAMA_URL` | Where the proxy sends chats/saves/restores |

Example:

```sh
llama-server -m ./model.gguf -np 4 --api-key 777 \
  --slot-save-path "$PWD/kvcache" --host 0.0.0.0 --port 8081
```

---

## 4. Precedence & gotchas

- **Rust:** CLI flag > env var > built-in default.
- **Launcher:** your env > script defaults; the script never sets variables
  you already exported.
- `META_DIR` and relative `SLOT_SAVE_PATH` are resolved against the **current
  working directory** (the launcher `cd`s to its own dir first).
- `META_MAX=0` disables pruning; a later restore of a pruned key simply fails
  and falls back to a full prefill.
- With `BACKENDS` set, `LLAMA_URL`, `N_SLOTS`, and `SLOT_SAVE_PATH` env vars
  are **ignored** — per-backend values in the JSON array win.
- Blank `LLAMA_API_KEY` means "no auth" (the launcher explicitly unsets it).
- `RUST_LOG` (Rust only) overrides `LOG_LEVEL` when set.
- Slot acquire timeout (300 s) and circuit-breaker cooldowns (5 s → 60 s,
  probe recovery) are built-in behaviour, not configurable.

