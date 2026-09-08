#!/usr/bin/env python3
"""LongMemEval-S over Polis Memory (plan §6.2 item 2).

Two Polis configs: `polis-default` (user turns only — the capture hook's
world; assistant-answer categories will score near zero by construction) and
`polis-full` (every role). One fresh `POLIS_HOME` per question, driven over
MCP stdio (`polis mcp`), the haystack ingested with `ts` and `origin = import`,
the question answered by a fixed model from `memory_context(q, 4000)`, judged
by one fixed judge. Polis makes zero model calls on write.

  python3 bench/longmemeval/run.py --config polis-default --stub                 # keyless pipeline check
  python3 bench/longmemeval/run.py --config polis-full --data longmemeval_s.json --subset 100 \
      --provider anthropic --answer-model claude-sonnet-5 --judge-model claude-sonnet-5 --budget-tokens 6000000

Results: bench/results/<date>-<sha>-longmemeval-<config>.json; render with
`python3 -m bench.common.report`.
"""
from __future__ import annotations

import argparse
import os
import sys
import time

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..")))

from bench.common.polis_mcp import PolisMcp, find_polis  # noqa: E402
from bench.common.runner import IngestStats, common_args, run  # noqa: E402

ROLES = {"polis-default": {"user"}, "polis-full": {"user", "assistant"}}


class PolisBackend:
    def __init__(self, polis_bin: str, max_tokens: int):
        self.client = PolisMcp(polis_bin)
        self.max_tokens = max_tokens

    def ingest(self, question, items) -> IngestStats:
        t0 = time.perf_counter()
        self.client.ingest(items)
        return IngestStats(wall_ms=(time.perf_counter() - t0) * 1000.0, llm_calls=0, llm_tokens=0)

    def context(self, question):
        return self.client.context(question["question"], self.max_tokens)

    def close(self) -> None:
        self.client.close()


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--config", default="polis-default", choices=sorted(ROLES))
    p.add_argument("--polis", help="path to the polis binary (else $POLIS_BIN, PATH, target/)")
    common_args(p, "polis-default")
    args = p.parse_args()
    args.system = args.config
    polis_bin = find_polis(args.polis)
    roles = ROLES[args.config]
    run(args, lambda a: PolisBackend(polis_bin, a.max_context_tokens), roles)


if __name__ == "__main__":
    main()
