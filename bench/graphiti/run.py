#!/usr/bin/env python3
"""LongMemEval-S over Graphiti (`pip install graphiti-core[falkordb]`, FalkorDB
from bench/graphiti/docker-compose.yml), under the same conditions as the
Polis runner: one `group_id = question_id` per question, every haystack turn
added as an episode (Graphiti extracts entities and edges with an LLM on
write — its write-path cost is part of the table), retrieval by
`graphiti.search(question, group_ids=[...])`, the same answer model and judge.

  python3 bench/graphiti/run.py --stub                                        # pipeline check, no graphiti needed
  docker compose -f bench/graphiti/docker-compose.yml up -d
  python3 bench/graphiti/run.py --data longmemeval_s.json --subset 100 --provider openai \
      --answer-model gpt-5.6-sol --judge-model gpt-5.6-sol
"""
from __future__ import annotations

import argparse
import asyncio
import os
import sys
import time
from datetime import datetime, timezone

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..")))

from bench.common.runner import IngestStats, common_args, run  # noqa: E402
from bench.common.llm import approx_tokens  # noqa: E402
from bench.mem0.run import StubMemory  # noqa: E402


class GraphitiBackend:
    def __init__(self, args):
        if args.processing_profile != "immediate":
            raise ValueError("this backend supports the immediate processing profile only")
        self.stub = args.stub or args.provider == "stub"
        self.max_tokens = args.max_context_tokens
        if not self.stub:
            raise RuntimeError("Graphiti scored adapter unavailable: extraction/embedder usage and empty-group verification are not yet instrumented")
        self.calls = 0
        self.tokens = 0
        if self.stub:
            self.mem = StubMemory()
            self.g = None
        else:
            try:
                from graphiti_core import Graphiti  # type: ignore
                from graphiti_core.driver.falkordb_driver import FalkorDriver  # type: ignore
            except ImportError as e:
                raise SystemExit("pip install 'graphiti-core[falkordb]' (see bench/requirements.txt)") from e
            driver = FalkorDriver(host=os.environ.get("FALKORDB_HOST", "127.0.0.1"), port=int(os.environ.get("FALKORDB_PORT", "6379")))
            self.g = Graphiti(graph_driver=driver)
            asyncio.run(self.g.build_indices_and_constraints())

    def ingest(self, question, items) -> IngestStats:
        t0 = time.perf_counter()
        if self.stub:
            self.mem.add([{"role": it["role"], "content": f"[session={it.get('session')} date={it.get('sourceDate')} role={it['role']}] {it['body']}"} for it in items], user_id=question["namespace"])
        else:
            from graphiti_core.nodes import EpisodeType  # type: ignore

            async def go():
                for i, it in enumerate(items):
                    ts = datetime.fromtimestamp((it.get("ts") or 0) / 1000.0, tz=timezone.utc)
                    await self.g.add_episode(name=f"{it.get('session')}-{i}", episode_body=it["body"], source=EpisodeType.message,
                                             source_description=it["role"], reference_time=ts, group_id=question["namespace"])
            asyncio.run(go())
        return IngestStats(wall_ms=(time.perf_counter() - t0) * 1000.0, llm_calls=self.calls, llm_tokens=self.tokens, readiness={"verified": True, "emptyStart": True, "backend": "lexical stub; not Graphiti"})

    def context(self, question):
        t0 = time.perf_counter()
        if self.stub:
            res = self.mem.search(question["question"], user_id=question["namespace"], limit=20)
            rows = [r["memory"] for r in res["results"]]
        else:
            edges = asyncio.run(self.g.search(question["question"], group_ids=[question["namespace"]]))
            rows = [getattr(e, "fact", str(e)) for e in edges]
        text, used = [], 0
        for r in rows:
            t = approx_tokens(r)
            if used + t > self.max_tokens:
                break
            text.append(f"- {r}")
            used += t
        return ("\n".join(text) or None), (time.perf_counter() - t0) * 1000.0

    def close(self) -> None:
        if self.g is not None:
            try:
                asyncio.run(self.g.close())
            except Exception:
                pass


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    common_args(p, "graphiti-falkordb")
    args = p.parse_args()
    run(args, GraphitiBackend, {"user", "assistant"})


if __name__ == "__main__":
    main()
