#!/usr/bin/env python3
"""LongMemEval-S over Mem0 OSS (v3, `pip install mem0ai`), under the same
conditions as the Polis runner: same questions, same answer model, same judge,
same hardware, one memory namespace (`user_id = question_id`) per question.

Mem0 extracts memories with an LLM on `add`, so its write-path cost (calls,
tokens) is part of the table — the Polis row's write cost is 0 by design.

  python3 bench/mem0/run.py --stub                                            # pipeline check, no mem0 needed
  python3 bench/mem0/run.py --data longmemeval_s.json --subset 100 --provider openai \
      --answer-model gpt-5.6-sol --judge-model gpt-5.6-sol --mem0-config bench/mem0/config.example.json

Mem0's own LLM + embedder come from `--mem0-config` (a JSON `Memory.from_config`
document); keys from the environment.
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time

sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..")))

from bench.common.runner import IngestStats, common_args, run  # noqa: E402
from bench.common.llm import approx_tokens  # noqa: E402


class StubMemory:
    """Keyword recall over the ingested turns — the stub that keeps the
    pipeline runnable with no mem0 and no key. Not a measurement of Mem0."""

    def __init__(self):
        self.rows: list[str] = []
        self.namespace = None

    def add(self, messages, user_id):
        if self.namespace not in (None, user_id):
            raise RuntimeError("namespace collision")
        self.namespace = user_id
        for m in messages:
            self.rows.append(m["content"])
        return {"results": []}

    def search(self, query, user_id, limit=10):
        if self.namespace not in (None, user_id):
            raise RuntimeError("namespace mismatch")
        words = {w for w in query.lower().split() if len(w) > 3}
        scored = sorted(self.rows, key=lambda r: -len(words & set(r.lower().split())))
        return {"results": [{"memory": r} for r in scored[:limit]]}


class Mem0Backend:
    def __init__(self, args):
        if args.processing_profile != "immediate":
            raise ValueError("this backend supports the immediate processing profile only")
        self.stub = args.stub or args.provider == "stub"
        self.max_tokens = args.max_context_tokens
        if not self.stub:
            raise RuntimeError("Mem0 scored adapter unavailable: extraction/embedder usage and empty-store verification are not yet instrumented")
        self.calls = 0
        self.tokens = 0
        if self.stub:
            self.mem = StubMemory()
        else:
            try:
                from mem0 import Memory  # type: ignore
            except ImportError as e:
                raise SystemExit("pip install mem0ai (see bench/requirements.txt)") from e
            cfg = json.load(open(args.mem0_config)) if args.mem0_config else None
            self.mem = Memory.from_config(cfg) if cfg else Memory()

    def ingest(self, question, items) -> IngestStats:
        t0 = time.perf_counter()
        by_session: dict[str, list[dict]] = {}
        for it in items:
            by_session.setdefault(it.get("session", ""), []).append({"role": "user" if it["role"] == "user" else "assistant", "content": f"[session={it.get('session')} date={it.get('sourceDate')} role={it['role']}] {it['body']}"})
        for sid, msgs in by_session.items():
            self.mem.add(msgs, user_id=question["namespace"])
        return IngestStats(wall_ms=(time.perf_counter() - t0) * 1000.0, llm_calls=self.calls, llm_tokens=self.tokens, readiness={"verified": True, "emptyStart": True, "backend": "lexical stub; not Mem0"})

    def context(self, question):
        t0 = time.perf_counter()
        res = self.mem.search(question["question"], user_id=question["namespace"], limit=20)
        rows = [r.get("memory", "") for r in (res.get("results") if isinstance(res, dict) else res)]
        text, used = [], 0
        for r in rows:
            t = approx_tokens(r)
            if used + t > self.max_tokens:
                break
            text.append(f"- {r}")
            used += t
        return ("\n".join(text) or None), (time.perf_counter() - t0) * 1000.0

    def close(self) -> None:
        pass


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--mem0-config", help="JSON for Memory.from_config (llm, embedder, vector store)")
    common_args(p, "mem0-oss")
    args = p.parse_args()
    run(args, Mem0Backend, {"user", "assistant"})


if __name__ == "__main__":
    main()
