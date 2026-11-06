---
name: Bug report
about: Create a report to help us improve the lpcache proxy
title: ''
labels: ''
assignees: ''

---

**Describe the bug**
A clear and concise description of what is wrong.

**To Reproduce**
1. llama-server command (note `-np`/`--parallel`, `--slot-save-path`, `--api-key`)
2. proxy configuration (`BACKENDS` JSON or env vars / CLI flags)
3. the request that triggers it (e.g. the `curl` command)

**Expected behavior**
A clear and concise description of what you expected to happen.

**Environment (please complete the following information):**
 - `lpcache -V` output
 - OS and architecture
 - llama.cpp / llama-server version
 - log excerpt around the failure (`LOG_LEVEL=DEBUG` or `RUST_LOG=debug`)

**Additional context**
Add any other context about the problem here.
