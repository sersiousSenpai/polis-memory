"""A minimal MCP stdio client for the `polis` binary.

One question = one fresh `POLIS_HOME` = one `polis init` + one `polis mcp`
process: total isolation between questions, the same isolation the
competitor runners get from a per-question user / group id. Standard library
only, so the stub path needs no installs.
"""
from __future__ import annotations

import json
import hashlib
from contextlib import closing
import queue
import sqlite3
import threading
import os
import shutil
import subprocess
import tempfile
import time
from typing import Any
from .assets import prestage


class PolisMcp:
    def __init__(self, polis_bin: str, home: str | None = None, device: str = "bench", timeout: float = 120, assets: str | None = None):
        self.polis_bin = os.path.abspath(polis_bin)
        self.timeout = timeout
        with open(self.polis_bin, "rb") as binary:
            self.binary_hash = hashlib.file_digest(binary, "sha256").hexdigest()
        self.asset_hashes = {}
        self.tmp = None
        if home is None:
            self.tmp = tempfile.mkdtemp(prefix="polis-bench-")
            home = os.path.join(self.tmp, ".polis")
        self.home = home
        self.env = {
            **{k: v for k, v in os.environ.items() if not k.startswith("POLIS_")},
            # Do not repurpose HOME; scope storage with POLIS_HOME. An empty
            # PATH also prevents the no-network fallback from invoking CLI LLMs.
            "PATH": os.path.join(home, "no-model-binaries"),
            "POLIS_EMBED": "model2vec" if assets else "none",
            "POLIS_HOME": home,
            "POLIS_NO_NETWORK": "1",
        }
        if assets:
            self.asset_hashes = prestage(assets, os.path.join(home, "models"))
        init = subprocess.run([self.polis_bin, "init", "--device", device], env=self.env, capture_output=True, text=True, timeout=timeout)
        if init.returncode != 0:
            raise RuntimeError(f"polis init failed: {init.stderr.strip()}")
        self.verify_empty()
        self.proc = subprocess.Popen(
            [self.polis_bin, "mcp"], env=self.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, bufsize=1,
        )
        self._lines = queue.Queue()
        def read_lines():
            for line in self.proc.stdout:
                self._lines.put(line)
            self._lines.put(None)
        threading.Thread(target=read_lines, daemon=True).start()
        self._id = 0
        self._call("initialize", {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "polis-bench", "version": "1"},
        })
        self._notify("notifications/initialized", {})

    def _send(self, msg: dict[str, Any]) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def _notify(self, method: str, params: dict[str, Any]) -> None:
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def _call(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self._id += 1
        my_id = self._id
        self._send({"jsonrpc": "2.0", "id": my_id, "method": method, "params": params})
        assert self.proc.stdout is not None
        deadline = time.monotonic() + self.timeout
        while True:
            try:
                line = self._lines.get(timeout=max(.001, deadline - time.monotonic()))
            except queue.Empty as exc:
                self.proc.kill()
                raise TimeoutError(f"{method} exceeded {self.timeout}s") from exc
            if not line:
                raise RuntimeError("polis mcp closed its stdout")
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if msg.get("id") == my_id:
                if "error" in msg:
                    raise RuntimeError(f"{method}: {msg['error']}")
                return msg.get("result", {})

    def tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        res = self._call("tools/call", {"name": name, "arguments": arguments})
        if res.get("isError"):
            raise RuntimeError(f"{name}: {res.get('structuredContent') or res.get('content')}")
        return res.get("structuredContent") or {}

    def ingest(self, items: list[dict[str, Any]], batch: int = 200) -> int:
        """`memory_ingest` in batches; returns how many rows were recorded."""
        recorded = 0
        for i in range(0, len(items), batch):
            batch_items = [{k: v for k, v in item.items() if k not in {"sourceDate", "turnIndex"}} for item in items[i:i + batch]]
            out = self.tool("memory_ingest", {"items": batch_items})
            recorded += len(out.get("recorded", []) or [])
        return recorded

    def context(self, q: str, max_tokens: int = 4000, roles: list[str] | None = None) -> tuple[str | None, float]:
        """`memory_context` → (text or None, wall ms)."""
        t0 = time.perf_counter()
        arguments = {"q": q, "max_tokens": max_tokens}
        if roles:
            arguments["filter"] = {"roles": roles}
        out = self.tool("memory_context", arguments)
        ms = (time.perf_counter() - t0) * 1000.0
        return out.get("text"), ms

    def _db(self):
        return closing(sqlite3.connect(f"file:{os.path.join(self.home, 'polis.db')}?mode=ro", uri=True))

    def verify_empty(self):
        with self._db() as db:
            for table in ("prompts", "browse_events", "user_notes"):
                # Initialization appends principal_bind identity metadata, not memory.
                if db.execute(f"SELECT count(*) FROM {table}").fetchone()[0]:
                    raise RuntimeError(f"benchmark starting store is not empty: {table}")
            if db.execute("SELECT count(*) FROM ledger_events WHERE kind != 'principal_bind'").fetchone()[0]:
                raise RuntimeError("benchmark starting ledger contains pre-existing evidence")

    def process(self, profile):
        start = time.perf_counter()
        readiness = {"processingProfile": profile, "emptyStart": True, "assets": self.asset_hashes,
                     "captureLlmCalls": 0, "processingLlmCalls": 0, "binarySha256": self.binary_hash}
        deadline = time.monotonic() + self.timeout
        with self._db() as db:
            captured_head = db.execute("SELECT COALESCE(MAX(seq),0) FROM ledger_events").fetchone()[0]
        if profile != "immediate":
            if not self.asset_hashes:
                raise RuntimeError("indexed/organized profiles require --embedding-assets containing pinned local assets")
            result = subprocess.run([self.polis_bin, "--json", "reindex", "--all"], env=self.env,
                                    capture_output=True, text=True, timeout=self.timeout, check=True)
            readiness["reindex"] = json.loads(result.stdout)
            if readiness["reindex"].get("provider") == "absent":
                raise RuntimeError("requested semantic index unavailable")
        if profile == "organized":
            passes = []
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("organization did not reach captured evidence before deadline")
                result = subprocess.run([self.polis_bin, "--json", "organize"], env=self.env,
                                        capture_output=True, text=True, timeout=remaining, check=True)
                outcome = json.loads(result.stdout)
                passes.append(outcome)
                if outcome.get("seqTo", 0) >= captured_head:
                    break
                if not outcome.get("ran"):
                    raise RuntimeError("organization stopped before processing the captured evidence")
            readiness["organization"] = passes
        with self._db() as db:
            total = db.execute("SELECT count(*) FROM prompts WHERE length(body)>0").fetchone()[0]
            indexed = db.execute("SELECT count(DISTINCT target_id) FROM embeddings WHERE target_kind='prompt' AND model='model2vec/potion-base-8M'").fetchone()[0]
        readiness.update(promptTargets=total, indexedPromptTargets=indexed, indexCoverage=indexed / total if total else 1,
                         processingMs=round((time.perf_counter() - start) * 1000, 2), verified=True)
        if profile != "immediate" and indexed != total:
            raise RuntimeError(f"index readiness incomplete: {indexed}/{total} prompt targets (including requested roles)")
        return readiness

    def close(self) -> None:
        try:
            if self.proc.stdin:
                self.proc.stdin.close()
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()
            self.proc.wait(timeout=5)
        if self.proc.stdout:
            self.proc.stdout.close()
        if self.tmp:
            shutil.rmtree(self.tmp, ignore_errors=True)


def find_polis(explicit: str | None = None) -> str:
    """The `polis` binary: `--polis`, `$POLIS_BIN`, PATH, or the repo's target/."""
    for cand in [explicit, os.environ.get("POLIS_BIN"), shutil.which("polis")]:
        if cand and os.path.exists(cand):
            return cand
    here = os.path.dirname(os.path.abspath(__file__))
    repo = os.path.abspath(os.path.join(here, "..", ".."))
    for profile in ("release", "debug"):
        cand = os.path.join(repo, "target", profile, "polis")
        if os.path.exists(cand):
            return cand
    raise SystemExit("no `polis` binary: pass --polis, set POLIS_BIN, or `cargo build -p polis-memory --features cli`")
