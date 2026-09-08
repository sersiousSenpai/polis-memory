# Changelog

All notable changes to Polis Memory. The format follows Keep a Changelog;
the project is pre-1.0 and every crate shares one version.

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
