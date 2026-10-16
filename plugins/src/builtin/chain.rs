//! `chain` plugin — runs several prefilter plugins in order; the first
//! non-`Accept` decision wins (a `Reject` or `Serve` from an earlier
//! adapter stops the chain, and `Accept` from every adapter forwards the
//! request).
//!
//! Two ways to configure it:
//! - the proxy synthesizes a chain automatically when the `PLUGINS` list
//!   (or the legacy `PREFILTER_*` settings) enables several prefilter
//!   plugins;
//! - explicitly, with a `chain` spec whose `filters` parameter is an
//!   array of nested plugin specs — resolved against the same registry,
//!   so chains can be built from arbitrary (custom) plugins:
//!
//! ```json
//! { "name": "chain",
//!   "params": { "filters": [
//!     { "name": "keyword", "params": { "keywords": ["a"] } },
//!     { "name": "result_cache", "params": { "dir": "/var/rc" } }
//!   ] } }
//! ```

use crate::api::{Prefilter, PrefilterDecision, PrefilterRequest, ResultStore};
use crate::registry::{PluginError, PluginFactory, PluginSpec, Registry};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// Runs several prefilter adapters in order; the first non-`Accept`
/// decision wins (a `Reject` or `Serve` from an earlier adapter stops the
/// chain, and `Accept` from every adapter forwards the request).
#[derive(Clone)]
pub struct ChainPrefilter {
    filters: Vec<Arc<dyn Prefilter>>,
}

impl ChainPrefilter {
    /// Build from an ordered list of adapters (evaluation order).
    pub fn new(filters: Vec<Arc<dyn Prefilter>>) -> Self {
        Self { filters }
    }

    /// The adapters in evaluation order.
    pub fn filters(&self) -> &[Arc<dyn Prefilter>] {
        &self.filters
    }
}

#[async_trait]
impl Prefilter for ChainPrefilter {
    fn name(&self) -> &str {
        "chain"
    }

    async fn check(&self, req: &PrefilterRequest) -> PrefilterDecision {
        for f in &self.filters {
            match f.check(req).await {
                PrefilterDecision::Accept => {}
                other => return other,
            }
        }
        PrefilterDecision::Accept
    }

    /// The store of the first member that provides one.
    fn result_store(&self) -> Option<Arc<dyn ResultStore>> {
        self.filters.iter().find_map(|f| f.result_store())
    }
}

// ---------------------------------------------------------------------------
// Plugin factory (configurable, dynamically loadable)
// ---------------------------------------------------------------------------

const NAME: &str = "chain";

/// Factory for the `chain` plugin (registered by
/// [`crate::builtin::register`]). Resolves the nested `filters` specs
/// against the registry the factory was created with, so a `chain` spec
/// can combine arbitrary (custom) plugins.
pub struct ChainPluginFactory {
    registry: Registry,
}

impl ChainPluginFactory {
    /// Build a factory that resolves nested specs against `registry`.
    pub fn new(registry: Registry) -> Self {
        Self { registry }
    }
}

impl PluginFactory for ChainPluginFactory {
    fn create_prefilter(&self, params: &Value) -> Result<Option<Arc<dyn Prefilter>>, PluginError> {
        let items = params
            .get("filters")
            .and_then(Value::as_array)
            .ok_or_else(|| PluginError::BadParams {
                name: NAME.to_string(),
                message: "params must contain a `filters` array of plugin specs".to_string(),
            })?;
        let specs = items
            .iter()
            .enumerate()
            .map(|(i, v)| {
                PluginSpec::try_from(v).map_err(|e| PluginError::BadParams {
                    name: NAME.to_string(),
                    message: format!("filters[{i}]: {e}"),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if specs.is_empty() {
            return Err(PluginError::BadParams {
                name: NAME.to_string(),
                message: "`filters` must not be empty".to_string(),
            });
        }
        Ok(Some(Arc::new(ChainPrefilter::new(
            specs
                .iter()
                .map(|spec| self.registry.create_prefilter(spec))
                .collect::<Result<Vec<_>, _>>()?,
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::keyword::KeywordPrefilter;
    use serde_json::json;

    fn preq(messages: Value, n_words: usize) -> PrefilterRequest {
        PrefilterRequest {
            body: json!({ "messages": messages }),
            messages,
            model: "llama.cpp".to_string(),
            backend_model_id: "backend-model".to_string(),
            stream: false,
            key: "k".to_string(),
            n_words,
            is_big: n_words > 500,
        }
    }

    /// An adapter that always serves a fixed body (chain-test fixture).
    struct ServeFilter {
        body: Value,
    }

    #[async_trait]
    impl Prefilter for ServeFilter {
        fn name(&self) -> &str {
            "serve_fixture"
        }

        async fn check(&self, _req: &PrefilterRequest) -> PrefilterDecision {
            PrefilterDecision::Serve {
                status: 200,
                body: self.body.clone(),
            }
        }
    }

    #[tokio::test]
    async fn chain_first_non_accept_wins() {
        let chain = ChainPrefilter::new(vec![
            Arc::new(KeywordPrefilter::new(["banned"], true)),
            Arc::new(ServeFilter {
                body: json!({ "object": "chat.completion" }),
            }),
        ]);
        assert_eq!(chain.name(), "chain");
        assert_eq!(chain.filters().len(), 2);

        // Clean request: keyword accepts, serve fixture serves.
        let req = preq(json!([{ "role": "user", "content": "hello" }]), 1);
        assert_eq!(
            chain.check(&req).await,
            PrefilterDecision::Serve {
                status: 200,
                body: json!({ "object": "chat.completion" })
            }
        );

        // Blocked request: the earlier reject wins over the later serve.
        let req = preq(json!([{ "role": "user", "content": "banned word" }]), 2);
        assert!(matches!(
            chain.check(&req).await,
            PrefilterDecision::Reject { status: 400, .. }
        ));
    }

    #[tokio::test]
    async fn chain_all_accepts_accepts() {
        let chain = ChainPrefilter::new(vec![
            Arc::new(KeywordPrefilter::new(["banned"], true)),
            Arc::new(KeywordPrefilter::new(["other"], true)),
        ]);
        let req = preq(json!([{ "role": "user", "content": "hello" }]), 1);
        assert_eq!(chain.check(&req).await, PrefilterDecision::Accept);
    }

    #[test]
    fn factory_builds_nested_chain_from_specs() {
        use crate::registry::{PluginFactory, Registry};
        let registry = Registry::with_builtins();
        let f = ChainPluginFactory::new(registry.clone());
        let pf = f
            .create_prefilter(&json!({
                "filters": [
                    { "name": "keyword", "params": { "keywords": ["x"] } },
                    { "name": "keyword", "params": { "keywords": ["y"] } }
                ]
            }))
            .unwrap()
            .expect("prefilter");
        assert_eq!(pf.name(), "chain");
        // Unknown nested plugin -> error naming the bad index.
        let err = match f.create_prefilter(&json!({ "filters": [{ "name": "nope" }] })) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(matches!(err, PluginError::UnknownPlugin(n) if n == "nope"));
        // Missing / empty filters -> bad params.
        assert!(matches!(
            f.create_prefilter(&json!({})),
            Err(PluginError::BadParams { .. })
        ));
        assert!(matches!(
            f.create_prefilter(&json!({ "filters": [] })),
            Err(PluginError::BadParams { .. })
        ));
    }
}
