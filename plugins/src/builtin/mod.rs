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
//! | `qwen_guard`     | prefilter (Qwen3Guard-Gen safety check over a separate llama-server) | `url` (required), `api_key`, `model`, `timeout_secs` (default `30`), `max_input_chars` (default `0` = none), `reject_levels` (default `["Unsafe"]`), `reject_categories` (default `[]`), `on_error` / `on_unparseable` (`"accept"`, default, or `"reject"`), `cache_max` (default `0` = off) |
//! | `chain`          | prefilter (runs nested plugins in order) | `filters`: array of plugin specs |
//! | `result_cache`   | prefilter + result store             | `dir`, `ttl_secs` (default `300`), `cleanup` (`"remove_after_serve"` or omitted) |
//! | `llama`          | llama.cpp backend client             | `url`, `api_key` (optional), `timeout_secs` (default `600`) |
//! | `request_log`    | request logger (W3C-XLF request log file) | `path` (default `lpcache-request.log`), `max_bytes` (default 10 MiB, `0` = no rotation), `keep` (default `5`, `0` = keep all), `fields` (optional subset) |
//! | `dashboard`      | real-time statistics web dashboard       | `path` (default `/dashboard`), `refresh_ms` (default `2000`), `window_secs` (default `300`), `bucket_secs` (default `5`), `max_recent` (default `100`, `0` = off) |

pub mod chain;
pub mod dashboard;
pub mod keyword;
pub mod llama;
pub mod qwen_guard;
pub mod request_log;
pub mod result_cache;

use crate::registry::Registry;
use std::sync::Arc;

/// Register all built-in plugins in `registry` under their names.
/// Existing entries under those names are replaced.
pub fn register(registry: &Registry) {
    registry.register("keyword", Arc::new(keyword::KeywordPluginFactory));
    registry.register("qwen_guard", Arc::new(qwen_guard::QwenGuardPluginFactory));
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
    registry.register("dashboard", Arc::new(dashboard::DashboardPluginFactory));
}
