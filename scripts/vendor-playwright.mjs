// Regenerates assets/playwright/*, the YAML quoting oracle in tests/fixtures and the version in
// NOTICE from a pinned playwright-core release.
// Usage: bun scripts/vendor-playwright.mjs   (needs npm, tar and bun on PATH)
// To upgrade: set VERSION, run once (the integrity check fails and prints the new value), check
// the value against `npm view playwright-core@VERSION dist.integrity`, set INTEGRITY, run again.
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const VERSION = "1.64.1";
const INTEGRITY = "sha512-BC4s8y6SEVA6Q1rSxZTto8hI1KUI3xXlHGZgvGqP4IqPdI/a6FX9H29IDNz7vKo9l21b6LfYosScPL6M3xxyDQ==";
const root = fileURLToPath(new URL("..", import.meta.url));
const out = join(root, "assets/playwright");
const work = mkdtempSync(join(tmpdir(), "hba-vendor-"));

try {
  execFileSync("npm", ["pack", `playwright-core@${VERSION}`, "--silent"], { cwd: work, stdio: ["ignore", "ignore", "inherit"] });
  const tarball = readFileSync(join(work, `playwright-core-${VERSION}.tgz`));
  const integrity = `sha512-${createHash("sha512").update(tarball).digest("base64")}`;
  if (integrity !== INTEGRITY) throw new Error(`playwright-core@${VERSION} has integrity ${integrity}, expected ${INTEGRITY}`);
  execFileSync("tar", ["-xzf", `playwright-core-${VERSION}.tgz`], { cwd: work });
  const pkg = join(work, "package");
  const bundle = readFileSync(join(pkg, "lib/coreBundle.js"), "utf8");
  mkdirSync(out, { recursive: true });

  // The injected script is embedded as one single-quoted string literal.
  const injected = /\/\/ packages\/playwright-core\/src\/generated\/injectedScriptSource\.ts[\s\S]*?\bsource\d* = ('(?:[^'\\]|\\.)*');/.exec(bundle);
  if (!injected) throw new Error("injected script source not found; the bundle layout changed");
  const source = Function(`return ${injected[1]}`)();
  if (!source.includes("module.exports") || !source.includes("ariaSnapshotJSON(")) throw new Error("unexpected injected script shape");
  // One function expression: no top-level names leak into the world and the minifier may rename everything.
  const wrapped = join(work, "injected.js");
  writeFileSync(wrapped, `globalThis.__hbaFactory = function (options) {\nconst module = {};\n${source}\nreturn new (module.exports.InjectedScript())(globalThis, options);\n};\n`);
  execFileSync("bun", ["build", "--no-bundle", "--minify", wrapped, "--outfile", join(work, "injected.min.js")], { stdio: ["ignore", "ignore", "inherit"] });
  const minified = readFileSync(join(work, "injected.min.js"), "utf8").trim();
  const factory = /^globalThis\.__hbaFactory=(function[\s\S]*);?$/.exec(minified);
  if (!factory) throw new Error("minifier output has an unexpected shape");
  writeFileSync(join(out, "injected.js"), factory[1].replace(/;$/, "") + "\n");

  // Keyboard layout: an object literal that refers to `keypadLocation`.
  const layout = /USKeyboardLayout = (\{[\s\S]*?\n {4}\});/.exec(bundle);
  if (!layout) throw new Error("USKeyboardLayout not found");
  const keys = Function("keypadLocation", `return ${layout[1]}`)(3);
  writeFileSync(join(out, "us-keyboard-layout.json"), JSON.stringify(keys) + "\n");

  const { devices } = createRequire(import.meta.url)(pkg);
  const compact = Object.fromEntries(
    Object.keys(devices)
      .sort()
      .map((name) => {
        const { userAgent, viewport, screen, deviceScaleFactor, isMobile, hasTouch } = devices[name];
        return [name, { userAgent, viewport, screen, deviceScaleFactor, isMobile, hasTouch }];
      }),
  );
  writeFileSync(join(out, "devices.json"), JSON.stringify(compact) + "\n");
  for (const file of ["LICENSE", "NOTICE"]) writeFileSync(join(out, file), readFileSync(join(pkg, file)));
  writeFileSync(join(out, "VERSION"), `playwright-core ${VERSION}\n`);

  // The YAML quoting oracle: the inputs stay as they are, the answers come from Playwright's own
  // functions. Each case is [text, needs quotes, as a value, as a key].
  const yamlSource = /\/\/ packages\/isomorphic\/yaml\.ts\n([\s\S]*?)\nvar init_yaml\b/.exec(bundle);
  if (!yamlSource) throw new Error("yaml helpers not found; the bundle layout changed");
  const yaml = Function(`${yamlSource[1]}\nreturn { yamlStringNeedsQuotes, yamlEscapeValueIfNeeded, yamlEscapeKeyIfNeeded };`)();
  const oracle = join(root, "tests/fixtures/playwright-yaml-quoting.json");
  const inputs = JSON.parse(readFileSync(oracle, "utf8")).map(([text]) => text);
  const cases = inputs.map((text) => [text, yaml.yamlStringNeedsQuotes(text), yaml.yamlEscapeValueIfNeeded(text), yaml.yamlEscapeKeyIfNeeded(text)]);
  writeFileSync(oracle, JSON.stringify(cases));

  const notice = join(root, "NOTICE");
  const text = readFileSync(notice, "utf8");
  const updated = text.replace(/Playwright \d+\.\d+\.\d+ \(playwright-core\)/, `Playwright ${VERSION} (playwright-core)`);
  if (updated === text && !text.includes(`Playwright ${VERSION} (playwright-core)`)) throw new Error("NOTICE does not name the Playwright version");
  writeFileSync(notice, updated);
  console.log(`vendored playwright-core ${VERSION} into ${out}`);
} finally {
  rmSync(work, { recursive: true, force: true });
}
