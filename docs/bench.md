# Measurement — the budget, the eval, the canary, the baseline

Polis Memory is measured at every point (plan §6). This document is the
budget the code is held to, the instruments that produce the numbers, and the
baseline the repository was last measured at. Every number below was taken on
the machine and date named in `bench/results/baseline.json`; CI re-checks the
gate rows on every push.

**What the accuracy numbers mean, stated first:** the eval and the canary
measure *reachability* — whether something the lake recorded can be reached
again through the answer pack by the words that were written. They use no
labels and no model, so they are honest about the record and say nothing
about human relevance. LongMemEval and the competitor comparisons (§6.2
items 2–4) are Program F.

## Latency budget (§6.1)

| Operation | Corpus | p50 | p95 | Instrument |
|---|---|---|---|---|
| ingest (one item) | any | < 5 ms | < 20 ms | `ingest.item`; bench `ingest` |
| answer pack, warm / cold | 10k prompts | < 50 / < 300 ms | < 200 / < 800 ms | `pack`; bench `pack_warm` / `pack_cold` |
| semantic search alone | 100k chunks | < 60 ms | < 120 ms | bench `semantic` (bag-of-words embedder) |
| grep (trigram) | 10k | < 30 ms | < 100 ms | `grep`; bench `grep` |
| MCP `memory_context` round-trip | 10k | < 100 ms | < 300 ms | `context` (the block; the transport is E1's) |
| canary set (~200 packs) | real DB | < 3 s | < 8 s | `canary.elapsed_ms` |
| organize per item / per run | any | < 3 s / p50 < 20 s | p90 < 60 s | `class_runs.duration_ms` / `items` |
| embed one chunk | — | < 1 ms | — | Program C (no portable model yet) |

Instruments, all in the repository:

- **Spans + ring.** Every retrieval arm (`pack.resolve`, `pack.node`,
  `pack.notes`, `pack.lexical`, `pack.semantic`, `pack.browse`, `pack.grep`),
  the pack as a whole (`pack`), `context`, `grep`, the write paths
  (`ingest`, `ingest.item`, `ingest.capture`, `remember`), every gardener pass
  (`gardener.organize/.compaction/.observations/.embed`) and every route
  (`route:<pattern>`) runs inside an `info_span!("polis.op", op, ms)` and
  records into `polis_core::latency` — a 256-sample ring per op.
  `GET /v1/context/stats` (`latency`) and `MemoryApi::stats` report p50/p95/max
  per op, live, for this process.
- **Benches.** `crates/polis-memory/benches/memory.rs` (criterion): ingest,
  pack cold (a fresh connection per iteration) and warm, grep, context, and
  semantic search + the fused pack over a deterministic hashed bag-of-words
  embedder (CI has no on-device model; it measures the vector path, not a
  model). Sizes via `POLIS_BENCH_PROMPTS`; `POLIS_BENCH_SCALE` scales
  agent/system bodies and page text (the 100k corpus uses 0.1 so the file
  stays under a gigabyte).
- **Eval.** `polis_memory::eval` — `cargo test -p polis-memory --features
  eval -- --ignored eval_synthetic_writes_results --nocapture` writes
  `bench/results/<date>-<sha>.json`.
- **Real-DB instrument.** `POLIS_REAL_DB=<a COPY of the backup> cargo test -p
  polis-memory --features eval -- --ignored eval_real_db_instrument
  --nocapture` opens the copy, prints p50/p95 per op (cold pack included), the
  eval and the canary; `eval_canary_calibration_real_db` the noise floor.
- **Gate.** `cargo test -p polis-memory --features eval -- eval_gate`: the
  deterministic 1k synthetic corpus against `bench/results/baseline.json`
  (Recall@10 within 2 points of the baseline; p95 of `ingest.item`, `pack`,
  `grep`, `context` under the §6.1 ceilings — the 10k rows applied at 1k,
  which is conservative). CI's `eval` job runs it on every push and PR.

## The corpus

`polis_memory::corpus` writes a seeded lake through the store's own paths
(prompt, decision, browse, note, supersession, class filing + acceptance), so
what the benches and the gate measure is the production write path. Its shape
was fitted to a copy of the author's real lake on 2026-09-07 — 2,164 prompts,
5,735 events, 229 class nodes (30 roots), 3,842 links, 1,483 browse events,
66 days:

| Axis | Real | Fitted |
|---|---|---|
| Roles | user 61% · agent 28% · system 11% | same |
| User body, chars p10/p50/p90/p99 | 16 / 135 / 896 / 8,202 | quantile table, log-linear |
| Agent / system body, chars p50 | 8,004 / 16,404 | quantile tables × `machine_body_scale` |
| Surfaces | pty 68% · fork 9% · external 7% · browse 4% · front-door 3% … | same weights |
| Project roots | 44; 7.5% of prompts have none | `prompts/49` clamped 4..=44 |
| Decisions (approvals) | 1 per 17 prompts | same |
| Browse events | 0.69 per prompt; 34% distinct pages; ~2.9 KB text | same |
| Class nodes | prompts / 9.5; depth 1 70% · 2 15% · deeper 2% | same |
| Links | 1.78 × prompts; per node p50 9 · p90 28 · max 407 | Zipf class draw |
| Notes | 0 | 1% of prompts (synthetic, so the Note subject exists) |
| Supersessions | 1 | 1 per 8 approvals (over-provisioned for the sample) |

Text is pseudo-English from a fixed vocabulary with a topic bag per class
(~55% of a prompt's words), plus identifiers (`--flag`, `src/x/y.rs`,
`ERR_CODE`) for the grep arm. Not English; see the note at the top.

## The canary (§5.3)

Frozen at run start, evaluated before and after a gardener run, DB-only,
~200 `build_answer_pack` calls at `limit = 8`:

| Subject | Probe | Hit |
|---|---|---|
| Decision | the class a filed decision sits under (its title; the host's evidence text when there is any) | the resolved node's links carry the decision seq |
| Supersession | the class linking the superseding decision | new seq present unsuperseded; old, if present, marked |
| Note | six words of a note | the note is in the pack's notes |
| PromptSpan | an 8–12-token span of a recent user prompt, seeded by run id | the prompt's seq is a prompt hit (or absorbed as a duplicate) |
| ClassReach | a class title | the pack resolves that class, with links |

An **unfiled** decision has no text of its own in the store and is
unreachable today; the set counts them (`unfiled_decisions`) and probes the
filed ones. The C-program's decision arm (FTS over decision evidence) is what
changes that.

**Regression rule** (`canary::regression`): zero tolerance on Supersession
and Note; aggregate `recall_after < recall_before − max(0.05, 3/N)`.

### Calibration (noise floor)

Ten freezes with different run ids (the PromptSpan spans move; the other
subjects are deterministic), same lake:

| Lake | Aggregate mean | Aggregate σ | PromptSpan σ | 3σ | Rule tolerance at N | Set wall p50 |
|---|---|---|---|---|---|---|
| synthetic 1k (168 probes) | 0.984 | 0.006 | 0.018 | 0.017 | 0.050 | 1.7 s |
| real-DB copy (201 probes) | 0.756 | 0.006 | 0.023 | 0.017 | 0.050 | 3.4 s |

**Recommended thresholds:** keep the plan's rule. The observed 3σ (0.017) is
a third of the 0.05 floor on both lakes, so the aggregate rule cannot fire on
reseeding noise; the floor only binds at N < 60 (`3/N` takes over below
that). The zero-tolerance subjects showed σ = 0 across runs (their probes do
not depend on the run id), so any drop there is a real change. One caveat
for B3: a run that FILES a previously unfiled decision changes the Decision
probe set between freezes — compare a run against its own frozen set (as
§5.3 says), never against a set frozen after it.

## Baseline (2026-09-07)

Machine: `bench/results/baseline.json` → `machine`. Numbers from
`eval_synthetic_writes_results`, `eval_real_db_instrument` and the criterion
run below.

### Accuracy (reachability)

| Lake | Probes | Recall@5 / @10 / @20 | MRR | pack-contains-gold | Canary (n, recall) | bytes/pack p50 / p95 |
|---|---|---|---|---|---|---|
| synthetic 1k | 218 | 0.97 / 0.97 / 0.97 | 0.95 | 0.972 | 168, 0.976 | see baseline.json |
| real-DB copy | 251 | 0.66 / 0.70 / 0.70 | 0.59 | 0.701 | 201, 0.751 | 14.3 KB / 29.3 KB |

Per subject on the real copy: Decision 55/100, Supersession 1/1, PromptSpan
45/50, ClassReach 50/50, BrowseTitle 25/50 (Note: none exist yet).

**The Decision number is the pack's fan-out cap, measured.** 45 of the 100
probed decisions sit beyond the pack's per-node cap of 40 links (34 of them
in one 317-link class), so the class resolves but the link is truncated
away. That is §6.3's "no live node > 150 links" pressure made visible; the
consolidation prompt (B3) and a smarter cap (newest-first, or the pack's own
budget instead of a count) are the levers. Recall@k is computed over the
ranked subjects (prompt spans, browse titles) only — a node's links are
filing order, not a ranking.

### Latency, measured

Real-DB copy (2,164 prompts), this machine, `eval_real_db_instrument`:

| Op | n | p50 | p95 | Budget row |
|---|---|---|---|---|
| pack (warm) | 272 | 14 ms | 27 ms | < 50 / < 200 ✓ |
| pack cold (first after open) | 1 | 19 ms | — | < 300 / < 800 ✓ |
| context | 20 | 19 ms | 32 ms | < 100 / < 300 ✓ |
| grep | 5 | 8 ms | 8 ms | < 30 / < 100 ✓ |
| ingest.item | 100 | 0 ms | 2 ms | < 5 / < 20 ✓ |
| canary set (201 packs) | 10 runs | 3.36 s | 3.47 s | < 3 s p50 ✗ (12% over) / < 8 s p95 ✓ |
| MCP `memory_context` round-trip (synthetic 10k, `polis mcp` over stdio, debug binary) | 200 | 38.5 ms | 76.7 ms | < 100 / < 300 ✓ (F1, `eval_mcp_roundtrip`) |

The canary set's p50 is over its row; the pack's time is the browse arm
(`pack.browse` p50 9 of the pack's 14 ms — the FTS5 MATCH over 1,483 page
texts with the recency bonus). That arm is the first thing the C-program
should look at.

Synthetic 1k, `eval_synthetic_writes_results`: pack p50/p95 8/14 ms, context
8/12, grep 3/3, ingest.item 0/0, canary set 1.7 s.

### Criterion (synthetic corpus, this machine)

`cargo bench -p polis-memory --bench memory`, criterion means (30 samples,
8 s per op); the corpus is written to a temp FILE in WAL mode, as an install
has it; the `ingest` bench runs last so its rows never inflate the reads.

| Bench | 1k | 10k | 100k (bodies × 0.1) | Budget row (10k) |
|---|---|---|---|---|
| `pack_cold` (fresh connection per pack) | 4.9 ms | 24.8 ms | 101 ms | < 300 / < 800 ms ✓ |
| `pack_warm` | 4.7 ms | 24.9 ms | 101 ms | < 50 / < 200 ms ✓ |
| `grep` (trigram, dense identifier needles) | 1.6 ms | 7.5 ms | 31 ms | < 30 / < 100 ms ✓ |
| `context` (the grounding block) | 4.7 ms | 25.3 ms | 100 ms | < 100 / < 300 ms ✓ |
| `semantic` (bag-of-words 64-d, brute force) | 0.21 ms / 1,074 targets | 2.2 ms / 10,604 | 18.2 ms / 80,000 (partial) | < 60 / < 120 ms at 100k chunks ✓ |
| `pack_warm_fused` (lexical + semantic, RRF) | 5.5 ms | 30.3 ms | 138 ms | — |
| `ingest` (one item through `MemoryApi::ingest`) | 0.33 ms | 0.39 ms | 0.53 ms | < 5 / < 20 ms ✓ |

Corpora as written: 1k → 1,000 prompts / 649 pages / 105 classes / 1,247
links, 58 MB file; 10k → 10,000 / 6,900 / 1,053 / 12,720, 183 MB; 100k
(machine bodies and page text × 0.1) → 100,000 / 68,310 / 10,526 / 126,171,
558 MB, seeded in 168 s. Every 10k budget row holds on this machine; the
pack scales about linearly with the lake (4.7 → 25 → 101 ms), which is the
browse arm's FTS5 MATCH growing with the page text — the C-program's first
target.

**Semantic at 100k is a partial index.** Indexing throughput fell from
1.2k targets/s (10k corpus) to 0.8k/s (80k targets), and the first
unbounded run did not finish the 100k corpus's ~140k eligible targets in 11
minutes: `embedding_backlog`'s correlated `NOT EXISTS` over the growing
`embeddings` table costs more per tick as the table grows. The bench bounds
indexing (`POLIS_BENCH_INDEX_SECS`, 60 s) and reports the partial count;
the fix — an indexed "already embedded" set or a high-water mark — belongs
to C2 with the portable embedder.

### File vs memory

The same 1k lake as a FILE (WAL, as an install has it) and in memory, the
same 30 pack queries and 18 grep needles (`eval_file_vs_memory_diagnostic`):

| Store | pack p50 | grep p50 |
|---|---|---|
| file, as opened (WAL after seeding) | 13.6 ms | 3.8 ms |
| file, after `wal_checkpoint(TRUNCATE)` | 14.0 ms | 3.8 ms |
| file, 64 MB page cache | 13.0 ms | 3.1 ms |
| file, 64 MB cache + 256 MB mmap | 12.9 ms | 3.1 ms |
| memory | 13.0 ms | 2.8 ms |

No file penalty worth a pragma: the OS page cache carries it. (The first
bench run reported pack 27 ms at 1k — its `ingest` bench ran first and
criterion drove it ~26,000 iterations, so the read benches ran over a lake
26k prompts larger than labelled. `ingest` now runs last.)



## Gardener efficacy (§6.3)

`class_runs` now carries `duration_ms`, `items`, `ops`, `model`, `outcome`
(`done | error | reverted_by_canary`), `canary_before`, `canary_after`,
`error` (store schema version 2); `organize_once` fills the first six on
every run. `catalog_health` (B3) reads them for organize p50/p90, the error
rate over the last 50 and the canary trend. On the real lake the plan
measured organize at median 79.6 s, p90 275 s, error 6.1% over the last 50
— those are the numbers B3 is held to improving.

## Filing (C1, docs/filing.md)

Centroid-first filing, measured 2026-09-08 on this machine (the full
account is in docs/filing.md):

| Row | Real corpus (apple-sentence-en) | Synthetic 2k (bag-of-words) | Budget |
|---|---|---|---|
| Filing consistency (top-1, leave-one-out) | 0.404 | 0.803 | ≥ 0.85 |
| Chosen (T1, M) at precision ≥ 0.90 | none — tier OFF | (0.45, 0.02): precision 0.914, coverage 76 % | precision ≥ 0.90 |
| Organize, no model, 400-item window | p50 581 ms · p90 675 ms | — | p50 < 20 s · p90 < 60 s |
| Ambiguous batch prompt bytes (median) | — | 6,776 B (max 7,139, 16 batches) | ≤ 10 KB |
| Canary after C1 (B1's instrument, real copy) | recall 0.751, pack p50 18 ms | — | flat |

The real corpus's consistency row is the embedder's number, not the
mechanism's: the plan's "decided by measurement" gate for embeddings (C2)
re-runs `real_db_filing_calibration` per provider, and the tier turns on for
the first one that clears the floor.

## LongMemEval (§6.2, Session F1)

The runner, the competitor scripts, the nightly job and the table live under
[`bench/`](../bench/README.md). What follows was written **before the first
scored run**, as the plan requires, and the first scored run is scored
against it.

### The kill criterion (§6.2 item 4, verbatim)

After F2 and F3, `polis-full` within 5 points of the best competitor overall
on LongMemEval-100 is acceptable and is reported as such next to the
write-path cost difference (Polis 0 LLM calls per add). Trailing the best
competitor by more than 10 points overall, or `multi-session` trailing by
more than 5 after the 2-hop expansion, changes the strategy: the response is
an opt-in, background, cited write-side "enrich" pass (an extraction over
user text, filed and superseded like claims), not a substrate change. No
landing-page number is published until this table exists.

### Conditions

LongMemEval-S (HF `xiaowu0162/longmemeval-cleaned`; 500 questions over six
types plus `_abs` abstention variants), a seeded stratified subset of 100
for the nightly job and the full 500 per release. Every system under
identical conditions: the same questions, the same fixed answer model given
a ≤ 4,000-token grounding context, the same fixed judge with the
LongMemEval type-specific yes/no templates (`bench/common/judge.py`), the
same hardware, one memory namespace per question (a fresh `POLIS_HOME`; a
Mem0 `user_id`; a Graphiti `group_id`). Two Polis configs: `polis-default`
(user turns only — the capture hook's world) and `polis-full` (every role).
Cost columns beside accuracy: ingest wall, LLM calls and tokens on the
write path (Polis: 0 by design; Mem0 and Graphiti extract with a model on
every add — their runners count the calls), query p50, context tokens p50.

### The table

| system | n | overall | ss-user | ss-asst | ss-pref | temporal | kn-update | multi-sess | abstain | write calls | write tokens | query p50 ms | ctx tokens p50 | models (answer / judge) | date | commit | hardware |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| polis-default | — | not yet run — needs a keyed run | | | | | | | | 0 | 0 | | | | | | |
| polis-full | — | not yet run — needs a keyed run | | | | | | | | 0 | 0 | | | | | | |
| mem0-oss (v3) | — | not yet run — needs a keyed run | | | | | | | | | | | | | | | |
| graphiti-falkordb | — | not yet run — needs a keyed run | | | | | | | | | | | | | | | |

A row enters this table by hand, from a results file the nightly job or a
release dispatch uploaded (`bench/results/<date>-<sha>-longmemeval-<system>.json`,
rendered with `python3 -m bench.common.report`), with the commit, the
models, the date and the hardware it was measured on. **Verdict against
the kill criterion: not yet scored.**

### What F1 could and could not run on this machine (2026-09-07)

- Built and exercised end to end, keyless: all three runners in `--stub`
  mode (a deterministic stub answer model and stub judge; the stub judge
  says yes iff the gold answer's words reach the response, so a stub row is
  retrieval *reachability*, never a score — the renderer labels it STUB),
  on a six-question built-in sample covering every type; the four results
  files join into one table. The FalkorDB compose file starts healthy on
  this machine (validated, then torn down).
- Not run: any scored question. There is no model key on this machine and
  nothing was spent. The nightly job (`.github/workflows/bench.yml`) skips
  cleanly until the repository carries `LONGMEMEVAL_API_KEY`.
- A finding the stub run surfaced before any key: `polis-full` retrieves
  the same rows as `polis-default` today, because the answer pack excludes
  `role = agent` prompts by design (§5.5 keeps agent and system text out of
  prompts). Making assistant turns retrievable for this benchmark is a
  retrieval decision (a role filter on the pack under an explicit scope),
  not a benchmark knob; until it is made, the two Polis rows will differ
  only in what they ingested, and the assistant-answer categories will
  score near zero for both, as the plan predicted for `polis-default`.
- Cost estimate for a keyed run (from the dataset's shape: ~115k tokens of
  haystack per question, an answer call over ≤ 4k context, a judge call of
  ~300 tokens): Polis ≈ 2 calls and ≈ 5k tokens per question → ~200 calls /
  ~0.5M tokens for 100 questions, ~1,000 calls / ~2.5M tokens for 500, per
  config; Mem0 and Graphiti add their write-path extraction over the whole
  haystack (~40 sessions per question) — tens of calls and ~150–300k
  tokens per question, i.e. an order of magnitude more. The nightly job's
  default cap is 6M tokens for the whole run.

