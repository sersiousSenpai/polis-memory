"""One LLM client for the answer model and the judge: Anthropic Messages API or
an OpenAI-compatible chat endpoint, standard library only (urllib), with a
deterministic stub so the whole pipeline runs with no key and no spend.

Spend is metered here: every call adds to `Spend.tokens`, and a call past the
budget raises `BudgetExceeded`, which the runners turn into a partial table
marked as such (§6.2's spend cap).
"""
from __future__ import annotations

import hashlib
import json
import os
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field


class BudgetExceeded(RuntimeError):
    pass


@dataclass
class Spend:
    budget_tokens: int | None = None
    tokens_in: int = 0
    tokens_out: int = 0
    calls: int = 0
    by_role: dict[str, int] = field(default_factory=dict)

    @property
    def tokens(self) -> int:
        return self.tokens_in + self.tokens_out

    def charge(self, role: str, tin: int, tout: int) -> None:
        self.calls += 1
        self.tokens_in += tin
        self.tokens_out += tout
        self.by_role[role] = self.by_role.get(role, 0) + tin + tout
        if self.budget_tokens is not None and self.tokens > self.budget_tokens:
            raise BudgetExceeded(f"token budget {self.budget_tokens} exceeded at {self.tokens}")

    def as_dict(self) -> dict:
        return {"budgetTokens": self.budget_tokens, "tokensIn": self.tokens_in, "tokensOut": self.tokens_out,
                "calls": self.calls, "byRole": self.by_role}


def approx_tokens(text: str) -> int:
    # ~4 bytes a token, the same rule polis uses for its own budgets.
    return max(1, len(text.encode("utf-8")) // 4)


class Llm:
    """`provider` ∈ {stub, anthropic, openai}. Keys from the environment only."""

    def __init__(self, provider: str, model: str, spend: Spend, base_url: str | None = None, temperature: float = 0.0):
        self.provider = provider
        self.model = model
        self.spend = spend
        self.temperature = temperature
        if provider == "anthropic":
            self.key = os.environ.get("ANTHROPIC_API_KEY") or os.environ.get("LONGMEMEVAL_API_KEY")
            self.base_url = base_url or "https://api.anthropic.com"
        elif provider == "openai":
            self.key = os.environ.get("OPENAI_API_KEY") or os.environ.get("LONGMEMEVAL_API_KEY")
            self.base_url = base_url or os.environ.get("OPENAI_BASE_URL", "https://api.openai.com")
        elif provider == "stub":
            self.key = None
            self.base_url = None
        else:
            raise ValueError(f"unknown provider {provider}")
        if provider != "stub" and not self.key:
            raise SystemExit(f"{provider}: no API key in the environment (ANTHROPIC_API_KEY / OPENAI_API_KEY / LONGMEMEVAL_API_KEY)")

    def complete(self, role: str, system: str, user: str, max_tokens: int = 512) -> str:
        if self.provider == "stub":
            return self._stub(role, system, user)
        if os.environ.get("POLIS_NO_NETWORK") == "1":
            raise SystemExit("POLIS_NO_NETWORK=1 forbids a model call; use --stub")
        text, tin, tout = self._http(system, user, max_tokens)
        self.spend.charge(role, tin, tout)
        return text

    def _stub(self, role: str, system: str, user: str) -> str:
        # Deterministic: the judge's stub says "yes" iff the gold answer's
        # tokens appear in the context the answer model saw; the answer
        # model's stub echoes the retrieved context's first line. Together
        # they measure retrieval reachability with no model at all, which is
        # exactly what a keyless run can honestly claim.
        tin, tout = approx_tokens(system + user), 16
        self.spend.charge(role, tin, tout)
        if role == "judge":
            gold = _between(user, "<gold>", "</gold>").lower()
            resp = _between(user, "<response>", "</response>").lower()
            words = [w for w in gold.replace(",", " ").split() if len(w) > 3]
            hit = words and sum(1 for w in words if w in resp) >= max(1, len(words) // 2)
            return "yes" if hit else "no"
        ctx = _between(user, "<context>", "</context>")
        if not ctx.strip():
            return "I don't know."
        digest = hashlib.sha256(ctx.encode()).hexdigest()[:8]
        return ctx.strip()[:600] + f"\n(stub answer {digest})"

    def _http(self, system: str, user: str, max_tokens: int) -> tuple[str, int, int]:
        if self.provider == "anthropic":
            url = f"{self.base_url}/v1/messages"
            body = {"model": self.model, "max_tokens": max_tokens, "temperature": self.temperature,
                    "system": system, "messages": [{"role": "user", "content": user}]}
            headers = {"x-api-key": self.key, "anthropic-version": "2023-06-01", "content-type": "application/json"}
        else:
            url = f"{self.base_url}/v1/chat/completions"
            body = {"model": self.model, "max_tokens": max_tokens, "temperature": self.temperature,
                    "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}]}
            headers = {"authorization": f"Bearer {self.key}", "content-type": "application/json"}
        data = json.dumps(body).encode()
        for attempt in range(5):
            try:
                req = urllib.request.Request(url, data=data, headers=headers, method="POST")
                with urllib.request.urlopen(req, timeout=120) as resp:
                    out = json.loads(resp.read())
                break
            except urllib.error.HTTPError as e:
                if e.code in (429, 500, 502, 503, 529) and attempt < 4:
                    time.sleep(2 ** attempt)
                    continue
                raise
        if self.provider == "anthropic":
            text = "".join(c.get("text", "") for c in out.get("content", []) if c.get("type") == "text")
            u = out.get("usage", {})
            return text, int(u.get("input_tokens", 0)), int(u.get("output_tokens", 0))
        text = out["choices"][0]["message"]["content"] or ""
        u = out.get("usage", {})
        return text, int(u.get("prompt_tokens", 0)), int(u.get("completion_tokens", 0))


def _between(s: str, a: str, b: str) -> str:
    i = s.find(a)
    j = s.find(b, i + len(a)) if i >= 0 else -1
    return s[i + len(a):j] if i >= 0 and j >= 0 else ""
