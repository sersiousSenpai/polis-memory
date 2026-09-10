#!/usr/bin/env python3
"""Measure 400-item, no-model inbox filing in fresh on-disk homes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from bench.common.polis_mcp import PolisMcp
from bench.baselines.local_model_readiness import summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--polis", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=10)
    parser.add_argument("--binary-profile", default="unspecified")
    args = parser.parse_args()
    if args.samples < 1:
        parser.error("samples must be positive")
    binary = args.polis.resolve()
    measurements = []
    for sample in range(args.samples):
        # No inherited credentials or executable model backends.
        original = os.environ.copy()
        clean = {key: value for key, value in original.items() if key in {"HOME", "USER", "TMPDIR", "LANG"}}
        os.environ.clear()
        os.environ.update(clean)
        try:
            client = PolisMcp(str(binary))
        finally:
            os.environ.clear()
            os.environ.update(original)
        try:
            # Advance the initialization-only principal_bind event separately,
            # so the measured 400-event window contains 400 source items.
            subprocess.run([str(binary), "--json", "organize"], env=client.env,
                           text=True, capture_output=True, timeout=30, check=True)
            items = [{"body": f"Preserve database evidence item {i} for the coding project.",
                      "role": "user" if i % 2 == 0 else "assistant", "run": f"filing-{sample}",
                      "session": f"session-{i // 20}", "project": "/bench/filing"} for i in range(400)]
            assert client.ingest(items) == 400
            started = time.perf_counter()
            result = subprocess.run([str(binary), "--json", "organize"], env=client.env,
                                    text=True, capture_output=True, timeout=30, check=True)
            elapsed = (time.perf_counter() - started) * 1000
            outcome = json.loads(result.stdout)
            with client._db() as db:
                linked = db.execute("SELECT COUNT(DISTINCT p.id) FROM prompts p JOIN ledger_events le ON le.prompt_id=p.id AND le.kind='prompt' JOIN class_links cl ON cl.target_id=CAST(le.seq AS TEXT) AND cl.target_kind='prompt'").fetchone()[0]
                usage = db.execute("SELECT COUNT(*) FROM model_usage").fetchone()[0]
                calls = db.execute("SELECT COALESCE(SUM(llm_calls),0) FROM class_runs").fetchone()[0]
                vectors = db.execute("SELECT COUNT(*) FROM embeddings").fetchone()[0]
                schema = db.execute("SELECT value FROM polis_meta WHERE key='schema_version'").fetchone()
            assert linked == 400, (linked, outcome)
            assert usage == calls == vectors == 0
            measurements.append({"wallMs": elapsed, "linkedSources": linked, "outcome": outcome,
                                 "llmUsageRows": usage, "llmCalls": calls, "embeddingRows": vectors,
                                 "schemaVersion": schema[0] if schema else None})
        finally:
            client.close()
    times = summary([item["wallMs"] for item in measurements])
    report = {"schema": "polis.local-filing/1", "measuredAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
              "binarySha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "binaryProfile": args.binary_profile,
              "platform": platform.platform(), "sourcesPerSample": 400, "latency": times,
              "p50BelowOneSecond": times["p50Ms"] < 1000, "samples": measurements,
              "method": "Each sample initializes a fresh home and processes its identity-only bookkeeping event, captures 200 user and 200 assistant sources through MCP, then times one CLI organize call including process startup/store open and verifies all source links. Capture/setup and the identity-only pass are excluded.",
              "limits": "No model or embeddings; measures deterministic inbox filing, not semantic classification precision or model-backed filing. Single machine; filesystem cache is not evicted."}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"samples": len(measurements), "latency": times, "out": str(args.output)}))


if __name__ == "__main__":
    main()
