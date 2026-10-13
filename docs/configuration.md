# Configuration

Configuration is read once when a process starts, from
`~/.config/helium-browser-agent/config.toml` (respecting `XDG_CONFIG_HOME`).
`HELIUM_AGENT_CONFIG=/path/to/config.toml` selects an explicit file. A missing
explicit file, unknown key or invalid value fails instead of silently falling back.

| Key | Default | Meaning |
| --- | --- | --- |
| `default_profile` | `Personal` | Profile when no flag or workspace rule matches |
| `personal_profile` | `Personal` | Display name used by `--personal` |
| `work_profile` | `Work` | Display name used by `--work` |
| `profile_dir` | `$XDG_CONFIG_HOME/net.imput.helium` | Existing Helium user data root |
| `cache_dir` | `$XDG_CACHE_HOME/helium-browser-agent` | Private task/cache files |
| `launcher` | `helium-browser` resolved on PATH | Package/desktop launcher executable |
| `agent_browser` | `agent-browser` | Optional native backend executable |
| `workspaces` | empty mapping | Directory paths mapped to profile display names |

XDG defaults are `~/.config` and `~/.cache`. `~` is expanded in configured paths.
Executable values are single paths or command names, never shell command strings.
Choose a launcher that uses your configured data root. Custom launcher wrappers
may supply browser flags; the profile-directory argument is passed explicitly.

Profile selection for a new session is: explicit flag, `HELIUM_PROFILE_NAME`,
longest matching workspace directory, configured default. Rules use resolved
paths with directory boundaries, so `~/work` does not match `~/work-other`.
`HELIUM_PROFILE_DIR` overrides the configured data root for compatibility.
Cleanup without a profile flag uses the session's saved profile identity.

Profile display names must uniquely identify a profile in Helium's `Local State`.
Exact names are preferred; case-insensitive/substring matching is accepted only
when unambiguous. No profile is created, renamed or copied.

## State and privacy

`harness/` holds Python session ownership and locks; `agent/` holds adapter state,
relay connection metadata and private native daemon sockets. These are runtime
files, not source code. They may include local endpoint tokens. Do not publish
them, clear them while sessions are active, or use a shared network cache.
State files are atomic and private; uncertain cleanup retains ownership for retry.

Python tasks use a private `workspace/` beneath the cache. Do not put arbitrary
global helpers or `.env` files there. Telemetry/cloud synchronization, recording,
and title markers are disabled for Python tasks. Native tasks sanitize inherited
backend configuration and disable restoration, autosave and automatic dialogs.
They do not override machine-wide preferences for unrelated tools.

## Endpoint troubleshooting

Run `helium-agent doctor`. A running browser with an unverifiable endpoint is
refused. Ensure its CDP listener belongs to the Helium process, uses loopback,
and matches the port configuration/DevToolsActivePort file. More than one Helium
main process is currently refused; isolated-browser workflows require additional
process/data-root selection support. The controller recognizes an executable
replaced during a package update via Linux's ` (deleted)` suffix.

For an unknown profile context, creation launches an exactly marked owned probe,
verifies `chrome://version` profile identity, and registers ownership before
navigation. It never navigates an existing user tab to discover its profile.
Only Niri currently supports best-effort previous-window focus restoration.
