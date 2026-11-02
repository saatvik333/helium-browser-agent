# Set up hba (instructions for a coding agent)

You are setting up `hba`, a CLI that lets coding agents drive the user's own running browser:
Helium (the default and recommended), Google Chrome or Brave. Follow these steps in order. Run each
command yourself, check its result, and stop to ask the user only where a step says so. Tell the
user what you did at the end.

## 1. Check the machine

- `uname -s` must print `Linux`. Otherwise stop: `hba` supports Linux only.
- Find the browser: Helium is `command -v helium-browser || ls /opt/helium-browser-bin/helium`,
  Chrome is `command -v google-chrome-stable`, Brave is `command -v brave`. Use Helium when it is
  installed. If only Chrome or Brave is, **ask the user** which browser `hba` should drive. If
  none is, stop and tell the user to install Helium from https://helium.computer/.
- Rust 1.89 or newer: `cargo --version`. If cargo is missing or older, ask the user before
  installing or updating it with rustup (https://rustup.rs).
- `git --version` must work.

## 2. Install

```sh
src="${XDG_DATA_HOME:-$HOME/.local/share}/hba"
if [ -d "$src/.git" ]; then git -C "$src" pull --ff-only; else git clone https://github.com/saatvik333/helium-browser-agent "$src"; fi
cargo install --path "$src" --locked
hba --version
```

If `hba` is not found afterwards, `~/.cargo/bin` is missing from `PATH`: tell the user which shell
file to add it to.

## 3. Turn on the browser's DevTools access

**Helium** (and Brave): start it with a remote debugging port on loopback.

```sh
flags="${XDG_CONFIG_HOME:-$HOME/.config}/helium-browser-flags.conf"   # Brave: brave-flags.conf
grep -q -- '--remote-debugging-port=' "$flags" 2>/dev/null || echo '--remote-debugging-port=9222' >> "$flags"
```

Then check whether the running browser already has it: `pgrep -af -- '--remote-debugging-port'`.
If it is running without it, **ask the user** to restart it (or allow you to run
`hba browser restart`, which reopens their windows). Never kill the browser without asking.

For Brave, also tell `hba` which browser to drive (Brave support is untested):

```sh
config="${XDG_CONFIG_HOME:-$HOME/.config}/hba/config.toml"
mkdir -p "$(dirname "$config")" && grep -q '^browser' "$config" 2>/dev/null || echo 'browser = "brave"' >> "$config"
```

**Chrome**: Chrome ignores `--remote-debugging-port` on the user's own profile, and the flag even
keeps remote debugging off there, so do not add it to `chrome-flags.conf` (remove it if it is
there). Write `browser = "chrome"` to the config as above, then **ask the user** to open
`chrome://inspect/#remote-debugging` in Chrome and turn on "Allow remote debugging for this
browser instance". From then on Chrome asks them to Allow each new connection (`hba` keeps one
open while its daemon runs), and shows a "Chrome is being controlled by automated test software"
bar while `hba` is connected; that is expected.

Tell the user that DevTools access gives full control of the browser to local programs, which is
why `hba` only accepts it on loopback.

## 4. Teach the agents

Link the skill into every agent skills directory that exists on this machine, creating
`~/.agents/skills` if none does:

```sh
for dir in ~/.agents/skills ~/.claude/skills ~/.codex/skills; do
  [ -d "$dir" ] || [ "$dir" = ~/.agents/skills ] || continue
  mkdir -p "$dir" && ln -sfn "$src/skill" "$dir/hba"
done
```

If the agent you are running in reads skills from another directory, link it there too.

## 5. Verify

```sh
hba doctor
hba -s setup open https://example.com
hba -s setup snap -i
hba -s setup close
```

`doctor` must report the endpoint as verified and connected; `snap -i` must list the page's "Learn
more" link. With Chrome, run `hba --timeout 120 doctor` and ask the user to click Allow in the
dialog Chrome shows; if the command gives up first, the request stays open, so an Allow clicked
later still counts for the next command. If `doctor` reports a problem, follow what it says, then
run it again.

## 6. Report

Tell the user: where `hba` is installed, which browser it drives, which skill directories you
linked, whether the browser needs a restart (or, for Chrome, the switch and the Allow click), and
that updating later is this same prompt again (it pulls and reinstalls).
