// Guided terminal menu (`lpcache-config configure`): walk through every
// section with validated prompts, then review and save.
//
// Scalar fields are driven through the model's own `get_key` / `set_key`
// accessors, so the wizard validates exactly like `set` does (same parsers,
// same messages) and can never drift from the key list. Backends get a
// dedicated add / edit / remove submenu since they are array entries rather
// than dot-keys.

use crate::model::{self, BackendEntry, LpcacheConfig};
use crate::plugin_schema::{self, FieldKind, Param, Plugin};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};
use serde_json::Value;
use std::path::Path;

/// Menu entries shown on every loop iteration.
const SECTIONS: [&str; 6] = [
    "proxy",
    "backends",
    "kv_cache",
    "prefilter",
    "request_log",
    "plugins",
];

/// Keys where an empty value is legitimate (disable / inherit semantics).
fn empty_allowed(key: &str) -> bool {
    matches!(
        key,
        "proxy.api_key"
            | "kv_cache.slot_save_path"
            | "prefilter.blocklist"
            | "prefilter.result_cache_dir"
            | "request_log.fields"
            | "plugins.json"
    )
}

/// Dot-keys belonging to one scalar section, in model order.
fn section_keys(prefix: &str) -> Vec<&'static str> {
    let dot = format!("{prefix}.");
    LpcacheConfig::keys()
        .iter()
        .copied()
        .filter(|k| k.starts_with(&dot))
        .collect()
}

fn theme() -> ColorfulTheme {
    ColorfulTheme::default()
}

/// Map a dialoguer error (Ctrl-C, Esc, closed terminal) to a clean abort.
fn aborted<E: std::fmt::Display>(e: E) -> String {
    format!("wizard aborted — nothing was saved ({e})")
}

/// One free-text input; `Some(default)` lets Return keep the current value.
/// Rebuilt per attempt because dialoguer consumes the prompt on interact.
fn ask_text(
    t: &ColorfulTheme,
    label: String,
    default: Option<String>,
    allow_empty: bool,
) -> Result<String, String> {
    let mut input = Input::<String>::with_theme(t)
        .with_prompt(label)
        .allow_empty(allow_empty);
    if let Some(d) = default {
        input = input.default(d);
    }
    input.interact_text().map_err(aborted)
}

/// Entry point for the `configure` subcommand.
pub fn run(path: &Path) -> Result<i32, String> {
    let exists = path.exists();
    let mut cfg = if exists {
        model::load(path)?
    } else {
        LpcacheConfig::default()
    };
    println!(
        "lpcache-config guided setup — {} ({})",
        path.display(),
        if exists { "loaded" } else { "new file — starting from built-in defaults" }
    );
    println!("Return keeps the shown default · Esc goes back · Ctrl-C quits.\n");

    let mut dirty = false;
    loop {
        let items = menu_items(&cfg, dirty);
        let sel = Select::with_theme(&theme())
            .with_prompt("What would you like to configure?")
            .items(&items)
            .default(0)
            .interact_opt()
            .map_err(aborted)?;
        let Some(sel) = sel else { break }; // Esc on the root menu = quit
        match sel {
            0..=5 => {
                let prefix = SECTIONS[sel];
                let changed = match prefix {
                    "backends" => backends_menu(&mut cfg)?,
                    "plugins" => plugins_menu(&mut cfg)?,
                    _ => edit_section(&mut cfg, prefix)?,
                };
                dirty |= changed;
            }
            6 => review(&cfg, dirty),
            7 => {
                if save(path, &cfg)? {
                    return Ok(0);
                }
                // Blocked (errors) or declined — back to the menu.
            }
            _ => {
                if !dirty
                    || Confirm::with_theme(&theme())
                        .with_prompt("You have unsaved changes — quit without saving?")
                        .default(false)
                        .interact()
                        .map_err(aborted)?
                {
                    println!("quit — nothing saved.");
                    return Ok(0);
                }
            }
        }
    }
    println!("quit — nothing saved.");
    Ok(0)
}

/// Build the labeled menu items, each with a one-line hint of its state.
fn menu_items(cfg: &LpcacheConfig, dirty: bool) -> Vec<String> {
    let mark = if dirty { "*" } else { " " };
    let be = match cfg.backends.len() {
        0 => "no backends!".to_string(),
        1 => format!("1 backend: {}", cfg.backends[0].url),
        n => format!("{n} backends"),
    };
    vec![
        format!(
            "{mark} proxy         (port {}, model {:?})",
            cfg.proxy.port, cfg.proxy.model_id
        ),
        format!("{mark} backends      ({be})"),
        format!(
            "{mark} kv_cache      (lcp_th {}, block {})",
            cfg.kv_cache.lcp_th, cfg.kv_cache.words_per_block
        ),
        format!(
            "{mark} prefilter     ({} blocklisted terms)",
            cfg.prefilter.blocklist.len()
        ),
        format!(
            "{mark} request_log   ({})",
            if cfg.request_log.enabled { "enabled" } else { "disabled" }
        ),
        format!("{mark} plugins       ({})", plugins_state(cfg)),
        "  review & validate".to_string(),
        "  save & exit".to_string(),
        "  quit without saving".to_string(),
    ]
}

/// Prompt every scalar key of one section; report whether anything changed.
fn edit_section(cfg: &mut LpcacheConfig, prefix: &str) -> Result<bool, String> {
    println!("\n── {prefix} ──");
    let mut changed = false;
    for key in section_keys(prefix) {
        changed |= prompt_key(cfg, key)?;
    }
    println!();
    Ok(changed)
}

/// Prompt one dot-key: Confirm for booleans, Select for log level, free
/// text (validated through `set_key`) for everything else.
fn prompt_key(cfg: &mut LpcacheConfig, key: &str) -> Result<bool, String> {
    let cur = cfg
        .get_key(key)
        .ok_or_else(|| format!("wizard: unknown key {key}"))?;
    let next = if key == "proxy.log_level" {
        let levels = ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"];
        let def = levels
            .iter()
            .position(|l| l.eq_ignore_ascii_case(&cur))
            .unwrap_or(2);
        match Select::with_theme(&theme())
            .with_prompt(key)
            .items(&levels)
            .default(def)
            .interact_opt()
            .map_err(aborted)?
        {
            Some(i) => levels[i].to_string(),
            None => return Ok(false), // Esc keeps the current value
        }
    } else if cur == "true" || cur == "false" {
        Confirm::with_theme(&theme())
            .with_prompt(key)
            .default(cur == "true")
            .interact()
            .map_err(aborted)?
            .to_string()
    } else {
        let t = theme();
        let label = if key.ends_with("blocklist") || key.ends_with(".fields") {
            format!("{key} (comma-separated)")
        } else {
            key.to_string()
        };
        let allow = empty_allowed(key);
        let def = (!cur.is_empty() || allow).then(|| cur.clone());
        loop {
            let v = ask_text(&t, label.clone(), def.clone(), allow)?;
            match cfg.set_key(key, &v) {
                Ok(()) => break v,
                Err(e) => println!("  ! {e} — try again"),
            }
        }
    };
    cfg.set_key(key, &next).map_err(|e| format!("wizard: {e}"))?;
    Ok(next != cur)
}

/// Show one backend the way the menu lists it.
fn format_backend(i: usize, b: &BackendEntry) -> String {
    let save = if b.slot_save_path.trim().is_empty() {
        "save:none"
    } else {
        b.slot_save_path.as_str()
    };
    format!(
        "edit {}: {} — {} slot(s), kv {}, {save}",
        i + 1,
        b.url,
        b.n_slots,
        if b.kv_cache { "on" } else { "off" }
    )
}

/// Add or edit one backend; the entry passed in carries the defaults.
fn prompt_backend(be: &BackendEntry) -> Result<BackendEntry, String> {
    let url = prompt_text(
        "backend url (e.g. http://127.0.0.1:8000)",
        &be.url,
        false,
        |v| {
            if v.trim().is_empty() {
                Err("url must not be empty".to_string())
            } else {
                Ok(())
            }
        },
    )?;
    let slots = prompt_text(
        "n_slots — must match the backend's -np/--parallel",
        &be.n_slots.to_string(),
        false,
        |v| {
            v.trim()
                .parse::<usize>()
                .map(|_| ())
                .map_err(|_| "not a non-negative integer".to_string())
        },
    )?;
    let save = prompt_text(
        "slot_save_path — the backend's --slot-save-path (empty = none)",
        &be.slot_save_path,
        true,
        |_| Ok(()),
    )?;
    let kv = Confirm::with_theme(&theme())
        .with_prompt("supports KV cache save/restore (llama-server >= b1.05.40)")
        .default(be.kv_cache)
        .interact()
        .map_err(aborted)?;
    Ok(BackendEntry {
        url: url.trim().trim_end_matches('/').to_string(),
        n_slots: slots
            .trim()
            .parse::<usize>()
            .map_err(|_| "wizard: bad slot count".to_string())?,
        slot_save_path: save,
        kv_cache: kv,
    })
}

/// Free-text prompt with an extra validation closure; re-prompts on failure.
fn prompt_text(
    label: &str,
    cur: &str,
    allow_empty: bool,
    validate: impl Fn(&str) -> Result<(), String>,
) -> Result<String, String> {
    let t = theme();
    let def = (!cur.is_empty() || allow_empty).then(|| cur.to_string());
    loop {
        let v = ask_text(&t, label.to_string(), def.clone(), allow_empty)?;
        match validate(&v) {
            Ok(()) => return Ok(v),
            Err(e) => println!("  ! {e} — try again"),
        }
    }
}

/// Add / edit / remove backends; report whether anything changed.
fn backends_menu(cfg: &mut LpcacheConfig) -> Result<bool, String> {
    println!("\n── backends ──");
    let mut changed = false;
    loop {
        let mut items: Vec<String> = cfg
            .backends
            .iter()
            .enumerate()
            .map(|(i, b)| format_backend(i, b))
            .collect();
        items.push("+ add a backend".to_string());
        if !cfg.backends.is_empty() {
            items.push("- remove a backend".to_string());
        }
        items.push("done".to_string());
        let sel = Select::with_theme(&theme())
            .with_prompt("backends")
            .items(&items)
            .default(0)
            .interact_opt()
            .map_err(aborted)?;
        match sel {
            None => return Ok(changed), // Esc = done
            Some(i) if i < cfg.backends.len() => {
                let updated = prompt_backend(&cfg.backends[i].clone())?;
                cfg.backends[i] = updated;
                changed = true;
            }
            Some(i) if i == cfg.backends.len() => {
                let be = prompt_backend(&BackendEntry::default())?;
                cfg.backends.push(be);
                changed = true;
            }
            Some(i) if i == cfg.backends.len() + 1 && !cfg.backends.is_empty() => {
                let labels: Vec<String> = cfg
                    .backends
                    .iter()
                    .enumerate()
                    .map(|(j, b)| format_backend(j, b))
                    .collect();
                if let Some(j) = Select::with_theme(&theme())
                    .with_prompt("remove which backend?")
                    .items(&labels)
                    .default(0)
                    .interact_opt()
                    .map_err(aborted)?
                {
                    if Confirm::with_theme(&theme())
                        .with_prompt(format!("Remove {}?", cfg.backends[j].url))
                        .default(false)
                        .interact()
                        .map_err(aborted)?
                    {
                        cfg.backends.remove(j);
                        changed = true;
                    }
                }
            }
            _ => return Ok(changed), // "done"
        }
    }
}

/// Root-menu hint for the plugins section.
fn plugins_state(cfg: &LpcacheConfig) -> String {
    let raw = cfg.plugins.json.trim();
    if raw.is_empty() {
        return "none".into();
    }
    match serde_json::from_str::<Vec<Value>>(raw) {
        Ok(arr) => format!("{} plugin(s)", arr.len()),
        Err(_) => "INVALID JSON".into(),
    }
}

/// The parsed spec array (`[]` when unset or unparseable — the menu
/// refuses to guess at broken JSON and routes to the raw editor instead).
fn plugin_specs(cfg: &LpcacheConfig) -> Vec<Value> {
    let raw = cfg.plugins.json.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    serde_json::from_str(raw).unwrap_or_default()
}

/// Store specs back into the model; empty clears the setting (the proxy
/// then falls back to the legacy prefilter settings).
fn set_plugin_specs(cfg: &mut LpcacheConfig, specs: &[Value]) {
    cfg.plugins.json = if specs.is_empty() {
        String::new()
    } else {
        serde_json::to_string(specs).unwrap_or_default()
    };
}

fn format_plugin(i: usize, spec: &Value) -> String {
    format!("edit {}: {}", i + 1, plugin_schema::spec_summary(spec))
}

/// Add / edit / remove plugin specs with guided per-plugin prompts
/// (params come from `plugin_schema`); entries with unknown names keep a
/// raw-JSON editor so nothing is lost. The list order is the prefilter
/// evaluation order.
fn plugins_menu(cfg: &mut LpcacheConfig) -> Result<bool, String> {
    println!("\n── plugins ──");
    let raw = cfg.plugins.json.trim().to_string();
    if !raw.is_empty() && serde_json::from_str::<Vec<Value>>(&raw).is_err() {
        println!("  the current PLUGINS JSON does not parse — let's fix it as raw JSON.");
        let fixed = raw_plugins_prompt(&raw)?;
        let changed = fixed != raw;
        cfg.plugins.json = fixed;
        println!();
        return Ok(changed);
    }
    let mut specs = plugin_specs(cfg);
    let mut changed = false;
    loop {
        let n = specs.len();
        let mut items: Vec<String> =
            specs.iter().enumerate().map(|(i, s)| format_plugin(i, s)).collect();
        items.push("+ add a plugin".into());
        if n > 0 {
            items.push("- remove a plugin".into());
        }
        items.push("~ edit the raw JSON".into());
        items.push("done".into());
        let sel = Select::with_theme(&theme())
            .with_prompt("plugins")
            .items(&items)
            .default(0)
            .interact_opt()
            .map_err(aborted)?;
        let raw_i = n + if n > 0 { 2 } else { 1 };
        match sel {
            None => break,
            Some(i) if i < n => {
                let updated = plugin_entry_prompt(&specs[i])?;
                if updated != specs[i] {
                    specs[i] = updated;
                    changed = true;
                }
            }
            Some(i) if i == n => {
                let Some(spec) = prompt_new_plugin()? else { continue };
                specs.push(spec);
                changed = true;
            }
            Some(i) if n > 0 && i == n + 1 => {
                let labels: Vec<String> =
                    specs.iter().map(plugin_schema::spec_summary).collect();
                let Some(j) = Select::with_theme(&theme())
                    .with_prompt("remove which plugin?")
                    .items(&labels)
                    .default(0)
                    .interact_opt()
                    .map_err(aborted)?
                else {
                    continue;
                };
                if Confirm::with_theme(&theme())
                    .with_prompt(format!(
                        "Remove {}?",
                        plugin_schema::spec_summary(&specs[j])
                    ))
                    .default(false)
                    .interact()
                    .map_err(aborted)?
                {
                    specs.remove(j);
                    changed = true;
                }
            }
            Some(i) if i == raw_i => {
                let before = serde_json::to_string(&specs).unwrap_or_default();
                let fixed = raw_plugins_prompt(&before)?;
                if fixed != before {
                    specs = serde_json::from_str(&fixed).unwrap_or_default();
                    changed = true;
                }
            }
            _ => break, // "done"
        }
        set_plugin_specs(cfg, &specs);
    }
    println!();
    Ok(changed)
}

/// Print the full effective configuration plus any validation problems.
/// Pick a plugin (name + summary) and prompt for its params → a new spec.
fn prompt_new_plugin() -> Result<Option<Value>, String> {
    let pickable: Vec<&Plugin> = plugin_schema::plugins()
        .iter()
        .filter(|p| p.selectable)
        .collect();
    let Some(plg) = pick_plugin(&pickable)? else {
        return Ok(None);
    };
    let params = params_prompt(plg, &serde_json::Map::new())?;
    let mut spec = serde_json::Map::new();
    spec.insert("name".into(), Value::String(plg.name.to_string()));
    spec.insert("params".into(), params);
    Ok(Some(Value::Object(spec)))
}

/// Re-edit one existing spec. Unknown plugin names (a custom registry) have
/// no schema, so they fall back to editing the whole entry as raw JSON.
fn plugin_entry_prompt(spec: &Value) -> Result<Value, String> {
    let name = spec.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let Some(plg) = plugin_schema::find(name).filter(|p| p.selectable) else {
        println!("  {name:?} has no built-in schema — editing it as raw JSON.");
        let before = serde_json::to_string(spec).unwrap_or_default();
        let after = raw_json_prompt("plugin spec", &before)?;
        return serde_json::from_str(&after)
            .map_err(|e| format!("wizard: that spec is not valid JSON: {e}"));
    };
    let existing = spec
        .get("params")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut out = spec.as_object().cloned().unwrap_or_default();
    let params = params_prompt(plg, &existing)?;
    out.insert("params".into(), params);
    Ok(Value::Object(out))
}

/// Prompt every param of `plg`; existing values are the defaults and values
/// left at the plugin default are omitted, keeping `PLUGINS` compact.
/// Params the schema does not know about are carried over untouched (they
/// are reported by `validate`) instead of being silently dropped.
fn params_prompt(
    plg: &Plugin,
    existing: &serde_json::Map<String, Value>,
) -> Result<Value, String> {
    let mut out = serde_json::Map::new();
    for par in plg.params {
        if let Some(v) = prompt_param(par, existing.get(par.key))? {
            out.insert(par.key.to_string(), v);
        }
    }
    for (k, v) in existing {
        if !plg.params.iter().any(|p| p.key == k.as_str()) {
            out.insert(k.clone(), v.clone());
        }
    }
    Ok(Value::Object(out))
}

/// The plugin picker (name + summary) over a candidate set.
fn pick_plugin(candidates: &[&'static Plugin]) -> Result<Option<&'static Plugin>, String> {
    let labels: Vec<String> = candidates
        .iter()
        .map(|p| format!("{} — {}", p.name, p.summary))
        .collect();
    let Some(i) = Select::with_theme(&theme())
        .with_prompt("plugin")
        .items(&labels)
        .default(0)
        .interact_opt()
        .map_err(aborted)?
    else {
        return Ok(None);
    };
    Ok(Some(candidates[i]))
}

/// Prompt one param. `None` means "leave unset" (the plugin default applies
/// and the key is omitted from the JSON).
fn prompt_param(par: &Param, cur: Option<&Value>) -> Result<Option<Value>, String> {
    let cur_text = cur.map(render_value).unwrap_or_default();
    let label = if par.help.is_empty() {
        par.key.to_string()
    } else {
        format!("{} — {}", par.key, par.help)
    };
    let val = match par.kind {
        FieldKind::Text | FieldKind::Secret | FieldKind::Url => {
            let def = if cur_text.is_empty() {
                par.default.to_string()
            } else {
                cur_text.clone()
            };
            let allow_empty = !par.required;
            let v = prompt_text(&label, &def, allow_empty, |v| {
                if par.required && v.trim().is_empty() {
                    return Err("this param is required".to_string());
                }
                if par.kind == FieldKind::Url
                    && !(v.starts_with("http://") || v.starts_with("https://"))
                {
                    return Err("url must start with http:// or https://".to_string());
                }
                Ok(())
            })?;
            Value::String(v)
        }
        FieldKind::Int | FieldKind::Float => {
            let v = prompt_text(&label, &num_default(par, cur), par.min == Some(0.0) && !par.required, |v| {
                parse_num(par, v).map(|_| ())
            })?;
            parse_num(par, &v).unwrap_or(Value::Null)
        }
        FieldKind::Bool => Confirm::with_theme(&theme())
            .with_prompt(&label)
            .default(match cur.and_then(Value::as_bool) {
                Some(b) => b,
                None => par.default == "true",
            })
            .interact()
            .map_err(aborted)?
            .into(),
        FieldKind::Enum => {
            let unset = par.default.is_empty();
            let mut choices: Vec<String> = par.choices.iter().map(|s| s.to_string()).collect();
            if unset {
                choices.insert(0, "(unset)".to_string());
            }
            let def = cur
                .and_then(Value::as_str)
                .and_then(|c| choices.iter().position(|x| x == c))
                .unwrap_or(if unset { 0 } else { 1 }.min(choices.len() - 1));
            let sel = Select::with_theme(&theme())
                .with_prompt(&label)
                .items(&choices)
                .default(def)
                .interact_opt()
                .map_err(aborted)?;
            match sel {
                None => Value::Null,
                Some(i) if unset && i == 0 => Value::Null,
                Some(i) => Value::String(choices[if unset { i - 1 } else { i }].clone()),
            }
        }
        FieldKind::StrList | FieldKind::CiList => {
            let def = match cur {
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|i| i.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                Some(Value::String(s)) => s.clone(),
                _ => par.default.to_string(),
            };
            let v = prompt_text(&format!("{label} (comma-separated)"), &def, true, |v| {
                list_error(par, v)
            })?;
            Value::Array(
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| Value::String(s.to_string()))
                    .collect(),
            )
        }
        FieldKind::Nested => nested_filters_prompt(cur)?,
    };
    if val.is_null() || (!par.required && omittable(par, &val)) {
        return Ok(None);
    }
    Ok(Some(val))
}

/// Render one JSON value as the text a prompt shows as its default.
fn render_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Array(items) => items
            .iter()
            .map(render_value)
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

/// The text default for a numeric param (current value, else schema default).
fn num_default(par: &Param, cur: Option<&Value>) -> String {
    match cur {
        Some(v) if v.is_number() => v.to_string(),
        _ => par.default.to_string(),
    }
}

/// Parse + range-check a numeric param input.
fn parse_num(par: &Param, raw: &str) -> Result<Value, String> {
    let raw = raw.trim();
    if par.kind == FieldKind::Int {
        let n: i64 = raw.parse().map_err(|_| "not a whole number".to_string())?;
        if let Some(min) = par.min && (n as f64) < min {
            return Err(format!("must be >= {}", min as i64));
        }
        if let Some(max) = par.max && (n as f64) > max {
            return Err(format!("must be <= {}", max as i64));
        }
        return Ok(Value::from(n));
    }
    let f: f64 = raw.parse().map_err(|_| "not a number".to_string())?;
    if let Some(min) = par.min && f < min {
        return Err(format!("must be >= {min}"));
    }
    if let Some(max) = par.max && f > max {
        return Err(format!("must be <= {max}"));
    }
    serde_json::Number::from_f64(f)
        .map(Value::Number)
        .ok_or_else(|| "not a finite number".to_string())
}

/// Validate a comma-separated list input against the param's choices.
fn list_error(par: &Param, raw: &str) -> Result<(), String> {
    let items: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if par.required && items.is_empty() {
        return Err("this param is required (at least one value)".to_string());
    }
    if par.choices.is_empty() {
        return Ok(());
    }
    let ci = par.kind == FieldKind::CiList;
    for item in items {
        let hit = if ci {
            par.choices.iter().any(|c| c.eq_ignore_ascii_case(item))
                || par.extra_ok.is_some_and(|f| f(item))
        } else {
            par.choices.contains(&item)
        };
        if !hit {
            return Err(format!(
                "unknown value {item:?} — known: {}",
                par.choices.join(", ")
            ));
        }
    }
    Ok(())
}

/// True when a value equals the plugin's own default, so the key is dropped
/// from `PLUGINS` and the runtime default applies (keeps the JSON minimal).
fn omittable(par: &Param, val: &Value) -> bool {
    if !par.default.is_empty() {
        if render_value(val).eq_ignore_ascii_case(par.default) {
            return true;
        }
        return val
            .as_f64()
            .is_some_and(|f| par.default.parse::<f64>().is_ok_and(|d| d == f));
    }
    match val {
        Value::String(s) => s.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Bool(false) => true,
        _ => false,
    }
}
/// Edit the `chain` plugin's nested `filters` list (prefilter kinds only).
fn nested_filters_prompt(cur: Option<&Value>) -> Result<Value, String> {
    let mut specs: Vec<Value> = cur
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let pickable = plugin_schema::prefilter_plugins();
    loop {
        let n = specs.len();
        let mut items: Vec<String> = specs
            .iter()
            .map(plugin_schema::spec_summary)
            .map(|s| format!("edit {s}"))
            .collect();
        items.push("+ add filter".into());
        if n > 0 {
            items.push("- remove filter".into());
        }
        items.push("done".into());
        let sel = Select::with_theme(&theme())
            .with_prompt("chain filters")
            .items(&items)
            .default(0)
            .interact_opt()
            .map_err(aborted)?;
        let raw_i = n + if n > 0 { 2 } else { 1 };
        match sel {
            None => break,
            Some(i) if i < n => specs[i] = plugin_entry_prompt(&specs[i].clone())?,
            Some(i) if i == n => {
                if let Some(plg) = pick_plugin(&pickable)? {
                    let params = params_prompt(plg, &serde_json::Map::new())?;
                    let mut spec = serde_json::Map::new();
                    spec.insert("name".into(), Value::String(plg.name.to_string()));
                    spec.insert("params".into(), params);
                    specs.push(Value::Object(spec));
                }
            }
            Some(i) if n > 0 && i == n + 1 => {
                let labels: Vec<String> =
                    specs.iter().map(plugin_schema::spec_summary).collect();
                if let Some(j) = Select::with_theme(&theme())
                    .with_prompt("remove which filter?")
                    .items(&labels)
                    .default(0)
                    .interact_opt()
                    .map_err(aborted)?
                {
                    specs.remove(j);
                }
            }
            Some(i) if i == raw_i => {
                let before = serde_json::to_string(&specs).unwrap_or_default();
                let after = raw_json_prompt("chain filters", &before)?;
                specs = serde_json::from_str(&after).unwrap_or_default();
            }
            _ => break, // "done"
        }
    }
    Ok(Value::Array(specs))
}

/// Edit the whole plugin list as raw JSON (`plugins.json`'s value).
fn raw_plugins_prompt(cur: &str) -> Result<String, String> {
    raw_json_prompt("plugins JSON", cur)
}

/// Free-form JSON editor fallback: one line, re-prompted until it parses as
/// a JSON array/object; Return keeps the current value.
fn raw_json_prompt(label: &str, cur: &str) -> Result<String, String> {
    let t = theme();
    loop {
        let v = ask_text(&t, format!("{label} (raw JSON)"), Some(cur.to_string()), false)?;
        let parsed: Result<Value, _> = serde_json::from_str(v.trim());
        match parsed {
            Ok(Value::Array(_)) | Ok(Value::Object(_)) => return Ok(v.trim().to_string()),
            Ok(_) => println!("  ! expected a JSON array or object — try again"),
            Err(e) => println!("  ! not valid JSON: {e} — try again"),
        }
    }
}


/// Print the review screen: effective config + validation problems.
fn review(cfg: &LpcacheConfig, dirty: bool) {
    let source = if dirty { "(session — unsaved)" } else { "(session — unchanged)" };
    print!("{}", cfg.show(source));
    let (errors, _) = cfg.validate();
    for e in &errors {
        println!("  error:   {e}");
    }
    if errors.is_empty() {
        println!("  no errors — ready to save.");
    }
    println!();
}

/// Validate, confirm, and write the file; returns true when saved.
fn save(path: &Path, cfg: &LpcacheConfig) -> Result<bool, String> {
    let (errors, warnings) = cfg.validate();
    for e in &errors {
        println!("  error:   {e}");
    }
    for w in &warnings {
        println!("  warning: {w}");
    }
    if !errors.is_empty() {
        println!("Fix the errors above before saving — back to the menu.");
        println!();
        return Ok(false);
    }
    let ok = Confirm::with_theme(&theme())
        .with_prompt(format!("Save to {}?", path.display()))
        .default(true)
        .interact_opt()
        .map_err(aborted)?;
    if ok != Some(true) {
        return Ok(false);
    }
    model::save(path, cfg)?;
    println!(
        "\nsaved {}.\n\nNext steps:\n  \
         lpcache-config show                 # verify what was written\n  \
         lpcache-config export env           # shell exports for running lpcache\n  \
         lpcache-config export systemd       # systemd unit with the config baked in\n  \
         lpcache-config serve                # or edit in the browser instead",
        path.display()
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_keys_match_the_model() {
        assert_eq!(section_keys("proxy").len(), 7);
        assert_eq!(section_keys("kv_cache").len(), 8);
        assert_eq!(section_keys("prefilter").len(), 4);
        assert_eq!(section_keys("request_log").len(), 5);
        assert_eq!(section_keys("plugins").len(), 1);
        assert!(section_keys("backends").is_empty()); // managed via its own menu
        assert_eq!(section_keys("nope").len(), 0);
    }

    #[test]
    fn backend_line_shapes() {
        let be = BackendEntry {
            url: "http://h:8000".into(),
            n_slots: 4,
            slot_save_path: String::new(),
            kv_cache: false,
        };
        let s = format_backend(0, &be);
        assert!(s.starts_with("edit 1: http://h:8000"));
        assert!(s.contains("4 slot(s)"));
        assert!(s.contains("kv off"));
        assert!(s.contains("save:none"));
    }

    #[test]
    fn menu_covers_every_action() {
        let cfg = LpcacheConfig::default();
        let items = menu_items(&cfg, true);
        assert_eq!(items.len(), SECTIONS.len() + 3);
        assert!(items[0].starts_with('*')); // dirty marker
        assert!(items[1].contains("1 backend"));
        assert!(items.last().unwrap().contains("quit"));
    }
}



