# Filing — centroid-first, model-second, never model-dependent

Session C1 of the plan (§7.1). How a new lake item finds its class, and why
the answer no longer starts with a model call.

## The three tiers

1. **Centroid** (deterministic, every run). Each live class carries a running
   mean of its members' chunk-0 vectors — `class_centroids(node_id, model,
   dim, n, sum_vec)`, a cache over `class_links × embeddings` that the
   organize run rebuilds at its start (a few thousand vectors: milliseconds)
   and folds each filing into as it goes. A new item's vector (stored, or
   embedded now and stored so the semantic arm keeps the same vector) is
   scored by cosine against every centroid under its **provenance root** —
   its project's root when the tree has one, else `~general`; provenance is
   ground truth, never inferred. It files on the spot when
   `top1 ≥ T1 ∧ top1 − top2 ≥ M`.
2. **Batch** (a model, only for the ambiguous remainder). Items under the bar
   go to the model in batches of at most 20, in a candidates-only prompt: a
   legend of the classes any item in the batch may name (short handles —
   `c1`, `c2` — not 35-byte ids), then one fenced 160-character head per item
   with its handles. The reply vocabulary is closed: `file` to a class that
   was **shown for that seq**, or `inbox`; any other op, seq or class is
   dropped (the first valid op per seq wins, so a bad guess followed by
   `inbox` still parks the item). Every item is fenced with a per-run nonce
   under the §5.5 standing rule — a record to organize, never an instruction.
3. **Inbox** (no model). With no model configured the ambiguous items file
   under the root's `~inbox` sub-class, created on demand. The next run that
   has a model re-offers up to 20 inbox members to its batch; a member the
   model places elsewhere has its inbox link retired (`retired_by_run`, note
   `refiled → <class>`).

Capture, retrieval and filing never depend on a model (R12). What the tier
does not handle at all — revisions, session links, the organizer's own
events — is the consolidation classifier's, as before.

## Consolidation cadence

The big classifier pass (promote / split / merge / collapse / supersede, and
the leftovers the batch could not place) runs every fifth organize, or when
`polis_meta` `polis.health.pressure` is `"1"` (B3's `catalog_health` sets it;
the run clears it). The counter is `polis.filing.organizeCount`. With no
model there is nothing to consolidate: the counter still runs, the run does
not.

## Calibration — the (T1, M) pair

Leave-one-out over the store's own live links: hold each member out of its
class's centroid, rank it against every centroid under its root, and score
the margin rule over a grid (T1 0.40 … 0.85 by 0.05, M 0.00 … 0.20 by 0.02).
The chosen pair is the one with **precision ≥ 0.90** that covers the most
links (ties → the higher T1); it is written to `polis_meta`
(`polis.filing.t1`, `polis.filing.margin`) and read by every run. The same
pass reports **filing consistency**: top-1 agreement over the links whose
class keeps a centroid with them held out (n ≥ 2) — the plan's ≥ 85 % row.

Run it on a copy of a real store:

```sh
POLIS_REAL_DB=<copy> cargo test -p polis-memory --features eval -- --ignored real_db_filing_calibration --nocapture
POLIS_REAL_DB=<copy> cargo test -p polis-memory --features eval -- --ignored real_db_organize_no_model --nocapture
```

### Measured on the real corpus (2026-09-07, this machine)

Corpus: a copy of the live Redline DB (5,735 events, 2,164 prompts, 229
classes, 3,842 links) whose semantic index is Apple's `apple-sentence-en`
(the macOS sentence embedder — the weak fallback, per the plan's §2.5).

| Instrument | Result |
|---|---|
| Population (live prompt/page links with a chunk-0 vector) | 2,492 (2,484 coverable, n ≥ 2) |
| Filing consistency (top-1, leave-one-out) | **0.404** |
| Precision at the plan's initial pair (T1 0.55, M 0.10) | 0.738 over 7.3 % coverage (135/183) |
| Best precision on the grid (≥ 20 filings) | 0.864 at (0.80, 0.20), 1.8 % coverage |
| Pair at precision ≥ 0.90 | **none** → the centroid tier is OFF for this embedder (`polis_meta` T1 = 1.01) |
| Leave-one-out wall time | 1.7 s |

So on this corpus, with this embedder, the deterministic tier does not file:
every item goes to the batch (with a model) or to `~inbox` (without one),
and the ≥ 85 % consistency row is **not met** — by the embedder, not the
mechanism. The same pass over B1's synthetic corpus (2,000 prompts, classes
by topic) with the 64-dim bag-of-words embedder: population 1,295,
consistency **0.803**, chosen pair (T1 0.45, M 0.02) at precision 0.914 and
76 % coverage, and a perfect-precision point (1.0) at 33 % coverage. Program
C2 replaces the embedder and re-runs this calibration; the tier switches on
by itself the first time a pair clears the floor.

Organize with **no model** over the real copy, ten windows of the newest
400 lake items each (≈ 300 fileable per window; the remainder is the
organizer's own bookkeeping), stored vectors only, the initial thresholds:
**p50 581 ms, p90 675 ms, max 687 ms** — against the plan's 20 s / 60 s rows
(the classifier path measured 79.6 s median before). Ambiguous batch
prompts over the synthetic corpus with a recorded-reply model: **median 6,776 B, max 7,139 B** over 16 batches of up to 20 items (the 10 KB row).

Results: `bench/results/filing-2026-09-08.json`,
`filing-synthetic-2026-09-08.json`, `filing-organize-2026-09-08.json`.

## The model transport

A standalone `polis` picks its model from the environment, in this order:
`POLIS_NO_NETWORK=1` forbids HTTP; `ANTHROPIC_API_KEY` → the Anthropic API
(`POLIS_MODEL` overrides the model) — no process spawn, no 5–10 s CLI startup
per pass; `OPENAI_BASE_URL` + `OPENAI_API_KEY` (+ `OPENAI_MODEL`) → an
OpenAI-compatible endpoint; `claude` on PATH → the Claude Code CLI; `codex`
→ the Codex CLI; nothing → the no-model state. The `cli` feature turns on
polis-llm's `anthropic` and `openai-compat`; a host (Redline) never enables
`cli`, so its graph is unchanged. The embedder is this platform's on-device
provider (Apple's on macOS; Program C2 adds the portable ones) — with none,
every ambiguous item waits in `~inbox`.

## What CI proves (the keyless job)

No API key, no `claude` / `codex` / `ollama` on PATH: build the binary,
`polis init`, capture two prompts through the hook's own command, `polis
search` finds them, `polis organize` files them (`~inbox`, or by centroid),
`polis tree` shows where, `polis doctor` reports `no_model` as a fact and
offers no restore, `polis verify` is green.

## Seams left for the parallel sessions

- `filing::centroid_similarity(polis, a, b) -> Option<f32>` — B3's merge
  adjudication reads the centroid cosine through this (its
  `SimilarityOracle` seam); `None` when either node has no centroid or no
  embedder is configured.
- The batch prompt's fence (`filing::fence_item`, `fence_nonce`,
  `FENCE_RULE`) has the §5.5 shape; B3's `fence.rs` renderer replaces it
  once it lands (marked `FENCE(B3)` in the source).
