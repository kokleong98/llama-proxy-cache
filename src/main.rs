//! Entry point (port of `proxycache.py` — uvicorn replaced by axum::serve).

use lpcache::app::{AppState, router};
use lpcache::coalesce::SingleFlight;
use lpcache::config::{Cli, Config};
use lpcache::llama_client::{LlamaBackend, LlamaClient};
use lpcache::slot_manager::SlotManager;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let cli = match Cli::from_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("lpcache: {e}\n");
            eprintln!("{}", Cli::usage());
            std::process::exit(2);
        }
    };
    if cli.version {
        println!("{}", lpcache::config::version_string());
        return;
    }
    if cli.help {
        println!("{}", Cli::usage());
        return;
    }

    // Command line > environment > built-in defaults.
    let config = Config::from_cli(&cli);

    // RUST_LOG takes precedence, then LOG_LEVEL (uvicorn used LOG_LEVEL too)
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.log_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();

    if let Err(e) = std::fs::create_dir_all(&config.meta_dir) {
        tracing::warn!(
            "could not create meta dir {}: {e}",
            config.meta_dir.display()
        );
    }

    let port = config.port;
    let mut clients: Vec<Arc<dyn LlamaBackend>> = Vec::new();
    for be in &config.backends {
        let client = LlamaClient::new(&be.url, config.request_timeout, config.api_key.as_deref());
        match client {
            Ok(c) => clients.push(Arc::new(c)),
            Err(e) => tracing::error!("backend client init failed url={}: {e}", be.url),
        }
    }

    let sm = Arc::new(SlotManager::new(&config.backends, clients.clone()));
    let state = AppState {
        config: Arc::new(config.clone()),
        clients,
        sm,
        sf: Arc::new(SingleFlight::new()),
    };

    // Warn early if a configured --slot-save-path dir is missing: pruning
    // would then remove meta files but not the (invisible) KV files.
    for dir in config.slot_save_dirs() {
        if !dir.is_dir() {
            tracing::warn!(
                "slot_save_path {} does not exist; KV files will not be pruned there",
                dir.display()
            );
        }
    }

    tracing::info!(
        "app_start version={} n_backends={} port={} meta_max={}",
        lpcache::config::VERSION,
        state.clients.len(),
        port,
        config.meta_max
    );

    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("bind 0.0.0.0:{port} failed: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!("listening on 0.0.0.0:{port}");

    // Background recovery probe (deviation from Python): once per second,
    // backends still in failure cooldown are checked via GET /v1/models;
    // a live answer clears the cooldown so they re-join the slot pool
    // immediately instead of waiting out the (escalated) cooldown.
    {
        let sm = Arc::clone(&state.sm);
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                iv.tick().await;
                let _ = sm.probe_down_backends().await;
            }
        });
    }

    let app = router(state);
    let _ = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutdown signal received");
        })
        .await;
    tracing::info!("app stopped");
}
