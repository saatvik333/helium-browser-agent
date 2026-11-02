# Architecture

## Processes

`hba` is one binary playing two roles.

- **Client.** Each command is a short-lived process. It parses the arguments, reads stdin for `-`
  arguments and `run` scripts, sends one JSON request line over `$XDG_RUNTIME_DIR/hba/hba.sock`, and
  prints the one JSON reply line. It starts the daemon when no socket answers.
- **Daemon.** A background `hba` with one DevTools WebSocket to the browser. It keeps sessions,
  attached tabs, their isolated worlds and event buffers (console, network, downloads) warm, so a
  command costs milliseconds instead of a browser connection. Commands of one session run in
  order; different sessions run concurrently. A client that goes away (Ctrl-C, an agent's timeout)
  cancels its command, and whatever the command switched on (interception, `Runtime`, helper tabs)
  is switched off again; `drag`, `browser restart` and the commands that stop a recording and save
  it run to completion. The daemon exits after `idle_minutes` (in approval mode not while it holds
  a connection or a request awaiting Allow), on `hba daemon stop`, or when a client of a different
  build or `HBA_*` environment connects. On the way out it stops accepting connections and
  finishes the commands in flight, and in approval mode waits for the answer to an unanswered
  request (up to a minute in all); a new daemon takes over as soon as the old one releases its
  lock.

## Connecting

The browser is Helium, Chrome or Brave (`browser` in the configuration); its process is the one
of that name (`helium`, `chrome`, `brave`) named in the `SingletonLock` link that Chromium keeps in
the user data directory while it runs. How it serves DevTools follows from its command line:

- **`--remote-debugging-port=N`.** The port is `N`, and the WebSocket URL comes from
  `/json/version`. For port 0 the browser writes the chosen port and URL path to
  `DevToolsActivePort`, which is used instead.
- **No flag: approval mode.** Remote debugging was turned on with the switch at
  `chrome://inspect/#remote-debugging`, which Chromium keeps in `Local State`
  (`devtools.remote_debugging.user-enabled`). The browser listens on a loopback port it writes to
  `DevToolsActivePort`, answers `/json/version` with nothing, and holds each WebSocket handshake
  until its user answers an "Allow remote debugging?" dialog (Cancel answers 403). Chrome shows
  one dialog per request, one at a time and oldest first, and closing a request does not close
  its dialog: Allow on it then connects nothing, and the live request's dialog waits behind it
  (observed on Chrome 156; Chromium's `DevToolsConnectionDialog` has no cancel path). So the
  daemon never abandons a request: it runs the handshake on a task of its own and keeps exactly
  one; a command waits for it up to its timeout (other connections get 5 s) and then says to
  click Allow, while the handshake goes on, so a late Allow connects and the next command takes
  the connection. Commands that come meanwhile wait for the same handshake. A daemon that exits
  with the request still unanswered records it (`abandoned-approval` in the runtime directory),
  and the next daemon's messages say to Cancel the first dialog. Chrome shows its "being
  controlled by automated test software" bar while connected; `navigator.webdriver` stays false
  (checked on Chrome 156).
- **Chrome on its default data directory** ignores the flag (Chrome 136 and later) and then
  starts no DevTools server at all, not even the switch's; `hba` says to remove the flag.

Before connecting, `hba` reads `/proc/net/tcp*` to check that the port listens on loopback only and
that process's `/proc/PID/fd` to check that it holds the listening socket, in both modes. A port
exposed beyond loopback, or owned by anything else, is refused; a port file without its browser
is ignored.

The daemon enables target discovery for pages and tabs only, and `Browser` download events (the
browser's download settings stay as they are).

## Tabs and pages

`hba` attaches to **tab targets** (Chromium 150+), not page targets. A tab's session auto-attaches
to its page targets, including prerendered pages, and each page session auto-attaches to its
out-of-process iframes. New pages and frames wait for the debugger until `hba` has set up
interception, emulation and its event domains, so their first request and script are covered.
Workers and other children are released immediately.

- **Prerendering.** When Chromium activates a prerendered page, the page target loses its
  `prerender` subtype (`Target.targetInfoChanged`) and becomes the tab's document.
- **Back/forward cache.** A restored document fires no load events; `hba` treats it as loaded, and
  does not record its console messages a second time.
- **Lifecycle.** Load events are tracked per main-frame loader, and enabling them replays the ones a
  document already fired, so waiting works on tabs `hba` attached to after they loaded.
- **Background tabs.** New tabs open in the background (`Target.createTarget` with `background`)
  and get focus emulation, so they keep rendering, running timers and taking screenshots without
  being shown. Tabs opened by an agent tab (popups, `target=_blank`) join its session, and the tab
  the user was looking at is shown again.
- **Profiles.** Each profile is a browser context. `hba` learns which context is which profile by
  reading `chrome://version` in a short-lived probe tab, once per profile. New tabs are created in
  the profile's context over DevTools; Chromium accepts that only for the last-used profile, so
  for the others `hba` opens a marked tab through the launcher (`--profile-directory`) and
  restores the focused window on niri.

## Sessions and ownership

A session is a name (from `-s`, `HBA_SESSION`, or the project directory) with a list of tabs and a
current tab, persisted in `$XDG_RUNTIME_DIR/hba/state.json`. A tab is in at most one session.
Tabs a session opened are its own: `hba close` closes them. Other tabs are the user's: a session
touches one only after `hba tab use`, which marks it adopted, and `hba close` or `hba tab release`
detaches without closing it. Tab numbers (`t5`) are stable for the browser's lifetime.

## The page engine

`hba` runs Playwright's injected script (`assets/playwright/injected.js`, pinned by
`scripts/vendor-playwright.mjs`) in a private **isolated world** created with
`Page.createIsolatedWorld` in each frame. Page scripts share the DOM but not the JavaScript heap
with that world, so they cannot see `hba`'s objects. The few strings Playwright would put on the
page (event names) are randomized per daemon.

- **Snapshots.** Playwright's AI-mode accessibility tree with refs. Frames are snapshotted
  separately and stitched; refs in a frame carry its number (`f1e3`). The YAML renderer is a port
  of Playwright's (byte-identical in `--raw` mode); the default output drops nameless wrappers and
  redundant attributes. `-d` diffs against the tab's previous snapshot.
- **Refs.** A ref belongs to the document it was taken in. After a navigation, an old ref fails
  with "run `hba snap`" instead of resolving to a different element.
- **Selectors.** Anything Playwright's selector engines accept: CSS, `text=`, `role=`,
  `internal:label=`, XPath and chains. A selector that matches several elements is an error that
  shows the first matches.

## Actions

Pointer actions follow Playwright's actionability rules: the element must be visible, enabled and
stable (same box twice, 40 ms apart), is scrolled into view, and must be the element hit at the
click point; a covering element is named in the error. Then:

- **Input** is trusted `Input.dispatchMouseEvent` and `Input.dispatchKeyEvent` events with
  Playwright's US keyboard layout. On websites (`humanize = auto`) the pointer moves along a curve
  with eased, jittered steps, clicks land off-center and typing has per-key delays; local pages get
  instant input. Pointer moves are sent without waiting for each to be taken: while the browser's
  window is off screen, the page takes a lone move only with its next frame, about a second
  later, but at once when another event follows. Scrolling uses the wheel when the window is on
  screen, the page is painting and the pointer is over the scroller, and scrolls directly
  otherwise.
- **Settling.** After an action, `hba` waits until the DOM has been quiet for 100 ms and no
  document, fetch or XHR request started by the action is in flight (at most 1.5 s), and follows
  a navigation the action started. A link that opens another tab is not followed, and a
  navigation that ends without committing (a download, a 204 response) ends the wait.
- **Report.** The action's output lists what changed: the new URL, popups that joined the session,
  a dialog that opened, accepted alerts, console errors, failed requests and downloads.

## Dialogs

`alert` and `beforeunload` dialogs are accepted at once and reported. `confirm` and `prompt` stay
open for `hba dialog accept|dismiss`. A page showing a dialog answers no protocol call that needs
its renderer, so while one is open every page command fails at once with the dialog's text.

## Stealth

What a page could notice is kept to what a person's browser does anyway.

- No `Runtime.enable` or `Log.enable` on websites: console capture is automatic only while the
  tab shows a local page (localhost and loopback addresses, `.localhost` and `.test` hosts, `file:`,
  `data:`, `chrome:`, `devtools:` and extension pages, and `blob:` URLs of those) and is not
  navigating or redirected elsewhere, and needs `hba console --on` otherwise. The same rule decides
  where input is human-paced. Capture runs on a session of its own: while a navigation is pending
  Chromium holds back `Runtime.disable` until the next document has started, but detaching a
  session takes effect at once, before a website's document exists.
- Nothing is injected into the page's own JavaScript world; `hba js` evaluates without enabling
  any domain.
- Input is real input, human-paced on websites.
- The browser is the user's own: no automation flags, the user's profile, extensions and
  fingerprint, and with a fixed debugging port or in approval mode no `navigator.webdriver` (port
  0 sets it, which `hba doctor` reports). In approval mode Chrome shows its automation bar while
  `hba` is connected; the user turned that mode on and sees it.
- Emulation and interception apply to the tab's protocol sessions and vanish when `hba` detaches;
  `Fetch` is enabled only while block or mock rules exist. Permission overrides (`perms grant`,
  `emulate --geo`) apply to the whole profile and shadow its own settings. Once no tab `hba` holds
  in that profile keeps a grant, `hba` clears them (`Browser.resetPermissions`), so the profile's
  settings apply again; `perms reset` clears them at once.
- Screenshots never change the page's DOM: the requested part is cut out of a capture of the
  whole view, and `-a` labels are drawn onto the image, in `hba`'s isolated world. After a capture
  that resized the view, a style sheet is adopted and dropped within one task so the page lays out
  again at its own size.

## DevTools features

The `Network` and `Page` domains are on for every attached tab (they are invisible to pages), which
gives the request log, failed requests and response bodies. `perf metrics` turns `Performance` on at
first use and leaves it on; traces, CPU profiles, heap snapshots and coverage use their domains
only while recording. `hba devtools` opens the browser's own developer tools window for a human.

A screenshot captures the whole view, and `hba` cuts out the requested part, so Chromium does not
resize the view for it. The view comes from the compositor surface while the browser's window is
on screen (about 0.1 s). While it is not (another niri workspace, minimized), the browser draws no
surface, so `hba` grabs the view instead (about 1.5 s). Content beyond the viewport (`--full` on a
long page, an element taller than the view) needs a clipped capture, which Chromium renders by
resizing the view, and the window on screen. Whether the window is on screen comes from niri;
elsewhere a surface capture that does not answer falls back to the grab. When only some of the
browser's windows are on screen, or without niri, a capture beyond the viewport first checks with
a quick viewport capture and fails within about 4 s if the browser does not draw the tab.

## Extensions

- `ext load` and `ext rm` use the `Extensions` domain, which installs an unpacked extension into
  the default (last-used) profile until the browser restarts. Since Chromium 149 it works over the
  WebSocket without `--remote-debugging-pipe` or `--enable-unsafe-extension-debugging` (verified
  on Helium 156 and Chrome 156); a browser that refuses it gets an error naming that requirement.
- `ext eval`, `ext logs` and `ext sw` attach to the extension's service worker, background page or
  extension pages. An attached debugger keeps a service worker running, as DevTools does; captured
  logs continue across worker restarts and reloads.
- `ext errors` and `ext reload` use the browser's extension manager API from a hidden
  `chrome://extensions` page. Chromium records runtime errors only in developer mode, which
  `ext errors` turns on when needed and says so.
- `ext popup` opens the popup page in a session tab, so it can be read and driven like any page;
  an action without a popup is triggered on the current tab.
- `js --world EXT` evaluates in that extension's content-script world on the current page. Finding
  the world needs `Runtime`, which is enabled for that call only; on a website the output says so.

## Performance

Measured against the author's Helium 156 (97 open tabs, window on another workspace) with the
release build; times include the client process.

| Command | Time |
| --- | --- |
| `status`, `get url`, `js`, `text` | 2–4 ms |
| `tabs --all` (97 tabs) | 5–17 ms |
| `snap` / `snap -i` | 12–21 ms |
| `hover` | ~210 ms |
| `open https://example.com` | ~260 ms |
| `back` | ~265 ms |
| `click` that navigates | ~470 ms |
| `shot` with the window hidden | 1.7–4 s |

Actions spend most of their time waiting for the page to settle (100 ms of DOM quiet) and, on
websites, moving the pointer like a person. The daemon uses one thread and about 7 MB of memory.

## Source map

| Module | Role |
| --- | --- |
| `main`, `cli`, `client`, `ipc`, `commands`, `output` | Entry point, arguments, client process, socket protocol, dispatch, output formatting and private files |
| `daemon`, `session`, `config`, `trace`, `undo` | Daemon, sessions and tab ownership, configuration, timing log, cleanup that survives cancelled commands |
| `endpoint`, `cdp`, `browser`, `profiles`, `niri` | Endpoint discovery and verification, protocol transport, targets and contexts, profiles, window focus |
| `page`, `injected`, `snapshot` | Attached tab, isolated worlds and the Playwright engine, snapshots |
| `actions`, `input`, `keyboard`, `transfer` | Actionability, trusted input, keys, uploads and drag and drop |
| `inspect`, `nav`, `tabs`, `script`, `system` | Read commands, navigation, tabs and windows, escape hatches, browser and daemon commands |
| `console`, `network`, `routes`, `download`, `devtools` | Event buffers, interception and the DevTools commands |
| `media`, `emulate`, `storage`, `clip` | Screenshots and PDF, emulation, cookies and storage, clipboard |
| `extensions` | Extension catalog, debugger sessions and the extension manager |
