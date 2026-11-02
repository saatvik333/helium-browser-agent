# Set up hba (instructions for a coding agent)

You are setting up `hba`, a CLI that lets coding agents drive the user's own running Helium
browser. Follow these steps in order. Run each command yourself, check its result, and stop to ask
the user only where a step says so. Tell the user what you did at the end.

## 1. Check the machine

- `uname -s` must print `Linux`. Otherwise stop: `hba` supports Linux only.
- Find Helium: `command -v helium-browser || ls /opt/helium-browser-bin/helium`. If neither exists,
  stop and tell the user to install Helium from https://helium.computer/.
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

## 3. Turn on Helium's DevTools port

`hba` needs Helium started with a remote debugging port on loopback.

```sh
flags="${XDG_CONFIG_HOME:-$HOME/.config}/helium-browser-flags.conf"
grep -q -- '--remote-debugging-port=' "$flags" 2>/dev/null || echo '--remote-debugging-port=9222' >> "$flags"
```

Then check whether the running Helium already has it: `pgrep -af -- '--remote-debugging-port'`.
If Helium is running without it, **ask the user** to restart Helium (or allow you to run
`hba browser restart`, which reopens their windows). Never kill the browser without asking. Tell
the user that this port gives full control of the browser to local programs, which is why `hba`
only accepts it on loopback.

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

`doctor` must report the endpoint as verified; `snap -i` must list the page's "Learn more" link.
If `doctor` reports a problem, follow what it says, then run it again.

## 6. Report

Tell the user: where `hba` is installed, which skill directories you linked, whether Helium needs
a restart, and that updating later is this same prompt again (it pulls and reinstalls).
