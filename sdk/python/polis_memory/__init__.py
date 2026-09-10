"""Thin synchronous and asynchronous clients for the local Polis HTTP API."""
from __future__ import annotations

import asyncio
import builtins
from dataclasses import asdict, dataclass
import json
from typing import Any, Literal, TypedDict
import urllib.error
import urllib.parse
import urllib.request
import uuid
from .generated import AnswerPack, Claim, ClaimWrite, EvidenceRecord, RetrievalTrace, WriteReceipt, ContextBlock, EvidenceFilter, ForgetReceipt


@dataclass(frozen=True)
class Scope:
    principal: str | None = None
    project: str | None = None
    agent: str | None = None
    run: str | None = None
    org: str | None = None
    include_shared: bool = False

    def wire(self, query: bool = False) -> dict[str, Any]:
        out = {k: v for k, v in asdict(self).items() if v is not None and k != "include_shared"}
        out["include_shared" if query else "includeShared"] = str(self.include_shared).lower() if query else self.include_shared
        return out


ForgetTarget = Literal["ledger_event", "prompt", "browse_event", "note", "user_note"]


class IngestItem(TypedDict, total=False):
    body: str
    ts: int | None
    role: Literal["user", "assistant", "agent", "system"]
    session: str
    run: str
    project: str


class IngestReceipt(TypedDict):
    recorded: list[int]
    skipped: int


class PolisError(RuntimeError):
    def __init__(self, message: str, status: int | None = None, trace_id: str | None = None):
        super().__init__(message)
        self.status = status
        self.trace_id = trace_id


class AuthenticationError(PolisError): pass
class RejectedError(PolisError): pass
class NotFoundError(PolisError): pass
class UnavailableError(PolisError): pass
class TimeoutError(PolisError): pass


def _filter_query(filter: EvidenceFilter | None) -> dict[str, Any]:
    names = {"validAt": "valid_at", "knownAt": "known_at"}
    return {names.get(k, k): ",".join(v) if k == "roles" else v for k, v in (filter or {}).items()}


class Client:
    def __init__(self, base_url: str = "http://127.0.0.1:7677", *, token: str | None = None,
                 scope: Scope | None = None, timeout: float = 10):
        if timeout <= 0:
            raise ValueError("timeout must be positive")
        parsed = urllib.parse.urlparse(base_url)
        if parsed.scheme not in {"http", "https"} or not parsed.netloc:
            raise ValueError("base_url must be an HTTP(S) URL")
        self.base_url, self.token = base_url.rstrip("/"), token
        self.scope, self.timeout = scope or Scope(), timeout

    def _request(self, method: str, path: str, *, query: dict | None = None,
                 body: dict | None = None, trace_id: str | None = None) -> Any:
        trace_id = trace_id or uuid.uuid4().hex
        headers = {"accept": "application/json", "x-polis-trace-id": trace_id}
        if self.token:
            headers["authorization"] = f"Bearer {self.token}"
        if body is not None:
            headers["content-type"] = "application/json"
        url = self.base_url + path
        if query is not None:
            url += "?" + urllib.parse.urlencode({k: v for k, v in query.items() if v is not None})
        req = urllib.request.Request(url, data=json.dumps(body).encode() if body is not None else None,
                                     headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as response:
                return json.load(response)
        except urllib.error.HTTPError as exc:
            try:
                detail = json.loads(exc.read()).get("error", str(exc))
            except (ValueError, AttributeError):
                detail = str(exc)
            kind = {400: RejectedError, 401: AuthenticationError, 403: AuthenticationError,
                    404: NotFoundError, 503: UnavailableError}.get(exc.code, PolisError)
            raise kind(str(detail), exc.code, trace_id) from exc
        except (TimeoutError, builtins.TimeoutError) as exc:
            raise TimeoutError("Polis request timed out", trace_id=trace_id) from exc
        except urllib.error.URLError as exc:
            if isinstance(exc.reason, builtins.TimeoutError):
                raise TimeoutError("Polis request timed out", trace_id=trace_id) from exc
            raise UnavailableError(str(exc.reason), trace_id=trace_id) from exc

    def ingest(self, items: list[IngestItem], *, idempotency_key: str, trace_id: str | None = None) -> IngestReceipt:
        """Retry with the same key AND items. The server deduplicates (body hash, run).

        An item-supplied run is rejected rather than weakening the batch key.
        Agent identity stays in scope.agent, separate from this operation key.
        """
        if not idempotency_key.strip():
            raise ValueError("idempotency_key is required")
        if self.scope.run is not None and self.scope.run != idempotency_key:
            raise ValueError("scope.run must match idempotency_key when supplied")
        if any(item.get("run") not in (None, idempotency_key) for item in items):
            raise ValueError("item run conflicts with idempotency_key")
        return self._request("POST", "/v1/memory/events", body={
            "items": [dict(item, run=idempotency_key) for item in items], "scope": self.scope.wire()}, trace_id=trace_id)

    def context(self, q: str, *, max_tokens: int = 2000, roles: list[str] | None = None, filter: EvidenceFilter | None = None,
                trace_id: str | None = None) -> ContextBlock:
        trace_id = trace_id or uuid.uuid4().hex
        return self._request("GET", "/v1/memory/context", query={**self.scope.wire(True), **_filter_query(filter), "q": q,
            "max_tokens": max_tokens, **({"roles": ",".join(roles)} if roles else {}), "trace_id": trace_id}, trace_id=trace_id)

    def search(self, q: str, *, limit: int = 20, filter: EvidenceFilter | None = None, max_tokens: int | None = None, candidate_limit: int | None = None, cursor: str | None = None, trace_id: str | None = None) -> AnswerPack:
        trace_id = trace_id or uuid.uuid4().hex
        return self._request("GET", "/v1/memory/answer-pack", query={**self.scope.wire(True), **_filter_query(filter), "max_tokens": max_tokens, "candidate_limit": candidate_limit, "cursor": cursor,
            "q": q, "limit": limit, "trace_id": trace_id}, trace_id=trace_id)

    def decide(self, source_seq: int, *, kind: str = "decision", trace_id: str | None = None) -> WriteReceipt:
        return self._request("POST", "/v1/memory/decisions", body={"sourceSeq": source_seq, "kind": kind, "scope": self.scope.wire()}, trace_id=trace_id)

    def forget(self, target_id: int, *, confirm: Literal["forget"], target_kind: ForgetTarget = "ledger_event", trace_id: str | None = None) -> ForgetReceipt:
        """Forget a cited source; use note/user_note only for internal note row IDs.

        Confirmation is never supplied automatically.
        """
        if confirm != "forget":
            raise ValueError('confirm must be exactly "forget"')
        if target_kind not in {"ledger_event", "prompt", "browse_event", "note", "user_note"}:
            raise ValueError("unsupported forgetting target kind")
        return self._request("POST", "/v1/memory/forget", body={"targetKind": target_kind, "targetId": str(target_id), "confirm": confirm, "scope": self.scope.wire()}, trace_id=trace_id)

    def write_claim(self, claim: ClaimWrite, *, trace_id: str | None = None) -> Claim:
        return self._request("POST", "/v1/memory/claims", body={**claim, "scope": self.scope.wire()}, trace_id=trace_id)

    def claims(self, *, subject: str | None = None, valid_at: int | None = None, known_at: int | None = None) -> list[Claim]:
        return self._request("GET", "/v1/memory/claims", query={**self.scope.wire(True), "subject": subject,
            "valid_at": valid_at, "known_at": known_at})["claims"]

    def evidence(self, seq: int, *, chain_id: str | None = None) -> EvidenceRecord:
        return self._request("GET", f"/v1/memory/evidence/{seq}", query={**self.scope.wire(True), "chain_id": chain_id})

    def traces(self, *, id: str | None = None, limit: int = 20) -> list[RetrievalTrace]:
        return self._request("GET", "/v1/memory/traces", query={**self.scope.wire(True), "id": id, "limit": limit})["traces"]

    def health(self) -> dict[str, Any]:
        return self._request("GET", "/v1/memory/health")


class AsyncClient(Client):
    """Asyncio wrapper; socket timeout bounds each worker, no automatic retries.

    Cancelling a coroutine does not forcibly interrupt an in-flight urllib
    worker; its configured socket timeout remains the bound.
    """
    async def ingest(self, items: list[IngestItem], *, idempotency_key: str, trace_id: str | None = None) -> IngestReceipt:
        return await asyncio.to_thread(super().ingest, items, idempotency_key=idempotency_key, trace_id=trace_id)

    async def context(self, q: str, *, max_tokens: int = 2000, roles: list[str] | None = None, filter: EvidenceFilter | None = None,
                      trace_id: str | None = None) -> ContextBlock:
        return await asyncio.to_thread(super().context, q, max_tokens=max_tokens, roles=roles, filter=filter, trace_id=trace_id)

    async def search(self, q: str, *, limit: int = 20, filter: EvidenceFilter | None = None, max_tokens: int | None = None, candidate_limit: int | None = None, cursor: str | None = None, trace_id: str | None = None) -> AnswerPack:
        return await asyncio.to_thread(super().search, q, limit=limit, filter=filter, max_tokens=max_tokens, candidate_limit=candidate_limit, cursor=cursor, trace_id=trace_id)

    async def health(self) -> dict[str, Any]:
        return await asyncio.to_thread(super().health)

    async def decide(self, source_seq: int, *, kind: str = "decision", trace_id: str | None = None) -> WriteReceipt:
        return await asyncio.to_thread(super().decide, source_seq, kind=kind, trace_id=trace_id)

    async def forget(self, target_id: int, *, confirm: Literal["forget"], target_kind: ForgetTarget = "ledger_event", trace_id: str | None = None) -> ForgetReceipt:
        return await asyncio.to_thread(super().forget, target_id, confirm=confirm, target_kind=target_kind, trace_id=trace_id)

    async def write_claim(self, claim: ClaimWrite, *, trace_id: str | None = None) -> Claim:
        return await asyncio.to_thread(super().write_claim, claim, trace_id=trace_id)

    async def claims(self, *, subject: str | None = None, valid_at: int | None = None, known_at: int | None = None) -> list[Claim]:
        return await asyncio.to_thread(super().claims, subject=subject, valid_at=valid_at, known_at=known_at)

    async def evidence(self, seq: int, *, chain_id: str | None = None) -> EvidenceRecord:
        return await asyncio.to_thread(super().evidence, seq, chain_id=chain_id)

    async def traces(self, *, id: str | None = None, limit: int = 20) -> list[RetrievalTrace]:
        return await asyncio.to_thread(super().traces, id=id, limit=limit)
