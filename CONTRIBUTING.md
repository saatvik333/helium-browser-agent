# Contributing

Start with the README and [architecture](docs/architecture.md). Describe the concrete failure or
behavior you want to change, with the browser and its version (`hba doctor`) and the commands that
show it.

```sh
cargo fmt
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo test --test live -- --ignored    # starts its own headless Helium; never touches yours
HBA_BROWSER=chrome HBA_HELIUM=/opt/google/chrome/chrome cargo test --test live -- --ignored   # against Chrome
bun scripts/vendor-playwright.mjs      # refresh assets/playwright from the pinned version
```

`rust-toolchain.toml` pins the toolchain CI uses; the oldest supported compiler is `rust-version`
in `Cargo.toml`, which CI checks with `cargo +1.89.0 check`.

- Fix a bug with a test that fails without the fix: a unit test for pure logic, `tests/cli.rs` for
  daemon and configuration behavior, `tests/live.rs` with a page in `tests/fixtures/site/` for
  anything that needs a browser.
- Keep websites unable to tell `hba` is there: no `Runtime.enable` or `Log.enable` on websites, no
  code in the page's own world, real input. A change that needs more says why in the code.
- Output is read by models: one line per fact, no decoration, and errors that name the next
  command to run.
- Update `assets/playwright` only with `bun scripts/vendor-playwright.mjs`, as its own change, and
  run the live suite with it. The script also rewrites `tests/fixtures/playwright-yaml-quoting.json`
  and the version in `NOTICE`; commit them together.
- Never share logs, HAR files or screenshots from a real profile; use a fixture page.

Contributions are under the project's Apache-2.0 license.
