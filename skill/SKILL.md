---
name: hba
description: "Use the user's own running browser (Helium, Chrome or Brave), signed in to their accounts, from the shell: open pages, read them, click and fill forms, manage tabs, take screenshots, read the console, network and performance data, and develop extensions (load, reload, logs, errors, popup, storage). Use it for any browser task that needs real interaction, the user's logins, or Chromium DevTools."
---

# hba: the user's own browser, from the shell

`hba` drives the browser the user already has open (Helium, Chrome or Brave) in **your own tabs**,
with their profiles and logins. For a public page, use your fetch tool.

## The loop
```sh
hba open example.com            # prints `t5 url "title"`
hba snap -i                     # refs: - button "Sign in" [ref=e12]
hba click e12                   # prints what changed (→ new URL, + popup t6, ! dialog, ! errors)
hba fill e7 'me@example.com'
hba snap -d                     # only what changed
hba close                       # when done: closes your tabs, releases adopted ones
```
- Refs (`e12`, in frames `f1e3`) come from the latest `snap`/`find` and die on navigation: re-snap.
  A Playwright selector works wherever a ref does: `'#id'`, `'text=Sign in'`,
  `'role=button[name="Save"]'`, `'internal:label="Email"i'` (`i`: any case, partial match).
- `snap` flags: `-i` actions only, `-v` viewport, `-S e40` one subtree, `-d` diff, `--boxes`
  coordinates. Long output spills to a file; files are deleted after a day.
- Read: `hba find 'checkout'` (elements by text or name), `hba text [e40]`,
  `hba get url|title|value|attr|html|inner|count|box|style|locator TARGET` (`locator`: a stable
  selector), `hba is visible|enabled|checked|editable|focused TARGET` (prints true or false).

## Acting
`click` (`--count 2`, `--button right`, `--modifiers Control`), `hover`, `focus`, `fill` (`--submit`
presses Enter), `type 'text' --into e5` (real key events), `press Enter` / `press Control+a Delete`,
`check`/`uncheck`, `select e9 Pro`, `upload e4 ./file.pdf`, `drag e3 e8`, `scroll down 800`
(`up|down|left|right|top|bottom`, `-e TARGET`, `--into-view`), `mouse click 120 300`.
Actions wait until the element is actionable and name any blocker (`click --force` skips the
checks). `click`, `fill` and `press` take `-d` for a snapshot diff.
- Wait: `wait 'text=Done'`, `wait e5 --gone`, `wait --text 'Saved'`, `wait --url '*/checkout'`,
  `wait --load idle`, `wait --until 'window.app?.ready'`, `wait 500`.
- Alerts are accepted and reported; a confirm/prompt blocks the page until
  `hba dialog accept [text]` / `hba dialog dismiss`.
- Popups your tab opens join the session (`+ popup t6`): `hba tab use t6` to work in one.
- `hba download e8 -o ./out/` clicks, saves and prints the path.

## Tabs, sessions, profiles
- A session (`-s NAME`, `$HBA_SESSION`, default: the project directory name) owns its tabs and has a
  current tab. Use a distinct `-s` per parallel agent.
- `hba tabs` lists session tabs; `--all` lists every tab by profile and window.
- `hba tab new [URL]`, `hba tab close [t5]`, `hba tab front` (show it to the user); `-T t6` targets
  a session tab once without switching.
- **The user's own tabs are off-limits unless asked.** `hba tab use t42` (from `tabs --all`) adopts
  one, `hba tab release` hands it back untouched, `hba tab close` closes it for good: never navigate
  or close one unless that is the request.
- Profiles: `hba profiles`; `-p Work` (display name, directory or alias) picks the profile for new
  tabs.
- `hba window maximized|minimized|normal|fullscreen|size 1280x800`; `hba status`.

## Scripts and escape hatches
- `hba js 'expr'` runs in the page (top-level `await` ok; `--frame e5` for an iframe; `-` reads stdin).
- `hba cdp Domain.method '{"json":1}'` sends raw DevTools protocol (`--browser`: to the browser).
- `hba run <<'EOF' … EOF` runs many hba lines in one call (stops at the first failure; `--keep-going`).
- `--json`: machine-readable output for `tabs`, `status`, `tab new|use`, `open`, `js`, `profiles`.

## DevTools
- `hba console` / `hba errors` capture automatically on local pages (localhost, loopback,
  `.localhost`/`.test`, files, extensions); on websites capturing is detectable, so start it with
  `hba console --on`.
- `hba net [--failed] [--filter api]`, `net show 12`, `net body 12 [-o file]`, `net block '*ads*'`,
  `net mock '*/api/user' --body '{"name":"x"}'`, `net unroute`, `net har start` … `net har stop f.har`.
- `hba perf metrics|vitals`, `perf trace start` … `perf trace stop t.json`, `perf profile start|stop`,
  `perf heap`, `perf coverage start|stop`.
- `hba shot [path] [--full] [-e e5] [-a]` (`-a` labels refs), `hba pdf`. Shots work with the
  browser's window hidden; captures past the viewport (`--full`, a tall `-e`) need it on screen.
- `hba emulate --device 'iPhone 15' | --viewport 390x844 --dpr 3 --mobile | --tz Asia/Tokyo |
  --locale de-DE | --geo 48.85,2.35 | --scheme dark | --network slow3g | --cpu 4`; `--reset`.
- `hba cookies`, `hba storage [local|session] …`, `hba state save|load f.json` (load replaces the
  site's cookies and the saved storage areas), `hba perms grant geolocation`.
- `hba clip` prints the clipboard (`hba clip TEXT` sets it); `hba devtools console` opens DevTools
  for the user.

## Extension development
- `hba ext` lists extensions; `hba ext load ./dist` installs an unpacked one (until the browser
  restarts). EXT below is an id, name or directory.
- `ext reload EXT`, `ext errors EXT`, `ext logs EXT` (capture starts on first call),
  `ext eval EXT 'await chrome.storage.local.get()'`, `ext storage EXT local set key '{"a":1}'`,
  `ext sw EXT stop|start`.
- `ext popup EXT` opens the popup in a tab; `ext open EXT [page.html]` opens the options page or
  any extension page.
- `hba js --world EXT 'window.contentState'` runs in the content-script world (enables Runtime for
  the call: detectable).

## Rules
- Input is real. Don't use JS shortcuts (`element.click()` via `js`) when a real action exists:
  they look automated.
- Errors say what to run next (re-snap, answer a dialog, adopt a tab). Exit code 1 = failed,
  2 = bad usage.
- Login walls, MFA, captchas, purchases and anything irreversible: stop and ask the user.
- `hba doctor` for connection problems; `hba browser launch|restart|endpoint`.
- **Approval mode** (Chrome's own profile): on "did not allow the DevTools connection … click
  Allow", ask the user to click Allow and rerun (`--timeout 120` waits longer). Never restart
  anything or run `hba daemon stop`.
