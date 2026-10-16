//! Entry point (port of `proxycache.py` — uvicorn replaced by axum::serve).
//!
//! All adapter functionality (prefilter / result store / backend client)
//! lives in the `lpcache-plugins` crate. This binary only configures and
//! dynamically loads adapter instances from configuration at startup:
//! [`Config::plugin_specs`] (the `PLUGINS` env var / `--plugins` flag, or
//! the specs synthesized from the legacy `PREFILTER_*` settings) is fed to
//! the plugin registry, and the resulting `Arc<dyn Prefilter>` /
//! `Arc<dyn ResultStore>` / `Arc<dyn LlamaBackend>` objects are wired into
//! `AppState`.

use lpcache::app::{AppState, router};
use lpcache::coalesce::SingleFlight;
use lpcache::config::{Cli, Config};
use lpcache::plugins::{
    LlamaBackend, PluginSpec, Prefilter, ResultStore, ensure_builtin, global, load_prefilters,
};
use lpcache::slot_manager::SlotManager;
use serde_json::json;
use std::sync::Arc;

/// Plugin name of the built-in llama.cpp backend client (instantiated once
/// per `BACKENDS` entry).
const BACKEND_PLUGIN: &str = "llama";

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

    // Dynamically load the adapter plugins from configuration: the `PLUGINS`
    // env var / `--plugins` flag, or — when unset — the specs synthesized
    // from the legacy `PREFILTER_*` settings. A bad spec is a startup error
    // (it names the offending entry — fix it instead of working around it).
    let specs = match config.plugin_specs() {
        Ok(specs) => specs,
        Err(e) => {
            eprintln!("lpcache: {e}");
            std::process::exit(2);
        }
    };
    ensure_builtin();
    let registry = global();
    // Prefilter plugins (built-ins: keyword / chain / result_cache),
    // chained in spec order: short-circuit requests before any slot/backend
    // work.
    let prefilter: Option<Arc<dyn Prefilter>> = match load_prefilters(registry, &specs) {
        Ok(prefilter) => prefilter,
        Err(e) => {
            eprintln!("lpcache: failed to load prefilter plugin: {e}");
            std::process::exit(2);
        }
    };
    // The result store of the loaded prefilter (when it is backed by one,
    // e.g. the `result_cache` plugin): the proxy stores fresh non-streaming
    // backend results there after the backend call.
    let result_store: Option<Arc<dyn ResultStore>> =
        prefilter.as_ref().and_then(|pf| pf.result_store());
    if let Some(store) = &result_store
        && let Err(e) = store.prepare()
    {
        // Pre-create the backing store (e.g. the cache-result dir) so a
        // store failure is visible early.
        tracing::warn!("could not prepare result store: {e}");
    }
    if let Some(pf) = &prefilter {
        tracing::info!(
            "prefilter enabled: filter={} plugins={:?}",
            pf.name(),
            specs.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
        );
    }

    let port = config.port;
    // One backend client per `BACKENDS` entry, instantiated through the
    // plugin registry by the built-in `llama` plugin with the per-backend
    // URL plus the shared auth/timeout settings.
    let mut clients: Vec<Arc<dyn LlamaBackend>> = Vec::new();
    for be in &config.backends {
        let spec = PluginSpec::new(
            BACKEND_PLUGIN,
            json!({
                "url": be.url,
                "api_key": config.api_key,
                "timeout_secs": config.request_timeout.as_secs_f64(),
            }),
        );
        match registry.create_backend(&spec) {
            Ok(client) => clients.push(client),
            Err(e) => tracing::error!("backend client init failed url={}: {e}", be.url),
        }
    }

    let sm = Arc::new(
        SlotManager::new(&config.backends, clients.clone()).with_save_retry(
            config.save_retries,
            std::time::Duration::from_millis(config.save_retry_delay_ms),
        ),
    );
    let state = AppState {
        config: Arc::new(config.clone()),
        clients,
        sm,
        sf: Arc::new(SingleFlight::new()),
        prefilter,
        result_store,
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
        "app_start version={} n_backends={} port={} meta_max={} stream_queue={}",
        lpcache::config::VERSION,
        state.clients.len(),
        port,
        config.meta_max,
        config.stream_queue_size
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
