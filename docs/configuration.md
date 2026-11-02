# Configuration

`hba` needs no configuration when Helium uses its default data directory and `helium-browser` is
on `PATH`. Settings come from `$XDG_CONFIG_HOME/hba/config.toml` (or the file named by
`HBA_CONFIG`) and from `HBA_*` variables; a variable overrides the file. The daemon reloads the
file when it changes.

Everything is validated when it is read. An unknown key, a wrong type, an empty value or a bad
variable stops the command with an error naming the file, key or variable.

| File key | Variable | Default | Meaning |
| --- | --- | --- | --- |
| `user_data_dir` | `HBA_USER_DATA_DIR` | `$XDG_CONFIG_HOME/net.imput.helium` | Helium's user data directory: `Local State` lists its profiles, and the running Helium holds its `SingletonLock`. |
| `launcher` | `HBA_LAUNCHER` | `helium-browser` | Command that starts Helium, or opens a window of another profile in the running Helium. |
| `default_profile` | | Helium's last-used profile | Profile for new tabs when no flag, variable, session or directory rule picks one. Display name or directory (`Default`, `Profile 3`). |
| `humanize` | `HBA_HUMANIZE` | `auto` | Input timing. `auto`: curved mouse paths and typing rhythm on websites, instant on local, file and extension pages. `always` or `never`. |
| `idle_minutes` | | `30` | The daemon exits after this many minutes with no command running; `0` keeps it running. While the file does not load, the default applies. Tabs stay open either way. |
| `[aliases]` | | | Short profile names: `work = "Client Work"` makes `-p work` pick that profile. |
| `[workspaces]` | | | Directory to profile rules for new tabs. Keys must be absolute directories or start with `~/`. The longest matching directory wins; matching is by whole path components. |
| | `HBA_CONFIG` | `$XDG_CONFIG_HOME/hba/config.toml` | The config file. When set, the file must exist. |
| | `HBA_CDP` | discovery | A `ws://` DevTools URL to use instead of discovering and verifying Helium's endpoint. |
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

## Profile for new tabs

The first of these that is set decides:

1. `-p NAME` or `HBA_PROFILE`.
2. The profile the session used before.
3. The `[workspaces]` rule for the working directory.
4. `default_profile`.
5. Helium's last-used profile.

`NAME` may be a display name (`Personal`), a directory (`Default`, `Profile 3`) or an alias.
`hba profiles` lists them.

## Endpoint

Helium must run with `--remote-debugging-port` (put it in `~/.config/helium-browser-flags.conf`;
9222 is the usual choice). `hba` finds the Helium that holds the user data directory, takes the port
from that process's flags (or from `DevToolsActivePort` for port 0), and connects only after checking
that the port listens on loopback alone and belongs to that process. Several Helium instances with
different data directories are told apart this way.
When Helium is not running, commands that need a page start it through the launcher; status and
cleanup commands never do.

## Runtime files

`$XDG_RUNTIME_DIR/hba/` holds the daemon socket (`hba.sock`), its lock, its log (`daemon.log`) and
`state.json` (sessions, their tabs and which browser context is which profile). They are recreated
as needed; deleting the directory while no daemon runs resets every session. Without
`XDG_RUNTIME_DIR`, `$TMPDIR/hba-UID` (`/tmp/hba-UID` by default) is used, and it must be a private
(0700) directory of yours, not a link.
