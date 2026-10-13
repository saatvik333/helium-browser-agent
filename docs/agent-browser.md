# Native CLI adapter

Use `agent-browser-helium --session TASK --profile NAME COMMAND`. Session/profile
flags precede the command; `AGENT_BROWSER_SESSION` may supply the task name.
Names contain 1–48 letters, digits, underscores or hyphens. Reuse one task name
across calls. `--json` produces structured output. Use `COMMAND --help` to inspect
native command syntax without starting a browser session.

Ordinary navigation, snapshots, clicking, input, evaluation, screenshots, uploads
and inspection pass through. Endpoint/config/provider overrides, global close,
cloud/setup/auth/state, batch, MCP and chat are excluded from this shared-browser
adapter. `set`, network and other broad command families may contain operations
that the relay refuses when they would change global browser/profile state.

## Tabs and lifecycle

- `tab list` shows owned tabs with stable target IDs and active flags.
- `tab new [URL]` creates an owned tab in the selected profile.
- `tab TARGET_ID` selects an owned tab; `tab close [TARGET_ID]` closes one.
- Native `t<N>` IDs work while that backend exists; use target IDs across restarts.
- `detach` stops only this task's backend/relay, retaining its tabs and state.
- `close` stops them and removes owned tabs. No profile flag is needed for cleanup.

Bindings are reused to preserve snapshot references. Refresh snapshots after
actual page changes. Popup attribution follows opener ancestry; a severed or
missing opener can make automatic attribution impossible. Never compensate by
closing arbitrary newly observed tabs. Native idle exit is 15 minutes; an unused
relay exits after 60 seconds. Neither idle exit closes tabs.

## Input and dialogs

The adapter validates and replaces agent-browser 0.37.0's clear-then-insert fill
sequence. Readonly/disabled elements are refused before modification; native
insertion is followed by exact-value verification. Unexpected transformations
raise errors and are not blindly retried.

Automatic dialog dismissal is disabled. Use `dialog status`, `dialog accept`,
or `dialog dismiss` deliberately. Renderer evaluation interrupted by a dialog
fails promptly so callers can handle it. Unrelated tasks' dialogs are excluded.

## Downloads

```sh
agent-browser-helium --session example download '#download' /tmp/file.bin
```

Supported downloads are successful HTTP responses with `Content-Disposition:
attachment`. The adapter intercepts the owned page's response, streams into a
private temporary file, fsyncs and atomically replaces the destination. It does
not change global download policy. Failure before commit preserves an existing
destination; cleanup failure after commit can leave the valid saved file.

Blob/data downloads and arbitrary generated exports need another implementation.
Concurrent Fetch interception on the same page can conflict with this path.
Global cookie/cache/permission changes and certificate-ignore flags are refused.

## Recovery

Calls within a session serialize under a bounded lock (`--lock-timeout`, default
30 seconds). Different sessions use separate hashed daemon/socket names. Relay
or backend failure reuses verified owned pages after restarting the task backend.
If cleanup fails, retain the state and retry `close` after fixing the endpoint.
Do not manually delete state as a way to bypass a profile or ownership refusal.
