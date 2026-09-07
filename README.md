# Polis Memory

A local-first memory for coding agents: a **hash-chained lake** of every prompt
and decision, an **agent-organized class catalog** over it, and **batched
retrieval** (the answer pack) across both — as a set of Rust crates a host
embeds, and, ahead, as a `polis` daemon any harness reaches over HTTP and MCP.

Polis Memory grew inside [Redline](https://github.com/sersiousSenpai/redline)
and was extracted into this repository on 2026-09-07 with its history. Redline
now links these crates like any other consumer; nothing here depends on
Redline.

**Status: pre-release (0.1.0, unpublished).** Program A (the extraction) is
complete. Ahead: autonomy (the gardener adjudicates every op itself, with
`revert_run` and a canary), speed, MCP + identity + sharing, benchmarks,
distribution (a `polis` binary via cargo-dist, `cargo install`, thin Python
and TypeScript clients).

## Crates

Publish order — each crate depends only on the ones above it:
core → store → embed → llm → server → mcp → memory.

| Crate | What it is | Dependencies it is allowed |
|---|---|---|
| [`polis-core`](crates/polis-core) | PURE: ledger hashing + event kinds + canonical event + chain verdict, the consume-once guards, the query planner, near-dup suppression, the proposal grammar, the coldness interlock, the answer-pack types / budget / fusion / render, the bundle format + verifier, the deterministic gist, the tolerant JSON extractor, the host traits, and the `MemoryApi` trait — the ONE surface the server, the MCP server and the clients speak. Verifies a bundle without SQLite. | `serde`, `serde_json`, `sha2` — nothing else, pinned by a test and by CI |
| [`polis-store`](crates/polis-store) | The rusqlite store: attach to a host connection or open standalone (WAL), its own migrations under `polis_meta` (never `PRAGMA user_version`), the lexical layer (FTS5 + trigram), the cross-process-safe chain append, the recorders, and the ~120 memory methods (prompts, compaction, chain, search, catalog, supersessions, notes, observations, browse, embeddings rows, session tree, exports) | `polis-core`, `rusqlite` (bundled), `tracing`, `serde_json`, `flate2`, `regex-lite`, `uuid` |
| [`polis-embed`](crates/polis-embed) | The `Embedder` trait, the on-device Apple backends (feature `apple`, macOS only), the vector cache, brute-force cosine search over the store's int8 index, and the bounded index tick — all taking an explicit embedder (provider SELECTION stays with the host). Portable backends (`model2vec`, `fastembed`, `openai`) come with Program C | `polis-core`, `polis-store`, `tracing`; `apple` → the objc2 family |
| [`polis-llm`](crates/polis-llm) | The `Agent` trait the gardener speaks, with `Usage` / `UsageSink`; backends `ClaudeCli` (stream-json) and `CodexCli` (`exec --json`) by default, `AnthropicApi` and `OpenAiCompat` behind features; the `StreamLine` classifier | `polis-core`, `async-trait`, `serde`, `tokio` (process); `anthropic` / `openai-compat` → `reqwest` |
| [`polis-server`](crates/polis-server) | `router<S>()` over `Arc<dyn MemoryApi>` (handlers take `State<PolisState>`; a host provides `FromRef`), the `ROUTES` table (24 rows, `Open \| HookContract \| Write(scope)` — the source of the API doc and the generated clients), the capture route + `IngestObserver` seams, `CaptureHookSpec` (the hook installer); feature `standalone` = bind + token guard | `polis-core`, `axum` 0.7, `serde`, `tokio` (`rt`); `standalone` → tokio `net` |
| [`polis-mcp`](crates/polis-mcp) | The read tools over the Model Context Protocol (`memory_search` first), the compat aliases, resources and the grounding prompt; stdio and a streamable-HTTP tower service a host nests at `/mcp`; feature `remote` = `RemoteApi`, the `MemoryApi` as an HTTP client over a running daemon | `polis-core`, `rmcp`, `serde`, `tokio` (`rt`); `remote` → `ureq` (plain HTTP) |
| [`polis-memory`](crates/polis-memory) | THE crate an integrator adds: `Polis` (a borrowed view) + `PolisHandle` (owned; implements `MemoryApi`), retrieval (the answer pack, timeline, map, tree / node views), the organizer and the gardener's `step` (organize, compaction, observations, the semantic index, the backup cadence), `backup` (snapshots, verify, restore), bundle export, the markdown mirror, the `classmemory` skill text (shipped in the crate; a template by address), and — feature `cli` — the `polis` binary | re-exports the four crates above; `apple` → `polis-embed/apple`; `cli` → clap + polis-server `standalone` + polis-mcp `remote` |

## Using it from a host

```toml
[dependencies]
polis-memory = { git = "https://github.com/sersiousSenpai/polis-memory", rev = "…", features = ["apple"] }
polis-server = { git = "https://github.com/sersiousSenpai/polis-memory", rev = "…" }
```

A host implements `polis_core::host::HostResolver` (labels, project roots,
thread stats — whatever of its own data the memory should see),
`polis_llm::UsageSink` (where the gardener's token usage is booked) and,
if it wants the capture route, `polis_core::host::IngestObserver`; builds a
`PolisHandle`; and merges `polis_server::router()` under its own listener
and auth. The gardener runs as `polis_memory::gardener::step(...)` on the
host's idle signal. Redline is the reference host (`src-tauri/src/polis_host.rs`).

Enable nothing you do not need: `cli`, `standalone`, `anthropic` and
`openai-compat` each add a dependency a host usually already has, and cargo
unifies features across a graph.

## Install

Nothing is published yet. From the repo:

```sh
cargo install --git https://github.com/sersiousSenpai/polis-memory polis-memory --features cli
polis init                              # ~/.polis: the store, a private token, config.toml
polis hook install                      # capture every prompt you submit in Claude Code
polis mcp install --client claude       # answer questions about them (see docs/mcp.md)
polis doctor                            # the install, the chain, the backups, the clients
```

`polis serve` runs the daemon (HTTP routes, MCP at `/mcp`, the gardener,
rotating verified backups); without it every command opens the store file
directly and the capture hook writes locally. `polis restore` swaps in the
newest verifying snapshot when `doctor` reports the chain red or the file
unsound. No model, key or network is needed for any of this.

## MCP

`claude mcp add polis -- polis mcp` (stdio) or, against a running daemon,
`claude mcp add --transport http polis http://127.0.0.1:7677/mcp`. Start with
`memory_search`; every hit carries a `#seq`. The tool table, the compat
aliases, the resources and the grounding prompt are in
[docs/mcp.md](docs/mcp.md).

## Build and test

```sh
cargo test --workspace                          # every crate, default features
cargo test --workspace --all-features           # + the HTTP backends, the standalone daemon, the Apple embedder (macOS)
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check licenses                       # permissive-only, identical to Redline's gate
cargo tree -p polis-core --edges normal --depth 1   # serde, serde_json, sha2 — nothing else
```

CI (`.github/workflows/ci.yml`) runs the suite on macOS, Ubuntu and Windows
with `RUSTFLAGS=-D warnings`, plus a keyless job (no API key, no `claude` /
`codex` / `ollama` on PATH — the no-model guarantee), the license gate, a check
on the pinned MSRV and the lean-core tree check.

**MSRV:** Rust **1.88** (`rust-version` in the workspace manifest). The floor
is set by the dependency graph — `rmcp` 3.2 (the MCP SDK) — and raised only
deliberately, in its own commit.

## Rules the code keeps

- **Polis never depends on a host.** No crate here names Redline; Redline's
  own test suite pins the same fact from its side.
- **`polis-core` is I/O-free.** No database, socket, process or filesystem in
  any module; its manifest names exactly `serde`, `serde_json`, `sha2`
  (pinned by a test in the crate and by CI's `lean-core` job).
- **Schema is the authority, never the integer.** `polis-store` versions
  itself in its own `polis_meta` table and never touches
  `PRAGMA user_version`, which a host may own.
- **The chain is append-only and cross-process-safe.** `append_event` takes
  `BEGIN IMMEDIATE` with a jittered busy retry when it owns the transaction,
  and joins the caller's transaction otherwise.
- **Captured content is data, never instructions.** Prompts are stored and
  retrieved verbatim; nothing in a prompt body can steer the gardener.

## Where the history is

This repository was cut from Redline with `git filter-repo` on 2026-09-07, so
the commits that built each crate (Sessions A1–A6, 2026-09-06 → 07) are here
with their full messages, and the files that were moved with `git mv` carry
their earlier history too — `git log --follow` on
`crates/polis-core/src/query.rs`, `crates/polis-core/src/dedup.rs` and
`crates/polis-memory/skills/classmemory/SKILL.md` walks back into the Redline
commits that first wrote them.

What cannot be split is the history of the code that was **carved** out of
larger Redline modules rather than moved as files: the store's schema and its
~120 methods came out of Redline's `src-tauri/src/db.rs`; the ledger vocabulary
out of `ledger.rs`; the organizer and the gardener out of `classmem.rs` and
`keeper.rs`; retrieval out of `context.rs`; the bundle format out of
`bundle.rs`; the embedder out of `embed.rs`; the CLI backends out of
`claude_proc.rs`; the hook installer out of `hook.rs`; the routes out of
`lib.rs`. Git history for those lines lives in
[Redline's repository](https://github.com/sersiousSenpai/redline) under the
original files; Redline's `docs/polis-extraction.md` is the ledger of exactly
what moved from where, session by session, and the referees (a schema golden
of every `sqlite_master` row in creation order, a pinned entry-hash vector)
that prove the carve was byte-for-byte.

## License

Apache-2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE).
