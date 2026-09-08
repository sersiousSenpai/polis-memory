"""A minimal MCP stdio client for the `polis` binary.

One question = one fresh `POLIS_HOME` = one `polis init` + one `polis mcp`
process: total isolation between questions, the same isolation the
competitor runners get from a per-question user / group id. Standard library
only, so the stub path needs no installs.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import time
from typing import Any


class PolisMcp:
    def __init__(self, polis_bin: str, home: str | None = None, device: str = "bench"):
        self.polis_bin = polis_bin
        self.tmp = None
        if home is None:
            self.tmp = tempfile.mkdtemp(prefix="polis-bench-")
            home = os.path.join(self.tmp, ".polis")
        self.home = home
        self.env = {
            **{k: v for k, v in os.environ.items() if not k.startswith("POLIS_")},
            "HOME": os.path.dirname(home) if self.tmp else os.environ.get("HOME", "/tmp"),
            "POLIS_HOME": home,
            "POLIS_NO_NETWORK": "1",
        }
        init = subprocess.run([polis_bin, "init", "--device", device], env=self.env, capture_output=True, text=True)
        if init.returncode != 0:
            raise RuntimeError(f"polis init failed: {init.stderr.strip()}")
        self.proc = subprocess.Popen(
            [polis_bin, "mcp"], env=self.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, text=True, bufsize=1,
        )
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
        while True:
            line = self.proc.stdout.readline()
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
            out = self.tool("memory_ingest", {"items": items[i:i + batch]})
            recorded += len(out.get("recorded", []) or [])
        return recorded

    def context(self, q: str, max_tokens: int = 4000) -> tuple[str | None, float]:
        """`memory_context` → (text or None, wall ms)."""
        t0 = time.perf_counter()
        out = self.tool("memory_context", {"q": q, "max_tokens": max_tokens})
        ms = (time.perf_counter() - t0) * 1000.0
        return out.get("text"), ms

    def close(self) -> None:
        try:
            if self.proc.stdin:
                self.proc.stdin.close()
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()
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
