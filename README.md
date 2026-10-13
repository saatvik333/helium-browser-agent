# Helium Browser Agent

Control a running Helium browser on Linux through named, task-owned sessions.
Choose an existing browser profile, automate your own tabs, and clean them up
without borrowing unrelated tabs. Two interfaces share the same profile and
ownership layer: Python helpers through `helium-browser-use`, and the native
`agent-browser` CLI through `agent-browser-helium`.

This is an early standalone release extracted from a working local integration.
It includes literal text insertion, popup cleanup, reconnect recovery, bounded
session locks, and telemetry opt-outs. It does not include an LLM or require an
AI API key; an agent or your own scripts decide which actions to run.

## Requirements

- Linux with `/proc`, Python 3.12 or newer, and [uv](https://docs.astral.sh/uv/).
- [Helium](https://helium.com/) installed and configured for **loopback-only CDP**.
  Live tests ran against Chromium 154.0.8037.92. The installed update is Helium
  0.19.2.1 / Chromium 155.0.8059.39; it awaits a browser restart and retest.
- An existing Helium profile whose display name you know.
- For the native CLI adapter only: `agent-browser` **0.37.0** on PATH.
  The adapter depends on that version's fill, socket and target-binding behavior.

The Python dependencies are locked, including browser-harness 0.1.13 and
websockets 15.0.1. The larger browser-use/LLM SDK stack is not required.
Windows, macOS, remote browsers and multiple simultaneous Helium main processes
are outside the supported scope. Niri focus restoration is optional.

## Install

From this checkout:

```sh
uv sync --locked
uv run helium-agent --version
uv run helium-agent profiles
uv run helium-agent doctor
```

`doctor` reads endpoint ownership and versions; it does not launch Helium, open
tabs or restart it. `profiles` reads only profile display-name/directory metadata.
Its `running_browser` field reports the active process. `helium-browser --version`
reports the installed binary, which can differ until the browser is restarted.
Use `uv run` while developing to avoid replacing existing global commands.
To install the Python CLIs for general use:

```sh
uv tool install .
```

An existing command with the same name may prevent installation. Back up your
existing integration and close its task sessions before deliberately replacing
it. The new project uses a separate `helium-browser-agent` cache and `hba-`
harness daemon prefix; it does not adopt the old Python wrapper's sessions.
`uv tool uninstall helium-browser-agent` removes the installed tool, leaving
browser profiles and task state alone. Close sessions before uninstalling.

For the optional native adapter, install the compatible upstream CLI separately:

```sh
npm install --global agent-browser@0.37.0
```

Its browser-download command is unnecessary when attaching to your existing
Helium. See [the adapter guide](docs/agent-browser.md) for supported commands.

## Configure Helium and profiles

Helium must already expose a local CDP endpoint. This project discovers it from
`$XDG_CONFIG_HOME/helium-browser-flags.conf` (`--remote-debugging-port=PORT`) or
the profile root's `DevToolsActivePort` file, then verifies that the listener
belongs to the running Helium main process. Packaging and browser policy can
affect whether remote debugging of the chosen data directory is available.
The installer does not edit browser flags or restart your browser.

CDP grants extensive access to a browser. Keep the listener on loopback, never
expose it to a network, and run only trusted automation. A named task shares its
profile's cookies and storage; it is not a security boundary between accounts.

```sh
mkdir -p ~/.config/helium-browser-agent
cp examples/config.toml ~/.config/helium-browser-agent/config.toml
```

Edit the profile names to match `helium-agent profiles`. The default name is
`Personal`; `--profile 'Your exact name'` overrides it. `--personal` and `--work`
use configurable aliases. There are no built-in company or workspace mappings.
See [configuration](docs/configuration.md) for paths, environment overrides and
optional workspace rules.

## Python helper interface

Use a fresh session name for a task, then reuse it across invocations. The wrapper
creates and attaches an owned working tab before your code runs.

```sh
uv run helium-browser-use --session example --profile Personal <<'PY'
goto_url('https://example.com')
assert wait_for_load()
print(js('document.title'))
PY

uv run helium-browser-use --session example --close
```

For a form you own:

```python
assert wait_for_element('#message')
fill_input('#message', 'First line\nSecond line\twith a tab')
assert input_value('#message') == 'First line\nSecond line\twith a tab'
```

`fill_input` uses native bulk insertion and verifies the exact result. It refuses
readonly, disabled and unsupported elements. It preserves textareas/editor
newlines, tabs and Unicode, normalizing CRLF/CR to LF. Application-specific masks,
editors and asynchronous state changes still need assertions on the application.

`list_tabs()` lists task-owned pages. `new_tab`, `switch_tab`, `activate_tab` and
`close_tab` enforce ownership. Background operation is the default; profile
identification can briefly activate a newly launched probe. `list_tabs(all_tabs=True)`
is an explicit read-only wider inventory. Raw Python/CDP remains powerful.

## Native CLI interface

```sh
uv run agent-browser-helium --session example --profile Personal open https://example.com
uv run agent-browser-helium --session example snapshot -i
uv run agent-browser-helium --session example get title
uv run agent-browser-helium --session example close
```

Profile and session options go before the command. `close` removes owned tabs;
`detach` stops the task backend while retaining tabs. The relay filters target
discovery and attachment before the backend sees pages, preserving other tasks'
tabs and dialogs. Broader browser/cloud/setup commands are deliberately excluded.

## Develop and test

```sh
uv sync --locked
uv run python -m unittest discover -s tests -v
uv run ruff check .
uv build
```

Offline tests use fake transports and temporary state. The live suites create
local HTTP fixtures and owned tabs in the **Personal alias**, then clean up.
They require a running, verified Helium endpoint and the optional native CLI:

```sh
uv run python tests/live/live_checks.py
uv run python tests/live/live_agent.py
```

Do not run live tests against a profile whose use has not been authorized.
They are opt-in and excluded from CI. Results go to ignored `artifacts/`.
See [CONTRIBUTING.md](CONTRIBUTING.md), [architecture](docs/architecture.md),
and [validation and limits](docs/validation.md).

## Privacy and license

Both wrappers force browser-tool telemetry/cloud-sync opt-outs and disable
automatic recording or saved-session replay in their task environment.
Installation does not change global shell settings or other tools' preferences.
Browser-vendor traffic and explicit user-requested screenshots/traces are separate.
No profiles, cookies, credentials, browsing history or live task state are shipped.

Project code is Apache-2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE) for the
small upstream compatibility snippet and dependency attribution.
