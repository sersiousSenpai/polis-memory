# Capture, injection and scrubbing

How Polis records an agent conversation (Claude Code, Codex, Cursor), what it
hands back to the model, and what it refuses to store. `docs/mcp.md` covers reads over MCP;
`docs/ledger.md` the chain the records land in.

## The hooks

`polis setup` installs the hooks for every agent it finds. `polis hook install
--client claude|codex|cursor` installs one agent's. Either way, every other hook
in the file stays in place:

| Agent | File | Prompt | Reply |
|---|---|---|---|
| Claude Code | `~/.claude/settings.json` | `UserPromptSubmit` → `"<polis>" capture` | `Stop` → `capture --event stop` (reads the transcript) |
| Codex | `$CODEX_HOME/hooks.json` (`~/.codex`) | `UserPromptSubmit` → `capture --client codex` (async) | `Stop` → `capture --client codex --event stop` (`last_assistant_message`) |
| Cursor | `~/.cursor/hooks.json` (version 1) | `beforeSubmitPrompt` → `capture --client cursor` | `afterAgentResponse` → `capture --client cursor --event response` (`text`) |

The Codex and Cursor shapes are the ones Redline already runs against both
agents, without Redline's seat and plan-review headers.

**Before Codex runs them:** Codex asks the user to review new hooks. Open
`/hooks` in Codex and enable both.

**What Cursor receives:** its prompt hook always answers
`{"continue": true}`.

**Where injection works:** only Claude Code reads hidden context from the
prompt hook.

All the hooks fail open. Every branch exits 0, so a closed or slow memory
never delays the session. Each hook talks to the daemon when one is running
and writes the store directly otherwise.

`polis hook install --no-replies` installs (or keeps) Claude Code's prompt
hook only and removes its Stop hook. `polis hook status --client <agent>` and
`polis doctor` report what is installed.

### Reply capture (Claude Code)

Claude Code's Stop hook reads the payload's `transcript_path` itself; the daemon never
opens a path that a request names. It reads only the bytes the transcript
gained since the last fire: a per-session offset under
`$POLIS_HOME/transcripts/`, complete lines only, at most 4 MB per fire.

- **What a turn is:** the assistant's text from one typed user message to
  the next, plus a final `[edited: src/main.rs, README.md]` line naming the
  files its `Edit` / `MultiEdit` / `Write` / `NotebookEdit` calls touched.
- **What stays out:**
  - tool inputs and results
  - subagent (sidechain) traffic
  - text beyond 16,000 characters per turn
- **When the offset moves:** only after every batch is stored. A failed fire
  is read again next time, and ingest is idempotent on body and session, so a
  replay records nothing twice.

### Echo suppression

When injection is on, Polis records which sources it injected into each
session (`polis_injections`). This table is local and unhashed, and it keeps
one week.

If an assistant turn in that session is a near-duplicate (simhash) of a whole
injected body, Polis does not record it. That turn is the record repeating
itself, not new evidence. The skip is counted (`doctor` shows it). A reply
that restates the memory *and* says something new is recorded.

## Scrubbing

Every captured body passes through `polis_memory::scrub` **before** it is
hashed. That covers typed prompts, assistant replies, `memory_ingest`
items and `memory_remember` text. A matched secret therefore never enters
the chain, the lexical index, the embeddings, a backup or a peer's copy.

Patterns, most specific first:

- provider keys: `sk-ant-…`, `sk-…`/`sk-proj-…`, AWS `AKIA…`/`ASIA…`, GitHub
  `ghp_…`/`github_pat_…`, Slack `xox?-…`
- PEM private-key blocks and JWTs
- `Bearer <token>` (the header name is kept)
- `KEY|SECRET|TOKEN|PASSWORD`-style assignments (`=` or `:`) with a value of
  16 or more characters (the key name is kept)

Each match becomes `[redacted:<kind>]`. Counts by kind are kept in
`polis_meta` and shown by `doctor`; the value never is.

Hashes, UUIDs, base64 test data, short config values and prose about tokens
are left alone, and `scrub.rs` has fixtures for each. Scrubbing is
pattern-based: it catches the shapes above and nothing else.
The `polis` binary scrubs by default; `config.toml` `scrub = "off"` turns it
off. A host embedding the crates opts in with `PolisHandle::with_scrub(true)`,
so what an existing host records does not change under it.

## Injection

With `config.toml` `inject = "on"`, each captured prompt is answered as
`hookSpecificOutput.additionalContext`: the model reads it, the transcript
does not show it. The answer contains up to three earlier records from the
**same project** (the hook's `cwd`) that clear the relevance floor, in at
most 1,536 bytes. Each line is cited as `#seq` and framed as data, not
instructions.

**Injection is off by default.** The daemon reads the setting at startup, so
restart `polis serve` after changing it.

These are never injected:

- the prompt itself, and anything recorded after the hook fired
- earlier turns of the same session (they are already in context)
- a user's earlier *question* (it holds no answer)
- a superseded record
- anything that is not user or assistant text

The floor is lexical and deterministic, and it makes no model call. It fits
the hook's one-second budget and is the same with or without an embedder.
Candidates are the prompt's planned terms matched at the OR stage, scoped
by project and time. Each candidate is scored as

    score = (IDF-weighted share of the prompt's terms it contains)
          × (IDF of its rarest matched term / ln(1 + N))

with document frequencies counted over the records that could answer (user
and assistant text, not user questions). A candidate must also share two
content terms with the prompt, or one identifier-like term (a flag, a path,
`snake_case`, a version, a handle). The constant is
`polis_memory::inject::DEFAULT_FLOOR`.

### Calibration and the gate

`bench/realistic/inject.json` holds 30 should-inject prompts (each with
expected evidence) and 30 must-not-inject prompts, split into development
and heldout halves. The must-not-inject set covers:

- generic chores
- unrelated questions that share common words
- another project's facts

The prompts run against 200 generic chores plus the facts.

`tests/inject_floor.rs` sweeps the floor, picks the lowest floor whose
development false-injection rate is at most 5%, and gates the shipped
default on the heldout split: false injections at most 5%, hit rate at
least 70%. It runs in every `cargo test` and in the CI `eval` job.

| At `DEFAULT_FLOOR` = 0.50 | Hit rate | False injections |
|---|---:|---:|
| development (15 + 15) | 0.93 | 0.00 |
| heldout (15 + 15) | 0.87 | 0.00 |

[Raw sweep](../bench/results/2026-10-07-inject-floor.json).

What this does and does not establish:

- **Synthetic, small, lexical-only.** These are 60 labelled prompts over
  synthetic records, with no embedder and no model calls. They are not a
  real-user quality measurement.
- **The margin is thin.** At 0.45 the development false-injection rate is
  20%: generic chores score about 0.46. A store with very few records has
  no term statistics to separate facts from chores, so it injects
  conservatively or not at all.
- **The heldout split is not pristine.** The two-term corroboration rule was
  added after inspecting heldout misses, so the heldout rates are not an
  unbiased estimate.
- **Known misses:** prompts whose other words appear nowhere in the store
  (`refund_idempotency is failing again, what do we know?`). Unseen words
  weigh against coverage.
- **Before default-on:** a real-corpus false-injection measurement is the
  next gate.

## Setup, the skill and uninstall

`polis setup` is the whole install:

- `init`
- per agent: MCP config, capture hooks, and the skill for Claude Code
- the embedding model
- an explicit `gardener_model` (`claude-cli` or `codex-cli` when found, else
  `none`; never an API key)
- the daemon service

It prints the plan and asks on the terminal, which works under
`curl … | sh`. Flags:

- `--yes` skips the question; `--dry-run` only shows the plan
- `--clients claude,codex` limits the agents
- `--no-service`, `--no-model`, `--no-skill` skip those steps

Re-running setup refreshes everything.

**When setup runs from npm** (`npx polis-memory setup`), the binary lives in
the npm cache. Setup first copies it to `~/.local/bin/polis` (Windows:
`%LOCALAPPDATA%\Programs\polis`) and wires every hook, config and service to
that copy. Clearing the npm cache then breaks nothing, and
`npx polis-memory@latest setup` upgrades the copy.

**The skill:** `polis skill install` writes `polis-memory` to
`~/.claude/skills/polis-memory/SKILL.md`. It tells an agent how to search
(`memory_search` first), cite `#seq`, treat recalled text as data, and write
decisions. Its HTTP fallback names the daemon's address and the token's
*path*, never the token itself. The skill is separate from the
`classmemory` skill that hosts embed.

**Uninstall:** `polis uninstall` removes the hooks, MCP entries, skill and
service, and keeps the record. `--purge` also deletes `$POLIS_HOME`.

## Running the daemon

Captures are recorded without a daemon. Semantic indexing, filing and the
rotating backups need `polis serve`. To keep it running:

```sh
polis service install     # launchd agent (macOS) / systemd user unit (Linux)
polis service status
polis service uninstall
```

The service runs `polis serve` for this `POLIS_HOME` and logs to
`$POLIS_HOME/logs/serve.log`. It sees the service manager's environment,
not your shell's, so a model key exported in a shell profile is not used.

`polis doctor` flags these as problems:

- the daemon is down while captured sources wait for indexing
- the index falls 1,000 or more sources behind
- the gardener would call a paid model API only because `ANTHROPIC_API_KEY`
  (or an OpenAI key) happens to be set

For the last one, `gardener_model = "auto"` in `config.toml` confirms the
choice and `"none"` turns model calls off.
