---
name: hba
description: "Drive the user's own running browser (Helium, Chrome or Brave, with their logged-in profiles) from the shell: open, read, click, fill, tabs, screenshots, console/network/performance, and extension development (load, reload, logs, errors, popup, storage). Use for any browser task that needs real interaction, the user's sessions, or Chromium DevTools."
---

# hba — the user's browser, from the shell

`hba` talks to the user's running browser (Helium by default; Chrome or Brave when configured)
over DevTools through a background daemon (each call takes milliseconds). It works in **its own
tabs** in the user's real, logged-in profiles. A plain public fetch needs no browser: use your
fetch/read tool for that.

## The loop
```sh
hba open example.com            # opens/navigates this session's tab; prints `t5 url "title"`
hba snap -i                     # interactive elements with refs: - button "Sign in" [ref=e12]
hba click e12                   # act by ref; prints what changed (→ new URL, + popup t6, ! dialog, ! errors)
hba fill e7 'me@example.com'    # replaces a field's text; reports the value if the field changed it
hba snap -d                     # only what changed since the last snapshot
```
- Refs (`e12`, frames `f1e3`) come from the latest `snap`/`find` and die when the page navigates:
  re-snap after navigation. A Playwright selector works anywhere a ref does: `'#id'`,
  `'text=Sign in'`, `'role=button[name="Save"]'`, `'internal:label="Email"i'` (`i`: any case, part
  of the label; without it the whole label must match). `hba get locator e12` prints a stable one.
- `snap` = full page (Playwright AI format, compacted). `-i` actions only, `-v` viewport only,
  `-S e40` one subtree, `-d` diff, `--boxes` coordinates. Long output spills to a file.
- `find 'checkout'` finds elements by text/name without printing the whole page.
- Read content: `hba text` (whole page) or `hba text e40`; `hba get url|title|value|attr|html|count|box|style|locator TARGET`;
  `hba is visible|enabled|checked|editable|focused TARGET` → true/false.

## Acting
`click` (`--count 2`, `--button right`, `--modifiers Control`), `hover`, `focus`, `fill` (`--submit`
presses Enter), `type 'text' --into e5` (real key events), `press Enter` / `press Control+a Delete`,
`check`/`uncheck`, `select e9 Pro`, `upload e4 ./file.pdf`, `drag e3 e8`, `scroll down 800`
(`up|down|left|right|top|bottom`, `-e TARGET`, `--into-view`), `mouse click 120 300`.
Actions wait for the element to be visible, enabled, still and unobstructed; a covered element
fails with the blocker's description (`click --force` skips the checks). `click`, `fill` and
`press` take `-d` to print a snapshot diff.
- Wait: `wait 'text=Done'`, `wait e5 --gone`, `wait --text 'Saved'`, `wait --url '*/checkout'`,
  `wait --load idle`, `wait --until 'window.app?.ready'`, `wait 500`.
- Dialogs: alerts are accepted and reported; a confirm/prompt blocks the page until
  `hba dialog accept [text]` / `hba dialog dismiss`.
- Popups opened by your tab join the session (`+ popup t6`); `hba tab use t6` to work in one.
- Downloads: `hba download e8 -o ./out/` saves and prints the path.

## Tabs, sessions, profiles
- A session (`-s NAME`, `$HBA_SESSION`, default: the project directory name) owns its tabs and has a
  current tab. Use a distinct `-s` per parallel agent.
- `hba tabs` (session tabs) · `hba tabs --all` (every tab, by profile and window, in tab-strip order).
- `hba tab new [URL]`, `hba tab close [t5]`, `hba tab front` (show it to the user), `-T t6` to target a
  session tab once without switching.
- **The user's own tabs are off-limits until you adopt one on request:** `hba tab use t42` (from
  `tabs --all`) makes it current; `hba tab release` hands it back untouched (so does `hba close`).
  `hba tab close` closes an adopted tab for good: never navigate or close one unless that is the
  request.
- Profiles: `hba profiles`; `-p Work` (display name, directory or configured alias) picks the profile
  for new tabs; project directories can map to profiles in `~/.config/hba/config.toml`.
- `hba window maximized|minimized|normal|fullscreen`, `hba window size 1280x800` change the tab's
  window.
- `hba status` summarizes; `hba close` closes the session's own tabs and releases adopted ones —
  run it when the task is done.

## Scripts and escape hatches
- `hba js 'expr'` runs in the page (top-level `await` ok; `--frame e5` for an iframe; `-` reads stdin).
- `hba cdp Domain.method '{"json":1}'` sends raw DevTools protocol to the tab (`--browser` for the browser).
- `hba run <<'EOF' … EOF` runs many hba lines in one call (stops at the first failure; `--keep-going`).
- `--json` gives machine-readable output for `tabs`, `status`, `tab new|use`, `open`, `js`, `profiles`.

## DevTools
- `hba console` / `hba errors`: console and exceptions are captured automatically on local pages
  (localhost and loopback addresses, `.localhost`/`.test` hosts, files, extensions). On websites
  capturing is detectable, so it starts only with `hba console --on`.
- `hba net [--failed] [--filter api]`, `net show 12`, `net body 12 [-o file]`, `net block '*ads*'`,
  `net mock '*/api/user' --body '{"name":"x"}'`, `net unroute`, `net har start` … `net har stop f.har`.
- `hba perf metrics|vitals`, `perf trace start` … `perf trace stop t.json` (DevTools Performance),
  `perf profile start|stop`, `perf heap`, `perf coverage start|stop`.
- `hba shot [path] [--full] [-e e5] [-a]` (`-a` labels refs on the image), `hba pdf`. Shots work while
  the browser's window is hidden; only captures past the viewport (`--full`, a tall `-e`) need it on screen.
- `hba emulate --device 'iPhone 15' | --viewport 390x844 --dpr 3 --mobile | --tz Asia/Tokyo |
  --locale de-DE | --geo 48.85,2.35 | --scheme dark | --network slow3g | --cpu 4`; `--reset`.
- `hba cookies`, `hba storage [local|session] …`, `hba state save|load f.json`, `hba perms grant geolocation`.
- `hba clip` prints the system clipboard (`hba clip TEXT` sets it), e.g. to check a page's Copy button.
- `hba devtools console` opens DevTools for the user to watch.

## Extension development
```sh
hba ext load ./dist             # installs the unpacked extension (until the browser restarts)
hba ext                         # extensions, versions, service-worker state
hba ext reload my-ext           # after a rebuild; id, name or directory
hba ext logs my-ext             # service-worker/page console (capture starts on first call)
hba ext errors my-ext           # manifest and runtime errors
hba ext eval my-ext 'await chrome.storage.local.get()'
hba ext popup my-ext            # opens the popup page in a tab → snap/click/fill it
hba ext open my-ext [page.html] # options page or any extension page
hba ext storage my-ext local set key '{"a":1}'
hba ext sw my-ext stop|start    # test service-worker restarts
hba js --world my-ext 'window.contentState'   # in the content script's world (turns Runtime on for the call: detectable)
```

## Behaviour to rely on
- Input is real (trusted) mouse and keyboard; on websites it moves and types like a person, on local
  pages it is instant. hba's own page code runs in a hidden isolated world. Don't add your own JS
  shortcuts (e.g. `element.click()` via `js`) when a real action exists — they look automated.
- Errors say what to run next (re-snap, accept a dialog, adopt a tab). Exit code 1 = failed, 2 = bad
  usage.
- Login walls, MFA, captchas, purchases and anything irreversible: stop and ask the user.
- `hba doctor` diagnoses connection problems; `hba browser launch|restart|endpoint`.
- **Approval mode** (Chrome's own profile, or any browser whose remote debugging was turned on at
  `chrome://inspect/#remote-debugging` instead of with a flag): the browser asks the user to allow
  each new DevTools connection and shows an automation notice while `hba` is connected. A command
  failing with "did not allow the DevTools connection … click Allow" means the dialog is waiting:
  ask the user to click Allow, then run the command again (`--timeout 120` gives them longer). The
  request stays open after a command gives up, so an Allow clicked later still counts; don't
  restart anything, and don't run `hba daemon stop` to "retry": every abandoned request leaves a
  dead dialog in front of the next one. One Allow lasts as long as the `hba` daemon runs, which
  in this mode does not exit for idleness.
