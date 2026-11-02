# Configuration

`hba` needs no configuration when Helium uses its default data directory and `helium-browser` is
on `PATH`. For Chrome or Brave, set `browser`. Settings come from `$XDG_CONFIG_HOME/hba/config.toml`
(or the file named by `HBA_CONFIG`) and from `HBA_*` variables; a variable overrides the file. The
daemon reloads the file when it changes.

Everything is validated when it is read. An unknown key, a wrong type, an empty value or a bad
variable stops the command with an error naming the file, key or variable.

| File key | Variable | Default | Meaning |
| --- | --- | --- | --- |
| `browser` | `HBA_BROWSER` | `helium` | The browser to drive: `helium`, `chrome` or `brave`. It picks the defaults below, the process `hba` looks for and the names in messages. |
| `user_data_dir` | `HBA_USER_DATA_DIR` | the browser's (see below) | The browser's user data directory: `Local State` lists its profiles, and the running browser holds its `SingletonLock`. |
| `launcher` | `HBA_LAUNCHER` | the browser's (see below) | Command that starts the browser, or opens a window of another profile in the running browser. |
| `default_profile` | | the browser's last-used profile | Profile for new tabs when no flag, variable, session or directory rule picks one. Display name or directory (`Default`, `Profile 3`). |
| `humanize` | `HBA_HUMANIZE` | `auto` | Input timing. `auto`: curved mouse paths and typing rhythm on websites, instant on local, file and extension pages. `always` or `never`. |
| `idle_minutes` | | `30` | The daemon exits after this many minutes with no command running; `0` keeps it running. In approval mode it stays while it holds a connection or a request awaiting Allow, since a new daemon means a new dialog. While the file does not load, the default applies. Tabs stay open either way. |
| `[aliases]` | | | Short profile names: `work = "Client Work"` makes `-p work` pick that profile. |
| `[workspaces]` | | | Directory to profile rules for new tabs. Keys must be absolute directories or start with `~/`. The longest matching directory wins; matching is by whole path components. |
| | `HBA_CONFIG` | `$XDG_CONFIG_HOME/hba/config.toml` | The config file. When set, the file must exist. |
| | `HBA_CDP` | discovery | A `ws://` DevTools URL to use instead of discovering and verifying the browser's endpoint. |
| | `HBA_SESSION` | project directory name | Default for `-s`: the git root's name, else the working directory's. 1–48 characters of `A-Z a-z 0-9 . _ -`. |
| | `HBA_PROFILE` | | Default for `-p`. |
| | `HBA_TRACE` | off | `1` writes per-command timings to the daemon log. |

```toml
# ~/.config/hba/config.toml
default_profile = "Personal"
idle_minutes = 60

[aliases]
work = "Client Work"

[workspaces]
"~/code/client" = "Client Work"
```

| `browser` | Process | Default `user_data_dir` | Default `launcher` | Flags file | Tested |
| --- | --- | --- | --- | --- | --- |
| `helium` | `helium` | `$XDG_CONFIG_HOME/net.imput.helium` | `helium-browser` | `helium-browser-flags.conf` | yes (Helium 156) |
| `chrome` | `chrome` | `$XDG_CONFIG_HOME/google-chrome` | `google-chrome-stable` | `chrome-flags.conf` | yes (Chrome 156, throwaway profiles: the live suite with a flag, and approval mode with a person clicking Allow) |
| `brave` | `brave` | `$XDG_CONFIG_HOME/BraveSoftware/Brave-Browser` | `brave` | `brave-flags.conf` | no: the defaults follow Brave's Linux packages and are unverified |

The flags file (under `$XDG_CONFIG_HOME`) is where the Arch Linux packages' launchers read extra
flags; `hba doctor` reads it for its advice. Profile names in `default_profile`, `[aliases]` and
`[workspaces]` are those of the configured browser.

## Profile for new tabs

The first of these that is set decides:

1. `-p NAME` or `HBA_PROFILE`.
2. The profile the session used before.
3. The `[workspaces]` rule for the working directory.
4. `default_profile`.
5. The browser's last-used profile.

`NAME` may be a display name (`Personal`), a directory (`Default`, `Profile 3`) or an alias.
`hba profiles` lists them.

## Endpoint

`hba` finds the browser process that holds the user data directory (its `SingletonLock`), gets its
DevTools port, and connects only after checking that the port listens on loopback alone and belongs
to that process. Several browsers with different data directories are told apart this way. Remote
debugging comes on in one of two ways:

- **A flag (recommended where it works).** `--remote-debugging-port=9222` on the browser's command
  line, for Helium and Brave in their flags file. `hba` takes the port from the process's flags
  (port 0 from `DevToolsActivePort`, but port 0 makes `navigator.webdriver` true). Connections need
  no approval. Helium and Brave honor the flag on any data directory. Chrome 136 and later ignores
  it on its default data directory, and then keeps remote debugging off altogether, so do not set
  it there; and since `chrome-flags.conf` reaches every Chrome start, pass the flag to a separate
  `--user-data-dir` on that command line instead.
- **Approval mode.** "Allow remote debugging for this browser instance" at
  `chrome://inspect/#remote-debugging` (Brave: `brave://inspect/#remote-debugging`), with no flag.
  The browser listens on a loopback port it writes to `DevToolsActivePort`, and asks its user to
  allow each new connection in an "Allow remote debugging?" dialog in its last used window.
  Chrome shows these dialogs one at a time, oldest first, and a dialog stays when its request goes
  away, where Allow then connects nothing. So `hba` never gives a request up: the daemon keeps
  exactly one open, a command waits for the click up to its `--timeout` (30 s by default), and
  when it gives up the request stays open, so an Allow that comes later still connects and the
  next command uses it (`hba daemon status` says when a request is waiting). Commands meanwhile
  wait for the same request instead of opening dialogs of their own. While `hba` is connected,
  Chrome shows its "Chrome is being controlled by automated test software" bar; that is
  expected. In this mode the daemon does not exit for idleness, so you are asked again only when
  a new daemon starts (after an update, a change of `HBA_*` variables or `hba daemon stop`) or
  the browser restarts. A daemon that has to exit while its request is unanswered waits up to a
  minute for the answer first; if none comes, the next daemon's error says that the first dialog
  connects nothing (Cancel it, then Allow the next). This is the only way to drive Chrome's own
  profile.

`hba doctor` says which mode the browser is in and what to change. When the browser is not
running, commands that need a page start it through the launcher; status and cleanup commands
never do.

## Runtime files

`$XDG_RUNTIME_DIR/hba/` holds the daemon socket (`hba.sock`), its lock, its log (`daemon.log`) and
`state.json` (sessions, their tabs and which browser context is which profile). They are recreated
as needed; deleting the directory while no daemon runs resets every session. Without
`XDG_RUNTIME_DIR`, `$TMPDIR/hba-UID` (`/tmp/hba-UID` by default) is used, and it must be a private
(0700) directory of yours, not a link.
