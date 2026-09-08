# Sharing

Polis shares memory the way it stores it: as **signed segments of
per-device hash chains**. There is one substrate and three transports. A
peer's segment is verified on arrival and lands as *foreign* rows that are
never re-chained; every question is still answered from local rows. This is
Session E3 of the plan (§4.6); the org node transport is E4.

## The envelope

`polis.bundle/2` — one contiguous run of one device chain:

| Field | What it is |
|---|---|
| `header.chainId` | the device id (one chain has one writer: a device) |
| `header.principal` | the device card and its parent human's card, including the human's public key |
| `header.segment` | `fromSeq`, `toSeq`, `prevHashAtFrom` (what it links onto), `headHash` |
| `header.policy` | what the exporter allowed: roles, bodies (`auto` / `full` / `gist` / `stub`), tree, browse |
| `header.payloadSha256` | the payload's hash |
| `signature` | Ed25519 over the canonical header line, by the human's key |
| `payload.events` | the ledger rows, verbatim |
| `payload.prompts` | each prompt at its redaction (`full` / `gist` / `stub`), its `bodyHash`, its `project` |
| `payload.notes`, `principals`, `aliases`, `binds` | the user's notes on those seqs, the principal cards, the bind events' payloads |
| `payload.vectors` | optional: the exporter's vectors beside full bodies |
| `payload.redactions` | the payloads of the `redaction` events in the segment |
| `payload.acks` | `(chain, headSeq)` — the newest seq of each peer chain the exporter holds |

Stubs keep linkage verifiable across redactions: a stubbed prompt still
carries its `bodyHash`, which is what its `prompt` event committed to.

## Import — the checks, in order

1. **The key is the identity.** `sha256(pubkey) == human.principalId`,
   the device id derives from the key and its name and equals the chain id.
2. **Trust.** The human's key is in the trust store — added by an admin
   (`polis trust add`) or trusted on first use (`--tofu`, which prints the
   fingerprint to verify out of band). A trust row that names a different
   key for the same id is refused (`key_changed`); by construction a real
   key change is a new principal, never a silent update.
3. **Signature** over the header line; **payload hash**; a **bind** for the
   chain in the payload, signed by the same key; a bind *event* in the
   segment commits to a bind the envelope carries.
4. **Every event** recomputes its `entryHash` and links to its predecessor;
   the segment spans what its header says; a `redaction` event commits to
   a redaction payload the envelope carries.
5. **Continuity** against the copy of that chain we already hold:

   | We hold | Segment | Result |
   |---|---|---|
   | nothing | starts at 1 | append |
   | nothing | starts later | `gap` — fetch the earlier segments first |
   | head *H* | starts at *H*+1 and links onto our head hash | append |
   | head *H* | overlaps, every overlapping hash equal | no-op / append the tail |
   | head *H* | overlaps, a hash differs | **forked**: the chain is marked, nothing newer from it lands until an operator resets it (`polis subscribe rm --purge`) |
   | head *H* | starts past *H*+1 | `gap` |

6. **Subscriptions.** With no subscriptions every chain a transport offers
   is imported. With subscriptions, a chain lands only when a row matches
   its human (id, fingerprint prefix or display name); a row's `--project`
   keeps bodies for that project root and stubs the rest. `--force`
   imports regardless.

Then the rows are written in one transaction: `foreign_events`,
`foreign_prompts` (their own FTS index, the lake's tokenizer),
`foreign_notes`, `foreign_principals`, the redactions, and the chain's new
head. Our own chain, arriving through a transport, is verified for
continuity and stored nowhere (a chain has one writer).

## Redactions

`forget` compacts the body locally *and* appends a `redaction` event
naming the forgotten `(chain, seq)`. A peer honours it on import: the
foreign body becomes the tombstone, its vectors are dropped. An
acknowledgement is the peer's **next segment head past the redaction** —
its later segments carry `acks`, the newest seq of our chain it holds.
`polis doctor` lists redactions no peer has acknowledged yet.

Stated honestly: this is **cooperative, not enforcement**. A peer running
patched software can retain anything it was ever sent. That is why the
default policy ships bodies only for `role = "user"` rows the exporter
marked org-visible, and stubs everything else.

## Transports

One trait, four calls — `publish`, `list(chain, after)`, `fetch`,
`chains(filter)` — and the same append-only layout everywhere:
`<root>/<chain_id>/<from>-<to>.polis.json`. A segment file is never
rewritten; a changed one would read as a fork, which is the point.

- **Folder** (`polis sync --folder DIR`): a directory — a synced drive for a
  person's two machines, an NFS share for a room.
- **Git** (`polis sync --git URL`): a remote as the folder. The clone lives
  under `$POLIS_HOME/sync/git/<hash>`; every read pulls, every publish
  commits and pushes. It shells out to `git` — no crate, every dev machine
  has git, and the layout is plain files.
- **Org node** (E4): the same four calls over HTTPS, relaying envelopes it
  verified on receipt; never a trust root.

`polis sync` publishes this device's new segments (from the last published
seq), then fetches and imports every subscribed peer's. High-water marks
per transport live in `polis_meta`.

## Retrieval over the union

Foreign hits are returned **only** when a read asks for
`include_shared` (`polis search --shared`, the MCP `scope.includeShared`),
and always in their own list, `sharedHits`, labelled by source
(`shared:<display name>` or `shared:<fingerprint>`) and cited as
`chain:seq`, never `#seq`. They never sit in `promptHits` beside the
user's own words; the context block renders them last under a
"SHARED (third-party)" heading, and the MCP server's `instructions` say the
same. The union arm is BM25 over the foreign index fused with cosine over
foreign vectors; a peer's vector is reused only when its `model` id is the
local embedder's — otherwise it is discarded and the text is re-embedded
by the index tick, after the lake's own rows.

## Privacy defaults

`visibility = private`; `roles = ["user"]` (agent and system prompts never
leave); bodies `full` for org-visible user prompts, `stub` otherwise; the
catalog is not exported (the org node publishes its own); browse events
are not shared. `polis export --dry-run` prints the full / gist / stub
decision per seq before anything is written.

## The org node (E4)

A firm's node is **just another principal**: `polis init --org acme` gives it
its own Ed25519 key, a device chain named for the org, and a card that
carries the org's name. `polis serve --org` then does three things beside
the ordinary daemon:

- **Relay.** `/v1/sync/*` — a peer publishes a signed segment
  (`POST /v1/sync/segments`), the node verifies it on receipt exactly as an
  import (id, signature, bind, per-event hash and linkage, continuity
  against the head it holds), stores the bytes verbatim under
  `$POLIS_HOME/sync/org/<chain>/<from>-<to>.polis.json` (append-only, a
  segment is never rewritten) and imports it into its own store. A
  rewritten segment is refused as **forked** (409), the chain is marked, and
  nothing newer from it lands until an operator resets the mark. Every row
  of the surface is token-gated in both directions: a segment carries
  bodies, and the node listens on a network. A non-loopback bind refuses to
  start without an operator's `--token-file`; the home's own token is for
  loopback callers.
- **Never a trust root.** The node stores and relays; every peer
  re-verifies every segment it fetches. `--tofu` lets the node trust a
  peer's key on first use (the fingerprint is logged); without it an
  unknown key waits for `polis trust add`.
- **The firm's view, signed.** The node's own chain is published into the
  same folder whenever its head moves, with `policy.tree` set: its catalog
  (class nodes and links) rides the segment as `tree` / `treeLinks`, so a
  peer imports "the firm's classes" as `foreign_class_*` rows beside the
  node's chain — auditable and attributable, never merged into the peer's
  own tree. Its gardener runs over what it holds (its own lake plus the
  union of what it relays), and every organize it makes is an event on its
  chain like anyone else's.

**Acks through the node.** Acks still ride a peer's segments (a peer's
next head past a redaction), but a peer syncing through an org node also
posts a **signed ack report** after every fetch (`POST /v1/sync/acks`: the
chains and seqs it now holds, signed by its human key; the node verifies
against the key it trusted when it accepted the chain). The node serves
`GET /v1/sync/acks?chain=…` back to the emitter on its next sync, so a
redaction is acknowledged as soon as the subscriber has imported it — not
when it next has something to say — and an emitter learns of acks from
subscribers whose chains it does not hold. `GET /v1/sync/redactions` lists
every relayed redaction with the subscribers that have and have not moved
past it; `polis doctor` on the node prints the same per subscriber.

**The cooperative limit, stated plainly.** A redaction is a request every
honest peer honours on import (the body becomes a tombstone, its vectors
and claims go); it is not enforcement. A peer running patched software can
retain anything it was ever sent, and the node cannot know. That is why the
default policy ships bodies only for `role = "user"`, org-visible rows, and
why `polis doctor` shows acknowledgement per peer rather than pretending a
`forget` is global.

## Commands

```
polis trust fingerprint            # your human id + public key, for a peer to add
polis trust add <pubkey|@file> [--name N]
polis trust list | rm <id>
polis subscribe add --principal ID|FINGERPRINT|NAME [--project ROOT] [--class C]
polis subscribe list | rm <id> [--purge]
polis sync --folder DIR | --git URL [--tofu] [--force] [--publish-only|--fetch-only] [--bodies auto|full|gist|stub] [--include-vectors]
polis import FILE [--verify-only] [--tofu] [--force]
polis export [--from-seq N] [--bodies …] [--roles …] [--include-vectors] [--dry-run]
polis peers
polis search --shared … · polis context --shared …

polis init --org NAME                                        # the org node's home: its own key and chain
polis serve --org [--tofu] [--listen HOST:PORT --token-file FILE]   # relay at /v1/sync/* (token required)
polis sync --org URL --token-file FILE [--tofu] …            # a peer through the node (or POLIS_ORG_TOKEN)
polis doctor                                                 # on the node: pending redactions per subscriber
```
