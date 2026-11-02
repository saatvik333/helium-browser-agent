// Regenerates assets/playwright/* from a pinned playwright-core release.
// Usage: bun scripts/vendor-playwright.mjs   (needs npm, tar and bun on PATH)
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";

const VERSION = "1.64.1";
const root = new URL("..", import.meta.url).pathname;
const out = join(root, "assets/playwright");
const work = mkdtempSync(join(tmpdir(), "hba-vendor-"));

try {
  execFileSync("npm", ["pack", `playwright-core@${VERSION}`, "--silent"], { cwd: work, stdio: ["ignore", "ignore", "inherit"] });
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
  console.log(`vendored playwright-core ${VERSION} into ${out}`);
} finally {
  rmSync(work, { recursive: true, force: true });
}
