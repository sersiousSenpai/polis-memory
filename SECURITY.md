# Security

## Reporting a vulnerability

Email the maintainer (the address on the commits) with "polis-memory
security" in the subject, or open a private security advisory on GitHub
(Security → Advisories → Report a vulnerability). Please do not open a public
issue for anything exploitable. You will get an acknowledgement within a few
days and a fix or a plan before any public disclosure.

## The posture, in one paragraph

Polis Memory is local-first. The daemon (`polis serve`) binds loopback by
default and carries a persistent, file-mode 0600 bearer token; a non-loopback
`--listen` refuses to start without `--token-file`. Reads over MCP are open on
loopback; writes require the token. Nothing leaves the machine unless you
configure it: no update check, no analytics, no telemetry. The things that
can leave are each opt-in and named — an embedding model download (pinned
sha256), a model call for `ask`/`organize` when a key or a model CLI is
configured, a sync folder or git remote or org node you point `polis sync`
at. `polis doctor` reports what is configured and warns when full-disk
encryption is off, a peer chain has forked, or a redaction has not been
acknowledged. The full egress table lives in `docs/security.md` once
Session E4 lands it; until then this file is the statement of record.

## What is not covered

Sharing is cooperative, not enforcement: a peer running patched software can
retain anything it was ever sent. That is why the default export policy ships
bodies only for `role = "user"`, org-visible rows, and why redactions are a
request peers honour rather than a guarantee (`docs/sharing.md`).
