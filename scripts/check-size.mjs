#!/usr/bin/env node
// Size-budget checker for the `polis` binary (docs/distribution.md "Size").
//
//   node scripts/check-size.mjs [path/to/polis]     # report; warn on breach (exit 0)
//   node scripts/check-size.mjs --strict [path]     # CI: breach OR missing binary exits 1
//
// Budgets live in scripts/size-budget.json. Default path: the `dist`
// profile's output (`target/dist/polis`), else the release profile's.

import { readFileSync, statSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const strict = args.includes("--strict");
const given = args.find((a) => !a.startsWith("--"));
const budget = JSON.parse(readFileSync(join(root, "scripts", "size-budget.json"), "utf8"));
const candidates = given
  ? [given]
  : [join(root, "target", "dist", "polis"), join(root, "target", "release", "polis"), join(root, "target", "dist", "polis.exe"), join(root, "target", "release", "polis.exe")];
const bin = candidates.find((p) => existsSync(p));
const mb = (b) => (b / 1_000_000).toFixed(2) + " MB";

let failed = false;
if (!bin) {
  console.log(`SKIP  polis binary — not built (looked at ${candidates.join(", ")})`);
  if (strict) failed = true;
} else {
  const actual = statSync(bin).size;
  const limit = budget.binaryBytes;
  const ok = actual <= limit;
  console.log(`${ok ? " ok " : "OVER"}  ${bin} — ${mb(actual)} of ${mb(limit)} (${((actual / limit) * 100).toFixed(1)}%)`);
  if (!ok) failed = true;
}
if (failed) {
  console.error(strict ? "\nsize budget exceeded (or binary missing) — see docs/distribution.md 'Size'" : "\nWARNING: size budget exceeded — see docs/distribution.md 'Size'");
  if (strict) process.exit(1);
}
