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
core → store → embed → llm → server → memory (`polis-mcp` joins between
server and memory).

| Crate | What it is | Dependencies it is allowed |
|---|---|---|
| [`polis-core`](crates/polis-core) | PURE: ledger hashing + event kinds + canonical event + chain verdict, the consume-once guards, the query planner, near-dup suppression, the proposal grammar, the coldness interlock, the answer-pack types / budget / fusion / render, the bundle format + verifier, the deterministic gist, the tolerant JSON extractor, the host traits, and the `MemoryApi` trait — the ONE surface the server, the MCP server and the clients speak. Verifies a bundle without SQLite. | `serde`, `serde_json`, `sha2` — nothing else, pinned by a test and by CI |
| [`polis-store`](crates/polis-store) | The rusqlite store: attach to a host connection or open standalone (WAL), its own migrations under `polis_meta` (never `PRAGMA user_version`), the lexical layer (FTS5 + trigram), the cross-process-safe chain append, the recorders, and the ~120 memory methods (prompts, compaction, chain, search, catalog, supersessions, notes, observations, browse, embeddings rows, session tree, exports) | `polis-core`, `rusqlite` (bundled), `tracing`, `serde_json`, `flate2`, `regex-lite`, `uuid` |
| [`polis-embed`](crates/polis-embed) | The `Embedder` trait, the on-device Apple backends (feature `apple`, macOS only), the vector cache, brute-force cosine search over the store's int8 index, and the bounded index tick — all taking an explicit embedder (provider SELECTION stays with the host). Portable backends (`model2vec`, `fastembed`, `openai`) come with Program C | `polis-core`, `polis-store`, `tracing`; `apple` → the objc2 family |
| [`polis-llm`](crates/polis-llm) | The `Agent` trait the gardener speaks, with `Usage` / `UsageSink`; backends `ClaudeCli` (stream-json) and `CodexCli` (`exec --json`) by default, `AnthropicApi` and `OpenAiCompat` behind features; the `StreamLine` classifier | `polis-core`, `async-trait`, `serde`, `tokio` (process); `anthropic` / `openai-compat` → `reqwest` |
| [`polis-server`](crates/polis-server) | `router<S>()` over `Arc<dyn MemoryApi>` (handlers take `State<PolisState>`; a host provides `FromRef`), the `ROUTES` table (24 rows, `Open \| HookContract \| Write(scope)` — the source of the API doc and the generated clients), the capture route + `IngestObserver` seams, `CaptureHookSpec` (the hook installer); feature `standalone` = bind + token guard | `polis-core`, `axum` 0.7, `serde`, `tokio` (`rt`); `standalone` → tokio `net` |
| `polis-mcp` | *(Program E1)* rmcp server over `Arc<dyn MemoryApi>` (stdio + streamable HTTP) | `polis-core`, `rmcp` |
| [`polis-memory`](crates/polis-memory) | THE crate an integrator adds: `Polis` (a borrowed view) + `PolisHandle` (owned; implements `MemoryApi`), retrieval (the answer pack, timeline, map, tree / node views), the organizer and the gardener's `step` (organize, compaction, observations, the semantic index), bundle export, the markdown mirror, and the `classmemory` skill text (`skills/classmemory/SKILL.md`, shipped in the crate) | re-exports the four crates above; `apple` → `polis-embed/apple` |

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

Enable nothing you do not need: `cli` (ahead), `standalone`, `anthropic` and
`openai-compat` each add a dependency a host usually already has, and cargo
unifies features across a graph.

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

**MSRV:** Rust **1.85** (`rust-version` in the workspace manifest). The floor
is set by the dependency graph — `reqwest` 0.13 and `uuid` 1.23 — and raised
only deliberately, in its own commit.

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

## Benchmarks

The measurement program lives in [`docs/bench.md`](docs/bench.md): the latency
budget per operation (p50/p95), the corpus-derived eval (Recall@k, MRR,
pack-contains-gold, bytes per pack — reachability of what was recorded, not
human relevance), the canary the gardener is measured against, and the
baseline this repository was last measured at, on a synthetic lake fitted to
a real one and on a copy of that real one.

```sh
cargo bench -p polis-memory                                   # criterion, 1k prompts (POLIS_BENCH_PROMPTS=10000 …)
cargo test -p polis-memory --features eval -- eval_gate        # CI's gate against bench/results/baseline.json
cargo test -p polis-memory --features eval -- --ignored eval_ --nocapture   # write today's results file
POLIS_REAL_DB=/path/to/a/COPY.db cargo test -p polis-memory --features eval -- --ignored eval_real_db --nocapture
```

Every op reports its own p50/p95 at runtime on `GET /v1/context/stats`
(`latency`), so a slow install can be read rather than guessed at.

