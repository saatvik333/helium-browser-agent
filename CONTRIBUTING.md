# Contributing

Start with the README and [architecture](docs/architecture.md). Describe the concrete failure or
behavior you want to change, with the Helium version (`hba doctor`) and the commands that show it.

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
HBA_LIVE=1 cargo test --test live    # starts its own headless Helium; never touches yours
```

- Fix a bug with a test that fails without the fix: a unit test for pure logic, `tests/cli.rs` for
  daemon and configuration behavior, `tests/live.rs` with a page in `tests/fixtures/site/` for
  anything that needs a browser.
- Keep websites unable to tell `hba` is there: no `Runtime.enable` or `Log.enable` on websites, no
  code in the page's own world, real input. A change that needs more says why in the code.
- Output is read by models: one line per fact, no decoration, and errors that name the next
  command to run.
- Update `assets/playwright` only with `bun scripts/vendor-playwright.mjs`, as its own change, and
  run the live suite with it.
- Never share logs, HAR files or screenshots from a real profile; use a fixture page.

Contributions are under the project's Apache-2.0 license.
