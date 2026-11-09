//! Built-in plugin knowledge for the `PLUGINS` config: names, kinds, and
//! per-plugin `params` schemas, used by `validate`, the wizard's plugin
//! editor, and the web UI's plugin forms.
//!
//! Mirrors the `lpcache-plugins` built-ins — keep in sync with
//! `plugins/src/builtin/` and PLUGINS.md §3. Every check here mirrors what
//! the plugin factories accept at lpcache startup, so `lpcache-config
//! validate` catches exactly the specs that would stop the proxy from
//! starting (or that quietly do nothing).

use serde_json::{Map, Value};

/// The kind of one JSON param, driving validation and prompt/control shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FieldKind {
    /// Free text (JSON string).
    Text,
    /// Free text that should render masked (API keys).
    Secret,
    /// String that must start with `http://` or `https://`.
    Url,
    /// Integer number (JSON strings are rejected — serde would fail).
    Int,
    /// Any number.
    Float,
    /// Boolean.
    Bool,
    /// String that must equal one of `choices` exactly.
    Enum,
    /// Array of strings (or, when `comma_ok`, a comma-separated string);
    /// items must be in `choices` when it is non-empty (exact match).
    StrList,
    /// Like `StrList` but matched case-insensitively against `choices`
    /// plus `extra_ok` aliases.
    CiList,
    /// Array of nested plugin specs (`chain.filters`) — validated recursively.
    Nested,
}

/// One param of a plugin's `params` object (schema + prompt metadata).
#[derive(Debug, Clone, Copy)]
pub struct Param {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: FieldKind,
    /// Runtime refuses to start without it (missing **or** empty).
    pub required: bool,
    /// The `StrList`/`CiList` forms also accept a comma-separated string.
    pub comma_ok: bool,
    /// Human-readable default (`""` = none) shown by the prompts.
    pub default: &'static str,
    pub help: &'static str,
    /// Accepted values for `Enum`/`StrList`/`CiList` (empty = free).
    pub choices: &'static [&'static str],
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Extra accepted spellings for `CiList` (category aliases the plugin's
    /// own parser accepts, e.g. `PII` ↔ `personally identifiable information`).
    pub extra_ok: Option<fn(&str) -> bool>,
}

/// One built-in plugin: registry name + adapter kind + param schema.
#[derive(Debug, Clone, Copy)]
pub struct Plugin {
    pub name: &'static str,
    /// `prefilter`, `prefilter+store`, `backend`, `logger`, `dashboard`.
    pub kind: &'static str,
    pub summary: &'static str,
    /// Offered by the add-plugin picker (false for the auto `llama`).
    pub selectable: bool,
    /// The runtime deserializes this plugin with `deny_unknown_fields`.
    pub deny_unknown_params: bool,
    pub params: &'static [Param],
}

/// Safety levels emitted by Qwen3Guard-Gen (`SafetyLevel::from_name`).
pub const LEVELS: &[&str] = &["Safe", "Controversial", "Unsafe"];

/// Harm categories emitted by Qwen3Guard-Gen (`GuardCategory::ALL`).
pub const CATEGORIES: &[&str] = &[
    "Violent",
    "Non-violent Illegal Acts",
    "Sexual Content or Sexual Acts",
    "PII",
    "Suicide & Self-Harm",
    "Unethical Acts",
    "Politically Sensitive Topics",
    "Copyright Violation",
    "Jailbreak",
];

/// The extra spellings `GuardCategory::from_name` accepts besides the
/// canonical names (lowercase compare).
fn category_alias_ok(raw: &str) -> bool {
    matches!(
        raw.trim().to_lowercase().as_str(),
        "nonviolent illegal acts"
            | "personally identifiable information"
            | "suicide & self harm"
            | "suicide and self-harm"
            | "politically sensitive"
    )
}

/// The 28 W3C-XLF field names the request log accepts (`KNOWN_FIELDS`).
pub const XLF_FIELDS: &[&str] = &[
    "datetime",
    "cs-method",
    "cs-uri",
    "c-ip",
    "cs(User-Agent)",
    "cs-bytes",
    "cs-time",
    "sc-status",
    "sc-bytes",
    "lpcache-model",
    "lpcache-stream",
    "lpcache-key",
    "lpcache-n-words",
    "lpcache-is-big",
    "lpcache-backend",
    "lpcache-slot",
    "lpcache-restored",
    "lpcache-cache-hit",
    "lpcache-coalesced",
    "lpcache-reason",
    "lpcache-prompt-tokens",
    "lpcache-completion-tokens",
    "lpcache-cached-tokens",
    "lpcache-prefill-ms",
    "lpcache-gen-ms",
    "lpcache-prefill-tps",
    "lpcache-gen-tps",
    "lpcache-ttft-ms",
];

const FAIL_ACTIONS: &[&str] = &["accept", "reject", "allow", "block"];

// ---------------------------------------------------------------------------
// Schema table (built once behind an OnceLock; chained builders keep it short)
// ---------------------------------------------------------------------------

fn p(key: &'static str, label: &'static str, kind: FieldKind) -> Param {
    Param {
        key,
        label,
        kind,
        required: false,
        comma_ok: false,
        default: "",
        help: "",
        choices: &[],
        min: None,
        max: None,
        extra_ok: None,
    }
}

impl Param {
    const fn req(mut self) -> Self { self.required = true; self }
    const fn comma(mut self) -> Self { self.comma_ok = true; self }
    const fn def(mut self, d: &'static str) -> Self { self.default = d; self }
    const fn help(mut self, h: &'static str) -> Self { self.help = h; self }
    const fn ch(mut self, c: &'static [&'static str]) -> Self { self.choices = c; self }
    const fn ge(mut self, m: f64) -> Self { self.min = Some(m); self }
    const fn le(mut self, m: f64) -> Self { self.max = Some(m); self }
    const fn alias(mut self, f: fn(&str) -> bool) -> Self { self.extra_ok = Some(f); self }
}

fn plugin(
    name: &'static str,
    kind: &'static str,
    summary: &'static str,
    params: Vec<Param>,
) -> BuiltPlugin {
    BuiltPlugin {
        spec: Plugin {
            name,
            kind,
            summary,
            selectable: true,
            deny_unknown_params: false,
            params: Box::leak(params.into_boxed_slice()),
        },
    }
}

struct BuiltPlugin {
    spec: Plugin,
}

impl BuiltPlugin {
    fn no_pick(mut self) -> Self {
        self.spec.selectable = false;
        self
    }
    fn deny(mut self) -> Self {
        self.spec.deny_unknown_params = true;
        self
    }
    fn build(self) -> Plugin {
        self.spec
    }
}

fn build() -> Vec<Plugin> {
    use FieldKind::*;
    vec![
        plugin(
            "keyword",
            "prefilter",
            "reject requests whose messages contain a blocked keyword",
            vec![
                p("keywords", "keywords", StrList)
                    .req()
                    .comma()
                    .help("blocked keywords (JSON array or comma-separated string)"),
                p("case_insensitive", "case_insensitive", Bool)
                    .def("true")
                    .help("match keywords ignoring case"),
            ],
        )
        .build(),
        plugin(
            "qwen_guard",
            "prefilter",
            "content-safety check via a Qwen3Guard-Gen model on a separate server",
            vec![
                p("url", "url", Url)
                    .req()
                    .help("guard server base URL — POSTs to {url}/v1/chat/completions"),
                p("api_key", "api_key", Secret)
                    .help("sent as Authorization: Bearer <key>"),
                p("model", "model", Text)
                    .help("model id in the chat-completions request (server default when unset)"),
                p("timeout_secs", "timeout_secs", Int)
                    .def("30")
                    .ge(0.0)
                    .help("per-check timeout in seconds"),
                p("max_input_chars", "max_input_chars", Int)
                    .def("0")
                    .ge(0.0)
                    .help("cap on the text sent to the guard (0 = no truncation; guard models have small contexts)"),
                p("reject_levels", "reject_levels", CiList)
                    .comma()
                    .def("Unsafe")
                    .ch(LEVELS)
                    .help("safety levels that reject outright"),
                p("reject_categories", "reject_categories", CiList)
                    .comma()
                    .ch(CATEGORIES)
                    .alias(category_alias_ok)
                    .help("categories that reject when present (independent of the level)"),
                p("on_error", "on_error", Enum)
                    .def("accept")
                    .ch(FAIL_ACTIONS)
                    .help("what to do when the guard call fails (allow/block are aliases)"),
                p("on_unparseable", "on_unparseable", Enum)
                    .def("accept")
                    .ch(FAIL_ACTIONS)
                    .help("what to do when the verdict is unparseable"),
                p("cache_max", "cache_max", Int)
                    .def("0")
                    .ge(0.0)
                    .help("LRU size for verdicts keyed by the request cache key (0 = off)"),
            ],
        )
        .build(),
        plugin(
            "result_cache",
            "prefilter+store",
            "serve fresh same-key requests from a disk cache; store fresh results",
            vec![
                p("dir", "dir", Text)
                    .req()
                    .help("cache-result dir holding the {key}.json entries (created on first write)"),
                p("ttl_secs", "ttl_secs", Float)
                    .def("300")
                    .ge(0.0)
                    .help("per-entry expiry in seconds (0 = never expire)"),
                p("cleanup", "cleanup", Enum)
                    .ch(&["remove_after_serve"])
                    .help("remove_after_serve deletes an entry once served (omitted/null = none)"),
                p("cache_streaming", "cache_streaming", Bool)
                    .def("false")
                    .help("also cache streaming results and serve cached entries to streaming clients"),
                p("cache_control", "cache_control", Bool)
                    .def("false")
                    .help("honor a Cache-Control: no-cache request header (bypass the cache)"),
            ],
        )
        .build(),
        plugin(
            "chain",
            "prefilter",
            "run nested prefilter plugins in order",
            vec![p("filters", "filters", Nested)
                .req()
                .help("nested prefilter plugin specs, evaluated in order")],
        )
        .build(),
        plugin(
            "request_log",
            "logger",
            "W3C-XLF request log to a size-rotated file",
            vec![
                p("path", "path", Text)
                    .def("lpcache-request.log")
                    .help("log file location (relative paths resolve against the proxy's cwd)"),
                p("max_bytes", "max_bytes", Int)
                    .def("10485760")
                    .ge(0.0)
                    .help("rotation threshold in bytes (0 = no rotation)"),
                p("keep", "keep", Int)
                    .def("5")
                    .ge(0.0)
                    .help("rotated generations kept (0 = keep all)"),
                p("fields", "fields", StrList)
                    .def("(all 28)")
                    .ch(XLF_FIELDS)
                    .help("ordered subset of the known XLF field names (empty = all)"),
            ],
        )
        .build(),
        plugin(
            "dashboard",
            "dashboard",
            "real-time statistics web page + JSON stats endpoint",
            vec![
                p("path", "path", Text)
                    .def("/dashboard")
                    .help("page URL path (must start with /); stats at {path}/api/stats"),
                p("refresh_ms", "refresh_ms", Int)
                    .def("2000")
                    .ge(100.0)
                    .le(3_600_000.0)
                    .help("page polling interval in ms"),
                p("window_secs", "window_secs", Int)
                    .def("300")
                    .ge(10.0)
                    .le(86_400.0)
                    .help("trailing stats window in seconds"),
                p("bucket_secs", "bucket_secs", Int)
                    .def("5")
                    .ge(1.0)
                    .le(3_600.0)
                    .help("time-series bucket size in seconds"),
                p("max_recent", "max_recent", Int)
                    .def("100")
                    .ge(0.0)
                    .le(10_000.0)
                    .help("recent-requests ring size (0 = no recent list)"),
            ],
        )
        .deny()
        .build(),
        plugin(
            "llama",
            "backend",
            "the llama.cpp backend client (created automatically per backend)",
            vec![
                p("url", "url", Url).req().help("the backend base URL"),
                p("api_key", "api_key", Secret)
                    .help("sent as Authorization: Bearer <key>"),
                p("timeout_secs", "timeout_secs", Float)
                    .def("600")
                    .ge(0.0)
                    .help("per-request timeout in seconds"),
            ],
        )
        .no_pick()
        .build(),
    ]
}

/// All built-in plugin schemas (registry order: prefilter kinds first).
pub fn plugins() -> &'static [Plugin] {
    static CELL: std::sync::OnceLock<Vec<Plugin>> = std::sync::OnceLock::new();
    CELL.get_or_init(build)
}

/// The built-in plugin with this registry name, if any.
pub fn find(name: &str) -> Option<&'static Plugin> {
    plugins().iter().find(|p| p.name == name)
}

/// Prefilter-kind plugins (offered inside `chain.filters`).
pub fn prefilter_plugins() -> Vec<&'static Plugin> {
    plugins()
        .iter()
        .filter(|p| p.selectable && p.kind.starts_with("prefilter"))
        .collect()
}


/// Case-insensitive Levenshtein distance (short strings only — names).
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The closest built-in name for a likely typo (`qwen3_guard` → qwen_guard).
pub fn did_you_mean(name: &str) -> Option<&'static str> {
    plugins()
        .iter()
        .map(|plg| (distance(name, plg.name), plg.name))
        .filter(|(d, _)| *d <= 2)
        .min()
        .map(|(_, n)| n)
}

/// A one-line summary of one parsed spec for menus:
/// `qwen_guard — http://guard:8080`, `keyword — 3 item(s)`, ...
pub fn spec_summary(v: &Value) -> String {
    let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("?");
    let Some(plg) = find(name) else {
        return format!("{name} (unknown plugin)");
    };
    let params = v.get("params").and_then(Value::as_object);
    let sig = |key: &str| {
        params.and_then(|m| m.get(key)).map(|v| match v {
            Value::String(s) if !s.is_empty() => s.clone(),
            Value::Array(items) => format!("{} item(s)", items.len()),
            _ => "(unset)".to_string(),
        })
    };
    let detail = match name {
        "keyword" => sig("keywords").unwrap_or_else(|| "no keywords".into()),
        "chain" => match params.and_then(|m| m.get("filters")).and_then(Value::as_array) {
            Some(items) => format!("{} nested filter(s)", items.len()),
            None => "no filters".into(),
        },
        "qwen_guard" => sig("url").unwrap_or_else(|| "no url".into()),
        "result_cache" => sig("dir").unwrap_or_else(|| "no dir".into()),
        "request_log" => sig("path").unwrap_or_else(|| "default path".into()),
        "dashboard" => sig("path").unwrap_or_else(|| "/dashboard".into()),
        "llama" => sig("url").unwrap_or_else(|| "auto per backend".into()),
        _ => plg.kind.to_string(),
    };
    format!("{name} — {detail}")
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate the raw `PLUGINS` JSON (a JSON array of plugin specs), mirroring
/// what the plugin factories accept at lpcache startup. Blank input is the
/// unset default and valid. Returns `(errors, warnings)`.
pub fn validate_plugins(raw: &str) -> (Vec<String>, Vec<String>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    if raw.trim().is_empty() {
        return (errors, warnings);
    }
    let arr: Vec<Value> = match serde_json::from_str(raw.trim()) {
        Ok(a) => a,
        Err(e) => {
            errors.push(format!("plugins.json is not a valid JSON array: {e}"));
            return (errors, warnings);
        }
    };
    for (i, v) in arr.iter().enumerate() {
        validate_spec(v, &format!("plugins.json[{i}]"), 0, &mut errors, &mut warnings);
    }
    // request_log / dashboard are singletons — a second spec is a startup
    // error ("only one … plugin is supported").
    let count = |name: &str| {
        arr.iter()
            .filter(|v| v.get("name").and_then(|n| n.as_str()) == Some(name))
            .count()
    };
    for (name, n) in [
        ("request_log", count("request_log")),
        ("dashboard", count("dashboard")),
    ] {
        if n > 1 {
            errors.push(format!(
                "plugins.json: {n} {name:?} specs — only one {name} plugin is supported"
            ));
        }
    }
    (errors, warnings)
}

fn names_joined() -> String {
    plugins()
        .iter()
        .filter(|p| p.selectable)
        .map(|p| p.name)
        .collect::<Vec<_>>()
        .join(", ")
}

fn validate_spec(
    v: &Value,
    at: &str,
    depth: usize,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    if depth > 8 {
        errors.push(format!("{at}: nested filters too deep (possible loop)"));
        return;
    }
    let Some(obj) = v.as_object() else {
        errors.push(format!(r#"{at}: must be an object with a "name" string"#));
        return;
    };
    let name = obj.get("name").and_then(|n| n.as_str()).unwrap_or("");
    if name.is_empty() {
        errors.push(format!(r#"{at}: missing "name""#));
        return;
    }
    let Some(plg) = find(name) else {
        let mut msg = format!("{at}: unknown plugin {name:?} (built-ins: {})", names_joined());
        if let Some(sug) = did_you_mean(name) {
            msg += &format!(", did you mean {sug:?}?");
        }
        errors.push(msg);
        return;
    };
    if !plg.selectable {
        warnings.push(format!(
            r#"{at}: backend clients are created automatically per [[backend]] — a {name:?} spec in PLUGINS has no effect"#
        ));
        return;
    }
    let params: Option<&Map<String, Value>> = match obj.get("params") {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => Some(m),
        Some(_) => {
            errors.push(format!(r#"{at} ({name}): "params" must be a JSON object"#));
            None
        }
    };
    for par in plg.params {
        match params.and_then(|m| m.get(par.key)) {
            None | Some(Value::Null) if par.required => {
                errors.push(format!(
                    r#"{at} ({name}): missing required param {:?} — {}"#,
                    par.key, par.help
                ));
            }
            None | Some(Value::Null) => {}
            Some(pv) => check_param(par, pv, at, name, depth, errors, warnings),
        }
    }
    if let Some(m) = params {
        let unknown: Vec<&str> = m
            .keys()
            .map(String::as_str)
            .filter(|k| !plg.params.iter().any(|p| &p.key == k))
            .collect();
        if !unknown.is_empty() {
            let known = plg.params.iter().map(|p| p.key).collect::<Vec<_>>().join(", ");
            let msg = format!(
                "{at} ({name}): unknown param(s): {} — known: {known}",
                unknown.join(", ")
            );
            if plg.deny_unknown_params {
                errors.push(msg);
            } else {
                warnings.push(msg);
            }
        }
    }
    if name == "dashboard"
        && let Some(Value::String(path)) = params.and_then(|m| m.get("path"))
        && !path.starts_with('/')
    {
        errors.push(format!(r#"{at} (dashboard): path {path:?} must start with "/""#));
    }
}




fn type_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Type/range/membership check for one present param value.
fn check_param(
    par: &Param,
    v: &Value,
    at: &str,
    name: &str,
    depth: usize,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let mut bad = |m: String| errors.push(format!(r#"{at} ({name}): param {:?}: {m}"#, par.key));
    match par.kind {
        FieldKind::Text | FieldKind::Secret => {
            if !v.is_string() {
                bad(format!("expected a string, got {}", type_of(v)));
            }
        }
        FieldKind::Url => match v.as_str() {
            Some(s) if s.starts_with("http://") || s.starts_with("https://") => {}
            Some(_) => bad("must start with http:// or https://".into()),
            None => bad(format!("expected a string, got {}", type_of(v))),
        },
        FieldKind::Int | FieldKind::Float => {
            let Some(f) = v.as_f64() else {
                bad(format!("expected a number, got {} (numbers are not quoted)", type_of(v)));
                return;
            };
            if par.kind == FieldKind::Int && f.fract() != 0.0 {
                bad(format!("must be a whole number (got {f})"));
            }
            if let Some(min) = par.min && f < min {
                bad(format!("must be >= {min} (got {f})"));
            }
            if let Some(max) = par.max && f > max {
                bad(format!("must be <= {max} (got {f})"));
            }
        }
        FieldKind::Bool => {
            if !v.is_boolean() {
                bad(format!("expected true or false, got {}", type_of(v)));
            }
        }
        FieldKind::Enum => {
            if !v.as_str().is_some_and(|s| par.choices.contains(&s)) {
                bad(format!(
                    "must be one of: {} (got {})",
                    par.choices.join(", "),
                    type_of(v)
                ));
            }
        }
        FieldKind::StrList | FieldKind::CiList => {
            let items: Option<Vec<&str>> = match v {
                Value::Array(items) => items
                    .iter()
                    .map(|i| i.as_str())
                    .collect::<Option<Vec<_>>>()
                    .or_else(|| {
                        bad("list items must be strings".into());
                        None
                    }),
                Value::String(s) if par.comma_ok => {
                    Some(s.split(',').map(str::trim).filter(|s| !s.is_empty()).collect())
                }
                other => {
                    bad(format!("expected an array of strings, got {}", type_of(other)));
                    None
                }
            };
            let Some(items) = items else { return };
            if par.required && items.is_empty() {
                bad("must be a non-empty list".into());
            }
            if !par.choices.is_empty() {
                for item in items {
                    let hit = if par.kind == FieldKind::CiList {
                        par.choices.iter().any(|c| c.eq_ignore_ascii_case(item))
                            || par.extra_ok.is_some_and(|f| f(item))
                    } else {
                        par.choices.contains(&item)
                    };
                    if !hit {
                        bad(format!("unknown value {item:?}; known: {}", par.choices.join(", ")));
                    }
                }
            }
        }
        FieldKind::Nested => {
            let Some(items) = v.as_array() else {
                bad(format!("expected an array of plugin specs, got {}", type_of(v)));
                return;
            };
            if items.is_empty() {
                warnings.push(format!(
                    r#"{at} ({name}): "filters" is empty — the chain passes every request through"#
                ));
            }
            for (j, item) in items.iter().enumerate() {
                validate_spec(item, &format!("{at}.{}[{j}]", par.key), depth + 1, errors, warnings);
            }
        }
    }
}

/// The schema table as JSON, served to the web UI so its plugin forms are
/// generated from the same source as the validation.
pub fn schemas_json() -> Value {
    let kind_tag = |k: FieldKind| match k {
        FieldKind::Text => "text",
        FieldKind::Secret => "secret",
        FieldKind::Url => "url",
        FieldKind::Int => "int",
        FieldKind::Float => "float",
        FieldKind::Bool => "bool",
        FieldKind::Enum => "enum",
        FieldKind::StrList => "list",
        FieldKind::CiList => "cilist",
        FieldKind::Nested => "nested",
    };
    let arr: Vec<Value> = plugins()
        .iter()
        .map(|p| {
            let params: Vec<Value> = p
                .params
                .iter()
                .map(|par| {
                    serde_json::json!({
                        "key": par.key,
                        "label": par.label,
                        "kind": kind_tag(par.kind),
                        "required": par.required,
                        "commaOk": par.comma_ok,
                        "default": par.default,
                        "help": par.help,
                        "choices": par.choices,
                        "min": par.min,
                        "max": par.max,
                        // Nested lists (chain.filters) only take prefilter kinds.
                        "accepts": if par.kind == FieldKind::Nested {
                            "prefilter"
                        } else {
                            ""
                        },
                    })
                })
                .collect();
            serde_json::json!({
                "name": p.name,
                "kind": p.kind,
                "summary": p.summary,
                "selectable": p.selectable,
                "unique": p.unique,
                "params": params,
            })
        })
        .collect();
    Value::Array(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errs(json: &str) -> Vec<String> {
        validate_plugins(json).0
    }

    #[test]
    fn registry_names_are_known() {
        let names: Vec<&str> = plugins().iter().map(|p| p.name).collect();
        assert_eq!(
            names,
            vec![
                "keyword",
                "qwen_guard",
                "result_cache",
                "chain",
                "request_log",
                "dashboard",
                "llama"
            ]
        );
    }

    #[test]
    fn unset_and_valid_specs_pass() {
        assert!(validate_plugins("").0.is_empty());
        assert!(validate_plugins("   ").0.is_empty());
        let ok = r#"[
          {"name":"keyword","params":{"keywords":["a","b"],"case_insensitive":true}},
          {"name":"qwen_guard","params":{"url":"http://guard:8080","api_key":"k",
             "model":"Qwen3Guard-Gen-8B","timeout_secs":30,"max_input_chars":1500,
             "reject_levels":["Unsafe","Controversial"],"reject_categories":["Jailbreak","PII"],
             "on_error":"reject","on_unparseable":"accept","cache_max":256}},
          {"name":"dashboard","params":{"path":"/dash","refresh_ms":1000,"window_secs":600,
             "bucket_secs":10,"max_recent":50}},
          {"name":"chain","params":{"filters":[{"name":"keyword","params":{"keywords":"x, y"}}]}}
        ]"#;
        let (errors, warnings) = validate_plugins(ok);
        assert!(errors.is_empty(), "valid config flagged: {errors:?}");
        assert!(warnings.is_empty(), "valid config warned: {warnings:?}");
    }

    #[test]
    fn typo_gets_did_you_mean() {
        let e = errs(r#"[{"name":"qwen3_guard","params":{"url":"http://g:8080"}}]"#);
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(e[0].contains("unknown plugin"), "{}", e[0]);
        assert!(e[0].contains("did you mean \"qwen_guard\""), "{}", e[0]);
        assert_eq!(did_you_mean("qwen3_guard"), Some("qwen_guard"));
        assert_eq!(did_you_mean("dashboard2"), Some("dashboard"));
        assert_eq!(did_you_mean("completely_made_up_name"), None);
    }

    #[test]
    fn required_params_are_enforced() {
        let e = errs(r#"[{"name":"qwen_guard","params":{"timeout_secs":"thirty"}}]"#);
        assert!(e.iter().any(|x| x.contains("missing required param \"url\"")), "{e:?}");
        assert!(e.iter().any(|x| x.contains("timeout_secs") && x.contains("expected a number")), "{e:?}");
        let e = errs(r#"[{"name":"result_cache"}]"#);
        assert!(e.iter().any(|x| x.contains("missing required param \"dir\"")), "{e:?}");
        let e = errs(r#"[{"name":"keyword","params":{"keywords":[]}}]"#);
        assert!(e.iter().any(|x| x.contains("non-empty list")), "{e:?}");
        let e = errs(r#"[{"name":"chain"}]"#);
        assert!(e.iter().any(|x| x.contains("missing required param \"filters\"")), "{e:?}");
    }

    #[test]
    fn chain_filters_validate_recursively() {
        let e = errs(r#"[{"name":"chain","params":{"filters":["not-a-spec",{}]}}]"#);
        assert!(e.iter().any(|x| x.contains("filters[0]") && x.contains("must be an object")), "{e:?}");
        assert!(e.iter().any(|x| x.contains("filters[1]") && x.contains("missing \"name\"")), "{e:?}");
        let e = errs(r#"[{"name":"chain","params":{"filters":[{"name":"chain","params":{"filters":"nope"}}]}}]"#);
        assert!(
            e.iter()
                .any(|x| x.contains("plugins.json[0].filters[0]") && x.contains("array of plugin specs")),
            "{e:?}"
        );
        let (_, w) = validate_plugins(r#"[{"name":"chain","params":{"filters":[]}}]"#);
        assert!(w.iter().any(|x| x.contains("passes every request")), "{w:?}");
    }

    #[test]
    fn llama_is_noop_and_singletons_are_unique() {
        let (e, w) = validate_plugins(r#"[{"name":"llama","params":{"url":"http://b:1"}}]"#);
        assert!(e.is_empty(), "{e:?}");
        assert!(w.iter().any(|x| x.contains("no effect")), "{w:?}");
        let e = errs(r#"[{"name":"request_log"},{"name":"request_log"}]"#);
        assert!(e.iter().any(|x| x.contains("only one request_log")), "{e:?}");
        let e = errs(r#"[{"name":"dashboard"},{"name":"dashboard"}]"#);
        assert!(e.iter().any(|x| x.contains("only one dashboard")), "{e:?}");
    }

    #[test]
    fn value_semantics() {
        // categories/levels: case-insensitive, aliases accepted, unknown rejected
        let (e, _) = validate_plugins(
            r#"[{"name":"qwen_guard","params":{"url":"http://g","reject_levels":["unsafe"],
               "reject_categories":["JAILBREAK","personally identifiable information"]}}]"#,
        );
        assert!(e.is_empty(), "{e:?}");
        let e = errs(r#"[{"name":"qwen_guard","params":{"url":"http://g","reject_levels":["spicy"],"on_error":"shrug"}}]"#);
        assert!(e.iter().any(|x| x.contains("unknown value \"spicy\"")), "{e:?}");
        assert!(e.iter().any(|x| x.contains("on_error") && x.contains("must be one of")), "{e:?}");
        // fields are exact and array-only (runtime errors on unknown)
        let e = errs(r#"[{"name":"request_log","params":{"fields":["datetime","nope"]}}]"#);
        assert!(e.iter().any(|x| x.contains("unknown value \"nope\"")), "{e:?}");
        let e = errs(r#"[{"name":"request_log","params":{"fields":"datetime"}}]"#);
        assert!(e.iter().any(|x| x.contains("array of strings")), "{e:?}");
        // dashboard denies unknown params; path needs a leading slash; ranges enforced
        let e = errs(r#"[{"name":"dashboard","params":{"colour":"red","path":"dash","refresh_ms":5}}]"#);
        assert!(e.iter().any(|x| x.contains("unknown param(s): colour")), "{e:?}");
        assert!(e.iter().any(|x| x.contains("must start with \"/\"")), "{e:?}");
        assert!(e.iter().any(|x| x.contains("refresh_ms") && x.contains("must be >= 100")), "{e:?}");
        // other plugins only warn on unknown params
        let (_, w) = validate_plugins(r#"[{"name":"keyword","params":{"keywords":["a"],"colour":"red"}}]"#);
        assert!(w.iter().any(|x| x.contains("unknown param(s): colour")), "{w:?}");
        // params must be an object
        let e = errs(r#"[{"name":"keyword","params":"a,b"}]"#);
        assert!(e.iter().any(|x| x.contains("\"params\" must be a JSON object")), "{e:?}");
    }

    #[test]
    fn summaries_and_schemas_json() {
        let v: Value = serde_json::from_str(
            r#"{"name":"qwen_guard","params":{"url":"http://guard:8080"}}"#,
        )
        .unwrap();
        assert_eq!(spec_summary(&v), "qwen_guard — http://guard:8080");
        let v: Value = serde_json::from_str(r#"{"name":"mystery"}"#).unwrap();
        assert!(spec_summary(&v).contains("unknown plugin"));
        let s = schemas_json();
        let guard = s
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "qwen_guard")
            .unwrap();
        assert_eq!(guard["kind"], "prefilter");
        let url = guard["params"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["key"] == "url")
            .unwrap();
        assert_eq!(url["required"], true);
        assert_eq!(url["kind"], "url");
    }
}
