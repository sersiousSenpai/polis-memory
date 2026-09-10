# Temporal claims and recoverable background work

Schema versions 6–10 add cited claim records and their rebuildable projection,
leased background jobs, a forget registry and redaction outbox, and local model
usage records. Migrations add tables and indexes without changing old ledger
events or hashes. Atomic scoped capture distinguishes user, assistant, and agent-preface
evidence across namespaces without deleting historical rows during identity
adoption. Its lookup index remains nonunique so later namespace convergence
cannot make a migration fail.

## Claim semantics

`polis_core::claims::ClaimWrite` is a structured, model-free write. It names a
stable ID, subject, closed predicate, typed value, scope, supporting sources,
derivation, and a half-open valid interval. Supporting sources carry the device
chain, ledger sequence, original role, exact quotation, and optional byte offset.
Gardener-labelled writes additionally require a running organizer run and model version.
Repeating an identical ID and assertion is idempotent; changing its content is
rejected. New IDs and explicit supersession preserve the earlier assertion.

`ClaimQuery.valid_at` selects the subject's valid time;
`ClaimQuery.known_at` independently selects what had been recorded. Both default
to the query's current wall-clock time. A correction valid from day 10 but
recorded on day 20 changes a query for day 15 only when `known_at` reaches day 20.
Explicit retirement also has recorded-time history. Source forgetting applies
to every time view and removes the copied claim text from both record and
projection.

Predicates cover preferences, project configuration, selected technology,
constraints, decisions, and relationships. Different eligible values remain
visible as unresolved alternatives. An assistant or model-derived assertion
cannot supersede explicit user evidence. Related claims must share a subject,
predicate, and source namespace. Source scope, role and timestamp eligibility
are checked before relevance ranking and response limits.

Current validation resolves local prompt sources, verifies chain and role,
checks exact quotation/offset, and requires the typed value to appear in every
supporting quotation. This is literal support validation, not a semantic
entailment proof. Non-prompt and foreign claim sources are rejected explicitly.
The bounded consolidation classifier may return up to eight claims alongside
its existing proposals. It can cite only exact passages actually shown in its
bounded prompt, from user or assistant sources. The runtime binds source role,
namespace, byte offset, source timestamp, current organizer run and the model
version reported by the backend. Missing model versions, invented metadata,
unshown passages and unsupported values become journaled refusals. These
derived alternatives cannot supersede prior claims. Claim writes and their
revert journal commit together; manual or canary rollback retires the claims.
This adds no capture-path model call. General semantic entailment and automatic
valid-date extraction are outside this literal extraction path.

## Background behavior

The standalone daemon schedules indexing and basic inbox filing every two
seconds in workers independent of idle consolidation. A short run with pinned
Model2Vec assets measured p95 semantic availability at 1.96 seconds for 50
captures at approximately five per second; overload and sustained backfill
remain unmeasured. [Run details](../bench/results/2026-09-10-local-model-readiness-final.json).
Basic filing uses its
own durable cursor and does not consume the consolidation window. It uses no
model or embedding request. The existing idle model pass can later refile the
inbox with centroids and consolidation.

Organization and basic filing use idempotency keys, persisted attempt counts,
leases, and checkpoints. Only one job of each kind can hold a live lease. Old
attempts cannot checkpoint or finish a reclaimed job. Failures retry up to the
configured limit; `PolisStore::retry_job` permits a targeted retry of a terminal
failure. Interrupted class runs do not consume their source window. Capped
organization windows now advance only through the evidence actually loaded.

Model requests default to a 120-second deadline, configurable per request up to
600 seconds. CLI stdout and HTTP response bodies stop at a 2 MB wire limit;
stderr is retained only up to 2 KB. CLI timeout kills and reaps the direct child
and cancels pipe readers. HTTP deadlines include response-body consumption.
Organization also has a 180-second whole-operation deadline. Native embedding
providers remain synchronous: their execution is isolated in a blocking worker
but cannot be forcibly cancelled safely. CLI descendants are not process-group
managed. API model backends retain their existing output-token caps; arbitrary
external CLI backends do not promise an exact provider token ceiling.

The frozen canary now samples role/scope eligibility, a 4096-byte response
ceiling, an explicitly absent namespace, exact claim support, and independent
valid/known boundaries, with zero tolerance for regressions in those subjects.
It samples up to eight recent source rows and eight recent claims. These are
invariant probes; the absent-namespace check is not a general natural-language
no-answer benchmark.

Gardener ownership uses an exclusive SQLite sidecar lock, acquired atomically
before diagnostic lock-file metadata is written. OS file locking releases
ownership after process exit without relying on a health probe. Daemon handles
load and adopt the same identity as local handles.

`polis doctor --json` includes recent job state, pending redaction emission,
and pending peer acknowledgments. The daemon's `model_usage` table keeps the
latest 10,000 seat/model/token records without prompt text. Missing reported
usage remains distinguishable from an observed count; provider-side costs not
reported by the backend cannot be reconstructed from those records.

## Forget and restore

Forgetting an already compacted prompt now removes its archive, prior gist,
extracted user text, embeddings, cited observations, copied link notes, and
dependent claims. A source tombstone and redaction-outbox entry commit with the
purge. Sharing redaction event and payload commit atomically; daemon ticks,
exports, and sync retry pending emission idempotently.

Use `target_kind: "ledger_event"` with a returned citation sequence to resolve
the captured source before forgetting. Ledger sequences and source row IDs are
different namespaces. `prompt`, `browse_event`, `note` and `user_note` targets accept row IDs.
Page forgetting removes its URL, title, text, caption, screenshot reference and
embeddings, while exact evidence reports `redacted`. Copied pending proposals
expire and their retained payloads are removed with affected run journals and
derived summaries.

Note forgetting covers standalone and targeted annotations, including all their
text edits and star/unstar events. Schema 10 backfills `note_events` from ledger
references without altering historical hashes. Citation lookup verifies that
mapping against the original reference and text commitment; an old edited body
that is no longer retained reports `unavailable`, and every forgotten version
reports `redacted`. Note text and scope, its event and history mapping now write
in one transaction. A forgotten note rejects later edits.

Every historical note citation enters the durable redaction outbox atomically
with the local purge. Upgraded peers remove their held version on the next sync,
including when redaction arrives before the body or an older bundle is replayed.
Only the source chain can issue its signed redaction. Pending emission and peer
acknowledgments remain visible in `doctor`. This requires peer code that supports
note redaction; previously exported bundle files themselves are not rewritten.

Forget also invalidates affected organizer runs. Delayed filing, observation,
collapse, queue-deferral, journal and run-summary writes cannot repopulate the
removed copies. Admission and mutation share a writer transaction.

File-backed forgetting also appends and syncs a body-hash marker to
`<database>.forgotten`. Managed restore reads this registry even when the live
SQLite file is corrupt, sanitizes a staged snapshot, verifies its chain, and
only then installs the replacement. Keep the registry with the database when
moving a home. Existing backup files are retained according to backup policy;
restoring them outside the managed workflow bypasses this protection. The
registry stores hashes and source identities, not original body text. Typed page,
note-origin and foreign-capture markers extend the legacy prompt markers. Tests
restore snapshots taken before note edits and before forgetting, including
recovery when the current database file is corrupt. Peer restore suppression
stays local; it does not fabricate a source-signed redaction event.

Deterministic regressions cover both temporal axes, contradictory roles,
unsupported citations, pre-limit source filters, projection rebuilding,
compact-then-forget, corrupt-live-file restore, lease crash recovery, stale
worker fencing, immediate basic filing, a 407-item organization backlog,
request timeout, oversized newline-free output, atomic ownership, and bearer
authentication on the assembled MCP service, bounded gardener claim admission,
claim rollback, and temporal/authority canary regressions. No paid benchmark is needed to
run these fixtures.
