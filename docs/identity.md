# Identity — keys, devices, agents, aliases, the bind, the envelope

Session E2 of the plan (§4.5, §4.6). Who wrote a memory is a hash of a public
key, never a chosen name.

## The key

`polis init` generates one Ed25519 keypair per human and writes it to
`$POLIS_HOME/identity.key` (the 32-byte seed, hex; the file is opened with
mode `0600` **before** any byte lands) and `identity.pub`. On Windows the
parent directory's ACL is the boundary until the owner-only ACL lands.

- `principal_id = hex(sha256(pubkey))` — the human's id.
- fingerprint = the first 16 hex characters, what a person reads aloud.

## One human, many devices, one chain per device

A chain has exactly one writer. Copying a key to a second machine must not
yield two divergent chains under one id, so every install is a **device**
sub-principal derived from the key and a name:

```
device_id = hex(sha256(pubkey ‖ 0x00 ‖ "device:" ‖ name))      chain_id = device_id
agent_id  = hex(sha256(pubkey ‖ 0x00 ‖ "agent:"  ‖ name))      under the device that runs it
```

Byte layout, exactly: the 32 raw public-key bytes, one zero byte, the ASCII
label, the name's UTF-8 bytes — no length prefix, nothing after the name.
`polis init --device <name>` names the device (default: the hostname). Two
devices of one human are peers that sync like anyone else (E3); the human's
signature vouches for every device and agent, so a trust store needs only
the human key.

Agents are derived, not keyed: `agent:keeper`, `agent:classifier`,
`agent:router`, `agent:claude-code` (the capture hook), `agent:codex`,
`agent:mcp:<clientInfo.name>` (an MCP client's writes), `agent:surface:<name>`
(a host surface). The `agent:` prefix is not part of the hashed name.

## Tables

```
principals(principal_id PK, kind human|device|agent|org, pubkey, parent_id, display_name, created_at)
principal_aliases(alias PK, principal_id)
```

Scope columns — non-hashed, like `gist` — on `prompts`, `browse_events`,
`user_notes`, `class_nodes`, `class_observations`:
`principal_id` (the human), `device_id`, `agent_id`, `run_id`, `org_id`,
`visibility private|org`; index `(principal_id, org_id, project_path)` where
a project column exists. Store schema version 3.

## The bind event

`principal_bind` (`ref_kind = principal`, `ref_id = device_id`) is appended
once per device, at `init` (and at a rotation, later). Its payload — the
human and device cards, the chain id, the chain head at binding time, and an
Ed25519 signature over `"polis.bind/1\n<chain_id>\n<head_hash>"` — is
committed by the event's `payload_hash` and kept readable in `polis_meta`
under `polis.identity.bind.<device_id>` so an export can carry it. Binding
to the head is what stops a bind from being replayed onto another chain
state. `CanonicalEvent` is untouched; `author` on every NEW event is the
device id, or the agent id when a seat or an MCP client writes.

## Aliases: existing history is never rewritten

Every event ever recorded keeps its author string (it is hashed). Adoption
writes one alias row per legacy string, by a total rule:

| author string | resolves to |
|---|---|
| the login, `local`, empty | this device |
| `classifier`, `keeper`, `router` | `agent:<name>` |
| anything else (`fork`, `browse`, `plan-approval`, `voice`, …) | `agent:surface:<name>` |

Reads resolve through `COALESCE(alias.principal_id, author)`; the timeline's
`principal` scope binds the id set below a principal (a human → their
devices → their agents). Aliases are insert-only.

## Adoption

`polis_memory::identity::adopt(store, &identity, login)` — idempotent, so a
host runs it on every boot: seeds the human, the device and the builtin
agents; aliases every author string the lake has seen; appends the bind if
this device has none; stamps every unscoped row from its ledger author
(`stamp_unscoped`, a partial-index sweep that is also the post-write stamp).

- `polis init [--device NAME]` — the key in `$POLIS_HOME`, then adopt.
- `polis init --from-redline <redline-data-dir>` — **points** this home at
  `<dir>/redline.db` (config `db`) and shares the key under `<dir>/polis/`
  (config `identity_dir`). Pointing, not copying: a copy would be a second
  writer on one chain — two heads under one id — which devices exist to
  prevent. Redline's own first attach adopts the same key and device.
- Every local open (`polis search`, `polis mcp`, `polis serve`) re-runs
  adoption; it costs a few indexed queries.
- `polis doctor` shows the principal, the device, the bind seq, and any rows
  still unscoped.

## Scope on the read paths

`Scope { principal, org, agent, run, project, include_shared }` reaches the
store as bound WHERE clauses: `principal` matches the human, one of their
devices or one of their agents on the row (the id set); `agent` an id or an
aliased seat name; `run` the run id (`claude_session_id` for captured
prompts); `org` the org id; `project` the project path. `include_shared`
is a no-op until E3. Over MCP every tool takes `scope`; over HTTP
`/v1/context/prompts` and `/v1/memory/ledger` take the same names as query
params.

## The writes

`memory_remember` (`as_user`), `memory_ingest` (idempotent on body hash +
run), `memory_annotate`, `memory_forget` (destructive; `confirm: "forget"`),
`memory_supersede`; the same five as routes (`POST /v1/memory/{remember,
events, annotate, forget, supersede}`). A write from an MCP client with no
`scope.agent` is stamped `agent:mcp:<clientInfo.name>`. `revert_run` is
never an MCP tool.

## The envelope `polis.bundle/2` (export; verify-only import)

```
header line (canonical JSON): { schema, chainId, principal{device, human}, orgId,
                                segment{fromSeq, toSeq, prevHashAtFrom, headHash},
                                policy{roles, bodies, tree, browse}, payloadSha256, exportedAt }
signature: Ed25519 over the header line, by the human's key (hex)
payload:   events (verbatim rows), prompts (id, role, body_hash, redaction full|gist|stub, text),
           notes, principals, aliases, binds
```

`polis export [--from-seq N] [--bodies auto|full|gist|stub] [--roles user,agent]
[--out FILE] [--dry-run]`. The default policy: `roles = user`, bodies `full`
only for org-visible user prompts and `stub` otherwise, no tree, no browse
events. `--dry-run` prints the decision per seq.

`polis import --verify-only FILE` checks, in order, each with a named reason:
the schema; `sha256(pubkey) == human id` (`id_mismatch`); the device derives
from the key and its name and is the chain id (`device_mismatch`); the header
signature (`bad_signature`); the payload hash (`payload_hash`); a bind for the
chain whose signature verifies (`no_bind`, `bad_bind_signature`); every
event's hash and link and the segment's bounds (`event_hash`, `linkage`,
`segment`); a `principal_bind` event's payload against the carried bind
(`bind_hash_mismatch`); every `full` body against its `body_hash`
(`body_hash`); and, for this device's own chain, continuity with the local
rows (`continuity`: "forked" when a seq differs). It stores nothing — the
`foreign_*` tables, subscriptions and transports are Session E3.
