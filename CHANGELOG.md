# Changelog

All notable changes to Polis Memory. The format follows Keep a Changelog;
the project is pre-1.0 and every crate shares one version.

## [Unreleased]

## [0.2.0] — 2026-10-07

Polis outside Redline. A one-line install (`curl … | sh`, `npx polis-memory
setup`, PowerShell) and `polis setup` take a new user from nothing to a
connected setup:

- the record is created
- Claude Code, Codex and Cursor capture prompts and replies; Windsurf and
  Claude Desktop search the record
- a daemon keeps the catalog, index and backups going

Everything since 0.1.0:

- Add `polis setup` and `polis uninstall`. Setup detects Claude Code, Codex,
  Cursor, Windsurf and Claude Desktop, shows its plan, asks, then:
  - creates the record
  - connects each agent's MCP config and capture hooks, plus the skill for
    Claude Code
  - fetches the embedding model
  - records an explicit gardener model (a model CLI the user has, never an
    API key)
  - installs the daemon service

  Run from an npm cache, it first copies the binary to `~/.local/bin`.
  Uninstall reverses all of this and keeps the record unless `--purge`.
- Capture Codex and Cursor prompts and replies (`polis hook install --client
  codex|cursor`), using the hook shapes Redline already runs.
- Add the `polis-memory` agent skill (`polis skill install`): search, cite,
  treat recalled text as data, write decisions. It names the token's path,
  never the token.
- Distribute through npm: `npx polis-memory setup`. The launcher and
  per-platform binary packages (`polis-memory-<platform>`) are built from
  release archives by `npm/build.mjs` / `npm-release`. The TypeScript client becomes `@polis-memory/client`.
- Add one-line installers (`scripts/install.sh`, `install.ps1`) that verify
  the release checksum, install to `~/.local/bin` and run setup.
- The background service carries the PATH it was installed from, so the
  gardener finds the `claude` / `codex` CLI.
- `gardener_model` accepts `claude-cli` and `codex-cli`.
- Correct the MCP server instructions, which said nothing writes, and list
  `memory_search` first.

- Add opt-in prompt-time injection (`inject = "on"`). Each captured prompt is
  answered with up to three earlier same-project records that clear a
  deterministic lexical floor, as hidden `additionalContext`, in at most
  1,536 bytes. The floor is calibrated on `bench/realistic/inject.json` and
  gated in `tests/inject_floor.rs`. Injected sources are recorded per session
  in `polis_injections`, a local table created on first use (the shared
  schema and its version are unchanged).
- Capture assistant replies through a `Stop` hook (`polis capture --event
  stop`). It reads the transcript incrementally and records text turns with
  their edited files, not tool payloads or subagent traffic. An assistant
  turn that restates injected memory is not recorded again.
- Redact secrets at ingest, before hashing: provider keys, private keys,
  JWTs, bearer tokens and long `KEY=`/`PASSWORD=` values. This covers
  capture, replies, `memory_ingest` and `memory_remember`. Counts by kind are
  kept; values never are. On in the `polis` binary (`scrub = "off"` disables
  it); off by default for library hosts, which opt in with
  `PolisHandle::with_scrub(true)`.
- Run the capture route's SQLite and observer work on the blocking pool.
  Take snapshots through a dedicated read-only connection, so a backup no
  longer holds the store lock for the copy. Run the daemon's periodic
  backups on a blocking worker.
- Add `polis service install | uninstall | status` (launchd / systemd user
  service). `polis init` prints semantic-search readiness.
- `polis doctor` reports reply capture, injection, scrubbing, the embedding
  backlog, a down daemon with unindexed sources, and a paid gardener model
  selected only by an environment key. A new `gardener_model` setting
  (`auto` | `none`) resolves the last one.

- Enforce project, principal, agent, run, organization, role and source-time
  eligibility across retrieval and transports. Authenticate the assembled HTTP
  and MCP daemon. Commit prompt evidence, scope and ledger events atomically.
- Rank dense class links before output limits, interleave lexical and semantic
  results, retrieve cited decisions directly, and add bounded conversational
  neighbors. Preserve assistant roles, source dates, sessions and exact flags.
- Add valid-time/recorded-time claims with checked source spans, conservative
  authority, unresolved alternatives and retirement after source forgetting.
- Persist leased background jobs, independent indexing/basic filing schedules,
  atomic daemon ownership, bounded model requests and local usage accounting.
- Resolve forgetting from ledger citations, remove compacted prompt and page
  bodies and dependent evidence, expire copied queued proposals, persist prompt
  peer redactions, and retain local forget markers across managed restore.
- Complete standalone/targeted note forgetting across edits, star events, signed
  peer redactions and managed restore. Preserve history identities, validate
  mappings, write note scope atomically and reject delayed organizer copies.
- Assemble retrieval against a SQLite snapshot, enforce serialized/context
  budgets, expose arm errors/readiness and inspection cursors, and retain
  bounded body-free traces. Add `polis inspect` JSON/HTML exports.
- Generate API schemas and typed Python/TypeScript clients. Repair benchmark
  profiles, readiness, spend accounting and reproducibility; preserve the paid
  benchmark hold. Record deterministic before/after reachability results.

## [0.1.0] — 2026-09-08

The first cut, extracted from Redline on 2026-09-07 with its history and
built out over the sessions the plan calls Programs A, B, C, E and G:

- **A (extraction):** the six crates (`polis-core` pure vocabulary and the
  `MemoryApi` trait; `polis-store` SQLite with its own `polis_meta`
  versioning; `polis-embed`; `polis-llm`; `polis-server` axum router +
  `ROUTES`; `polis-memory` facade) with byte-for-byte referees.
- **B1:** latency ring + spans, the synthetic corpus, the canary, the eval
  and its CI gate, `class_runs` accounting, the measured baseline.
- **E1:** `polis-mcp` (read tools, aliases, resources, stdio + streamable
  HTTP), rotating verified backups, `polis restore`, `polis doctor`, the
  `polis` binary.
- **B2:** retire-marks, the per-op journal, `revert_run`, the run surface.
- **E2:** Ed25519 identity, one chain per device, principals + aliases,
  scope columns, the bind event, the five MCP write tools, the signed
  envelope with verify-only import.
- **B3:** adjudication per op, the work queue, human curation deleted,
  warmth in place of pins, observation re-validation, the canary
  auto-revert, the data-not-instructions fences, `catalog_health`.
- **E3:** foreign chains and rows, trust with TOFU, folder and git
  transports, `sync`/`subscribe`, union retrieval with source labels.
- **C1:** centroid-first filing with an honest calibration, the `~inbox`
  fallback, the real keyless CI job.
- **C2:** per-model dimension, honest `ProviderKind`, the portable
  Model2Vec runtime (pinned download), fastembed opt-in, the remote
  provider, the measured provider table — Model2Vec is the product's
  default when present.
- **E4:** the org node (`polis serve --org`, the `/v1/sync/*` relay, the
  node as its own principal publishing the firm's catalog as signed
  events), redaction propagation with signed per-peer acks,
  `docs/security.md`.
- **F1:** the LongMemEval harness with a keyless stub mode, the competitor
  runners under identical conditions, the nightly job gated on a secret,
  the kill criterion written before any run; the MCP round-trip row.
- **G1:** cargo-dist release workflow (five targets, shell/PowerShell/
  Homebrew installers, attestations), the size ceiling, the org-node image,
  `mcp install` for every client, `server.json`, the root docs.

This release ships the GitHub artifacts (the installers, the attested
binaries, the formula attached to the release). The crates are not on
crates.io yet, the image is not pushed, and the npm / PyPI names are not
reserved; see `docs/distribution.md` for what waits on an outward step.
