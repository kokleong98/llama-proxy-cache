//! Plugin registry: dynamic, configuration-driven adapter loading.
//!
//! Plugins are **loaded by name from JSON configuration at runtime**: the
//! proxy takes a list of [`PluginSpec`] (from the `PLUGINS` env var /
//! `--plugins` flag, or synthesized from the legacy `PREFILTER_*` settings)
//! and asks a [`Registry`] to instantiate each named plugin with its JSON
//! `params`. Which plugins run is therefore decided by configuration, not
//! by compile-time wiring in the main module.
//!
//! A registry maps plugin names to [`PluginFactory`]s. The built-in
//! plugins (`keyword`, `chain`, `result_cache`, `llama`, `request_log`)
//! are registered by
//! [`ensure_builtin`] (process-wide [`global`] registry) or
//! [`Registry::with_builtins`] (private registry). Custom plugin crates
//! register their own factories under new names — in the global registry
//! for the `lpcache` binary, or a private [`Registry`] for embedded use —
//! and become loadable from configuration like any built-in.
//!
//! ```
//! use lpcache_plugins::registry::{PluginSpec, Registry, load_prefilters};
//! use serde_json::json;
//!
//! let reg = Registry::with_builtins();
//! let specs = vec![PluginSpec {
//!     name: "keyword".to_string(),
//!     params: json!({ "keywords": ["forbidden"], "case_insensitive": true }),
//! }];
//! let prefilter = load_prefilters(&reg, &specs).unwrap().expect("one spec");
//! assert_eq!(prefilter.name(), "keyword");
//! ```

use crate::api::{Dashboard, LlamaBackend, Prefilter, RequestLogger};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// One entry of the `PLUGINS` configuration: a plugin `name` (as
/// registered in the [`Registry`]) and its JSON `params` object
/// (`{}` when the plugin takes none).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSpec {
    pub name: String,
    /// Plugin parameters (plugin-specific JSON object).
    #[serde(default)]
    pub params: Value,
}

impl PluginSpec {
    /// Convenience constructor.
    pub fn new(name: impl Into<String>, params: Value) -> Self {
        Self {
            name: name.into(),
            params,
        }
    }
}

impl TryFrom<&Value> for PluginSpec {
    type Error = String;

    /// Parse a spec object (`{"name": "...", "params": {...}}`) from a
    /// JSON value (used by the `chain` plugin's nested `filters`).
    fn try_from(v: &Value) -> Result<Self, String> {
        let name = v
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "spec must be an object with a `name` string".to_string())?;
        let params = v
            .get("params")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        Ok(Self { name, params })
    }
}

/// Errors from creating a plugin from a [`PluginSpec`].
#[derive(Debug)]
pub enum PluginError {
    /// No plugin is registered under the spec's name.
    UnknownPlugin(String),
    /// The registered plugin exists, but it does not provide the requested
    /// adapter kind (e.g. asked for a prefilter from a backend-only plugin).
    UnsupportedKind { name: String, kind: &'static str },
    /// The spec's `params` were missing or invalid for this plugin.
    BadParams { name: String, message: String },
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginError::UnknownPlugin(name) => {
                write!(f, "no plugin registered under name {name:?}")
            }
            PluginError::UnsupportedKind { name, kind } => {
                write!(f, "plugin {name:?} does not provide a {kind} adapter")
            }
            PluginError::BadParams { name, message } => {
                write!(f, "invalid params for plugin {name:?}: {message}")
            }
        }
    }
}

impl std::error::Error for PluginError {}

/// A factory that constructs adapter instances from JSON parameters.
///
/// Each built-in plugin implements exactly the kind(s) it provides and
/// leaves the rest at the default (`Ok(None)`). Invalid parameters must be
/// reported as [`PluginError::BadParams`] (naming the plugin).
pub trait PluginFactory: Send + Sync {
    /// Build a prefilter adapter, or `Ok(None)` when this plugin does not
    /// provide one.
    fn create_prefilter(&self, params: &Value) -> Result<Option<Arc<dyn Prefilter>>, PluginError> {
        let _ = params;
        Ok(None)
    }

    /// Build a llama.cpp backend client, or `Ok(None)` when this plugin
    /// does not provide one.
    fn create_backend(&self, params: &Value) -> Result<Option<Arc<dyn LlamaBackend>>, PluginError> {
        let _ = params;
        Ok(None)
    }

    /// Build a request logger, or `Ok(None)` when this plugin does not
    /// provide one.
    fn create_request_logger(
        &self,
        params: &Value,
    ) -> Result<Option<Arc<dyn RequestLogger>>, PluginError> {
        let _ = params;
        Ok(None)
    }

    /// Build a dashboard adapter, or `Ok(None)` when this plugin does not
    /// provide one.
    fn create_dashboard(&self, params: &Value) -> Result<Option<Arc<dyn Dashboard>>, PluginError> {
        let _ = params;
        Ok(None)
    }
}

/// A name → factory registry. Clones are cheap (shared map) and may be
/// handed to factories (the `chain` plugin resolves its nested specs
/// against the registry it was built from).
#[derive(Clone, Default)]
pub struct Registry {
    factories: Arc<Mutex<HashMap<String, Arc<dyn PluginFactory>>>>,
}

impl Registry {
    /// An empty registry (no plugins).
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry with all built-in plugins registered (private use).
    pub fn with_builtins() -> Self {
        let r = Self::new();
        crate::builtin::register(&r);
        r
    }

    /// Register (or replace) the factory served under `name`.
    pub fn register(&self, name: impl Into<String>, factory: Arc<dyn PluginFactory>) {
        self.factories
            .lock()
            .expect("plugin registry poisoned")
            .insert(name.into(), factory);
    }

    /// Register all built-in plugins (`keyword`, `chain`, `result_cache`,
    /// `llama`). Existing entries under those names are replaced.
    pub fn register_builtin(&self) {
        crate::builtin::register(self);
    }

    /// `true` when a plugin is registered under `name`.
    pub fn is_registered(&self, name: &str) -> bool {
        self.factories
            .lock()
            .expect("plugin registry poisoned")
            .contains_key(name)
    }

    /// The factory registered under `name` (the loaders use this to tell
    /// "not this adapter kind" — `Ok(None)` — apart from "unknown plugin").
    pub fn factory(&self, name: &str) -> Option<Arc<dyn PluginFactory>> {
        self.factories
            .lock()
            .expect("plugin registry poisoned")
            .get(name)
            .cloned()
    }

    /// Registered plugin names, sorted.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .factories
            .lock()
            .expect("plugin registry poisoned")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Instantiate the spec's plugin as a prefilter adapter.
    pub fn create_prefilter(&self, spec: &PluginSpec) -> Result<Arc<dyn Prefilter>, PluginError> {
        let factory = self
            .factories
            .lock()
            .expect("plugin registry poisoned")
            .get(&spec.name)
            .cloned()
            .ok_or_else(|| PluginError::UnknownPlugin(spec.name.clone()))?;
        factory
            .create_prefilter(&spec.params)?
            .ok_or_else(|| PluginError::UnsupportedKind {
                name: spec.name.clone(),
                kind: "prefilter",
            })
    }

    /// Instantiate the spec's plugin as a llama.cpp backend client.
    pub fn create_backend(&self, spec: &PluginSpec) -> Result<Arc<dyn LlamaBackend>, PluginError> {
        let factory = self
            .factories
            .lock()
            .expect("plugin registry poisoned")
            .get(&spec.name)
            .cloned()
            .ok_or_else(|| PluginError::UnknownPlugin(spec.name.clone()))?;
        factory
            .create_backend(&spec.params)?
            .ok_or_else(|| PluginError::UnsupportedKind {
                name: spec.name.clone(),
                kind: "backend",
            })
    }

    /// Instantiate the spec's plugin as a request logger.
    ///
    /// Returns `Ok(None)` when the plugin is registered but does not
    /// provide a request logger (a request-logger spec mixed into a
    /// prefilter list, or vice versa — the loaders skip such specs);
    /// `Err(PluginError::UnknownPlugin)` when no plugin is registered
    /// under the spec's name at all.
    pub fn create_request_logger(
        &self,
        spec: &PluginSpec,
    ) -> Result<Option<Arc<dyn RequestLogger>>, PluginError> {
        let factory = self
            .factories
            .lock()
            .expect("plugin registry poisoned")
            .get(&spec.name)
            .cloned()
            .ok_or_else(|| PluginError::UnknownPlugin(spec.name.clone()))?;
        factory.create_request_logger(&spec.params)
    }

    /// Instantiate the spec's plugin as a dashboard adapter.
    ///
    /// Returns `Ok(None)` when the plugin is registered but does not
    /// provide a dashboard (a dashboard spec mixed into a prefilter list,
    /// or vice versa — the loaders skip such specs);
    /// `Err(PluginError::UnknownPlugin)` when no plugin is registered
    /// under the spec's name at all.
    pub fn create_dashboard(
        &self,
        spec: &PluginSpec,
    ) -> Result<Option<Arc<dyn Dashboard>>, PluginError> {
        let factory = self
            .factories
            .lock()
            .expect("plugin registry poisoned")
            .get(&spec.name)
            .cloned()
            .ok_or_else(|| PluginError::UnknownPlugin(spec.name.clone()))?;
        factory.create_dashboard(&spec.params)
    }
}

/// The process-wide registry (the `lpcache` binary loads plugins from it).
pub fn global() -> &'static Registry {
    static GLOBAL: OnceLock<Registry> = OnceLock::new();
    GLOBAL.get_or_init(Registry::new)
}

/// Idempotently register all built-in plugins in the [`global`] registry.
/// The `lpcache` binary calls this before loading plugins from
/// configuration; embedding users who build their own [`Registry`] do not
/// need it.
pub fn ensure_builtin() {
    global().register_builtin();
}

/// Load prefilter plugins from `specs` in order:
/// - zero specs → `Ok(None)` (no prefilter — the default);
/// - one spec → that single adapter;
/// - several → a `ChainPrefilter` running them in order (the first
///   non-`Accept` decision wins).
///
/// The `PLUGINS` list may also contain specs for other adapter kinds
/// (e.g. `request_log`); those are skipped here, and only unknown plugin
/// names are an error.
///
/// This is the main module's entry point for dynamically loading the
/// request-prefilter pipeline from configuration.
pub fn load_prefilters(
    registry: &Registry,
    specs: &[PluginSpec],
) -> Result<Option<Arc<dyn Prefilter>>, PluginError> {
    if specs.is_empty() {
        return Ok(None);
    }
    let mut filters: Vec<Arc<dyn Prefilter>> = Vec::new();
    for spec in specs {
        let factory = registry
            .factory(&spec.name)
            .ok_or_else(|| PluginError::UnknownPlugin(spec.name.clone()))?;
        // Specs for other adapter kinds (e.g. request_log) return None —
        // skip.
        if let Some(filter) = factory.create_prefilter(&spec.params)? {
            filters.push(filter);
        }
    }
    Ok(match filters.len() {
        0 => None,
        1 => Some(filters.into_iter().next().expect("one filter")),
        _ => Some(Arc::new(crate::builtin::chain::ChainPrefilter::new(
            filters,
        ))),
    })
}

/// Load the request-logger plugin from `specs`: the first spec whose
/// plugin provides a request logger (zero specs of that kind → `Ok(None)`
/// = request logging disabled). Specs for other adapter kinds are skipped;
/// an unknown plugin name is an error, as is a second request-logger spec.
pub fn load_request_loggers(
    registry: &Registry,
    specs: &[PluginSpec],
) -> Result<Option<Arc<dyn RequestLogger>>, PluginError> {
    if specs.is_empty() {
        return Ok(None);
    }
    let mut logger: Option<Arc<dyn RequestLogger>> = None;
    for spec in specs {
        // Specs for other adapter kinds (e.g. a prefilter) return None —
        // skip.
        let Some(l) = registry.create_request_logger(spec)? else {
            continue;
        };
        if logger.is_some() {
            return Err(PluginError::BadParams {
                name: spec.name.clone(),
                message: "only one request logger plugin is supported".to_string(),
            });
        }
        logger = Some(l);
    }
    Ok(logger)
}

/// Load the dashboard plugin from `specs`: the first spec whose plugin
/// provides a dashboard (zero specs of that kind → `Ok(None)` = the
/// dashboard is disabled). Specs for other adapter kinds are skipped; an
/// unknown plugin name is an error, as is a second dashboard spec.
pub fn load_dashboard(
    registry: &Registry,
    specs: &[PluginSpec],
) -> Result<Option<Arc<dyn Dashboard>>, PluginError> {
    if specs.is_empty() {
        return Ok(None);
    }
    let mut dashboard: Option<Arc<dyn Dashboard>> = None;
    for spec in specs {
        // Specs for other adapter kinds (e.g. a prefilter) return None —
        // skip.
        let Some(d) = registry.create_dashboard(spec)? else {
            continue;
        };
        if dashboard.is_some() {
            return Err(PluginError::BadParams {
                name: spec.name.clone(),
                message: "only one dashboard plugin is supported".to_string(),
            });
        }
        dashboard = Some(d);
    }
    Ok(dashboard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn keyword_spec(kw: &str) -> PluginSpec {
        PluginSpec {
            name: "keyword".to_string(),
            params: json!({ "keywords": [kw], "case_insensitive": true }),
        }
    }

    /// `Result::expect` needs `T: Debug`; `dyn RequestLogger` is not
    /// `Debug`, so match instead.
    fn logger_of(
        r: Result<Option<Arc<dyn RequestLogger>>, PluginError>,
    ) -> Option<Arc<dyn RequestLogger>> {
        match r {
            Ok(v) => v,
            Err(e) => panic!("expected Ok: {e}"),
        }
    }

    #[test]
    fn with_builtins_registers_all_builtin_plugins() {
        let reg = Registry::with_builtins();
        assert_eq!(
            reg.names(),
            vec![
                "chain".to_string(),
                "dashboard".to_string(),
                "keyword".to_string(),
                "llama".to_string(),
                "request_log".to_string(),
                "result_cache".to_string()
            ]
        );
        assert!(reg.is_registered("keyword"));
        assert!(!reg.is_registered("nope"));
    }

    #[test]
    fn loaders_skip_specs_of_other_adapter_kinds() {
        let reg = Registry::with_builtins();
        let specs = vec![
            keyword_spec("a"),
            PluginSpec::new(
                "request_log",
                json!({ "path": "req.log", "max_bytes": 0, "keep": 0 }),
            ),
        ];
        // load_prefilters skips the request-logger spec,
        let prefilter = load_prefilters(&reg, &specs).unwrap().expect("prefilter");
        assert_eq!(prefilter.name(), "keyword");
        // load_request_loggers skips the prefilter spec,
        let Some(logger) = logger_of(load_request_loggers(&reg, &specs)) else {
            panic!("expected a logger");
        };
        assert_eq!(logger.name(), "request_log");
        // a known non-logger plugin is skipped (no logger at all),
        assert!(logger_of(load_request_loggers(&reg, &[keyword_spec("a")])).is_none());
        // unknown names are errors for both loaders,
        let unknown = PluginSpec {
            name: "nope".to_string(),
            params: Value::Object(Default::default()),
        };
        assert!(matches!(
            load_request_loggers(&reg, std::slice::from_ref(&unknown)),
            Err(PluginError::UnknownPlugin(_))
        ));
        assert!(matches!(
            load_prefilters(&reg, &[unknown]),
            Err(PluginError::UnknownPlugin(_))
        ));
        // a second request-logger spec is an error,
        let err = match load_request_loggers(&reg, &[specs[1].clone(), specs[1].clone()]) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(matches!(err, PluginError::BadParams { .. }));
    }

    #[test]
    fn register_custom_plugin_and_create_it() {
        use crate::api::{Prefilter, PrefilterDecision, PrefilterRequest};
        use async_trait::async_trait;

        struct AlwaysReject;
        #[async_trait]
        impl Prefilter for AlwaysReject {
            fn name(&self) -> &str {
                "custom_reject"
            }
            async fn check(&self, _req: &PrefilterRequest) -> PrefilterDecision {
                PrefilterDecision::Reject {
                    status: 418,
                    message: "teapot".to_string(),
                }
            }
        }

        #[derive(Default)]
        struct CustomFactory;
        impl PluginFactory for CustomFactory {
            fn create_prefilter(
                &self,
                _params: &Value,
            ) -> Result<Option<Arc<dyn Prefilter>>, PluginError> {
                Ok(Some(Arc::new(AlwaysReject)))
            }
        }

        let reg = Registry::new();
        reg.register("custom_reject", Arc::new(CustomFactory));
        let pf = reg
            .create_prefilter(&PluginSpec {
                name: "custom_reject".to_string(),
                params: Value::Object(Default::default()),
            })
            .expect("custom plugin loads");
        assert_eq!(pf.name(), "custom_reject");
    }

    #[test]
    fn unknown_plugin_is_an_error() {
        let reg = Registry::with_builtins();
        let err = match reg.create_prefilter(&PluginSpec {
            name: "nope".to_string(),
            params: Value::Object(Default::default()),
        }) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(matches!(err, PluginError::UnknownPlugin(ref n) if n == "nope"));
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn wrong_kind_is_an_error() {
        let reg = Registry::with_builtins();
        // keyword provides a prefilter, not a backend client.
        let err = match reg.create_backend(&keyword_spec("x")) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(matches!(err, PluginError::UnsupportedKind { name, kind }
            if name == "keyword" && kind == "backend"));
        // llama provides a backend client, not a prefilter.
        let err = match reg.create_prefilter(&PluginSpec {
            name: "llama".to_string(),
            params: json!({ "url": "http://127.0.0.1:1" }),
        }) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(matches!(err, PluginError::UnsupportedKind { name, kind }
            if name == "llama" && kind == "prefilter"));
    }

    #[test]
    fn load_prefilters_zero_one_many() {
        let reg = Registry::with_builtins();
        // zero specs -> no prefilter (the default)
        assert!(load_prefilters(&reg, &[]).unwrap().is_none());
        // one spec -> that single adapter (not wrapped in a chain)
        let one = load_prefilters(&reg, &[keyword_spec("a")])
            .unwrap()
            .expect("one");
        assert_eq!(one.name(), "keyword");
        // several -> a chain running them in order
        let many = load_prefilters(&reg, &[keyword_spec("a"), keyword_spec("b")])
            .unwrap()
            .expect("many");
        assert_eq!(many.name(), "chain");
    }

    #[test]
    fn load_prefilters_propagates_errors() {
        let reg = Registry::with_builtins();
        let err = match load_prefilters(
            &reg,
            &[PluginSpec {
                name: "missing".to_string(),
                params: Value::Object(Default::default()),
            }],
        ) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(matches!(err, PluginError::UnknownPlugin(_)));
    }

    #[test]
    fn plugin_spec_from_value_defaults_empty_params() {
        let v = json!({ "name": "keyword" });
        let spec = PluginSpec::try_from(&v).expect("spec");
        assert_eq!(spec.name, "keyword");
        assert!(spec.params.is_object());
        // a non-object spec is an error
        assert!(PluginSpec::try_from(&json!("keyword")).is_err());
    }

    #[test]
    fn global_registry_and_ensure_builtin_are_idempotent() {
        ensure_builtin();
        ensure_builtin(); // must not panic / duplicate
        let reg = global();
        assert!(reg.is_registered("keyword"));
        assert!(reg.is_registered("llama"));
        assert_eq!(reg.names(), Registry::with_builtins().names());
    }

    fn dashboard_spec() -> PluginSpec {
        PluginSpec::new("dashboard", json!({ "path": "/stats" }))
    }

    #[test]
    fn load_dashboard_zero_one_many() {
        let reg = Registry::with_builtins();
        // zero specs -> no dashboard (the default)
        assert!(load_dashboard(&reg, &[]).unwrap().is_none());
        // one spec -> that adapter; prefilter specs in the list are skipped
        let specs = vec![keyword_spec("a"), dashboard_spec()];
        let Some(d) = load_dashboard(&reg, &specs).unwrap() else {
            panic!("expected a dashboard");
        };
        assert_eq!(d.name(), "dashboard");
        assert_eq!(d.path(), "/stats");
        // a second dashboard spec is an error
        let err = match load_dashboard(&reg, &[dashboard_spec(), dashboard_spec()]) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(matches!(err, PluginError::BadParams { ref name, .. } if name == "dashboard"));
        // an unknown plugin name is an error
        let err = match load_dashboard(
            &reg,
            &[PluginSpec {
                name: "missing".to_string(),
                params: Value::Object(Default::default()),
            }],
        ) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(matches!(err, PluginError::UnknownPlugin(_)));
    }
}
