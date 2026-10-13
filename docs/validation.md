# Validation and known limits

Verified locally on 2026-10-13 against running Helium reporting Chromium
154.0.8037.92 through CDP on Linux. The installed package is Helium 0.19.2.1
(Chromium 155.0.8059.39), but the running process still has its replaced executable
open. The owner deferred restart; the installed update is not yet live-tested.
Offline tests pass on Python 3.12.15 and 3.13.16. Live tests used 3.13.16, browser-harness 0.1.13,
websockets 15.0.1, and agent-browser 0.37.0. Dependency upgrades require renewed
validation. CI is configured for Python 3.12 and 3.13 offline; browser tests are opt-in.

## Test coverage

- 78 offline tests pass: 36 shared/Python regressions, 30 native-adapter
  regressions and 12 configuration/endpoint tests.
- 16 Python live groups and 16 native-adapter live groups pass on synthetic HTTP
  fixtures in the Personal profile. Both cleanups report zero owned pages left
  and no cleanup errors.
- Live coverage includes trusted input/clicks, literal text, readonly/disabled
  refusal, failed-navigation rollback, popups, owned iframe access, screenshot,
  upload, hash-verified HTTP download, dialogs, daemon/relay recovery, snapshot
  references, lock contention and profile/endpoint guards.
- Ruff lint and formatting checks pass. A wheel and complete source distribution
  are built from this checkout. The wheel was installed in a separate environment
  and both interfaces exercised on owned Personal tabs from outside the checkout,
  followed by successful cleanup.

Run the commands in CONTRIBUTING.md to reproduce checks. Raw runtime results
belong in ignored `artifacts/`, not version control. A live test failure must
still attempt cleanup in `finally`; a forcibly killed process cannot guarantee
that cleanup runs.

## Text entry benchmark

The retained [measurement data](benchmark-2026-10-13.json) compares the old
installed fill algorithm with native bulk insertion in the same warm, owned
Personal session. Seven alternating samples per size include an exact-value
read/check, excluding CLI startup and navigation:

| Characters | Old median | New median | Ratio |
| --- | --- | --- | --- |
| 32 | 67.5 ms | 4.27 ms | 15.8× |
| 512 | 1,374.9 ms | 5.37 ms | 256.1× |
| 2,048 | 6,167.8 ms | 7.23 ms | 852.9× |

Both algorithms used the hardened scoped transport. This isolates the input
algorithm, not an end-to-end comparison of historical wrapper versions. The old
multiline/tab cases lost text; the replacement preserved it. Both passed the
Unicode case. These chosen regressions do not establish a general accuracy rate.

To repeat using the packaged implementation and an authorized Personal profile:

```sh
uv run python examples/benchmark_text.py
```

Ordinary command overhead is still generally a few tenths of a second on the
local fixture. Website loading and application behavior dominate many workflows.

## Limits

- Whole-browser restart and long-duration stress have not been tested here.
- Full arbitrary-site compatibility, masked inputs and complex asynchronous
  editors require application-specific assertions.
- A primary helper evaluation interrupted by a dialog can wait about five seconds
  for harness metadata fallback. The native relay fails blocked reads promptly.
- Downloads support HTTP attachments, not blob/data exports, and can conflict
  with concurrent Fetch interception on the same page.
- Lost transport after target creation can leave completion uncertain. No blind
  retry or global target-difference cleanup is used.
- Popups with no attributable opener and process death before recording a target
  cannot be guaranteed automatically recoverable.
- Real-profile cookies and storage remain shared. Ownership guards prevent many
  accidental cross-tab operations; they are not a hostile-code sandbox.
- Only Linux and a single Helium main process are supported. Niri focus
  restoration is best effort; other desktops may retain focus on a probe.
