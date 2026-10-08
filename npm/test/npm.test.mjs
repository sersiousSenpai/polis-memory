// SPDX-License-Identifier: Apache-2.0
import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { build, TARGETS } from "../build.mjs";

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const launcherPath = join(here, "..", "polis-memory", "bin", "polis.js");
const { binaryPath, PLATFORMS } = require(launcherPath);

test("the launcher maps every built target to its platform package", () => {
  assert.deepEqual(Object.keys(PLATFORMS).sort(), Object.values(TARGETS).map((t) => t.name).sort());
  const seen = [];
  const resolve = (spec) => { seen.push(spec); return `/nm/${spec}`; };
  assert.equal(binaryPath("darwin", "arm64", resolve), "/nm/@polis-memory/darwin-arm64/bin/polis");
  assert.equal(binaryPath("win32", "x64", resolve), "/nm/@polis-memory/win32-x64/bin/polis.exe");
  assert.throws(() => binaryPath("freebsd", "x64", resolve), /no prebuilt binary for freebsd-x64/);
  assert.throws(() => binaryPath("linux", "x64", () => { throw new Error("missing"); }), /optional dependency/);
});

test("the launcher runs the binary with the same arguments and exit code", () => {
  if (process.platform === "win32") return;
  const dir = mkdtempSync(join(tmpdir(), "polis-launch-"));
  const fake = join(dir, "polis");
  writeFileSync(fake, '#!/bin/sh\necho "args:$* launcher:$POLIS_LAUNCHER"\nexit 3\n');
  chmodSync(fake, 0o755);
  const r = spawnSync(process.execPath, [launcherPath, "setup", "--dry-run"], { env: { ...process.env, POLIS_BINARY: fake }, encoding: "utf8" });
  assert.equal(r.stdout.trim(), "args:setup --dry-run launcher:npm");
  assert.equal(r.status, 3);
});

test("build stages one package per archive and pins the launcher to the version", () => {
  if (process.platform === "win32") return;
  const root = mkdtempSync(join(tmpdir(), "polis-npm-build-"));
  const archives = join(root, "archives");
  for (const triple of ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"]) {
    const dir = join(root, "src", `polis-memory-${triple}`);
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, "polis"), "#!/bin/sh\necho polis\n");
    mkdirSync(archives, { recursive: true });
    execFileSync("tar", ["-cJf", join(archives, `polis-memory-${triple}.tar.xz`), "-C", join(root, "src"), `polis-memory-${triple}`]);
  }
  const out = join(root, "out");
  const result = build({ version: "0.2.0", archives, out });
  assert.deepEqual(result.platforms.sort(), ["darwin-arm64", "linux-x64"]);
  const plat = JSON.parse(readFileSync(join(out, "linux-x64", "package.json"), "utf8"));
  assert.equal(plat.name, "@polis-memory/linux-x64");
  assert.deepEqual([plat.os, plat.cpu, plat.version], [["linux"], ["x64"], "0.2.0"]);
  assert.ok(existsSync(join(out, "linux-x64", "bin", "polis")));
  const launcher = JSON.parse(readFileSync(join(out, "polis-memory", "package.json"), "utf8"));
  assert.equal(launcher.version, "0.2.0");
  assert.equal(launcher.scripts, undefined);
  assert.ok(Object.values(launcher.optionalDependencies).every((v) => v === "0.2.0"));
  assert.equal(launcher.bin.polis, "bin/polis.js");
  assert.throws(() => build({ version: "latest", archives, out }), /not a version/);
});
