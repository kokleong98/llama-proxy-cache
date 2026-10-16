//! Hashing + meta files (port of `hashing.py`).
//!
//! - `raw_prefix`: message contents (no roles) joined by a double newline.
//! - Words: `\w+` on the lowercased text (letters, digits, underscore;
//!   Unicode aware, like Python's `\w` on `str`).
//! - Blocks: chunks of `wpb` words, each SHA256-hashed.
//! - Cache key: `sha256(model_id + "\n" + raw_prefix)`.
//! - Meta files: `{key}.meta.json` in META_DIR containing
//!   `key`, `model_id`, `prefix_len`, `wpb`, `blocks`, `timestamp`.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// A parsed `.meta.json` file describing a cached KV prefix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub key: String,
    pub model_id: String,
    pub prefix_len: usize,
    pub wpb: usize,
    pub blocks: Vec<String>,
    pub timestamp: f64,
}

/// Join message contents (no roles) with `"\n\n"`, skipping empty ones
/// (port of `raw_prefix`).
pub fn raw_prefix(messages: &[serde_json::Value]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for msg in messages {
        let content = match msg.get("content") {
            None | Some(serde_json::Value::Null) => continue,
            Some(serde_json::Value::String(s)) => s.clone(),
            // Non-string content: use its JSON text representation
            // (Python used `str(content)`; null is treated as empty).
            Some(other) => other.to_string(),
        };
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    let text = parts.join("\n\n").trim().to_string();
    tracing::debug!("raw_prefix len_chars={}", text.chars().count());
    text
}

/// Word tokens: `\w+` on the lowercased text
/// (Unicode letters/digits + underscore — same spirit as Python's `\w+`).
pub fn words_from_text(text: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    for c in text.to_lowercase().chars() {
        if c.is_alphanumeric() || c == '_' {
            cur.push(c);
        } else if !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// SHA256 hex of each `wpb`-word block of the text (port of
/// `block_hashes_from_text`).
pub fn block_hashes_from_text(text: &str, wpb: usize) -> Vec<String> {
    let words = words_from_text(text);
    let mut hashes: Vec<String> = Vec::new();
    if wpb > 0 {
        for chunk in words.chunks(wpb) {
            let block = chunk.join(" ");
            hashes.push(prefix_key_sha256(&block));
        }
    }
    tracing::debug!("block_hashes n_blocks={} wpb={wpb}", hashes.len());
    hashes
}

/// Length of the common block prefix of two block-hash lists
/// (port of `lcp_blocks`).
pub fn lcp_blocks(a: &[String], b: &[String]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// SHA256 hex wrapper; the cache key is computed over
/// `model_id + "\n" + raw_prefix` (port of `prefix_key_sha256`).
pub fn prefix_key_sha256(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
/// Scan META_DIR for `*.meta.json` files, most recently modified first
/// (port of `scan_all_meta`). Corrupt files are skipped with a warning.
pub fn scan_all_meta(meta_dir: &Path) -> Vec<Meta> {
    let mut entries: Vec<(std::time::SystemTime, String, Meta)> = Vec::new();
    let rd = match std::fs::read_dir(meta_dir) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::debug!("scan_meta read_dir failed: {e}");
            return Vec::new();
        }
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if !name.ends_with(".meta.json") {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(txt) => match serde_json::from_str::<Meta>(&txt) {
                Ok(meta) => {
                    let mtime = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    entries.push((mtime, name, meta));
                }
                Err(e) => tracing::warn!("scan_meta_fail {}: {e}", path.display()),
            },
            Err(e) => tracing::warn!("scan_meta_fail {}: {e}", path.display()),
        }
    }
    // mtime desc; ties -> filename asc (mirrors Python's sorted glob +
    // stable sort by mtime, reverse=True)
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let metas: Vec<Meta> = entries.into_iter().map(|(_, _, m)| m).collect();
    tracing::debug!("scan_meta n_found={}", metas.len());
    metas
}

/// Find the best restore candidate among meta files of `model_id` with the
/// same `wpb`; requires LCP ratio `>= th` (port of
/// `find_best_restore_candidate`). Returns `(key, ratio)`.
pub fn find_best_restore_candidate(
    meta_dir: &Path,
    req_blocks: &[String],
    wpb: usize,
    th: f64,
    model_id: &str,
) -> Option<(String, f64)> {
    let mut best_key: Option<String> = None;
    let mut best_ratio = 0.0f64;
    for meta in scan_all_meta(meta_dir) {
        if meta.model_id != model_id {
            continue;
        }
        if meta.wpb != wpb {
            continue;
        }
        let lcp = lcp_blocks(req_blocks, &meta.blocks);
        let denom = 1usize.max(req_blocks.len().min(meta.blocks.len()));
        let ratio = lcp as f64 / denom as f64;
        if ratio >= th && ratio > best_ratio {
            best_ratio = ratio;
            best_key = Some(meta.key.clone());
        }
    }
    best_key.map(|k| (k, best_ratio))
}

/// Write/overwrite the meta file for `key` (port of `write_meta`).
pub fn write_meta(
    meta_dir: &Path,
    key: &str,
    prefix_text: &str,
    blocks: &[String],
    wpb: usize,
    model_id: &str,
) -> std::io::Result<()> {
    let meta = Meta {
        key: key.to_string(),
        model_id: model_id.to_string(),
        prefix_len: prefix_text.chars().count(),
        wpb,
        blocks: blocks.to_vec(),
        timestamp: now_secs(),
    };
    std::fs::create_dir_all(meta_dir)?;
    let path = meta_dir.join(format!("{key}.meta.json"));
    let txt = serde_json::to_string_pretty(&meta).map_err(std::io::Error::other)?;
    std::fs::write(&path, txt)?;
    Ok(())
}

/// Update the timestamp of an existing meta file (port of `touch_meta`).
/// Missing/unreadable files only produce warnings.
pub fn touch_meta(meta_dir: &Path, key: &str) {
    let path = meta_dir.join(format!("{key}.meta.json"));
    let Ok(txt) = std::fs::read_to_string(&path) else {
        tracing::warn!("touch_meta_missing key={}", short(key));
        return;
    };
    let Ok(mut meta) = serde_json::from_str::<Meta>(&txt) else {
        tracing::warn!("touch_meta_read_fail key={}", short(key));
        return;
    };
    meta.timestamp = now_secs();
    let Ok(out) = serde_json::to_string_pretty(&meta) else {
        tracing::warn!("touch_meta_serialize_fail key={}", short(key));
        return;
    };
    if let Err(e) = std::fs::write(&path, out) {
        tracing::warn!("touch_meta_fail key={}: {e}", short(key));
    } else {
        tracing::debug!("touch_meta_ok key={}", short(key));
    }
}

/// Bounded LRU pruning (deviation from Python, which never prunes).
///
/// Removes the oldest `.meta.json` files until `META_DIR` holds at most
/// `max` files. "Oldest" is the smallest `timestamp` (the last successful
/// save/restore time), with ties broken by file mtime and then filename.
/// Unreadable/unparseable files are treated as oldest — they are useless
/// to candidate lookup anyway. `max == 0` disables pruning.
///
/// Returns the keys of the removed meta files, oldest first.
pub fn prune_meta(meta_dir: &Path, max: usize) -> Vec<String> {
    if max == 0 {
        return Vec::new();
    }
    let mut entries: Vec<(f64, std::time::SystemTime, String)> = Vec::new();
    let rd = match std::fs::read_dir(meta_dir) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::debug!("prune_meta read_dir failed: {e}");
            return Vec::new();
        }
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if !name.ends_with(".meta.json") {
            continue;
        }
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let ts = match std::fs::read_to_string(&path) {
            Ok(txt) => serde_json::from_str::<Meta>(&txt)
                .map(|m| m.timestamp)
                .unwrap_or(0.0),
            Err(e) => {
                tracing::debug!("prune_meta unreadable {}: {e}", path.display());
                0.0
            }
        };
        entries.push((ts, mtime, name));
    }
    let excess = entries.len().saturating_sub(max);
    if excess == 0 {
        return Vec::new();
    }
    // oldest first (timestamp asc, then mtime asc, then name asc)
    entries.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    let mut removed = Vec::with_capacity(excess);
    for (_, _, name) in entries.iter().take(excess) {
        let key = name.trim_end_matches(".meta.json").to_string();
        let path = meta_dir.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                tracing::debug!("prune_meta_removed key={}", short(&key));
                removed.push(key);
            }
            Err(e) => tracing::warn!("prune_meta_remove_fail {}: {e}", path.display()),
        }
    }
    tracing::info!("prune_meta removed={} max={max}", removed.len());
    removed
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn short(s: &str) -> String {
    s.chars().take(16).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn raw_prefix_joins_and_strips() {
        let msgs = vec![
            json!({"role": "user", "content": "  hello  "}),
            json!({"role": "assistant", "content": "world"}),
            json!({"role": "user", "content": "   "}),
        ];
        assert_eq!(raw_prefix(&msgs), "hello\n\nworld");
    }

    #[test]
    fn raw_prefix_empty_and_missing_content() {
        assert_eq!(raw_prefix(&[]), "");
        let msgs = vec![json!({"role": "user"})];
        assert_eq!(raw_prefix(&msgs), "");
        let msgs = vec![json!({"role": "user", "content": null})];
        assert_eq!(raw_prefix(&msgs), "");
    }

    #[test]
    fn raw_prefix_non_string_content() {
        let msgs = vec![json!({"role": "user", "content": 42})];
        assert_eq!(raw_prefix(&msgs), "42");
    }

    #[test]
    fn words_basic_lowercase_and_split() {
        assert_eq!(
            words_from_text("Hello, World! hello"),
            vec!["hello", "world", "hello"]
        );
    }

    #[test]
    fn words_underscore_and_digits() {
        assert_eq!(words_from_text("a_b c9 d-e"), vec!["a_b", "c9", "d", "e"]);
    }

    #[test]
    fn words_no_word_chars() {
        assert!(words_from_text("   \n\n  ").is_empty());
        assert!(words_from_text("!!! ???").is_empty());
    }

    #[test]
    fn block_hashes_chunking() {
        let blocks = block_hashes_from_text("a b c d e", 2);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0], prefix_key_sha256("a b"));
        assert_eq!(blocks[1], prefix_key_sha256("c d"));
        assert_eq!(blocks[2], prefix_key_sha256("e"));
    }

    #[test]
    fn block_hashes_empty_text() {
        assert!(block_hashes_from_text("", 100).is_empty());
        assert!(block_hashes_from_text("!!!", 100).is_empty());
    }

    #[test]
    fn lcp_cases() {
        let a = vec!["1".into(), "2".into(), "3".into(), "4".into()];
        let b = vec!["1".into(), "2".into(), "x".into(), "y".into()];
        assert_eq!(lcp_blocks(&a, &b), 2);
        assert_eq!(lcp_blocks(&a, &a), 4);
        assert_eq!(lcp_blocks(&a, &[]), 0);
        assert_eq!(lcp_blocks(&["1".into()], &["1".into(), "2".into()]), 1);
        assert_eq!(lcp_blocks(&[], &[]), 0);
    }

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            prefix_key_sha256(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            prefix_key_sha256("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
    #[test]
    fn meta_roundtrip() {
        let td = tempfile::tempdir().unwrap();
        let blocks = vec!["aa".into(), "bb".into()];
        write_meta(td.path(), "key1", "prefix text", &blocks, 100, "model-x").unwrap();
        let metas = scan_all_meta(td.path());
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].key, "key1");
        assert_eq!(metas[0].model_id, "model-x");
        assert_eq!(metas[0].prefix_len, "prefix text".chars().count());
        assert_eq!(metas[0].wpb, 100);
        assert_eq!(metas[0].blocks, blocks);
        assert!(metas[0].timestamp > 0.0);
    }

    #[test]
    fn scan_skips_corrupt_and_wrong_extension() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("bad.meta.json"), "{not json").unwrap();
        std::fs::write(td.path().join("notes.txt"), "{}").unwrap();
        assert!(scan_all_meta(td.path()).is_empty());
    }

    #[test]
    fn scan_missing_dir_returns_empty() {
        let metas = scan_all_meta(std::path::Path::new("/nonexistent-dir-xyz"));
        assert!(metas.is_empty());
    }

    #[test]
    fn find_best_restore_candidate_filters_and_thresholds() {
        let td = tempfile::tempdir().unwrap();
        // wrong model -> ignored
        write_meta(
            td.path(),
            "k_other_model",
            "a",
            &["aa".into()],
            100,
            "other-model",
        )
        .unwrap();
        // wrong wpb -> ignored
        write_meta(td.path(), "k_other_wpb", "a", &["aa".into()], 50, "m1").unwrap();
        // full match
        write_meta(
            td.path(),
            "k_full",
            "a",
            &["aa".into(), "bb".into()],
            100,
            "m1",
        )
        .unwrap();
        // partial overlap (ratio 0.5 with req3)
        write_meta(
            td.path(),
            "k_partial",
            "a",
            &["aa".into(), "cc".into()],
            100,
            "m1",
        )
        .unwrap();

        let req = vec!["aa".into(), "bb".into()];
        let (key, ratio) = find_best_restore_candidate(td.path(), &req, 100, 0.6, "m1").unwrap();
        assert_eq!(key, "k_full");
        assert!((ratio - 1.0).abs() < f64::EPSILON);

        // nothing matches above threshold
        let req_none = vec!["zz".into(), "yy".into()];
        assert!(find_best_restore_candidate(td.path(), &req_none, 100, 0.6, "m1").is_none());

        // partial: ratio = 1/2 = 0.5 < 0.6 -> none
        let req3 = vec!["aa".into(), "yy".into()];
        assert!(find_best_restore_candidate(td.path(), &req3, 100, 0.6, "m1").is_none());

        // with threshold 0.5 the partial candidate qualifies
        let (key, ratio) = find_best_restore_candidate(td.path(), &req3, 100, 0.5, "m1").unwrap();
        assert_eq!(key, "k_partial");
        assert!((ratio - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn find_best_prefers_highest_ratio() {
        let td = tempfile::tempdir().unwrap();
        // two candidates that both clear the threshold: the higher LCP
        // ratio must win
        write_meta(
            td.path(),
            "k_a",
            "a",
            &["aa".into(), "zz".into()],
            100,
            "m1",
        )
        .unwrap();
        write_meta(
            td.path(),
            "k_b",
            "a",
            &["aa".into(), "bb".into()],
            100,
            "m1",
        )
        .unwrap();
        let req = vec!["aa".into(), "bb".into()];
        // k_a ratio = 1/2 = 0.5, k_b ratio = 2/2 = 1.0; both qualify at th 0.4
        let (key, ratio) = find_best_restore_candidate(td.path(), &req, 100, 0.4, "m1").unwrap();
        assert_eq!(key, "k_b");
        assert!((ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn touch_meta_updates_timestamp() {
        let td = tempfile::tempdir().unwrap();
        write_meta(td.path(), "k", "p", &["b".into()], 100, "m").unwrap();
        let before = scan_all_meta(td.path())[0].timestamp;
        std::thread::sleep(Duration::from_millis(5));
        touch_meta(td.path(), "k");
        let after = scan_all_meta(td.path())[0].timestamp;
        assert!(after >= before);
        // other fields preserved
        assert_eq!(scan_all_meta(td.path())[0].key, "k");

        // missing file -> warning only, no file created
        touch_meta(td.path(), "nope");
        assert!(!td.path().join("nope.meta.json").exists());
    }

    fn set_meta_ts(dir: &Path, key: &str, ts: f64) {
        let path = dir.join(format!("{key}.meta.json"));
        let txt = std::fs::read_to_string(&path).unwrap();
        let mut m: Meta = serde_json::from_str(&txt).unwrap();
        m.timestamp = ts;
        std::fs::write(&path, serde_json::to_string_pretty(&m).unwrap()).unwrap();
    }

    #[test]
    fn prune_meta_removes_oldest_beyond_limit() {
        let td = tempfile::tempdir().unwrap();
        for k in ["ka", "kb", "kc", "kd", "ke"] {
            write_meta(td.path(), k, "p", &[], 100, "m").unwrap();
        }
        // distinct timestamps, deliberately NOT in filename order
        set_meta_ts(td.path(), "ka", 50.0);
        set_meta_ts(td.path(), "kb", 10.0);
        set_meta_ts(td.path(), "kc", 90.0);
        set_meta_ts(td.path(), "kd", 30.0);
        set_meta_ts(td.path(), "ke", 70.0);
        let pruned = prune_meta(td.path(), 3);
        // two oldest removed, oldest first
        assert_eq!(pruned, vec!["kb", "kd"]);
        let remaining: Vec<String> = scan_all_meta(td.path())
            .iter()
            .map(|m| m.key.clone())
            .collect();
        assert_eq!(remaining.len(), 3);
        assert!(!remaining.contains(&"kb".to_string()));
        assert!(!remaining.contains(&"kd".to_string()));
        // idempotent: nothing left to prune
        assert!(prune_meta(td.path(), 3).is_empty());
    }

    #[test]
    fn prune_meta_noop_under_limit_and_zero_max() {
        let td = tempfile::tempdir().unwrap();
        write_meta(td.path(), "k1", "p", &[], 100, "m").unwrap();
        write_meta(td.path(), "k2", "p", &[], 100, "m").unwrap();
        assert!(prune_meta(td.path(), 5).is_empty());
        assert!(prune_meta(td.path(), 0).is_empty());
        assert_eq!(scan_all_meta(td.path()).len(), 2);
        // missing dir -> no error, nothing pruned
        assert!(prune_meta(&td.path().join("nope"), 1).is_empty());
    }

    #[test]
    fn prune_meta_corrupt_file_pruned_first() {
        let td = tempfile::tempdir().unwrap();
        write_meta(td.path(), "good", "p", &[], 100, "m").unwrap();
        set_meta_ts(td.path(), "good", 100.0);
        std::fs::write(td.path().join("bad.meta.json"), "not json").unwrap();
        let pruned = prune_meta(td.path(), 1);
        // unparseable file is treated as oldest
        assert_eq!(pruned, vec!["bad"]);
        assert!(td.path().join("good.meta.json").exists());
    }
}
