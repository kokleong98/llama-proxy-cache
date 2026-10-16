//! lpcache — KV-cache-aware OpenAI-compatible proxy in front of llama.cpp.
//!
//! Main module of the proxy (the `lpcache` binary). Rust rewrite of the
//! Python (FastAPI) project:
//!
//! | Python module     | Rust module      |
//! |-------------------|------------------|
//! | `config.py`       | `config`         |
//! | `hashing.py`      | `hashing`        |
//! | `llama_client.py` | `lpcache_plugins` (built-in `llama` plugin) |
//! | `slot_manager.py` | `slot_manager`   |
//! | `app.py`          | `app`            |
//! | `proxycache.py`   | `main` (binary)  |
//!
//! **All adapter functionality lives in the `lpcache-plugins` library
//! crate** (`lpcache_plugins`): the prefilter / result-store /
//! post-cleanup / backend-client traits (`lpcache_plugins::api`), the
//! built-in plugins (`lpcache_plugins::builtin`) and the plugin registry
//! (`lpcache_plugins::registry`). The main module **dynamically loads**
//! adapter instances from configuration at startup: `Config::plugin_specs`
//! (from the `PLUGINS` env var / `--plugins` flag, or synthesized from the
//! legacy `PREFILTER_*` settings) is fed to the registry in `main`, and
//! the resulting `Arc<dyn Prefilter>` / `Arc<dyn ResultStore>` /
//! `Arc<dyn LlamaBackend>` objects are wired into `AppState`.
//!
//! `mock_backend` provides a mock llama.cpp server used by the integration
//! tests and by the `mock_llama` example for local smoke testing.

pub mod app;
pub mod coalesce;
pub mod config;
pub mod hashing;
pub mod mock_backend;
pub mod slot_manager;

/// The adapter plugin library (re-exported for downstream convenience).
pub use lpcache_plugins as plugins;
