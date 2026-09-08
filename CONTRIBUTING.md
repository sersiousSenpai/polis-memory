# Contributing to Polis Memory

Thanks for your interest in improving Polis Memory. This guide covers how to
build, test, and submit changes.

## Contributor License Agreement (required)

**Every pull request is gated by a CLA check.** Before your first contribution
can be merged, you must agree to the [Polis Memory Contributor License
Agreement](CLA.md), a copyright-assignment CLA that keeps the Project's
copyright unified in a single owner of record.

You don't sign anything manually. When you open a pull request, the CLA
Assistant bot checks whether you've already agreed. If not, it comments with
instructions. To agree, post a pull-request comment containing exactly:

> I have read the CLA Document and I hereby sign the CLA

One signature covers all of your present and future contributions. If you
contribute as part of your employment, see the "Corporate CLA" section of
[CLA.md](CLA.md) first.

## Development setup

Prerequisites: the **Rust** toolchain (`rustup`, stable; the workspace's
MSRV is in `Cargo.toml`), and for the org-node image, Docker. No Node, no
Python — the benchmark runners under `bench/` are the one place Python
appears, and they are optional.

```sh
cargo build --workspace                          # every crate
cargo build --features cli -p polis-memory       # the `polis` binary → target/debug/polis
cargo test --workspace                           # the suite, default features
cargo test --workspace --all-features            # + the HTTP backends, the standalone daemon, the Apple embedder (macOS)
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo deny check licenses                        # permissive-only
```

CI (`.github/workflows/ci.yml`) runs the suite on macOS, Ubuntu and Windows
with `RUSTFLAGS=-D warnings`, a keyless job (no API key, no model CLI on
PATH), the license gate, an MSRV check and the lean-core check. Its stable
toolchain is newer than what many machines run; `cargo +stable clippy` on the
current stable before pushing saves a red run.

## Submitting changes

1. Fork the repository and create a topic branch.
2. Make your change. Keep commits focused and write a clear commit message.
3. Add or update tests; keep every gate above green.
4. New first-party source files carry the SPDX header:
   ```
   // SPDX-License-Identifier: Apache-2.0
   // Copyright 2026 Yusuf Al-Bazian
   ```
5. A change to the store schema is a migration: it goes in
   `Migration::additive` (idempotent), bumps `STORE_SCHEMA_VERSION`, and says
   why in the commit. Never touch `PRAGMA user_version` — a host may own it.
6. Open a pull request describing the change and the motivation.
7. Agree to the CLA via the bot comment if you haven't already.

## License

By contributing, you agree that your contributions are assigned and licensed
under the terms of [CLA.md](CLA.md), and that the Project is distributed
under the [Apache License 2.0](LICENSE). The code is Apache-2.0; the Polis
Memory name is not (see [NOTICE](NOTICE)).
