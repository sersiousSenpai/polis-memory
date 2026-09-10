# Distribution

Verified from public registry/release endpoints on **2026-09-09**. Existence of
an artifact does not prove installation, upgrade, rollback or offline behavior
on a clean machine. No public publishing occurred during this implementation.

| Channel | Verified state | Remaining work |
|---|---|---|
| GitHub binaries/installers | [v0.1.0](https://github.com/sersiousSenpai/polis-memory/releases/tag/v0.1.0) published 2026-09-08 07:41:23 UTC. Five platform archives, shell/PowerShell installers, formula, checksums and dist manifest present. | Test this change on clean machines before another release; validate downloaded checksums/attestations. |
| Homebrew | Release includes `polis-memory.rb`; [public tap formula endpoint](https://api.github.com/repos/sersiousSenpai/homebrew-tap/contents/Formula/polis-memory.rb) returned 404. | Public tap/formula availability unverified; configure release credential and test `brew install`. |
| crates.io | [API](https://crates.io/api/v1/crates/polis-memory) returned 403; [sparse index](https://index.crates.io/po/li/polis-memory) returned 404. | Registry publication not established. Check names/credentials and publish workspace crates in dependency order. |
| Python | [PyPI package endpoint](https://pypi.org/pypi/polis-memory/json) returned 404. Typed sync/async source client and loopback tests now in `sdk/python`. | Wheel build and fresh-venv offline installation/import verified locally; publish workflow prepared. Review sdist/metadata and publish; test clean installation. |
| TypeScript | [npm package endpoint](https://registry.npmjs.org/polis-memory) returned 404. Typed async source client and keyless transport tests now in `sdk/typescript`. | `npm pack --dry-run` verified locally; publish workflow prepared. Publish and test clean installation. |
| Container / MCP registry | Dockerfile, release machinery and `server.json` exist in the checkout. External publication not checked in this pass. | Verify registry status, signed image and registry metadata. Local Polis does not require Docker. |

The release archives target `aarch64-apple-darwin`, `x86_64-apple-darwin`,
`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` and
`x86_64-pc-windows-msvc`. The source client instructions and tested cross-agent
example are in [sdk/README.md](../sdk/README.md). Generated JSON request/response
schemas ship in the server and are published at `/v1/memory/schema`.

## Installation verification matrix

| Scenario | macOS | Linux | Windows |
|---|---|---|---|
| Current checkout keyless regression suite | Run locally in this development session | CI configured; this session not a CI run | CI configured; this session not a CI run |
| Published v0.1.0 installer on clean machine | Not exercised | Not exercised | Not exercised |
| Install → upgrade → rollback | Not exercised | Not exercised | Not exercised |
| Offline startup with pinned assets | Harness supports verified prestaging; run result recorded separately | Not exercised here | Not exercised here |
| Backup → forget → managed restore safety | Rust regression fixtures; see current test output | CI contract gate | CI contract gate |
| Two agents in five minutes | Client/server contracts exercised; clean-install timer not measured | Not measured | Not measured |

Current artifact sizes must be read from the release's `dist-manifest.json` or
measured after building this revision. `scripts/size-budget.json` remains the
size gate; historical binary sizes are not measurements of this change.

For a release, run existing cargo-dist dry-run and platform gates, review API
schema/type drift, package SDKs, then publish with maintainer credentials. Do not
reserve package names by publishing placeholder clients. The existing GitHub
release is already public; there is no outstanding "first release" step.

`.github/workflows/sdk-release.yml` builds and uploads both client packages by
default. Publishing requires a maintainer to explicitly dispatch `publish=true`
with `PYPI_API_TOKEN` and `NPM_TOKEN` configured. This workflow was not dispatched
here; the local wheel and npm package dry-run are the verified packaging checks.
