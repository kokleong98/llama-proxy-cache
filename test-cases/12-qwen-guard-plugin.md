# 12. Qwen guard prefilter — `plugins/src/builtin/qwen_guard.rs` (16)

[← Suite index](README.md)

Part of the [lpcache regression test cases](README.md): every entry maps
1:1 to a test function; the full suite runs with
`cargo test --workspace`.

The `qwen_guard` prefilter classifies a request's combined message text
with a **Qwen3Guard-Gen** model served by a separate llama-server (or any
OpenAI-compatible `/v1/chat/completions` endpoint) and rejects requests
whose verdict hits the configured safety levels / harm categories before
any slot/backend work.

## Module overview

- **Target:** `lpcache-plugins` library — `builtin::qwen_guard::tests` (16) in [`qwen_guard.rs`](../plugins/src/builtin/qwen_guard.rs); 8 async (`#[tokio::test]` — `check()` is an async trait method and the factory tests drive real HTTP against the mock) + 8 sync (parser, decision policy, cache, factory validation).
- **Fixtures & helpers:**
  - `preq(messages, n_words)` builds a `PrefilterRequest` with a fixed model/backend-model/key.
  - `policy(reject_levels, reject_categories)` builds a `GuardPolicy` with fail-open defaults.
  - `mock_guard_server(resp)` — a one-shot std-thread TCP server on an ephemeral `127.0.0.1` port answering the first request with a canned OpenAI chat-completions body, or dropping the connection when `resp` is `None` (client transport error).
  - `guard_response(safety, categories)` renders the `Safety: …\nCategories: …` model output into an OpenAI-style JSON body.
  - `pf_from_url(url, policy)` builds a `QwenGuardPrefilter` with a 10 s timeout and no cache.
- **Surface under test:** `SafetyLevel` / `GuardCategory` vocab (canonical names, case-insensitive parsing, aliases), `parse_guard_output` (model-card formats, `None`, unknown names, duplicates, whitespace/case tolerance), `decide` (level rule first, category rule independent of level, accept path), `VerdictCache` bounded-LRU eviction, `QwenGuardPrefilter::check` end-to-end (reject/accept, fail-open/fail-closed, empty-text skip, per-key cache reuse), and the `qwen_guard` `PluginFactory` (param wiring, defaults, `BadParams` validation).

## How to run

```sh
cd lpcache
cargo test -p lpcache-plugins qwen_guard::tests   # all 16 qwen guard tests
cargo test -p lpcache-plugins builtin::qwen_guard::tests::check_rejects_unsafe_verdict   # one, exact path
```

The **Test case** column is the exact filter for that test: pass it
into `cargo test` as shown above.

Output parser (3 tests):

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 1 | `builtin::qwen_guard::tests::parses_model_card_outputs` | sync | [L693](../plugins/src/builtin/qwen_guard.rs#L693) | the exact Qwen3Guard-Gen model-card formats: prompt output, response output with the extra `Refusal:` line, multi-category list | level + categories parsed; unknown names ignored, duplicates dropped, `None` dropped |
| 2 | `builtin::qwen_guard::tests::parse_is_case_insensitive_and_tolerates_whitespace` | sync | [L716](../plugins/src/builtin/qwen_guard.rs#L716) | `safety:   UNsafe` / `categories:` with extra whitespace | still parses to `Unsafe` + `NonViolentIllegalActs` |
| 3 | `builtin::qwen_guard::tests::parse_category_aliases` | sync | [L724](../plugins/src/builtin/qwen_guard.rs#L724) | name aliases (`Personally Identifiable Information` → `PII`, `suicide and self-harm`) and unknown inputs | aliases map; unknown level/category → `None`; no guard output → empty verdict |

Decision policy (3 tests):

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 4 | `builtin::qwen_guard::tests::decide_level_rule_first` | sync | [L740](../plugins/src/builtin/qwen_guard.rs#L740) | verdict level in `reject_levels` | `Reject { 400 }` with `Safety=Unsafe` and the categories in the message; rejects even with an empty category list |
| 5 | `builtin::qwen_guard::tests::decide_category_rule_independent_of_level` | sync | [L770](../plugins/src/builtin/qwen_guard.rs#L770) | category in `reject_categories` while the level is allowed (and with no level at all) | Controversial + PII → `Reject { 400 }` naming PII; clean → `Accept`; level-less + rejected category → `Reject` |
| 6 | `builtin::qwen_guard::tests::decide_accepts_safe` | sync | [L808](../plugins/src/builtin/qwen_guard.rs#L808) | `Safe` verdict with every category configured as rejected | `Accept` (the level rule never trips for `Safe`) |

Verdict cache (1 test):

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 7 | `builtin::qwen_guard::tests::verdict_cache_is_bounded_lru` | sync | [L824](../plugins/src/builtin/qwen_guard.rs#L824) | bounded LRU put/get at capacity 2 | the least-recently-used entry is evicted; recently-accessed entries survive |

HTTP round-trip against a one-shot mock guard server (6 tests):

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 8 | `builtin::qwen_guard::tests::check_rejects_unsafe_verdict` | async | [L925](../plugins/src/builtin/qwen_guard.rs#L925) | end-to-end: guard answers `Unsafe` / `Violent` | `Reject { 400 }` with level + category in the message |
| 9 | `builtin::qwen_guard::tests::check_accepts_safe_verdict` | async | [L940](../plugins/src/builtin/qwen_guard.rs#L940) | guard answers `Safe` / `None` | `Accept` |
| 10 | `builtin::qwen_guard::tests::check_fail_open_on_error` | async | [L948](../plugins/src/builtin/qwen_guard.rs#L948) | guard drops the connection (transport error), default `on_error` | `Accept` (fail open) |
| 11 | `builtin::qwen_guard::tests::check_fail_closed_on_error_and_unparseable` | async | [L956](../plugins/src/builtin/qwen_guard.rs#L956) | `on_error: reject` on transport failure; `on_unparseable: reject` on garbage guard output (`Safety: Who knows`) | `Reject { 503 }` for the error; `Reject { 400 }` for the unparseable output |
| 12 | `builtin::qwen_guard::tests::check_skips_guard_for_empty_text` | async | [L982](../plugins/src/builtin/qwen_guard.rs#L982) | request with no message text, pointed at an unroutable URL | `Accept` without any HTTP call |
| 13 | `builtin::qwen_guard::tests::check_reuses_cached_verdict_per_key` | async | [L993](../plugins/src/builtin/qwen_guard.rs#L993) | mock answers exactly one connection; second `check()` for the same request key | first call `Reject`; second call served from the LRU (a second HTTP attempt would fail the test) |

Factory / config validation (3 tests):

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 14 | `builtin::qwen_guard::tests::factory_creates_prefilter_from_json_params` | async | [L1026](../plugins/src/builtin/qwen_guard.rs#L1026) | full param wiring: `reject_levels: ["unsafe","controversial"]`, comma-string `reject_categories: "jailbreak, pii"`, `on_error: "reject"` | Controversial verdict → `Reject { 400 }` (level rule); Safe + PII → `Reject { 400 }` (category rule); unreachable guard → `Reject { 503 }` |
| 15 | `builtin::qwen_guard::tests::factory_defaults` | async | [L1091](../plugins/src/builtin/qwen_guard.rs#L1091) | `url`-only spec (all defaults) | Controversial → `Accept` (default `reject_levels: ["Unsafe"]`), Unsafe → `Reject { 400 }`, transport error → `Accept` (fail open) |
| 16 | `builtin::qwen_guard::tests::factory_rejects_bad_params` | sync | [L1130](../plugins/src/builtin/qwen_guard.rs#L1130) | missing `url`, unknown level (`"maybe"`), unknown category (`"gore"`), unknown fail action (`"ban"`); `create_backend` | `BadParams` with a descriptive message in each case; factory provides no backend client |

## Coverage notes

- **No real model needed.** The mock guard server is a one-shot
  std-thread TCP listener that speaks enough HTTP/1.1 for `reqwest`
  (fixed `Content-Length` body, `connection: close`) — no `llama-server`,
  model files, or network access.
- **Fail-open is the default** (`on_error` / `on_unparseable` =
  `"accept"`): a down or confused guard degrades to normal proxying; the
  fail-closed variants return `503` (transport) and `400` (unparseable).
- **Policy is two orthogonal rules:** `reject_levels` is checked first,
  then `reject_categories` against the verdict's category list;
  rejections are `400` with the offending level/categories in the
  message and never reach the chat backend (no slot, no KV cache, no
  coalescing).
- **Verdict caching** (`cache_max > 0`) is a bounded LRU keyed by the
  request's KV cache key, so repeated same-key prompts skip the guard
  call; off by default (`cache_max: 0`).
