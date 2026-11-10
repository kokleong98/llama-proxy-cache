//! `lpcache-config` — CLI and web configurator for the lpcache proxy.
//!
//! Commands:
//! - `new` / `show` / `get` / `set` / `validate` — manage the TOML config
//!   file (default: `lpcache.config.toml` in the CWD)
//! - `export {env|cli|systemd|docker|llama-server|json}` — render the
//!   configuration into the run forms lpcache understands
//! - `serve` — the web configurator UI (edit + live preview + save)
//! - `check <URL>` — probe a backend's `GET /v1/models`
//!
//! Argument parsing is hand-rolled, matching the lpcache binary's style.

mod exporters;
mod model;
mod plugin_schema;
mod ui;
mod web;
mod wizard;

use model::{LpcacheConfig, DEFAULT_CONFIG_FILE};
use std::path::{Path, PathBuf};

fn version_string() -> String {
    format!(
        "lpcache-config {} (for lpcache {})",
        env!("CARGO_PKG_VERSION"),
        lpcache::config::VERSION
    )
}

fn help() -> &'static str {
    "lpcache-config — CLI and web configurator for the lpcache proxy

The configuration is a TOML file (default: lpcache.config.toml in the CWD,
overridable with -c/--config on serve) mirroring the lpcache environment
variables 1:1 — every lpcache setting has a field, and lpcache itself never
reads the file: the configurator exports env vars / CLI flags / unit files.

Commands:
  new [PATH]                  write a fresh config file with the built-in
                              defaults (PATH \"-\" prints to stdout; refuses
                              to overwrite an existing file)
  configure [PATH]            guided menu: walk through every section with
                              validated prompts, then review and save
                              (alias: wizard)
  show [PATH]                 show the effective configuration
  get KEY [PATH]              print one value (dot-separated key, e.g.
                              proxy.port — list with get --list)
  set KEY VALUE [PATH]        set one value and save the file (creates the
                              file from defaults when missing)
  validate [PATH]             check the configuration; exit 1 on errors
  export FORMAT [PATH]        print the configuration as one of:
                              env, cli, systemd, docker, llama-server, json
  serve [--host H] [--port P]
          [-c PATH]           start the web configurator UI (default
                              http://127.0.0.1:8086)
  check URL [--api-key KEY]
          [--timeout SECS]    probe a backend's GET /v1/models (the same
                              endpoint lpcache's recovery probe uses)
  -c, --config PATH           config file (serve; default lpcache.config.toml)
  -h, --help                  show this help and exit
  -V, --version               show version and exit

Examples:
  lpcache-config new lpcache.config.toml
  lpcache-config configure lpcache.config.toml
  lpcache-config set proxy.port 8090
  lpcache-config set prefilter.blocklist \"bad word,other\"
  lpcache-config export env
  lpcache-config export llama-server
  lpcache-config serve --port 8086
  lpcache-config check http://127.0.0.1:8000
"
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match run(&args).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("lpcache-config: {e}");
            std::process::exit(2);
        }
    };
    std::process::exit(code);
}

async fn run(args: &[String]) -> Result<i32, String> {
    if args.is_empty() {
        println!("{}", help());
        return Ok(2);
    }
    let cmd = args[0].as_str();
    let rest = &args[1..];
    match cmd {
        "-h" | "--help" => {
            println!("{}", help());
            Ok(0)
        }
        "-V" | "--version" => {
            println!("{}", version_string());
            Ok(0)
        }
        "new" => cmd_new(rest),
        "configure" | "wizard" => cmd_configure(rest),
        "show" => cmd_show(rest),
        "get" => cmd_get(rest),
        "set" => cmd_set(rest),
        "validate" => cmd_validate(rest),
        "export" => cmd_export(rest),
        "serve" => cmd_serve(rest).await,
        "check" => cmd_check(rest).await,
        other => Err(format!("unknown command {other:?} — try --help")),
    }
}

/// Resolve the config path from an optional trailing positional.
fn path_from_rest(rest: &[String], pos: usize) -> Result<PathBuf, String> {
    if rest.len() > pos {
        Ok(PathBuf::from(&rest[pos]))
    } else {
        Ok(PathBuf::from(DEFAULT_CONFIG_FILE))
    }
}

/// Load the config file, or the built-in defaults when the file does not
/// exist (the source string then says so).
fn load_for_cmd(path: &Path) -> Result<(LpcacheConfig, String), String> {
    if path.exists() {
        let cfg = model::load(path)?;
        Ok((cfg, path.display().to_string()))
    } else {
        Ok((
            LpcacheConfig::default(),
            format!("{} (file not found — built-in defaults)", path.display()),
        ))
    }
}

/// `new` template: a fully-commented config with all built-in defaults.
/// A unit test pins it to the model (it must parse and equal the defaults).
const NEW_TEMPLATE: &str = r#"# lpcache configuration — generated by lpcache-config.
#
# Every section maps 1:1 to the lpcache environment variables (see
# PARAMETERS.md); lpcache itself never reads this file — export it with
# `lpcache-config export env|cli|systemd|docker` and run lpcache with it.
#
#   [proxy]       PORT, MODEL_ID, LOG_LEVEL, REQUEST_TIMEOUT,
#                 STREAM_QUEUE_SIZE, COALESCE_REQUESTS, LLAMA_API_KEY
#   [[backend]]   LLAMA_URL / N_SLOTS / SLOT_SAVE_PATH (single backend) or
#                 the BACKENDS JSON array (several backends)
#   [kv_cache]    WORDS_PER_BLOCK, BIG_THRESHOLD_WORDS, LCP_TH, META_DIR,
#                 META_MAX, SLOT_SAVE_PATH, SAVE_RETRIES, SAVE_RETRY_DELAY_MS
#   [prefilter]   PREFILTER_* (ignored by the proxy when [plugins] is set)
#   [request_log] REQUEST_LOG, REQUEST_LOG_PATH, REQUEST_LOG_MAX_BYTES,
#                 REQUEST_LOG_KEEP, REQUEST_LOG_FIELDS
#   [plugins]     PLUGINS (raw JSON array of plugin specs)

[proxy]
port = 8081
model_id = "llama.cpp"
log_level = "INFO"
request_timeout_secs = 600
stream_queue_size = 16
coalesce_requests = false
api_key = ""

# One entry per llama-server backend; the url/port must match the
# backend's --host/--port, n_slots its -np/--parallel.
[[backend]]
url = "http://127.0.0.1:8000"
n_slots = 1
slot_save_path = ""
kv_cache = true

[kv_cache]
words_per_block = 100
big_threshold_words = 500
lcp_th = 0.1
meta_dir = "kv_meta"
meta_max = 10
slot_save_path = ""
save_retries = 3
save_retry_delay_ms = 1000

[prefilter]
blocklist = []
case_insensitive = true
result_cache_dir = ""
result_cache_ttl_secs = 300

[request_log]
enabled = false
path = "lpcache-request.log"
max_bytes = 10485760
keep = 5
fields = []

[plugins]
json = ""
"#;

fn cmd_new(rest: &[String]) -> Result<i32, String> {
    if rest.iter().any(|a| a.starts_with('-')) {
        return Err("new takes only an optional path (\"-\" for stdout)".into());
    }
    let path = path_from_rest(rest, 0)?;
    // Sanity: the template must always parse (it is user-facing output).
    let check: LpcacheConfig =
        toml::from_str(NEW_TEMPLATE).map_err(|e| format!("internal: template is broken: {e}"))?;
    let (errors, _) = check.validate();
    if !errors.is_empty() {
        return Err(format!("internal: template is invalid: {errors:?}"));
    }
    if rest.is_empty() || rest[0] == "-" {
        print!("{NEW_TEMPLATE}");
        return Ok(0);
    }
    if path.exists() {
        return Err(format!("{} already exists — refusing to overwrite", path.display()));
    }
    model::save(&path, &check)?;
    println!("wrote {} (all defaults — edit it or use `lpcache-config set`)", path.display());
    Ok(0)
}

/// `configure [PATH]` — the interactive guided menu (see `wizard` module).
fn cmd_configure(rest: &[String]) -> Result<i32, String> {
    let path = path_from_rest(rest, 0)?;
    wizard::run(&path)
}

fn cmd_show(rest: &[String]) -> Result<i32, String> {
    let path = path_from_rest(rest, 0)?;
    let (cfg, source) = load_for_cmd(&path)?;
    let text = cfg.show(&source);
    print!("{text}");
    if !text.ends_with('\n') {
        println!();
    }
    Ok(0)
}

fn cmd_get(rest: &[String]) -> Result<i32, String> {
    if rest.first().map(String::as_str) == Some("--list") {
        for k in LpcacheConfig::keys() {
            println!("{k}");
        }
        return Ok(0);
    }
    if rest.is_empty() {
        return Err("get needs a KEY (dot-separated — list them with get --list)".into());
    }
    let path = path_from_rest(rest, 1)?;
    let (cfg, _) = load_for_cmd(&path)?;
    let value = cfg
        .get_key(&rest[0])
        .ok_or_else(|| format!("unknown key {:?} (list them with get --list)", rest[0]))?;
    println!("{value}");
    Ok(0)
}

fn cmd_set(rest: &[String]) -> Result<i32, String> {
    if rest.len() < 2 {
        return Err("set needs KEY and VALUE (e.g. `set proxy.port 8090`)".into());
    }
    let (key, value) = (rest[0].as_str(), rest[1].as_str());
    let path = path_from_rest(rest, 2)?;
    let mut cfg = if path.exists() { model::load(&path)? } else { LpcacheConfig::default() };
    cfg.set_key(key, value)?;
    model::save(&path, &cfg)?;
    println!("set {key} = {} ({})", cfg.get_key(key).unwrap(), path.display());
    let (errors, warnings) = cfg.validate();
    for e in &errors {
        eprintln!("warning: validation error: {e}");
    }
    for w in &warnings {
        eprintln!("warning: {w}");
    }
    Ok(0)
}

fn cmd_validate(rest: &[String]) -> Result<i32, String> {
    let path = path_from_rest(rest, 0)?;
    if !path.exists() {
        return Err(format!("{} does not exist", path.display()));
    }
    let cfg = model::load(&path)?;
    let (errors, warnings) = cfg.validate();
    for w in &warnings {
        println!("warning: {w}");
    }
    if errors.is_empty() {
        println!("{}: OK ({} warnings)", path.display(), warnings.len());
        Ok(0)
    } else {
        for e in &errors {
            eprintln!("error: {e}");
        }
        eprintln!("{}: INVALID ({} errors)", path.display(), errors.len());
        Ok(1)
    }
}

fn cmd_export(rest: &[String]) -> Result<i32, String> {
    if rest.is_empty() {
        return Err(format!(
            "export needs a FORMAT (one of: {})",
            exporters::FORMATS.join(", ")
        ));
    }
    let path = path_from_rest(rest, 1)?;
    let (cfg, _) = load_for_cmd(&path)?;
    let text = exporters::export(&cfg, &rest[0])?;
    print!("{text}");
    if !text.ends_with('\n') {
        println!();
    }
    Ok(0)
}


/// `serve [--host H] [--port P] [-c PATH]` — start the web UI.
async fn cmd_serve(rest: &[String]) -> Result<i32, String> {
    let mut host = String::from("127.0.0.1");
    let mut port: u16 = 8086;
    let mut path = web::default_config_path();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--host" => host = it
                .next()
                .ok_or("--host needs a value")?
                .as_str()
                .to_string(),
            "--port" => port = it
                .next()
                .ok_or("--port needs a value")?
                .parse()
                .map_err(|_| "invalid --port".to_string())?,
            "-c" | "--config" => path = PathBuf::from(it.next().ok_or("-c needs a value")?),
            other => {
                // `--flag=value` form for host/port
                if let Some((k, v)) = other.split_once('=') {
                    match k {
                        "--host" => host = v.to_string(),
                        "--port" => port = v.parse().map_err(|_| "invalid --port".to_string())?,
                        "-c" | "--config" => path = PathBuf::from(v),
                        _ => return Err(format!("unknown flag {other:?} for serve — try --help")),
                    }
                } else {
                    return Err(format!("unknown argument {other:?} for serve — try --help"));
                }
            }
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(true)
        .init();
    web::serve(path, host.as_str(), port).await?;
    Ok(0)
}

/// `check URL [--api-key KEY] [--timeout SECS]` — probe `GET /v1/models`.
async fn cmd_check(rest: &[String]) -> Result<i32, String> {
    let mut url: Option<String> = None;
    let mut api_key = String::new();
    let mut timeout = 10u64;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--api-key" => api_key = it.next().ok_or("--api-key needs a value")?.clone(),
            "--timeout" => {
                timeout = it
                    .next()
                    .ok_or("--timeout needs a value")?
                    .parse()
                    .map_err(|_| "invalid --timeout".to_string())?
            }
            s if s.starts_with("--") => return Err(format!("unknown flag {s:?} for check — try --help")),
            s if url.is_none() => url = Some(s.to_string()),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let base = url
        .ok_or("check needs a backend URL (e.g. http://127.0.0.1:8000)")?
        .trim_end_matches('/')
        .to_string();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout))
        .build()
        .map_err(|e| e.to_string())?;
    let mut r = client.get(format!("{base}/v1/models"));
    if !api_key.trim().is_empty() {
        r = r.bearer_auth(api_key.trim());
    }
    let resp = r.send().await.map_err(|e| {
        eprintln!("check {base}: unreachable — {e}");
        e.to_string()
    })?;
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
    if status == 200 && !models.is_empty() {
        println!("check {base}: OK (HTTP 200) — models: {}", models.join(", "));
        Ok(0)
    } else {
        let msg = if status == 200 {
            "HTTP 200 but no model ids in the body".to_string()
        } else {
            format!("HTTP {status}")
        };
        eprintln!("check {base}: {msg}");
        Ok(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_and_matches_defaults() {
        let cfg: LpcacheConfig = toml::from_str(NEW_TEMPLATE).unwrap();
        let d = LpcacheConfig::default();
        assert_eq!(cfg.proxy, d.proxy);
        assert_eq!(cfg.backends, d.backends);
        assert_eq!(cfg.kv_cache, d.kv_cache);
        assert_eq!(cfg.prefilter, d.prefilter);
        assert_eq!(cfg.request_log, d.request_log);
        assert_eq!(cfg.plugins, d.plugins);
    }
}

