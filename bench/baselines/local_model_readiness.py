#!/usr/bin/env python3
"""Pinned local Model2Vec readiness; no answer/judge/LLM backends or downloads here."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.parse
import urllib.request

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "bench"))
from common.assets import prestage
from common.polis_mcp import PolisMcp


def summary(values):
    ordered = sorted(values)
    return {"n": len(values), "p50Ms": ordered[math.ceil(len(values) * .5) - 1],
            "p95Ms": ordered[math.ceil(len(values) * .95) - 1], "maxMs": max(values), "samplesMs": values}


def clean_env(home):
    # An allowlist prevents inherited API credentials or model selectors from
    # enabling a paid backend; no executable search path contains a CLI model.
    env = {key: os.environ[key] for key in ("HOME", "USER", "TMPDIR", "LANG") if key in os.environ}
    env.update(PATH=str(home / "no-model-binaries"), POLIS_HOME=str(home),
               POLIS_NO_NETWORK="1", POLIS_EMBED="model2vec")
    return env


def mcp_readiness(args, root):
    original = os.environ.copy()
    os.environ.clear()
    os.environ.update(clean_env(root / "mcp"))
    started = time.perf_counter()
    try:
        client = PolisMcp(str(args.polis), home=str(root / "mcp"), assets=str(args.assets))
    finally:
        os.environ.clear()
        os.environ.update(original)
    setup_ms = (time.perf_counter() - started) * 1000
    fixtures = [
        ("Keep passwords inside an encrypted vault.", "private login credentials", "user"),
        ("Back up the database each evening before maintenance.", "nightly snapshots", "assistant"),
        ("Travel to the office by train every morning.", "railway commuting", "assistant"),
        ("PostgreSQL retains transactions across power failures using a write-ahead log.", "database durability", "user"),
        ("Send an immediate email when a payment fails.", "urgent customer notifications", "assistant"),
    ]
    distractors = ["Oranges ripen in warm climates.", "A violin has four strings.",
                   "Bread dough rises slowly.", "Mountains collect snow during winter.",
                   "A triangle has three sides.", "The garden contains purple flowers.",
                   "Saturn has spectacular rings.", "The painter selected blue pigments."]
    try:
        items = [{"body": body, "role": role, "run": f"fixture-{i}", "project": "/bench/readiness"}
                 for i, (body, _, role) in enumerate(fixtures)]
        items += [{"body": body, "role": "user", "run": f"noise-{i}", "project": "/bench/readiness"}
                  for i, body in enumerate(distractors)]
        start = time.perf_counter()
        receipt = client.tool("memory_ingest", {"items": items})
        capture_ms = (time.perf_counter() - start) * 1000
        seqs = receipt["recorded"]
        assert len(seqs) == len(items)
        immediate = []
        for i, (_, query, _) in enumerate(fixtures):
            pack = client.tool("memory_search", {"q": query, "limit": 10})
            immediate.append(any(hit["seq"] == seqs[i] for hit in pack["promptHits"]))
        readiness = client.process("indexed")
        queries = []
        for i, (body, query, role) in enumerate(fixtures):
            start = time.perf_counter()
            pack = client.tool("memory_search", {"q": query, "limit": 10})
            elapsed = (time.perf_counter() - start) * 1000
            hit = next((hit for hit in pack["promptHits"] if hit["seq"] == seqs[i]), None)
            arms = [] if hit is None else [arm["arm"] for arm in hit["arms"]]
            exact = client.tool("memory_evidence", {"seq": seqs[i]})
            queries.append({"source": body, "sourceRole": role, "query": query, "seq": seqs[i],
                            "availableBeforeIndex": immediate[i], "returnedAfterIndex": hit is not None,
                            "arms": arms, "semanticOnly": "semantic" in arms and "lexical" not in arms,
                            "rank": None if hit is None else next(n + 1 for n, h in enumerate(pack["promptHits"]) if h["seq"] == seqs[i]),
                            "exactEvidenceStatus": exact["status"], "elapsedMs": elapsed})
        with client._db() as db:
            usage = db.execute("SELECT COUNT(*) FROM model_usage").fetchone()[0]
            index_rows = db.execute("SELECT COUNT(*) FROM embeddings").fetchone()[0]
        assert usage == 0
        assert all(q["semanticOnly"] and q["returnedAfterIndex"]
                   and not q["availableBeforeIndex"] and q["exactEvidenceStatus"] == "available"
                   for q in queries), f"semantic-only evidence checks failed: {queries}"
        return {"status": "passed", "setupMsIncludingVerifiedPrestageInitAndMcp": setup_ms,
                "captureBatchMs": capture_ms, "capturedSources": len(items), "readiness": readiness,
                "embeddingRows": index_rows, "queries": queries, "llmUsageRows": usage,
                "semanticOnlyQueries": sum(q["semanticOnly"] for q in queries),
                "interpretation": "small deterministic readiness fixture, not an answer-quality benchmark"}
    finally:
        client.close()


def daemon_freshness(args, root):
    home = root / f"daemon-{time.time_ns()}"
    env = clean_env(home)
    setup_start = time.perf_counter()
    pins = prestage(args.assets, home / "models")
    subprocess.run([str(args.polis), "init", "--device", "freshness-bench"], env=env,
                   capture_output=True, text=True, timeout=30, check=True)
    log_path = home / "daemon.log"
    with log_path.open("w") as log:
        daemon = subprocess.Popen([str(args.polis), "serve", "--listen", "127.0.0.1:0"], env=env,
                                  stdout=log, stderr=log)
    try:
        deadline = time.monotonic() + 15
        while not (home / "serve.json").exists():
            if daemon.poll() is not None:
                raise RuntimeError(f"daemon exited {daemon.returncode}: {log_path.read_text()[-2000:]}")
            if time.monotonic() > deadline:
                raise TimeoutError("daemon startup exceeded 15 seconds")
            time.sleep(.05)
        info = json.loads((home / "serve.json").read_text())
        base = "http://" + info["addr"]
        token = (home / "token").read_text().strip()
        def request(path, body=None):
            data = None if body is None else json.dumps(body).encode()
            headers = {"Authorization": "Bearer " + token}
            if data is not None:
                headers["Content-Type"] = "application/json"
            with urllib.request.urlopen(urllib.request.Request(base + path, data=data, headers=headers), timeout=10) as response:
                return json.load(response)
        health = request("/v1/memory/health")
        setup_ms = (time.perf_counter() - setup_start) * 1000
        capture_count = 50
        cadence_seconds = .2
        started = time.perf_counter()
        captures, found, gaps = [], {}, []
        db = sqlite3.connect(f"file:{home / 'polis.db'}?mode=ro", uri=True)
        try:
            while len(found) < capture_count and time.perf_counter() - started < 35:
                now = time.perf_counter()
                if len(captures) < capture_count and now - started >= len(captures) * cadence_seconds:
                    index = len(captures)
                    ts = time.time_ns() // 1_000_000
                    body = {"scope": {"project": "/bench/freshness"}, "items": [{"body": f"Morning commutes reach the office on a train. Observation number {index}.", "ts": ts,
                            "run": f"freshness-{index}", "role": "assistant" if index % 2 else "user"}]}
                    sent = time.perf_counter()
                    receipt = request("/v1/memory/events", body)
                    ack = time.perf_counter()
                    if captures:
                        gaps.append((sent - captures[-1]["sent"]) * 1000)
                    captures.append({"seq": receipt["recorded"][0], "sent": sent, "ack": ack, "ts": ts,
                                     "captureMs": (ack - sent) * 1000})
                ready = {row[0] for row in db.execute("SELECT le.seq FROM embeddings e JOIN ledger_events le ON le.prompt_id=e.target_id WHERE e.target_kind='prompt' AND e.model='model2vec/potion-base-8M'")}
                for capture in captures:
                    seq = capture["seq"]
                    if seq in ready and seq not in found:
                        query = urllib.parse.urlencode({"q": "railway commuting", "limit": 10,
                                "project": "/bench/freshness", "after": capture["ts"], "before": capture["ts"] + 1})
                        pack = request("/v1/memory/answer-pack?" + query)
                        hit = next((h for h in pack["promptHits"] if h["seq"] == seq), None)
                        if hit and any(a["arm"] == "semantic" for a in hit["arms"]):
                            found[seq] = (time.perf_counter() - capture["ack"]) * 1000
                time.sleep(.05)
            usage = db.execute("SELECT COUNT(*) FROM model_usage").fetchone()[0]
            llm_calls = db.execute("SELECT COALESCE(SUM(llm_calls),0) FROM class_runs").fetchone()[0]
            role_counts = dict(db.execute("SELECT role, COUNT(*) FROM prompts GROUP BY role"))
        finally:
            db.close()
        assert usage == 0 and llm_calls == 0
        assert len(found) == capture_count, f"only {len(found)}/{capture_count} sources became semantically retrievable"
        availability = summary(list(found.values()))
        return {"status": "passed", "setupMsIncludingVerifiedPrestageInitAndDaemon": setup_ms, "assets": pins,
                "captureCount": capture_count, "roleCounts": role_counts, "targetCaptureRatePerSecond": 1 / cadence_seconds,
                "observedCaptureIntervals": summary(gaps), "captureLatency": summary([r["captureMs"] for r in captures]),
                "semanticAvailabilityFromAck": availability, "p95Below5Seconds": availability["p95Ms"] < 5000,
                "capturingDurationMs": (captures[-1]["ack"] - started) * 1000,
                "completedDuringContinuousCapture": sum(c["ack"] + found[c["seq"]] / 1000 < captures[-1]["ack"] for c in captures),
                "llmUsageRows": usage, "classRunLlmCalls": llm_calls, "healthAtStart": health,
                "method": "fresh home with assets ready; normal 2s indexing cadence, batch16, default30s consolidation tick; 50 sources at5/s;50ms readiness polling followed by actual scoped HTTP search requiring semantic arm; no bulk reindex",
                "limits": "single short normal-load run; excludes download; not overload, sustained backfill or cold model download measurement"}
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=8)
        except subprocess.TimeoutExpired:
            daemon.kill()
            daemon.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--polis", type=Path, required=True)
    parser.add_argument("--assets", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--root", type=Path)
    parser.add_argument("--binary-profile", default="unspecified", help="Declared build configuration; recorded without inference")
    parser.add_argument("--daemon-only", action="store_true")
    args = parser.parse_args()
    args.polis = args.polis.resolve()
    args.assets = args.assets.resolve()
    root = args.root or Path(tempfile.mkdtemp(prefix="polis-local-readiness-"))
    root.mkdir(parents=True, exist_ok=True)
    if args.daemon_only:
        manifest = json.loads(args.output.read_text())
        if manifest["binarySha256"] != hashlib.sha256(args.polis.read_bytes()).hexdigest():
            raise ValueError("daemon-only continuation must use the same binary as the MCP measurement")
    else:
        manifest = {"schema": 1, "kind": "pinned-model2vec-mcp-and-daemon-readiness", "measuredAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                    "binarySha256": hashlib.sha256(args.polis.read_bytes()).hexdigest(), "binaryProfile": args.binary_profile,
                    "machine": {"os": platform.platform(), "architecture": platform.machine()},
                    "temporaryRoot": str(root), "provider": "model2vec/potion-base-8M", "paidEvaluation": "HOLD",
                    "llmBackends": "none: sanitized executable PATH, API credential allowlist excludes keys, POLIS_NO_NETWORK=1",
                    "assetSource": "https://huggingface.co/minishlab/potion-base-8M/resolve/main/; every file verified against source-controlled SHA256 and byte length",
                    "downloadTiming": "not instrumented; download was a separate approved prestaging step"}
        manifest["mcp"] = mcp_readiness(args, root)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(manifest, indent=2) + "\n")
        print("MCP_READINESS=" + json.dumps(manifest["mcp"]), flush=True)
    failed = False
    try:
        manifest["daemonFreshness"] = daemon_freshness(args, root)
    except Exception as error:
        manifest["daemonFreshness"] = {"status": "failed_or_unavailable", "error": str(error)}
        failed = True
    args.output.write_text(json.dumps(manifest, indent=2) + "\n")
    print("DAEMON_FRESHNESS=" + json.dumps(manifest["daemonFreshness"]), flush=True)
    return 2 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
