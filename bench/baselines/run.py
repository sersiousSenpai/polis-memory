#!/usr/bin/env python3
"""Raw history and simple lexical baselines with the common final token ceiling."""
import argparse
import os
import re
import sys
import time
sys.path.insert(0, os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..")))
from bench.common.runner import IngestStats, common_args, run


class Baseline:
    def __init__(self, args):
        if args.processing_profile != "immediate":
            raise ValueError("this backend supports the immediate processing profile only")
        self.mode = args.baseline
        self.rows = []

    def ingest(self, question, items):
        if self.rows:
            raise RuntimeError("nonempty baseline store")
        start = time.perf_counter()
        self.rows = [f"[session={it['session']} date={it.get('sourceDate')} turn={it['turnIndex']} role={it['role']}] {it['body']}" for it in items]
        return IngestStats((time.perf_counter() - start) * 1000, readiness={"verified": True, "emptyStart": True, "backend": self.mode})

    def context(self, question):
        start = time.perf_counter()
        rows = self.rows
        if self.mode == "lexical":
            terms = set(re.findall(r"\w+", question["question"].lower()))
            rows = sorted(rows, key=lambda row: -len(terms & set(re.findall(r"\w+", row.lower()))))
        return "\n".join(rows), (time.perf_counter() - start) * 1000

    def close(self):
        pass


if __name__ == "__main__":
    parser = common_args(argparse.ArgumentParser(description=__doc__), "baseline")
    parser.add_argument("--baseline", choices=["raw-history", "lexical"], default="lexical")
    args = parser.parse_args()
    args.system = args.baseline
    run(args, Baseline, {"user", "assistant"})
