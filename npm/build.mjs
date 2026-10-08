#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// Stage the npm packages for one release from its cargo-dist archives:
//   node npm/build.mjs --version 0.2.0 --archives <dir of polis-memory-*.tar.xz|zip> --out <dir>
// writes <out>/polis-memory (the launcher, optionalDependencies pinned to
// this version) and <out>/<platform> for each archive found. Publishing is
// the workflow's job (.github/workflows/npm-release.yml): platforms first,
// then the launcher.
import { execFileSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

export const TARGETS = {
  "aarch64-apple-darwin": { name: "darwin-arm64", os: "darwin", cpu: "arm64", exe: "polis" },
  "x86_64-apple-darwin": { name: "darwin-x64", os: "darwin", cpu: "x64", exe: "polis" },
  "aarch64-unknown-linux-gnu": { name: "linux-arm64", os: "linux", cpu: "arm64", exe: "polis" },
  "x86_64-unknown-linux-gnu": { name: "linux-x64", os: "linux", cpu: "x64", exe: "polis" },
  "x86_64-pc-windows-msvc": { name: "win32-x64", os: "win32", cpu: "x64", exe: "polis.exe" },
};

const here = dirname(fileURLToPath(import.meta.url));

function findFile(dir, name) {
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) {
      const found = findFile(path, name);
      if (found) return found;
    } else if (entry === name) {
      return path;
    }
  }
  return null;
}

export function build({ version, archives, out }) {
  if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$/.test(version)) throw new Error(`not a version: ${version}`);
  rmSync(out, { recursive: true, force: true });
  mkdirSync(out, { recursive: true });
  const staged = [];
  for (const [triple, t] of Object.entries(TARGETS)) {
    const archive = ["tar.xz", "zip"].map((ext) => join(archives, `polis-memory-${triple}.${ext}`)).find(existsSync);
    if (!archive) continue;
    const work = mkdtempSync(join(tmpdir(), "polis-npm-"));
    execFileSync(archive.endsWith(".zip") ? "unzip" : "tar", archive.endsWith(".zip") ? ["-q", archive, "-d", work] : ["-xJf", archive, "-C", work]);
    const binary = findFile(work, t.exe);
    if (!binary) throw new Error(`${archive} has no ${t.exe}`);
    const pkgDir = join(out, t.name);
    mkdirSync(join(pkgDir, "bin"), { recursive: true });
    cpSync(binary, join(pkgDir, "bin", t.exe));
    chmodSync(join(pkgDir, "bin", t.exe), 0o755);
    writeFileSync(
      join(pkgDir, "package.json"),
      JSON.stringify(
        {
          name: `@polis-memory/${t.name}`,
          version,
          description: `The polis binary for ${t.os}-${t.cpu} (installed by the polis-memory package)`,
          os: [t.os],
          cpu: [t.cpu],
          files: ["bin"],
          preferUnplugged: true,
          repository: { type: "git", url: "git+https://github.com/sersiousSenpai/polis-memory.git" },
          license: "Apache-2.0",
        },
        null,
        2
      ) + "\n"
    );
    rmSync(work, { recursive: true, force: true });
    staged.push(t.name);
  }
  if (staged.length === 0) throw new Error(`no polis-memory-<target> archives in ${archives}`);
  const launcher = join(out, "polis-memory");
  cpSync(join(here, "polis-memory"), launcher, { recursive: true });
  const pkg = JSON.parse(readFileSync(join(launcher, "package.json"), "utf8"));
  pkg.version = version;
  delete pkg.scripts;
  pkg.optionalDependencies = Object.fromEntries(Object.values(TARGETS).map((t) => [`@polis-memory/${t.name}`, version]));
  writeFileSync(join(launcher, "package.json"), JSON.stringify(pkg, null, 2) + "\n");
  return { launcher: "polis-memory", platforms: staged };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const arg = (name) => {
    const i = process.argv.indexOf(`--${name}`);
    if (i < 0 || !process.argv[i + 1]) throw new Error(`--${name} is required`);
    return process.argv[i + 1];
  };
  const result = build({ version: arg("version").replace(/^v/, ""), archives: arg("archives"), out: arg("out") });
  console.log(JSON.stringify(result));
}
