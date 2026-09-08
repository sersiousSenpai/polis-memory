# The ledger — the chain, its kinds, and reversibility

Polis Memory's record is a hash chain of events over a lake of prompts. The
chain is append-only and cross-process-safe; the catalog the gardener builds
over it is fully reversible per run. This document is the narrative for both;
the code is `polis-core/src/ledger.rs` (the vocabulary and the hashing),
`polis-store/src/ledger.rs` (the append), `polis-store/src/runs.rs` (the
journal and the inverse) and `polis-memory/src/revert.rs` (the facade and the
property test that is the gate).

## The chain

Every event is one row of `ledger_events`: `seq` (dense, 1-based), `ts`,
`kind`, `author`, optional `prompt_id` / `session_id` / `version_number`,
optional `(ref_kind, ref_id)` naming what the event is about, `payload_hash`
(sha256 of the event's own content — a prompt body, a decision's fields, a
reorg's detail), `prev_hash` and `entry_hash`.

```
entry_hash = sha256( prev_hash ‖ canonical_json(event) )
```

`canonical_json` is the fixed-order rendering of `seq, ts, kind, author,
prompt_id, session_id, version_number, ref_kind, ref_id, payload_hash` with
`null` spelled out; the genesis `prev_hash` is 64 zeros. The field order and
the concatenation are pinned by `entry_hash_vector_is_pinned` against a digest
computed outside Rust. `verify` re-walks the chain from genesis (the
incremental verify cannot see retroactive tampering of already-verified rows,
so the full walk runs on the gardener's backup cadence as well).

**Why `BEGIN IMMEDIATE`.** Under one in-process mutex the read-then-insert of
the head is already serial; across processes (a host app and the standalone
daemon on one file) two writers could each read the same head and fork the
chain. `append_event` takes the write lock before it reads the head — when it
owns the transaction it opens `BEGIN IMMEDIATE` with a jittered busy retry,
and when a caller already holds a transaction it joins it — so the chain has
exactly one writer at a time.

**The consume-once guards.** Prompts an agent constructs (a classifier's own
`-p` prompt, a keeper pass) are registered before the spawn and claimed once by
the capture route; an unclaimed registration expires. The guard is a generic
`TtlGuard<V>` in polis-core; it is what keeps a headless agent's prompt from
being captured as if a person typed it.

## Kinds

| Kind | About | Who appends |
|---|---|---|
| `prompt` | a captured or constructed prompt (the body's hash) | the capture route, the hosts' construction sites |
| `revision`, `resolution`, `approval`, `reopen`, `review_verdict`, `pin`, `source_trust`, `session_link`, `browse_event`, `note`, `work_file`, `work_claim`, `work_close`, `moot_turn`, `router_verdict` | the host's decisions, notes and surface events | the host, at its own write points |
| `class_curate` | a node or link filed / created / accepted / refused | the organizer |
| `taxonomy_reorg` | promote / split / merge / collapse applied | the organizer |
| `supersede` | one decision superseded by another | the verifier |
| `compaction` | a cold prompt's body replaced by its gist (the body archived) | the keeper |
| `observation` | a pattern observed over a node's links | the keeper |
| `gardener_revert` | **B2** — a run was undone (`ref_kind = class_run`, payload `{run, ops_reverted, by_run}`) | `revert_run` |
| `gardener_regression` | **reserved for B3** — the canary measured a recall regression after a run and it was auto-reverted; same reference shape, before/after recall in the payload | the gardener |

`verify_bundle` does not whitelist kinds: a bundle carrying `gardener_revert`
verifies like any other, which `a_full_bundle_with_the_b2_kinds_verifies`
pins so a future whitelist cannot make a reverted lake un-exportable.

## Reversibility (§5.2)

Nothing is ever deleted from the chain, and — since B2 — nothing a gardener
run does to the catalog is beyond undoing for as long as its journal lives.

### Retire-marks instead of deletes

A destructive op stamps the row with the run that retired it:
`class_nodes.retired_by_run` (+ `retired_into`, the digest or merge target the
sourcing moved to), `class_links.retired_by_run`,
`class_observations.retired_by_run`. Every reader filters
`retired_by_run IS NULL` — the catalog listings, the tree and node views, the
answer pack's link fetches, the classifier's snapshot, coldness, the
observations, the mirror, the bundle export. `delete_node_subtree` became
`retire_node_subtree(conn, id, run, into)`, which returns exactly what it
marked; merge's silently-dropped duplicate links are marked rows too, so the
merge that dropped them can be undone. Point lookups inside the apply paths
(`SELECT parent_id FROM class_nodes WHERE id = ?`) deliberately do not filter:
a revert must be able to read the rows it restores.

### The journal

`class_run_ops(run_id, op_ix, op, subject_ids, outcome, reason, pre_image,
pre_hash, post_image, ledger_seq, reverted_by_run)`, one row per op a run
applied, written under the same lock as the op itself. `pre_image` is the
deflated JSON of exactly the rows the inverse needs (`pre_hash` its sha256);
`subject_ids` names what the op touched (`node:<id>`, `link:<id>`, `obs:<id>`,
`prompt:<id>`, `seq:<n>`); `ledger_seq` is the event the op appended.
`class_runs` carries `mode` (`organize | compaction | observations | revert |
curation`), the cost columns (`llm_calls`, `prompt_bytes`, `tokens_in`,
`tokens_out`, `wall_ms`) beside B1's accounting, and `canary_json` for B3.
Compaction and observation passes are runs of their own now, so a bad pass is
one `revert_run` away.

### The inverse, per op

| op | pre-image | inverse |
|---|---|---|
| `file` | `{linkId, nodeId, targetKind, targetId, revivedFromRun}` | delete the link — or, when the filing revived a link an earlier run had retired, put that run's mark back |
| `create` | `{nodeId, parentId, title}` | retire the node |
| `promote` | `{nodeId, oldParent, newParent}` | restore the old parent |
| `split` | `{nodeId, parentId, created: [{id, title, linkIds}]}` | move every link back to the source, retire the parts |
| `merge` | `{target, oldTitle, oldParent, absorbed: [{id, parentId, title}], movedLinks: [{linkId, from}], leftoverLinks, children: [{id, oldParent}], observations}` | restore the target's title and parent, move the links back, re-parent the children, un-retire the absorbed nodes, their leftover links and their observations |
| `collapse` | `{nodeId, digestId, retiredNodes, retiredLinks, retiredObservations, citationLinks}` | retire the digest and its citation links, clear the subtree's marks |
| `supersede` | `{oldSeq, newSeq, eventSeq}` | delete the plain `supersessions` row (the `supersede` event stays in the chain) |
| `compact` | `{promptId, eventSeq}` | `restore_prompt_body`, hash-verified against `prompt_archive` (the archive is the pre-image) |
| `observe` | `{observationId, nodeId, eventSeq}` | retire the row |

`revert_run(run_id)` opens one `BEGIN IMMEDIATE`, walks the run's applied
ops `op_ix` descending, restores each from its image, marks the ops
`reverted` with `reverted_by_run = <the revert's own run row>`, appends
`gardener_revert`, and sets the run's `outcome = reverted`. It refuses — as
data, never a fault — when a later run's `subject_ids` overlap ("revert run
N+k first"), when the run is past the vacuum horizon, when it was already
reverted, or when an op has no image to restore from (a compacted body whose
archive row is gone). Reverting is a GUI / HTTP action
(`POST /v1/memory/runs/:id/revert`); it is deliberately not an MCP tool.

### The horizon

`vacuum_retired(older_than_runs = 50)` physically deletes retired rows whose
mark is at or below `max(run id) − 50` and drops the images of journal rows
behind that line, recording the horizon in `polis_meta`
(`polis.revert.horizonRun`); the gardener calls it after its compaction pass,
never a reader. A run at or below the horizon can no longer be reverted, and
`revert_run` says so by name.

### The gate

`polis-memory/src/revert.rs` holds the property test: a seeded corpus, runs of
random ops in the classifier's proposal shape (file / create / promote / split
/ merge / collapse / supersede, with compaction and an observation pass mixed
in) applied through the real stage → accept → apply path, a snapshot before
each run, reverts newest-first, and after each revert the live catalog is
byte-equal to that run's pre-snapshot; the chain verifies after every step; a
full bundle carrying `gardener_revert` verifies. Excluded from the equality,
and why: retired rows (a revert marks what a run created rather than deleting
it — the marks are what the vacuum later removes), `class_nodes.updated_at`
(re-parenting stamps it; the revert restores the parent, not the clock), the
journal itself and the chain (append-only by design), the derived FTS and
embedding tables, and autoincrement counters (a reverted filing's link is
deleted; the next id is higher). Everything else — every live node column but
`updated_at`, every live link and observation, the `supersessions` rows, every
prompt's body / gist / compaction marks, the archive — comes back exactly.

## No curation (B3)

Since Session B3 the chain records what an autonomous loop did, never what a
person approved: `class_curate` events carry `action = file | create |
organize | refuse | expire`, `taxonomy_reorg` the applied structural ops,
`supersede` the applied supersessions, `observation` the written and the
retired patterns (`action = retire`), `gardener_revert` the reverts and
`gardener_regression` the canary's own. The `status`, `pinned`, `dismissed`
and `starred` columns those older events referred to stay in the tables
unread; see `docs/architecture.md` for what decides instead.
