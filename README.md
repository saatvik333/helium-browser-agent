# hba

`hba` drives your running [Helium](https://helium.computer/) browser from the shell: your own
profiles and logins, tabs, pages, DevTools and extension development. Google Chrome and Brave work
too (`browser = "chrome"` or `"brave"`). It is built for coding agents (one short command per
step, compact output, errors that say what to do next) and works just as well by hand.

```sh
hba open github.com/notifications     # the session's own background tab in your profile; prints `t5 url "title"`
hba snap -i                           # interactive elements with refs: - link "Inbox" [ref=e12]
hba click e12                         # real mouse input; prints what changed (→ url, + popup, ! dialog, errors)
hba fill 'internal:label="Search"i' 'is:open' --submit
hba close                             # closes this session's tabs
```

- **Your browser, not a copy.** `hba` attaches to the browser you already run, over a loopback
  DevTools port whose owner it verifies. Agents get their own background tabs; your tabs are
  untouched unless you hand one over (`hba tab use t42`).
- **Fast.** A small daemon keeps one DevTools connection open, so commands take milliseconds and
  the browser never restarts.
- **Hard to detect.** Page work runs in an isolated world that pages cannot see, input is trusted
  mouse and keyboard events with human timing on websites, and the `Runtime` and `Log` domains that
  anti-bot scripts probe for stay off on websites unless you ask.
- **Full DevTools.** Console, exceptions, network log with bodies, blocking and mocking, HAR,
  performance traces, CPU profiles, heap snapshots, coverage, web vitals, emulation, cookies and
  storage.
- **Extension development.** Load, reload and remove unpacked extensions, read their service-worker
  and page consoles, errors and storage, evaluate in the service worker or a content script, and
  drive the popup like a page.

The page engine is [Playwright](https://playwright.dev/)'s injected script (accessibility snapshot,
selectors, actionability checks), vendored and pinned.

## Requirements

- Linux (endpoint verification reads `/proc`).
- A Chromium 150 or newer browser with remote debugging on a loopback port:
  - **Helium** (the default, and the recommended setup): add the flag to
    `~/.config/helium-browser-flags.conf` and restart Helium:

    ```
    --remote-debugging-port=9222
    ```
  - **Google Chrome**: set `browser = "chrome"` in `~/.config/hba/config.toml`. Chrome ignores
    the flag on its own profile, so open `chrome://inspect/#remote-debugging` and turn on "Allow
    remote debugging for this browser instance" (approval mode). Chrome then asks you to Allow
    each new connection: click Allow when `hba` first connects. A command waits up to its
    `--timeout` for you, and an Allow after that still connects, for the next command; the daemon
    keeps that one connection and, in this mode, does not exit when idle. While `hba` is
    connected, Chrome shows its "Chrome is being controlled by automated test software" bar;
    that is expected. Chrome with a separate `--user-data-dir` takes the flag on that command
    line (not in `chrome-flags.conf`, which its own profile reads too).
  - **Brave**: set `browser = "brave"` and use the flag in `~/.config/brave-flags.conf`. Brave
    support follows from it being Chromium, but it has not been tested.

  Either way, DevTools access is full control of the browser; keep it on loopback. `hba` refuses a
  port that listens beyond loopback or belongs to another process. `hba doctor` says which mode
  is in use and what to change; [configuration](docs/configuration.md#endpoint) has the details.
- Rust 1.89 or newer to build.

## Install

Paste this into your coding agent (Claude Code, Codex, omp, …):

```
Fetch and follow the instructions at https://raw.githubusercontent.com/saatvik333/helium-browser-agent/main/AGENT-SETUP.md to set up hba for me.
```

It installs `hba`, turns on your browser's DevTools access (asking before restarting it), links
the skill for your agents and checks everything works. Run it again to update.

By hand:

```sh
cargo install --path . --locked     # installs ~/.cargo/bin/hba
hba doctor                          # endpoint, profiles and daemon checks
ln -s "$PWD/skill" ~/.agents/skills/hba
```

## Commands

`hba --help` and `hba COMMAND --help` describe every option. A TARGET is a ref from the latest
`snap` (`e5`, or `f1e3` inside a frame) or a Playwright selector (`'#id'`, `'text=Save'`,
`'role=button[name="Save"]'`, `'internal:label="Email"i'`).

| Area | Commands |
| --- | --- |
| Navigate | `open`, `back`, `forward`, `reload` |
| Read | `snap` (`-i` interactive, `-v` viewport, `-d` diff, `-S` subtree), `find`, `text`, `get`, `is` |
| Act | `click`, `hover`, `focus`, `fill`, `type`, `press`, `check`, `uncheck`, `select`, `upload`, `drag`, `scroll`, `mouse`, `wait`, `dialog`, `download` |
| Capture | `shot` (`-a` labels refs), `pdf` |
| Tabs and sessions | `tabs`, `tab new/use/close/release/front`, `window`, `profiles`, `status`, `close` |
| DevTools | `console`, `errors`, `net` (`show`, `body`, `block`, `mock`, `unroute`, `har`), `perf` (`metrics`, `vitals`, `trace`, `profile`, `heap`, `coverage`), `devtools` |
| Environment | `emulate`, `cookies`, `storage`, `state`, `perms`, `clip` |
| Extensions | `ext` (`load`, `reload`, `rm`, `eval`, `logs`, `errors`, `popup`, `open`, `storage`, `sw`), `js --world EXT` |
| Escape hatches | `js`, `cdp`, `run` (many commands in one call) |
| Browser and daemon | `browser launch/restart/endpoint`, `daemon status/stop`, `doctor` |

**Sessions.** Every command belongs to a session: `-s NAME`, `$HBA_SESSION`, or by default the
project (git root) directory name. A session owns the tabs it opens and has a current tab; give
parallel agents distinct sessions. `hba close` closes the session's tabs and hands adopted tabs
back.

**Tab ownership.** A session can only act on its own tabs. Any other tab, including one it never
opened and one it released, fails with "not in session" and exit status 1, never an empty
result. One of your tabs becomes the session's only through `hba tab use t42`, and goes back with
`hba tab release` or `hba close`; `hba tab close` closes it for good. Commands in a session run
one at a time, so a release takes effect after the command already running on that tab returns.
Ownership is saved to disk and survives a daemon restart; a browser restart ends it, because tab
numbers start over.

**Profiles.** New tabs open in the profile chosen by `-p`/`$HBA_PROFILE`, the session's earlier
choice, a directory rule, the configured default, or the browser's last-used profile, in that order.

**Output.** Text is compact and meant to be read by a model; `--json` gives structured output for
`tabs`, `status`, `open`, `tab`, `js`, `profiles` and others. Exit status 1 means the command failed
(its message says why and usually what to run next); 2 means the command line was wrong.

## Configuration

Everything works without a config file for Helium. The browser, profile aliases, directory rules,
the browser location and input timing are set in `~/.config/hba/config.toml` or `HBA_*` variables,
checked when a command first needs them; see [configuration](docs/configuration.md).

## How it works

See [architecture](docs/architecture.md): the daemon, how tabs are attached, the page engine,
actionability and settling, dialogs, stealth rules, and extension debugging.

## Development

```sh
cargo build
cargo clippy --all-targets -- -D warnings
cargo test                                   # unit and CLI tests
HBA_LIVE=1 cargo test --test live            # end to end against a throwaway headless Helium
HBA_LIVE=1 HBA_BROWSER=chrome HBA_HELIUM=/opt/google/chrome/chrome cargo test --test live   # the same against Chrome
bun scripts/vendor-playwright.mjs            # refresh assets/playwright from the pinned version
```

The live tests start their own headless browser with a temporary profile and never touch your
browser. They pass against Helium 156 and Chrome 156; Brave is untested. Approval mode is covered
by a CLI test with a fake browser (one open request, a late Allow) and was checked by hand on
Chrome 156, where clicking Allow needs a person. `HBA_TRACE=1` adds per-command timings to
`$XDG_RUNTIME_DIR/hba/daemon.log`.

## License

Apache-2.0. `assets/playwright` holds files from Playwright (Apache-2.0, Microsoft); see
[NOTICE](NOTICE).
