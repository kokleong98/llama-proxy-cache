//! lpcache — KV-cache-aware OpenAI-compatible proxy in front of llama.cpp.
//!
//! Rust rewrite of the Python (FastAPI) project:
//!
//! | Python module     | Rust module      |
//! |-------------------|------------------|
//! | `config.py`       | `config`         |
//! | `hashing.py`      | `hashing`        |
//! | `llama_client.py` | `llama_client`   |
//! | `slot_manager.py` | `slot_manager`   |
//! | `app.py`          | `app`            |
//! | `proxycache.py`   | `main` (binary)  |
//!
//! `mock_backend` provides a mock llama.cpp server used by the integration
//! tests and by the `mock_llama` example for local smoke testing.

pub mod app;
pub mod coalesce;
pub mod config;
pub mod hashing;
pub mod llama_client;
pub mod mock_backend;
pub mod slot_manager;
