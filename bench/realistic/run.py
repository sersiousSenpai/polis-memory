#!/usr/bin/env python3
"""Keyless expected-evidence and citation resolution over actual MCP."""
import argparse
import hashlib
import json
from pathlib import Path
import sys
import time
import uuid
sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from bench.common.polis_mcp import PolisMcp, find_polis
from bench.common.schema import stamp


def run(args):
    path = Path(__file__).with_name("tasks.json")
    fixtures = json.loads(path.read_text())
    commit, date, hardware = stamp()
    report = {"schema": "polis.task-results/1", "commit": commit, "date": date, "hardware": hardware,
              "datasetSha256": hashlib.sha256(path.read_bytes()).hexdigest(), "split": args.split,
              "meaning": "synthetic evidence reachability; not answer-quality scoring", "llmCalls": 0, "tasks": []}
    for task in fixtures["tasks"]:
        if args.split != "all" and args.split != task["split"]:
            continue
        client = PolisMcp(find_polis(args.polis), timeout=args.timeout)
        try:
            namespace = uuid.uuid4().hex
            client.ingest([dict(it, run=namespace) for it in task["items"]])
            start = time.perf_counter()
            pack = client.tool("memory_search", {"q": task["question"], "filter": task.get("filter", {}), "max_tokens": 6000})
            elapsed = (time.perf_counter() - start) * 1000
            rows = pack.get("promptHits", []) + [dict(hit, body=hit.get("excerpt", "")) for hit in pack.get("grepHits", [])]
            body = "\n".join(row.get("body") or "" for row in rows)
            missing = [text for text in task["expected"] if text not in body]
            forbidden = [text for text in task.get("forbidden", []) if text in body]
            citations = []
            for row in rows:
                if row.get("seq") is None:
                    continue
                evidence = client.tool("memory_evidence", {"seq": row["seq"]})
                citations.append({"seq": row["seq"], "status": evidence["status"]})
            unresolved = [c for c in citations if c["status"] != "available"]
            report["tasks"].append({"id": task["id"], "category": task["category"], "split": task["split"],
                "passed": not missing and not forbidden and not unresolved, "missing": missing, "forbidden": forbidden,
                "citations": citations, "queryMs": round(elapsed, 3), "binarySha256": client.binary_hash,
                "retrieval": pack.get("retrieval"), "truncated": pack.get("truncated")})
        finally:
            client.close()
    report["categoryN"] = {category: sum(t["category"] == category for t in report["tasks"]) for category in sorted({t["category"] for t in report["tasks"]})}
    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    Path(args.out).write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"tasks": len(report["tasks"]), "passed": sum(t["passed"] for t in report["tasks"]), "out": args.out}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--polis")
    parser.add_argument("--split", choices=["development", "heldout", "all"], default="development")
    parser.add_argument("--timeout", type=float, default=30)
    parser.add_argument("--out", default="bench/results/realistic.json")
    run(parser.parse_args())
