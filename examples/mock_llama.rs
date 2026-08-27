//! Run a mock llama.cpp backend for manual smoke testing:
//!
//! ```bash
//! cargo run --example mock_llama -- [port] [model-id] [save-dir]
//! #   save-dir (optional): successful saves create a KV file there,
//! #   emulating the backend's --slot-save-path
//! ```
//!
//! Then point the proxy at it:
//!
//! ```bash
//! BACKENDS='[{"url":"http://127.0.0.1:8100","n_slots":4}]' \
//!   PORT=8081 META_DIR=/tmp/kv_meta cargo run
//! ```

use lpcache::mock_backend::MockLlama;
use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(8100);
    let model = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "mock-model".to_string());
    let save_dir = std::env::args().nth(3);
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("bad addr");
    let mock = match save_dir {
        Some(dir) => MockLlama::start_on_with_save_dir(&model, addr, Some(dir.into())).await,
        None => MockLlama::start_on(&model, addr).await,
    };
    println!("mock llama.cpp listening at {} (model={model})", mock.url());
    let _ = tokio::signal::ctrl_c().await;
    println!("bye");
}
