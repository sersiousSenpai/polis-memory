# polis-mcp

Polis Memory over the Model Context Protocol: the read tools (memory_search first), resources and the grounding prompt served over stdio by `polis mcp` and over streamable HTTP at `/mcp` by `polis serve` and by any host that nests the service; plus a remote MemoryApi client so one MCP process can front a running daemon.

Part of [Polis Memory](https://github.com/sersiousSenpai/polis-memory) — a local-first memory for coding agents. The README, the docs and the plan live in the repository; this crate is one layer of it.

License: Apache-2.0.
