#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// The `polis` launcher: finds the prebuilt binary npm installed for this
// platform (an optional dependency, @polis-memory/<platform>) and runs it
// with the same arguments, stdio and exit code. `polis setup` copies the
// binary somewhere stable before wiring hooks to it, so clearing the npm
// cache never breaks an install.
"use strict";

const { spawnSync } = require("node:child_process");

const PLATFORMS = {
  "darwin-arm64": "@polis-memory/darwin-arm64",
  "darwin-x64": "@polis-memory/darwin-x64",
  "linux-arm64": "@polis-memory/linux-arm64",
  "linux-x64": "@polis-memory/linux-x64",
  "win32-x64": "@polis-memory/win32-x64",
};

function binaryPath(platform = process.platform, arch = process.arch, resolve = require.resolve) {
  if (process.env.POLIS_BINARY) return process.env.POLIS_BINARY;
  const key = `${platform}-${arch}`;
  const pkg = PLATFORMS[key];
  if (!pkg) {
    throw new Error(
      `Polis has no prebuilt binary for ${key}. Supported: ${Object.keys(PLATFORMS).join(", ")}.\n` +
        "Build from source: cargo install --git https://github.com/sersiousSenpai/polis-memory polis-memory --features cli --locked"
    );
  }
  const exe = platform === "win32" ? "polis.exe" : "polis";
  try {
    return resolve(`${pkg}/bin/${exe}`);
  } catch {
    throw new Error(
      `The Polis binary package ${pkg} is not installed. It is an optional dependency: ` +
        "reinstall without --omit=optional / --no-optional."
    );
  }
}

function main() {
  let bin;
  try {
    bin = binaryPath();
  } catch (e) {
    console.error(`polis: ${e.message}`);
    return 1;
  }
  const result = spawnSync(bin, process.argv.slice(2), { stdio: "inherit", env: { ...process.env, POLIS_LAUNCHER: "npm" } });
  if (result.error) {
    console.error(`polis: could not run ${bin}: ${result.error.message}`);
    return 1;
  }
  if (result.signal) {
    process.kill(process.pid, result.signal);
    return 1;
  }
  return result.status ?? 1;
}

module.exports = { binaryPath, PLATFORMS };

if (require.main === module) {
  process.exitCode = main();
}
