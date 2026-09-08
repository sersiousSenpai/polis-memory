# polis-server

Polis Memory HTTP surface: an axum router over `dyn MemoryApi`, the ROUTES table it is generated from (the source of api-v1.md and the typed clients), the capture-hook ingest route with its host observer, the capture-hook installer, and — behind `standalone` — a bound, token-guarded server for the `polis` daemon.

Part of [Polis Memory](https://github.com/sersiousSenpai/polis-memory) — a local-first memory for coding agents. The README, the docs and the plan live in the repository; this crate is one layer of it.

License: Apache-2.0.
