# Security

What leaves the machine, when, and under whose control. Polis Memory is
local-first: the default install talks to nothing but itself.

## Egress, the whole table

| What | When | Where | Opt-in? |
|---|---|---|---|
| Embedding model download | first use of a downloadable provider (C2) | the provider's release URL, pinned by sha256 in code and docs | yes — off by default; `--features bundled-model` for air-gapped installs |
| `organize` / `ask` model calls | a gardener pass or a question, when a model is configured | the configured backend: the `claude` / `codex` CLI on this machine, or an HTTP API if a key is set | yes — no key and no CLI means no call; `POLIS_NO_NETWORK=1` forbids the HTTP backends outright |
| Sync through a folder | `polis sync --folder DIR` | the local filesystem (a synced drive is the user's choice) | yes |
| Sync through git | `polis sync --git URL` | that remote, with the user's own git credentials | yes |
| Sync through an org node | `polis sync --org URL` | that URL, bearer token from `--token-file` / `POLIS_ORG_TOKEN`; segments carry only what the export policy allows (bodies for `role = "user"`, org-visible rows; `--dry-run` prints the decision per seq) | yes |
| Update check | never | — | — |
| Analytics, telemetry, crash reports | never | — | — |

Nothing on the read path reaches the network: search, context, grep, the
tree, `verify` and `doctor` are local reads. Capture (`polis capture`)
posts to the local daemon when one is running and writes locally otherwise.

## The daemon and its token

- `polis serve` binds `127.0.0.1:7677` by default. Reads are open on
  loopback; writes need `Authorization: Bearer <token>`. The token lives at
  `$POLIS_HOME/token`, persistent, written 0600 before its bytes land
  (Windows: the directory's owner-only ACL until E2's keyring work).
- A non-loopback `--listen` refuses to start without a token — a network
  bind without a credential is a mistake, not a mode.
- An **org node** (`polis serve --org`) holds other people's segments, so
  its whole `/v1/sync/*` surface is token-gated in both directions, and a
  non-loopback bind additionally requires an operator's `--token-file`
  (the home's own token is for loopback callers). Per-peer credentials
  (OAuth) are a later step; today one token is the firm's, distributed by
  its operator.
- The route table (`polis_server::ROUTES`) is the auth contract: a route
  the router serves that is not in the table fails closed (401), never
  silently open.

## Keys and identity

- One Ed25519 key per human (`identity.key`, 0600); its hash is the
  principal id. Devices and agents are derived ids; a chain has exactly one
  writer. Copying a key to a second machine yields a second device chain,
  never a second head on the first.
- Trust is explicit: a peer's key is admitted by `polis trust add` (an
  admin-distributed key) or on first use with `--tofu`, and its fingerprint
  is printed. A key change without a bind event is refused.
- Everything that crosses the wire is signed: segments (the header line),
  ack reports, the org node's own catalog. Every peer re-verifies every
  segment it fetches; the node is never a trust root.

## Data at rest

- The store is a plain SQLite file under `$POLIS_HOME`; there is no
  application-level encryption in v1 — the operating system's full-disk
  encryption is the protection, and `polis doctor` warns when it can tell
  it is off (macOS via `fdesetup`; other platforms report unknown).
- Rotating `VACUUM INTO` snapshots under `$POLIS_HOME/backups/` (startup,
  every 6 h, shutdown; keep 7; chain verified after each) are the recovery
  path; `polis restore` swaps the newest verifying one in and keeps the
  live file as `polis.db.bad`.

## What `doctor` warns about

- Full-disk encryption off (or unknown).
- A red chain or a failed `PRAGMA quick_check` — with `polis restore`
  offered.
- A peer chain marked **forked** (a rewritten segment was refused).
- Redactions not yet acknowledged by a peer (on a peer: per peer it holds;
  on an org node: per subscriber).
- No model configured — reported as a fact (`no_model`), never as a fault:
  capture, retrieval and filing work without one.

## The cooperative limit

A redaction is honoured by every honest peer on import. It is cooperative,
not enforcement: a peer running patched software can retain anything it
was ever sent, and neither you nor the node can know. The defaults follow
from that — bodies leave the machine only for `role = "user"`, org-visible
rows, agent and system text never does, and acknowledgement is shown per
peer rather than as a global "forgotten".
