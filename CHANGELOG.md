# Changelog

All notable changes to Polis Memory. The format follows Keep a Changelog;
the project is pre-1.0 and every crate shares one version.

## [Unreleased] — 0.1.0

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
- **G1:** cargo-dist release workflow (five targets, shell/PowerShell/
  Homebrew installers, attestations), the size ceiling, the org-node image,
  `mcp install` for every client, `server.json`, the root docs.

Nothing is published yet; see `docs/distribution.md` for what is live and
what waits on an outward step.
