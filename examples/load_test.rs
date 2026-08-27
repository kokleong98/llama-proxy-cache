//! Concurrent load test against the proxy (any OpenAI-compatible endpoint).
//!
//! ```bash
//! cargo run --release --example load_test \
//!   -- [base_url] [workers] [duration_secs] [big_words]
//! # defaults: http://127.0.0.1:8091 100 120 600
//! ```
//!
//! Each worker sticks to one of three shared long prompts (`worker_id % 3`),
//! so most requests exercise the restore hot path; every 5th request is a
//! small prompt (uncached by design). Prints status breakdown and latency
//! percentiles (client-side, including proxy queue wait) when done.

use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn prompt(words: usize, seed: u32) -> String {
    (0..words)
        .map(|i| format!("seed{seed}word{i}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn body(text: &str, max_tokens: u32) -> Value {
    json!({
        "model": "llama.cpp",
        "stream": false,
        "max_tokens": max_tokens,
        "messages": [{ "role": "user", "content": text }]
    })
}

async fn worker(
    client: reqwest::Client,
    url: String,
    my_big: Value,
    small: Value,
    deadline: Instant,
) -> Vec<(u16, u128)> {
    let mut out = Vec::new();
    let mut n = 0u32;
    while Instant::now() < deadline {
        let req_body = if n % 5 == 4 { &small } else { &my_big };
        let t0 = Instant::now();
        match client.post(&url).json(req_body).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let _ = resp.bytes().await; // drain so the slot is released
                out.push((status, t0.elapsed().as_millis()));
            }
            Err(_) => out.push((0, t0.elapsed().as_millis())),
        }
        n += 1;
    }
    out
}

fn pct(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let base_url = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "http://127.0.0.1:8091".to_string());
    let workers: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100);
    let duration: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(120);
    let big_words: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(600);
    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .expect("client");

    let bigs: Vec<Value> = (0..3).map(|s| body(&prompt(big_words, s), 32)).collect();
    let small = body("Say one word.", 16);

    println!(
        "load_test url={url} workers={workers} duration={}s big_words={big_words}",
        duration
    );

    let deadline = Instant::now() + Duration::from_secs(duration);
    let t0 = Instant::now();
    let handles: Vec<_> = (0..workers)
        .map(|i| {
            let client = client.clone();
            let url = url.clone();
            let big = bigs[i % 3].clone();
            let small = small.clone();
            tokio::spawn(worker(client, url, big, small, deadline))
        })
        .collect();
    let mut all: Vec<(u16, u128)> = Vec::new();
    for h in handles {
        if let Ok(v) = h.await {
            all.extend(v);
        }
    }
    let wall = t0.elapsed().as_secs_f64();

    // status breakdown
    let mut by_status: std::collections::BTreeMap<u16, usize> = std::collections::BTreeMap::new();
    for (s, _) in &all {
        *by_status.entry(*s).or_default() += 1;
    }
    println!("status breakdown:");
    for (s, c) in &by_status {
        let label = if *s == 0 {
            "ERR (network/timeout)".to_string()
        } else {
            s.to_string()
        };
        println!("  {label}: {c}");
    }

    // latency percentiles for 200s only
    let mut ok_lat: Vec<u128> = all
        .iter()
        .filter(|(s, _)| *s == 200)
        .map(|(_, ms)| *ms)
        .collect();
    ok_lat.sort_unstable();
    let n = ok_lat.len();
    if n > 0 {
        let avg: u128 = ok_lat.iter().sum::<u128>() / n as u128;
        println!(
            "latency (HTTP 200, ms): n={n} avg={avg} min={} p50={} p90={} p99={} max={}",
            ok_lat[0],
            pct(&ok_lat, 50.0),
            pct(&ok_lat, 90.0),
            pct(&ok_lat, 99.0),
            ok_lat[n - 1]
        );
    }
    println!(
        "throughput: {} requests in {:.1}s = {:.1} req/s (end-to-end incl. queue)",
        all.len(),
        wall,
        all.len() as f64 / wall
    );
}
