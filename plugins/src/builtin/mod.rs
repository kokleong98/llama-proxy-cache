//! Built-in lpcache plugins.
//!
//! Each module implements one or more [`crate::api`] adapter traits plus a
//! [`crate::registry::PluginFactory`] so the plugin can be loaded from
//! configuration by name. [`register`] puts all built-ins into a
//! [`crate::registry::Registry`]:
//!
//! | name             | adapters provided                    | params |
//! |------------------|--------------------------------------|--------|
//! | `keyword`        | prefilter                            | `keywords` (array or comma string), `case_insensitive` (default `true`) |
//! | `chain`          | prefilter (runs nested plugins in order) | `filters`: array of plugin specs |
//! | `result_cache`   | prefilter + result store             | `dir`, `ttl_secs` (default `300`), `cleanup` (`"remove_after_serve"` or omitted) |
//! | `llama`          | llama.cpp backend client             | `url`, `api_key` (optional), `timeout_secs` (default `600`) |
//! | `request_log`    | request logger (W3C-XLF request log file) | `path` (default `lpcache-request.log`), `max_bytes` (default 10 MiB, `0` = no rotation), `keep` (default `5`, `0` = keep all), `fields` (optional subset) |

pub mod chain;
pub mod keyword;
pub mod llama;
pub mod request_log;
pub mod result_cache;

use crate::registry::Registry;
use std::sync::Arc;

/// Register all built-in plugins in `registry` under their names.
/// Existing entries under those names are replaced.
pub fn register(registry: &Registry) {
    registry.register("keyword", Arc::new(keyword::KeywordPluginFactory));
    registry.register(
        "chain",
        Arc::new(chain::ChainPluginFactory::new(registry.clone())),
    );
    registry.register(
        "result_cache",
        Arc::new(result_cache::ResultCachePluginFactory),
    );
    registry.register("llama", Arc::new(llama::LlamaPluginFactory));
    registry.register(
        "request_log",
        Arc::new(request_log::RequestLogPluginFactory),
    );
}
