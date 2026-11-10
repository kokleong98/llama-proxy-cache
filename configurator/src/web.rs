//! Web configurator: a single-page UI ([`crate::ui::HTML`]) plus a small
//! JSON API for loading/saving the config file, live export previews,
//! validation and backend probes.

use crate::exporters::export;
use crate::model::{self, LpcacheConfig, DEFAULT_CONFIG_FILE};
use crate::plugin_schema;
use axum::{
    extract::State,
    http::StatusCode,
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Server state: the config file the UI edits (fixed at `serve` time).
#[derive(Clone)]
pub struct WebState {
    pub config_path: PathBuf,
}

/// `lpcache-config serve`: build the app, bind and serve until Ctrl-C.
pub async fn serve(config_path: PathBuf, host: &str, port: u16) -> Result<(), String> {
    let state = Arc::new(WebState { config_path });
    let app = Router::new()
        .route("/", get(index))
        .route("/api/state", get(get_state))
        .route("/api/save", post(save_handler))
        .route("/api/validate", post(validate_handler))
        .route("/api/reset", post(reset_handler))
        .route("/api/plugins", get(get_plugins))
        .route("/api/export", post(export_handler))
        .route("/api/backend/check", post(check_backend))
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind((host, port))
        .await
        .map_err(|e| format!("bind {host}:{port} failed: {e}"))?;
    tracing::info!(
        "web UI: http://{}:{} (config: {})",
        if host == "0.0.0.0" { "127.0.0.1" } else { host },
        port,
        state.config_path.display()
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutdown signal received");
        })
        .await
        .map_err(|e| format!("server error: {e}"))
}

async fn index() -> Html<String> {
    Html(crate::ui::HTML.to_string())
}

#[derive(Serialize)]
struct StateResp {
    path: String,
    exists: bool,
    /// The config from disk, or `None` when the file is missing/unreadable.
    config: Option<LpcacheConfig>,
    load_error: Option<String>,
    errors: Vec<String>,
    warnings: Vec<String>,
}

async fn get_state(State(state): State<Arc<WebState>>) -> Json<StateResp> {
    let path = state.config_path.display().to_string();
    match model::load(&state.config_path) {
        Ok(cfg) => {
            let (errors, warnings) = cfg.validate();
            Json(StateResp {
                path,
                exists: true,
                config: Some(cfg),
                load_error: None,
                errors,
                warnings,
            })
        }
        Err(e) => Json(StateResp {
            path,
            exists: false,
            config: None,
            load_error: Some(e),
            errors: Vec::new(),
            warnings: Vec::new(),
        }),
    }
}

#[derive(Deserialize)]
struct SaveReq {
    config: LpcacheConfig,
}

/// Validate-then-write: a config with validation errors is rejected (400)
/// instead of saved, so a broken file never lands on disk.
async fn save_handler(
    State(state): State<Arc<WebState>>,
    Json(req): Json<SaveReq>,
) -> Result<
    (axum::http::StatusCode, Json<serde_json::Value>),
    (axum::http::StatusCode, Json<serde_json::Value>),
> {
    let (errors, warnings) = req.config.validate();
    if !errors.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "errors": errors, "warnings": warnings })),
        ));
    }
    model::save(&state.config_path, &req.config).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "errors": [e], "warnings": [] })),
        )
    })?;
    tracing::info!(path = %state.config_path.display(), "config saved");
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "path": state.config_path.display().to_string(),
            "warnings": warnings,
        })),
    ))
}

#[derive(Deserialize)]
struct ValidateReq {
    config: LpcacheConfig,
}

async fn validate_handler(Json(req): Json<ValidateReq>) -> Json<serde_json::Value> {
    let (errors, warnings) = req.config.validate();
    Json(serde_json::json!({ "errors": errors, "warnings": warnings }))
}

async fn reset_handler() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "config": LpcacheConfig::default() }))
}

/// The built-in plugin schema table (params, types, ranges, choices) the web
/// UI builds its plugin forms from — the same table validation uses.
async fn get_plugins() -> Json<serde_json::Value> {
    Json(plugin_schema::schemas_json())
}

#[derive(Deserialize)]
struct ExportReq {
    format: String,
    config: LpcacheConfig,
}

async fn export_handler(
    Json(req): Json<ExportReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    match export(&req.config, &req.format) {
        Ok(text) => Ok(Json(serde_json::json!({ "text": text }))),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e })),
        )),
    }
}

/// The default config path for `serve` when `-c` is not given.
pub fn default_config_path() -> PathBuf {
    PathBuf::from(DEFAULT_CONFIG_FILE)
}


#[derive(Deserialize)]
struct CheckReq {
    url: String,
    #[serde(default)]
    api_key: String,
}

/// Probe a backend's `GET /v1/models` (the same endpoint lpcache's
/// recovery probe uses) and report the model ids.
async fn check_backend(Json(req): Json<CheckReq>) -> Json<serde_json::Value> {
    let base = req.url.trim().trim_end_matches('/').to_string();
    if base.is_empty() {
        return Json(serde_json::json!({
            "ok": false, "status": null, "models": [], "error": "url is empty"
        }));
    }
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Json(serde_json::json!({
                "ok": false, "status": null, "models": [], "error": e.to_string()
            }));
        }
    };
    let mut r = client.get(format!("{base}/v1/models"));
    if !req.api_key.trim().is_empty() {
        r = r.bearer_auth(req.api_key.trim());
    }
    let resp = match r.send().await {
        Ok(r) => r,
        Err(e) => {
            return Json(serde_json::json!({
                "ok": false, "status": null, "models": [], "error": e.to_string()
            }));
        }
    };
    let status = resp.status().as_u16();
    let models: Vec<String> = resp
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|v| v.get("data").and_then(|d| d.as_array()).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
        .collect();
    Json(serde_json::json!({
        "ok": status == 200 && !models.is_empty(),
        "status": status,
        "models": models,
        "error": if status == 200 {
            if models.is_empty() {
                Some("HTTP 200 but no model ids in the response body".to_string())
            } else {
                None
            }
        } else {
            Some(format!("HTTP {status}"))
        }
    }))
}

