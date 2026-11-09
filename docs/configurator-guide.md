# Configuring lpcache with `lpcache-config`

`lpcache-config` is the configurator bundled with this repository
(package `lpcache-config`, crate `configurator/`). It edits, validates and
exports the lpcache configuration in three interchangeable styles:

| Style | Command | Best for |
| --- | --- | --- |
| One-shot CLI | `set` / `get` / `validate` … | scripts, CI, quick tweaks |
| Guided menu (TUI) | `configure` | first-time setup, hand-editing a full config |
| Web UI | `serve` | browsing every field with a live export preview |

All three write the **same TOML file** (default `lpcache.config.toml` in the
current directory) and run the **same validation**, so a value accepted in
one style is accepted everywhere. Note that `lpcache` itself never reads
the TOML file — the configurator converts it to environment variables /
CLI flags / a systemd unit when you deploy.

![lpcache-config help](configurator/cli-help.png)

## Build

```bash
cargo build --release -p lpcache-config
# binary: target/release/lpcache-config
```

## Five-minute quickstart

```bash
lpcache-config new lpcache.config.toml     # fresh file with built-in defaults
lpcache-config set proxy.port 8090         # change one value (also creates the file)
lpcache-config validate                    # exit code 1 on errors
```

![quickstart](configurator/cli-quickstart.png)

`validate` prints errors and warnings separately — the warning above is the
common "kv_cache on but no slot_save_path" case, which is harmless for
backends whose llama-server cannot save/restore KV caches.

Then export whatever your deployment needs (see
[Exports](#exporting-the-configuration)) and start `lpcache` with it.

## The guided menu — `configure`

```bash
lpcache-config configure            # or `configure PATH`, alias: `wizard`
```

You land on a section menu that summarises the current values, so you can
jump straight to the part you care about:

![main menu](configurator/wizard-1-menu.png)

Keys: **↑/↓** move, **Return** selects, **Esc** goes back one level,
**Ctrl-C** aborts without writing anything.

Every field prompt shows the current value as its default — press
**Return** to keep it, or type a new one. Values are validated as you
type: a bad port or an unparsable float is refused with a one-line
message, and the prompt just asks again.

![editing proxy.port](configurator/wizard-2-editing.png)

Sections you changed get a `*` marker in the menu, with the live summary
updated (port now `9091`):

![dirty sections](configurator/wizard-2b-dirty.png)

### Backends

The **backends** submenu is a small CRUD list: edit any entry, add new
ones, remove one, or done. Per backend you set the URL, `n_slots` (must
match the llama-server's `-np/--parallel`), `slot_save_path` (must match
its `--slot-save-path`), and a `kv` yes/no for KV save/restore support
(llama-server ≥ 1.05.40).

![backends submenu](configurator/wizard-3-backends.png)

### Review and save

**review & validate** echoes the complete effective configuration and
runs the same validator as `validate`. **save & exit** then prints any
warnings, asks for confirmation, and writes the file — but it refuses to
save while errors exist and bounces you back to the menu instead.

![review](configurator/wizard-4-review.png)
![save confirmation](configurator/wizard-5-save-confirm.png)
![saved](configurator/wizard-6-saved.png)
## The web configurator — `serve`

```bash
lpcache-config serve                     # http://127.0.0.1:8086
lpcache-config serve --port 9000 -c /etc/lpcache/config.toml
```

The page is a full form for every setting, a chip showing the file
status (`saved` / `modified` / `warnings` / `invalid`), a **Reload**
button to pick up on-disk edits, and a live export preview on the right:

![web form](configurator/web-1-form.png)

### Probe a backend

Each backend row has a **test** button that performs the same
`GET /v1/models` probe lpcache uses for recovery, so you can confirm the
URL and API key are right before deploying:

![backend probe](configurator/web-2-probe.png)

### Live validation

Every edit re-validates against the server (the same rules as the CLI).
Errors appear in a red banner at the top and **Save to file** refuses to
write until they are gone:

![validation error](configurator/web-3-validation.png)

### Live export preview

The tabs on the right regenerate the selected export format from the
**in-memory** form state on every change — so you can see exactly what
`export systemd` would produce *while* you tweak fields, and copy it with
one click:

![export preview](configurator/web-4-export.png)

> The UI has no authentication — keep it on `127.0.0.1` (the default) or
> front it with a reverse proxy that adds auth.

## Exporting the configuration

Six formats, one command (`PATH` optional, same default file):

| Format | What you get |
| --- | --- |
| `env` | shell `export` lines — `source` before starting lpcache |
| `cli` | the equivalent lpcache CLI flags |
| `systemd` | a full unit file with `Environment=` lines baked in |
| `docker` | a `docker run` invocation with `-e` flags |
| `llama-server` | the flags each backend's llama-server must run with (`--slot-save-path`, parallelism, …) |
| `json` | the config as JSON (for tooling) |

```bash
lpcache-config export env > /etc/lpcache/lpcache.env
```

![export env](configurator/cli-export-env.png)

## Checking a backend without saving anything

`check` probes a backend from the command line — handy when a proxy
setup fails and you need to know whether the llama-server itself is
healthy:

```bash
lpcache-config check http://gpu-box:8000 --api-key secret
```

![check](configurator/cli-check.png)

## Scripting & CI

Everything works non-interactively, so a config can be built in a
Dockerfile or CI job:

```bash
lpcache-config new -                          # defaults to stdout
lpcache-config set backend.url http://10.0.0.5:8000 deploy.toml
lpcache-config set prefilter.blocklist "politics,religion" deploy.toml
lpcache-config get --list                     # every dot-key that exists
lpcache-config validate deploy.toml || exit 1 # fails the build on errors
```

## Command reference

| Command | Purpose |
| --- | --- |
| `new [PATH]` | write a fresh defaults file (`-` = stdout, never overwrites) |
| `configure [PATH]` | guided menu (alias `wizard`) |
| `show [PATH]` | pretty-print the effective configuration |
| `get KEY [PATH]` | print one value (`get --list` enumerates all keys) |
| `set KEY VALUE [PATH]` | set one value and save (creates the file if missing) |
| `validate [PATH]` | validate; exit 1 on errors |
| `export FORMAT [PATH]` | print `env` / `cli` / `systemd` / `docker` / `llama-server` / `json` |
| `serve [--host H] [--port P] [-c PATH]` | web configurator (default `127.0.0.1:8086`) |
| `check URL [--api-key K] [--timeout S]` | probe a backend's `GET /v1/models` |
| `-h`, `--help` / `-V`, `--version` | help / version |

