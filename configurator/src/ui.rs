//! The embedded single-page configurator UI: a self-contained HTML page
//! (inline CSS + JS, no external assets — the same pattern as the lpcache
//! `dashboard` plugin's page). Served at `GET /` by `lpcache-config serve`.

pub const HTML: &str = include_str!("ui.html");
