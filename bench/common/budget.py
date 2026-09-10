"""Fail-closed aggregate token ledger shared by systems and repeated invocations.

A lock directory makes read/reserve/write atomic on Unix and Windows. Failed
requests retain their reservation because server-side usage is unknown. No
automatic retry can silently turn an unknown charge into zero.
"""
import contextlib
import json
import os
import time
import uuid


class BudgetExceeded(RuntimeError):
    pass


class AggregateBudget:
    def __init__(self, path, limit):
        if limit is None or limit <= 0:
            raise ValueError("a positive aggregate token limit is required")
        self.path = os.path.abspath(path)
        self.limit = limit
        os.makedirs(os.path.dirname(self.path), exist_ok=True)

    @contextlib.contextmanager
    def _locked(self):
        lock = self.path + ".lock"
        until = time.monotonic() + 10
        while True:
            try:
                os.mkdir(lock)
                break
            except FileExistsError:
                if time.monotonic() >= until:
                    raise RuntimeError("budget ledger lock busy; inspect interrupted writer before recovery")
                time.sleep(.02)
        try:
            yield
        finally:
            os.rmdir(lock)

    def _read(self):
        if not os.path.exists(self.path):
            return {"schema": "polis.budget/1", "limit": self.limit, "attempts": {}}
        with open(self.path) as f:
            state = json.load(f)
        if state["limit"] != self.limit:
            raise ValueError("aggregate budget limit differs from existing ledger")
        return state

    def _write(self, state):
        temporary = self.path + ".tmp"
        with open(temporary, "w") as f:
            json.dump(state, f, indent=2)
            f.flush()
            os.fsync(f.fileno())
        os.replace(temporary, self.path)

    def reserve(self, role, maximum):
        with self._locked():
            state = self._read()
            used = sum(a["charged"] for a in state["attempts"].values())
            if used + maximum > self.limit:
                raise BudgetExceeded(f"aggregate token limit {self.limit}: {used} used/reserved; next request needs {maximum}")
            attempt = uuid.uuid4().hex
            state["attempts"][attempt] = {"role": role, "charged": maximum, "status": "reserved_usage_unknown"}
            self._write(state)
            return attempt

    def settle(self, attempt, tokens_in, tokens_out):
        with self._locked():
            state = self._read()
            row = state["attempts"][attempt]
            ceiling = row["charged"]
            row.update(charged=tokens_in + tokens_out, tokensIn=tokens_in, tokensOut=tokens_out, status="measured")
            self._write(state)
            if row["charged"] > ceiling:
                raise BudgetExceeded("provider usage exceeded reserved ceiling; stop and audit tokenizer/accounting")
