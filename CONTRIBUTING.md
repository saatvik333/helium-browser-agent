# Contributing

Start with the README and [architecture](docs/architecture.md). Create a focused
branch and describe the concrete failure or behavior you want to change. Include
reproduction steps, dependency versions and relevant validation in your change.

```sh
uv sync --locked
uv run python -m unittest discover -s tests -v
uv run ruff check .
uv build
```

Add regression tests for meaningful ownership, cleanup, configuration, protocol
or input fixes. Use fake CDP transports for deterministic failures. Run the live
fixtures only with an explicitly authorized profile; preserve user tabs, browser
settings and dialogs. Full-browser restarts are not a normal test step.

Use the pinned dependency versions. Dependency upgrades should be separate
changes with live coverage of snapshot references, native fill, dialogs, relay
recovery, sockets and cleanup. Never weaken ownership checks to make a test pass.

Before sharing a patch or log, remove cookies, tokens, profile files, endpoint
URLs, page contents and account details. Use a synthetic fixture instead of a
private website. Keep runtime files under the cache or ignored `artifacts/`.
Report a security issue privately to the repository maintainer when a private
channel is available; avoid posting credentials or sensitive reproductions.

Contributions are under the project's Apache-2.0 license. Preserve upstream
notices and explain any copied compatibility code in NOTICE.
