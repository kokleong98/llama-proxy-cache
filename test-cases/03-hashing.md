# 3. Hashing, meta files, pruning — `src/hashing.rs` (19)

[← Suite index](README.md)

Part of the [lpcache regression test cases](README.md): every entry maps
1:1 to a test function; the full suite (197 tests) runs with
`cargo test --workspace`.

## Module overview

- **Target:** `lpcache` library — the `mod tests` at the bottom of [`src/hashing.rs`](../src/hashing.rs); 19 tests, all synchronous `#[test]`.
- **Fixtures & helpers:** `serde_json::json!` message vectors; `tempfile` dirs for the meta I/O tests; hand-written meta files with shuffled timestamps for the pruning tests.
- **Surface under test:** raw-prefix construction (`raw_prefix`), word tokenization (`words_from_text`), block hashing (`block_hashes_from_text`, `prefix_key_sha256` / SHA-256), LCP, meta file I/O (`write_meta`, `scan_all_meta`), restore-candidate selection (`find_best_restore_candidate`, `touch_meta`) and LRU pruning (`prune_meta`).

## How to run

```sh
cd lpcache
cargo test --lib hashing::tests                  # all 19 hashing/meta/pruning tests
cargo test --lib hashing::tests::meta_roundtrip  # one test, exact path
cargo test --lib meta_roundtrip                  # one test, name substring
```

The **Test case** column is the exact filter for that test: pass it
into `cargo test` as shown above.

| # | Test case (exact filter) | Kind | Source | What it verifies | Expected result |
|---|--------------------------|------|--------|------------------|-----------------|
| 1 | `hashing::tests::raw_prefix_joins_and_strips` | sync | [L306](../src/hashing.rs#L306) | message contents joined, trimmed, empties skipped | `"hello\n\nworld"` |
| 2 | `hashing::tests::raw_prefix_empty_and_missing_content` | sync | [L316](../src/hashing.rs#L316) | empty list / missing `content` / `null` content | `""` |
| 3 | `hashing::tests::raw_prefix_non_string_content` | sync | [L325](../src/hashing.rs#L325) | non-string content hashed from its JSON text | `42` → `"42"` |
| 4 | `hashing::tests::words_basic_lowercase_and_split` | sync | [L331](../src/hashing.rs#L331) | `\w+` tokenization on lowercased text | `["hello","world","hello"]` |
| 5 | `hashing::tests::words_underscore_and_digits` | sync | [L339](../src/hashing.rs#L339) | underscore/digits kept, `-` splits | `["a_b","c9","d","e"]` |
| 6 | `hashing::tests::words_no_word_chars` | sync | [L344](../src/hashing.rs#L344) | whitespace / punctuation only | empty word list |
| 7 | `hashing::tests::block_hashes_chunking` | sync | [L350](../src/hashing.rs#L350) | 5 words chunked at wpb=2 | 3 blocks = sha256 of `"a b"`, `"c d"`, `"e"` |
| 8 | `hashing::tests::block_hashes_empty_text` | sync | [L359](../src/hashing.rs#L359) | empty / punctuation-only text | no blocks |
| 9 | `hashing::tests::lcp_cases` | sync | [L365](../src/hashing.rs#L365) | longest common block prefix | 2 / 4 / 0 / 1 / 0 for the test pairs |
| 10 | `hashing::tests::sha256_known_vectors` | sync | [L376](../src/hashing.rs#L376) | SHA-256 correctness | known digests of `""` and `"abc"` |
| 11 | `hashing::tests::meta_roundtrip` | sync | [L387](../src/hashing.rs#L387) | `write_meta` → `scan_all_meta` | key, model, prefix_len, wpb, blocks, timestamp>0 all preserved |
| 12 | `hashing::tests::scan_skips_corrupt_and_wrong_extension` | sync | [L402](../src/hashing.rs#L402) | corrupt `.meta.json` + non-meta file | scan returns empty |
| 13 | `hashing::tests::scan_missing_dir_returns_empty` | sync | [L410](../src/hashing.rs#L410) | non-existent META_DIR | empty, no error |
| 14 | `hashing::tests::find_best_restore_candidate_filters_and_thresholds` | sync | [L416](../src/hashing.rs#L416) | model + wpb filters, full match, below threshold, threshold boundary, tie | `k_full` at ratio 1.0; none below 0.6; partial 0.5 qualifies at th 0.5 (tie → first scanned wins) |
| 15 | `hashing::tests::find_best_prefers_highest_ratio` | sync | [L471](../src/hashing.rs#L471) | two candidates both clear the threshold | higher ratio (1.0) beats 0.5 at th 0.4 |
| 16 | `hashing::tests::touch_meta_updates_timestamp` | sync | [L501](../src/hashing.rs#L501) | `touch_meta` on existing / missing key | timestamp bumped, other fields kept; missing file → no-op |
| 17 | `hashing::tests::prune_meta_removes_oldest_beyond_limit` | sync | [L526](../src/hashing.rs#L526) | 5 metas, max 3, shuffled timestamps | 2 oldest (by ts, not name) removed, oldest first; second prune is a no-op |
| 18 | `hashing::tests::prune_meta_noop_under_limit_and_zero_max` | sync | [L552](../src/hashing.rs#L552) | under limit / `max=0` / missing dir | nothing pruned, no error |
| 19 | `hashing::tests::prune_meta_corrupt_file_pruned_first` | sync | [L564](../src/hashing.rs#L564) | unparseable meta among healthy ones | treated as oldest, removed first |

## Coverage map — surface → tests

| Surface | Tests |
|---|---|
| Raw prefix construction | `raw_prefix_joins_and_strips`, `raw_prefix_empty_and_missing_content`, `raw_prefix_non_string_content` |
| Word tokenization | `words_basic_lowercase_and_split`, `words_underscore_and_digits`, `words_no_word_chars` |
| Block hashing, LCP + SHA-256 | `block_hashes_chunking`, `block_hashes_empty_text`, `lcp_cases`, `sha256_known_vectors` |
| Meta file I/O | `meta_roundtrip`, `scan_skips_corrupt_and_wrong_extension`, `scan_missing_dir_returns_empty` |
| Restore-candidate selection | `find_best_restore_candidate_filters_and_thresholds`, `find_best_prefers_highest_ratio`, `touch_meta_updates_timestamp` |
| LRU pruning | `prune_meta_removes_oldest_beyond_limit`, `prune_meta_noop_under_limit_and_zero_max`, `prune_meta_corrupt_file_pruned_first` |
