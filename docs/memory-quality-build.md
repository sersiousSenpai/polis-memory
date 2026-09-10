# Local memory quality build — 2026-09-09

This checkout implements the local correctness, retrieval, temporal, autonomy,
inspection and SDK work from the supplied build brief. It preserves the seven
crates, SQLite store, append-only ledger, signed sharing, and zero LLM calls on
capture. The changes are uncommitted and unreleased. External scored comparisons
remain on **HOLD**; no paid answer, judge or competitor calls were made.

## Verified improvements

Dense class evidence is ranked before a preview is limited. In an identical
fixture run against baseline commit `911d24e4348e` and the implementation, relevant
links were reachable at **100 / 1,864 positions before → 1,864 / 1,864 after**.

| Class size | Before | After |
|---:|---:|---:|
| 40 | 20 / 40 | 40 / 40 |
| 100 | 20 / 100 | 100 / 100 |
| 317 | 20 / 317 | 317 / 317 |
| 407 | 20 / 407 | 407 / 407 |
| 1,000 | 20 / 1,000 | 1,000 / 1,000 |

This measures link-preview reachability, not answer correctness. Scoring every
eligible link does more work: the 1,000-query debug batch increased from 1,638 ms
to 4,784 ms. [Raw comparison and reproduction notes](../bench/results/2026-09-09-dense-reachability.json).

The actual MCP realistic fixture improved from 6/8 to 8/8 evidence checks.
The two development misses involved an assistant answer whose question carried
the search terms, and a `--flag` inside a natural-language question. All four
heldout fixtures passed unchanged. Eight synthetic tasks are not enough to
establish general answer-quality leadership.
[Before](../bench/results/2026-09-09-realistic-development-before.json) ·
[After](../bench/results/2026-09-09-realistic-evidence.json).

A release-profile, on-disk measurement used 10,000 prompts on an Apple M3 Max
with 48 GiB RAM, macOS 14.3, Rust 1.95.0, SQLite WAL/NORMAL, no embedding provider
and no model backend. All 10,000 acknowledgments had immediate lexical evidence;
all 160 measured queries returned the expected citation within 4,096 bytes.

| Operation | Samples | p50 | p95 |
|---|---:|---:|---:|
| Single-item capture | 10,000 | 0.73 ms | 1.69 ms |
| Warm answer pack | 120 | 9.49 ms | 10.61 ms |
| First answer pack in a fresh process | 40 | 10.14 ms | 10.86 ms |

Fresh-process measurements exclude process startup and store opening; filesystem
caches were not evicted. This is not a cold-storage measurement. The fixture
spans ten projects, one hundred sessions, five hundred runs and both speakers,
with ten classes of one thousand links.
[Raw samples, hardware, limits and configuration](../bench/results/2026-09-10-acceptance-10k.json).

The 100k-chunk release semantic fixture with authoritative source rows and
snapshot-aware cache revision measured warm p95 33.24 ms at 256 dimensions and
35.52 ms at 512 dimensions. It exercises the scoped vector-search implementation
using deterministic vectors; it is not an embedding-model accuracy result.

With pinned `model2vec/potion-base-8M` assets, an actual MCP indexing pass made
13/13 sources ready in 95.11 ms. All five semantic-only paraphrase checks passed,
including three assistant sources; their exact citations resolved. A separate
normal-daemon run captured 50 sources at approximately five per second and
measured semantic availability from acknowledgment at **p50 1.04 seconds /
p95 1.96 seconds**. Forty sources became ready while capture was still running.
No LLM calls were made. This short, single-machine run excludes asset downloads,
overload and sustained backfill. This run used the schema 9 release binary before
the subsequent note-lifecycle completion; the earlier schema 8 run is also retained.
[Pinned assets, raw samples and method](../bench/results/2026-09-10-local-model-readiness-final.json).

No-model filing of 400 sources measured **p50 41.65 ms / p95 46.23 ms** across
ten fresh homes. Each sample contains 200 user and 200 assistant sources, and
all 400 source links were verified. Timing includes CLI startup and store open;
capture and an initial identity-bookkeeping pass are excluded. This is inbox
filing with zero LLM calls and no embeddings, not semantic classification quality.
[Raw samples and method](../bench/results/2026-09-10-local-filing-400.json).

## What is implemented

| Area | Behavior and evidence |
|---|---|
| Scope and roles | Principal, project, agent, run, organization, shared-source and role/time eligibility before candidate limits. Local, HTTP, remote and MCP contract fixtures cover the same requests. Unknown roles reject; human aliases normalize to user; assistant remains distinct from agent-preface text. |
| Atomic evidence | Prompt text, role, namespace, FTS and ledger event commit together. Idempotency includes the source namespace. Identity adoption can converge historical namespaces without deleting rows or invalidating the lookup index. Capture preserves the harness session in returned evidence. |
| Retrieval | Scoped lexical and semantic RRF interleaving, semantic-only pages with snippets, all-link relevance scoring, standalone cited decisions, one-hop conversational context, role-aware deduplication, exact flag matching, source citations and explicit legacy gaps. |
| Consistency | Read transactions pin the evidence snapshot. Store identity, model and embedding mutation revision invalidate vector caches; eligibility is checked in the snapshot. Stats and warmth do not bleed between stores. |
| Budgets and diagnostics | Separate candidate and response limits, every collection budgeted, final serialized-byte check, strict rendered-context ceiling, arm availability/errors, readiness, considered/returned counts and versioned configuration. |
| Claims | Closed predicates and typed values, checked quotations/offsets, independent valid and known times, conservative source authority, unresolved alternatives and retirement. The existing classifier may emit at most eight cited claims; runtime metadata is authoritative and run rollback retires them. |
| Autonomy | Independent two-second indexing/basic filing schedules, persisted organization/basic-filing jobs with attempts, leases and checkpoints, fenced stale workers, atomic gardener ownership, bounded HTTP/CLI calls and local model usage. |
| Forget and restore | Citation-based prompt/page/note forgetting, note history and signed peer redactions, compacted archives and dependent copies removed, stale organizer writes blocked, durable redaction outboxes and peer acknowledgment visibility, and managed restore filtering with a durable `.forgotten` marker registry. |
| Inspector | Scoped exact evidence and bounded persistent retrieval traces; `polis inspect` exports offline HTML and portable JSON, with per-arm timings, ranking evidence, coverage, configuration comparisons, gardener history, jobs and usage. |
| Adoption | Generated JSON request/response contracts; typed Python and TypeScript source clients, sync/async support, errors/timeouts, scope and trace propagation, idempotent ingestion and two-agent examples. Packaging workflow defaults to producing artifacts. |

The core dependency fence remains `serde`, `serde_json`, and `sha2`. New claim
and diagnostics modules participate in its I/O-free source check. Migrations
use Polis schema version 10 and preserve historical ledger hashes.

## Retrieval contract details

`SearchRequest.candidateLimit` defaults to 200 and is capped at 200. Class-link
scoring examines all eligible links before selecting its preview; conversational
expansion adds at most fifty one-hop candidates. The response limit applies
separately. Neighboring evidence retains its speaker, session and timestamp;
proximity does not establish a factual relationship.

`SearchRequest.maxTokens` uses a conservative UTF-8-byte ceiling rather than
claiming to know an arbitrary downstream tokenizer. The serialized AnswerPack
cannot exceed 60,000 bytes. Budgets below 1,024 bytes reject because the structured
contract has required metadata. Context text independently honors the smaller
of `maxTokens`, `maxBytes`, and 12,000 bytes, including zero. Transport envelopes
and duplicated MCP structured/text representations are outside the pack byte
limit. The benchmark harness separately counts the final answer context with
its declared tokenizer; unsupported tokenizer/provider combinations reject.

Source `after` is inclusive and `before` exclusive. Claim `validAt` and `knownAt`
are independent axes. Exact lookup returns `available`, `redacted`, `unavailable`
or `legacy_gap`; it does not fabricate a body for an unmapped historical event.
Local citations use `#seq`; shared citations use the full `chain:seq` identity.
Decision projections recheck their source hash and original ledger reference.

A continuation cursor begins **chronological inspection of all scoped sources**.
It is not the next relevance-ranked page. It carries the snapshot head and
query/scope binding and rejects changed evidence or a different request. It
starts from the beginning so exhaustive inspection does not silently skip items
that the first ranked pack omitted.

Retrieval traces retain at most 1,000 records of at most 32 KiB each; model usage
retains 10,000 records. These are payload retention bounds, not a promise about
total SQLite/WAL file size. Raw queries and source bodies are omitted. Context
traces distinguish pack selection from final text clipping. Correlation IDs are
prefixes; clients inspect the actual returned `retrieval.traceId`.

A trace declares the ledger snapshot and ranking configuration. Exact replay
requires the original query and a preserved evidence **and index** snapshot.
Polis does not archive every snapshot or reconstruct a forgotten source for
replay. End-to-end capture-to-model span trees and network exporters are not
implemented; the current diagnostics link source, retrieval, run and seat data.

## Verification and reproduction

The original schema 9 verification record is in
[build-verification.json](../bench/results/2026-09-09-build-verification.json).
It records the complete suite results after integration, strict Clippy, the
Rust 1.88 all-feature compile, schema drift checks, and keyless harness/SDK tests.
The schema 9 integrated CLI/evaluation suite passed 297 tests, with 15 opt-in tests skipped.
Both golden tests also passed with the real pinned Model2Vec assets.
The note-lifecycle follow-up adds deterministic checks for edited/starred and
targeted notes, forged mappings, atomic rollback, scope, delayed organizer output,
signed peer redactions, replay, and restore before edits/forgetting. The schema 10
workspace suite passed 312 tests, with 15 opt-in tests skipped; both SDK suites
passed all 6 tests. Strict Clippy, the Rust 1.88 compile, schema checks and a fresh
stdio MCP create/resolve/forget/resolve/search/verify workflow also passed, as
recorded in [note-forgetting-verification.json](../bench/results/2026-09-10-note-forgetting-verification.json).
Focused fixtures cover dense insertion positions, semantic-only prompts/pages,
assistant-only replies, zero/tiny context budgets, snapshot stability, cache
mutation, scoped successors, forged decision projections, both temporal axes,
claim rollback, compaction/forget/restore, stalled jobs and actual TCP MCP auth.

```sh
cargo test --workspace --features polis-memory/cli,polis-memory/eval --locked
cargo clippy --workspace --features polis-memory/cli,polis-memory/eval --all-targets --locked -- -D warnings
cargo +1.88.0 check --workspace --all-features --locked
python3 sdk/schema/generate.py --check
python3 sdk/schema/types.py --check
python3 -m unittest discover -s bench/tests -v
python3 -m unittest discover -s sdk/python/tests -v
node --test sdk/typescript/test/*.test.mjs
cargo test -p polis-memory --test dense_measurement -- --ignored --nocapture
```

For the 10k release fixture, use the exact command/environment recorded in its
JSON artifact. Its ignored status keeps a measurement workload out of routine CI.
The [benchmark README](../bench/README.md) includes the runnable pinned-model
readiness and no-model filing commands.

A fresh temporary offline home was initialized, captured a record, searched it,
exported inspector HTML/JSON and verified the ledger. The inspector JavaScript
passed a syntax check. Browser visual QA could not run because the computer-use
service failed to start. The HTML is self-contained and makes no network requests.

## Remaining acceptance work

| Brief target | Status of this build |
|---|---|
| Zero-LLM capture and immediate lexical evidence | Verified; 10k capture latency is within the proposed limits on the declared machine. |
| 10k retrieval latency | Warm and first-query-in-new-process measurements are within target; disk-cache eviction and startup-inclusive cold latency were not measured. |
| 100k semantic search | Scoped search meets the latency target with deterministic vectors; a 100k real-provider corpus remains a separate measurement. |
| Normal semantic freshness | Verified on the pinned local model and short 50-capture workload; overload/backfill remains separate. |
| Organization speed and precision | The fixed 400-item no-model workload meets the latency target; model-backed latency and precision/coverage calibration remain outstanding. |
| Provenance and lifecycle | Scope, citations, temporal history, prompt/page/note forgetting, sharing, delayed-output suppression and managed restore regressions pass. |
| Answer-quality leadership | Not established; scored comparisons remain held. |
| Five-minute cross-platform adoption | Source SDK and local CLI/transport checks pass; clean-machine OS matrix and publication remain outstanding. |

- Paid LongMemEval and competitor answer/judge runs remain held. Competitor
  extraction/embedding usage must be metered before enabling a scored run;
  unknown costs cannot become estimates. Leadership and category superiority
  are not established.
- Sustained overload/backfill behavior, new provider accuracy, model-backed
  filing latency and recalibrated filing coverage need their declared
  real-provider workloads. The short measured normal-load freshness result
  does not establish those separate targets.
- Broader entity/alias extraction and two-hop factual relationship expansion
  are deferred under the brief's measurement gate. Cited structured relationships
  are available; co-occurrence is not presented as a factual edge.
- Claim support is literal, not a semantic entailment proof. Automatic claims
  cite local user/assistant prompts and use source time, without inferring
  retroactive dates. General enrichment remains outside capture.
- Native synchronous embedding code cannot be forcibly cancelled safely. CLI
  deadlines kill and reap the direct child, without process-group management.
  Leased job records currently cover organization and basic filing; indexing
  resumes through its existing durable embedding backlog.
- Public package publication and clean macOS/Linux/Windows install, upgrade,
  rollback and five-minute adoption runs remain release gates. Existing GitHub
  v0.1.0 artifacts predate this change. This session ran on macOS only.

See [temporal/autonomy details](temporal-autonomy.md),
[benchmark configuration](../bench/README.md), [SDK usage](../sdk/README.md), and
[verified distribution state](distribution.md) for the operational contracts.
