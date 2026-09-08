# Distribution

The plan's §8 decision, as built in Session G1: prebuilt binaries through
cargo-dist are the primary channel; `cargo install` is the always-available
source path; Docker exists for org-node operators only. This page says what
is live, what is measured, and which steps are outward-facing and therefore
the maintainer's to take by hand.

## Channels

| Channel | Audience | How | State |
|---|---|---|---|
| `curl \| sh` / PowerShell installer, from the GitHub release | everyone | `polis-memory-installer.sh` / `.ps1` attached to each `v*` release by `.github/workflows/release.yml` (cargo-dist 0.32) | workflow live; first release pending a tag |
| Homebrew tap | macOS / Linux | `brew install sersiousSenpai/tap/polis-memory`; the formula is written into `sersiousSenpai/homebrew-tap` by the release workflow | pending: the tap repository + `HOMEBREW_TAP_TOKEN` |
| `cargo install polis-memory --features cli` / `cargo binstall polis-memory` | Rust developers | crates.io in dependency order (below); binstall reads the release layout cargo-dist publishes | pending: the crates.io publish |
| `pip install polis-memory` / `npm i polis-memory` | Python / TypeScript agent code | thin HTTP clients generated from `polis_server::ROUTES` | Session G2 |
| Docker `ghcr.io/sersiousSenpai/polis-memory` | org-node operators only | `Dockerfile` at the root: a static musl `polis` on distroless, non-root, `/data` volume, `serve --org` | image builds locally (below); the push and the cosign/SBOM job are gated on secrets |
| `npx` / `uvx` shim, `.mcpb` | Claude Desktop / Cursor one-click | | v1.1 (G3) |

## Targets

Five, built by the release workflow on GitHub-hosted runners:
`aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`. Linux is **gnu, not
musl**, for the installers: it is cargo-dist's proven path (no cross
toolchain, no `cargo-zigbuild`), and the runner's glibc (ubuntu-22.04, glibc
2.35) is older than every distribution the target audience runs. The one
place a static musl binary matters — the container image, where there is no
libc to link against — builds it inside `rust:1-alpine` in the `Dockerfile`,
so the two never have to share a toolchain. If a static Linux download turns
out to matter for individual users, add the two musl targets and
`cargo-zigbuild` to `dist-workspace.toml` then.

## Signing

Every artifact is sha256-checked (`sha256.sum` + per-file checksums) and
carries a GitHub artifact attestation (`github-attestations = true`;
`gh attestation verify <file> --owner sersiousSenpai`). macOS: Apple's
linker ad-hoc signs every arm64 binary (the local build shows an ad-hoc
`Identifier=polis-…`), and the installer, Homebrew and cargo paths set no
quarantine attribute, so v1 needs no Developer ID; a Finder double-click of
a downloaded tarball's binary would still be blocked, which is why the
install lines are the documented path. Developer ID + notarization lands
when Redline's certificate does (the same team). Windows: SmartScreen warns
on an unsigned binary launched from a download; the PowerShell installer
path does not trip it; Authenticode signing is G3.

## Size

| Artifact | Bytes | Note |
|---|---|---|
| `polis` (aarch64-apple-darwin, `dist` profile, `cli` features) | 14,907,904 | inside the plan's 12–16 MB no-model band; the ceiling in `scripts/size-budget.json` is 16,000,000 (93.2 % used). E1 measured 8.76 MB before B2–B3, E2–E3, C1 and the HTTP model backends the `cli` feature now enables. |
| `polis-memory-aarch64-apple-darwin.tar.xz` | 4,530,624 | what the installer downloads |
| org-node image (`polis-memory:g1`, linux/arm64, distroless static, non-root) | 8,079,044 (`docker image inspect .Size`); the static musl `polis` inside is 14,909,744 | `docker build` here (a full LTO build in `rust:1-alpine`, ~20 min); `init`, `doctor` and `verify` run inside it as `nonroot` against a named `/data` volume |
| int8 embedding model bundled (`--features bundled-model`) | — | C2's row when it lands (plan: ≈ 20–24 MB) |

`.github/workflows/size.yml` builds the `dist`-profile binary on macOS and
Ubuntu on every push to main and nightly and fails over the ceiling
(`node scripts/check-size.mjs --strict`). Ratchet the JSON down as levers
land; an increase is a reviewed diff of that file. The plan's §11 headroom
question (Cargo.toml comment vs JSON) is settled here: the JSON is the
authority.

## Releasing

```sh
# 1. version bump (one place: [workspace.package] in Cargo.toml), CHANGELOG, commit
# 2. tag and push — the release workflow builds the five targets, the installers,
#    the formula and the attestations, and publishes the GitHub release
git tag v0.1.0 && git push origin v0.1.0
# dry run first: Actions → release → Run workflow → tag = "dry-run"
```

## The outward steps — the maintainer's, in order

None of these ran in G1. Each is irreversible or public, so each is a
decision.

1. **crates.io** (needs `cargo login` with a token that has publish-new
   scope). The workspace publish resolves the dependency order (core → store
   → embed → llm → server → mcp → memory); the dry run is green:
   ```sh
   cargo publish --workspace            # or, one at a time, in that order
   ```
   Names to check first (§11): `polis-core`, `polis-store`, `polis-embed`,
   `polis-llm`, `polis-server`, `polis-mcp`, `polis-memory` — the existing
   `polis` crate (a different project) is library-only, so the `polis`
   command has no `~/.cargo/bin` clash.
2. **The Homebrew tap**: `gh repo create sersiousSenpai/homebrew-tap --public`,
   then a `HOMEBREW_TAP_TOKEN` repository secret on `polis-memory` (a
   fine-grained PAT with contents: write on the tap). The next release
   writes `Formula/polis-memory.rb` there.
3. **The first release**: `git tag v0.1.0 && git push origin v0.1.0` (after a
   `dry-run` dispatch). This is what makes the installer lines in the README
   real.
4. **The org-node image**: `docker build -t ghcr.io/sersiousSenpai/polis-memory:0.1.0 . && docker push …`
   (needs `docker login ghcr.io` with a `write:packages` token), or wire the
   `release.yml` image job to `GITHUB_TOKEN` + cosign keyless — the steps are
   written but disabled until E4's `serve --org` is on main.
5. **The MCP registry**: `server.json` validates against the 2025-12-11
   schema; publishing it (`mcp-publisher publish` after `mcp-publisher login
   github`) reserves the `io.github.sersiousSenpai/polis-memory` id — after
   the crates.io publish, because the registry checks the `cargo` package
   exists.
6. **Name reservations**: `polis-memory` on npm and PyPI (G2 ships the real
   clients; reserving now is a placeholder publish each — an npm 0.0.0 and a
   PyPI 0.0.0), and `polismemory.com` at the registrar (no DNS record today;
   the site scaffold under `site/` has no numbers and can go up on GitHub
   Pages meanwhile).
7. **Redline's flip to crates.io versions**: once (1) is done, Redline's
   seven `src-tauri/Cargo.toml` lines become `version = "0.1.0"` — a normal
   rev bump with the schema golden and `STORE_BUMP_OBJECTS` unchanged.

## What Redline needs now

Nothing. Redline keeps pinning a git rev until step 7.
