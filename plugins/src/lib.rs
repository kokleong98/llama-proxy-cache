//! lpcache-plugins — the adapter plugin library for the `lpcache` proxy.
//!
//! All adapter functionality of the proxy lives here, behind small traits
//! (the [`api`]) and a name-based plugin registry ([`registry`]) that the
//! main `lpcache` module uses to **configure and dynamically load** adapter
//! instances at startup from JSON plugin specs:
//!
//! ```json
//! [
//!   { "name": "keyword",
//!     "params": { "keywords": ["a", "b"], "case_insensitive": true } },
//!   { "name": "result_cache",
//!     "params": { "dir": "/var/lpcache/rc", "ttl_secs": 300,
//!                 "cleanup": "remove_after_serve" } }
//! ]
//! ```
//!
//! The proxy feeds these specs (from the `PLUGINS` env var /
//! `--plugins` flag, or synthesized from the legacy `PREFILTER_*` settings)
//! to [`registry::Registry`], which instantiates the named plugins and the
//! app wires the resulting trait objects into `AppState`.
//!
//! ## Modules
//!
//! - [`api`] — the adapter traits and shared types: [`Prefilter`] (request
//!   prefilter), [`ResultStore`] (storage of fresh backend results),
//!   [`PostCleanup`] (post-serve cleanup) and [`LlamaBackend`] (the
//!   llama.cpp backend HTTP adapter).
//! - [`registry`] — `PluginSpec`, `PluginFactory`, the [`Registry`], the
//!   process-wide [`global`] registry ([`ensure_builtin`] registers the
//!   built-ins into it) and [`load_prefilters`].
//! - [`builtin`] — the built-in plugins: `keyword`, `chain`,
//!   `result_cache` and `llama`, each with a JSON-parameter factory.
//!
//! ## Writing a custom plugin
//!
//! A plugin crate depends on `lpcache-plugins`, implements the relevant
//! [`api`] trait(s) and a [`registry::PluginFactory`], and registers
//! itself under a name — in the global registry for the `lpcache` binary,
//! or in a private [`Registry`] for embedded use:
//!
//! ```ignore
//! # use lpcache_plugins::registry::{PluginError, PluginFactory, Registry};
//! # use lpcache_plugins::{Prefilter, PrefilterDecision, PrefilterRequest};
//! # use async_trait::async_trait;
//! # use serde_json::Value;
//! # use std::sync::Arc;
//! struct MyFactory;
//! impl PluginFactory for MyFactory {
//!     fn create_prefilter(&self, _params: &Value)
//!         -> Result<Option<Arc<dyn Prefilter>>, PluginError> {
//!         Ok(Some(Arc::new(MyFilter)))
//!     }
//! }
//! Registry::global().register("my_filter", Arc::new(MyFactory));
//! ```
//!
//! The plugin then becomes loadable from configuration:
//! `[{"name": "my_filter", "params": {}}]`.

pub mod api;
pub mod builtin;
pub mod registry;

pub use api::{
    BackendError, JsonChat, LlamaBackend, PostCleanup, Prefilter, PrefilterDecision,
    PrefilterRequest, RestoreOutcome, ResultStore, StreamChat,
};
pub use builtin::llama::LlamaClient;
pub use registry::{PluginError, PluginSpec, Registry, ensure_builtin, global, load_prefilters};
