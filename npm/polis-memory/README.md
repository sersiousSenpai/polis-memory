# polis-memory

Local memory for coding agents. Polis records what you prompt and what your
agents reply, verbatim, in a hash-chained ledger on your machine, and lets
Claude Code, Codex, Cursor, Windsurf and Claude Desktop search it over MCP,
with citations.

```sh
npx polis-memory setup
```

`setup` shows its plan and asks before changing anything. It:

- creates your record in `~/.polis`
- connects each agent it finds
- installs capture hooks for Claude Code, Codex and Cursor
- keeps a small daemon running for search indexing, organizing and backups

It also copies the `polis` binary to `~/.local/bin`, so clearing the npm
cache never breaks the hooks. Re-run it with `npx polis-memory@latest setup`
to upgrade.

```sh
npx polis-memory doctor      # check the install
npx polis-memory uninstall   # remove hooks, configs and the service; keeps your record
```

Capture makes no model calls, and nothing leaves your machine.
More: [redline.dev/polis](https://redline.dev/polis) ·
[source](https://github.com/sersiousSenpai/polis-memory) (Apache-2.0).

This package is a launcher. npm installs the prebuilt binary for your
platform as an optional dependency (`@polis-memory/darwin-arm64`,
`darwin-x64`, `linux-x64`, `linux-arm64` or `win32-x64`). Linux builds
target glibc.
