"""The per-question loop every runner shares: ingest the haystack into a fresh
memory, retrieve a grounding context for the question, answer with the fixed
model, judge with the fixed judge, and fold the numbers into one `Result`.

A backend is anything with `ingest(question, items) -> IngestStats`,
`context(question) -> (text | None, wall_ms)` and `close()`; the three
runners (Polis, Mem0, Graphiti) differ only there, so their tables join.
"""
from __future__ import annotations

import argparse
import json
import os
import uuid
import sys
import time
from dataclasses import dataclass
from typing import Any, Callable, Protocol

from .budget import AggregateBudget
from .manifest import build_manifest, partition
from .tokenizer import ContextTokenizer
from .judge import judge
from .llm import BudgetExceeded, Llm, Spend
from .schema import (ANSWER_SYSTEM, QuestionResult, Result, answer_prompt, load_dataset,
                     messages_of, stamp, stratified_subset)


@dataclass
class IngestStats:
    wall_ms: float
    llm_calls: int = 0
    llm_tokens: int = 0
    readiness: dict[str, Any] | None = None


class Backend(Protocol):
    def ingest(self, question: dict[str, Any], items: list[dict[str, Any]]) -> IngestStats: ...
    def context(self, question: dict[str, Any]) -> tuple[str | None, float]: ...
    def close(self) -> None: ...


def common_args(p: argparse.ArgumentParser, system_default: str) -> argparse.ArgumentParser:
    p.add_argument("--system", default=system_default, help="row label in the table")
    p.add_argument("--data", help="LongMemEval-S JSON (see bench/README.md); omitted + --stub = the built-in six-question sample")
    p.add_argument("--subset", type=int, default=100, help="stratified subset size (100 nightly, 500 full)")
    p.add_argument("--seed", type=int, default=20260907, help="subset selection seed")
    p.add_argument("--stub", action="store_true", help="deterministic stub answer model + judge: no key, no spend, no scores")
    p.add_argument("--provider", default="anthropic", choices=["anthropic", "openai", "stub"])
    p.add_argument("--answer-model", default="claude-sonnet-5")
    p.add_argument("--judge-model", default="claude-sonnet-5")
    p.add_argument("--budget-tokens", type=int, default=None, help="spend cap; the run stops and the table is marked partial")
    p.add_argument("--max-context-tokens", type=int, default=4000)
    p.add_argument("--out", default=None, help="results dir (default bench/results)")
    p.add_argument("--limit", type=int, default=None, help="only the first N questions of the subset (smoke)")
    p.add_argument("--capture-profile", choices=["user-only", "all-roles"], default=None)
    p.add_argument("--processing-profile", choices=["immediate", "indexed", "organized"], default="immediate")
    p.add_argument("--split", choices=["development", "heldout", "full"], default="development")
    p.add_argument("--run-id", default=None, help="unique campaign id; generated unless declared")
    p.add_argument("--budget-ledger", help="shared ledger path, required for all scored systems/retries")
    p.add_argument("--timeout", type=float, default=120)
    return p


def run(args: argparse.Namespace, make_backend: Callable[[argparse.Namespace], Backend], roles: set[str]) -> str:
    if args.subset < 1 or args.timeout <= 0 or args.max_context_tokens < 1 or (args.limit is not None and args.limit < 1):
        raise ValueError("subset, timeout, context ceiling and limit must be positive")
    stub = args.stub or args.provider == "stub"
    if not stub:
        with open(os.path.join(os.path.dirname(__file__), "..", "hold.json")) as f:
            if not json.load(f)["paidRunsAllowed"]:
                raise SystemExit("Paid benchmarks are on HOLD; no backend or model was invoked")
        if not args.budget_ledger or not args.budget_tokens:
            raise SystemExit("scored runs require one shared --budget-ledger and --budget-tokens")
    args.run_id = args.run_id or uuid.uuid4().hex
    args.capture_profile = args.capture_profile or ("all-roles" if "assistant" in roles else "user-only")
    roles = {"user", "assistant"} if args.capture_profile == "all-roles" else {"user"}
    tokenizer = ContextTokenizer(args.provider, args.answer_model, stub)
    spend = Spend(budget_tokens=args.budget_tokens,
                  aggregate=AggregateBudget(args.budget_ledger, args.budget_tokens) if not stub else None)
    provider = "stub" if stub else args.provider
    answer_llm = Llm(provider, args.answer_model, spend, timeout=args.timeout)
    judge_llm = Llm(provider, args.judge_model, spend, timeout=args.timeout)
    dataset = load_dataset(args.data, sample=stub)
    questions = stratified_subset(partition(dataset, args.split, args.seed), args.subset, args.seed)
    if args.limit:
        questions = questions[: args.limit]
    commit, date, hardware = stamp()
    result = Result(system=args.system, subset=args.subset, seed=args.seed, stub=stub, commit=commit, date=date,
                    hardware=hardware, models={"answer": args.answer_model if not stub else "stub", "judge": args.judge_model if not stub else "stub"})
    result.manifest = build_manifest(args, dataset, questions, tokenizer)
    out: list[QuestionResult] = []
    ingest_wall = 0.0
    write_calls = 0
    write_tokens = 0
    t_start = time.time()
    for i, q in enumerate(questions):
        backend = None
        q = dict(q, namespace=f"{args.run_id}:{uuid.uuid4().hex}:{q['question_id']}")
        try:
            backend = make_backend(args)
            items = messages_of(q, roles)
            st = backend.ingest(q, items)
            ingest_wall += st.wall_ms / 1000.0
            write_calls += st.llm_calls
            write_tokens += st.llm_tokens
            text, q_ms = backend.context(q)
            text = tokenizer.trim(text or "", args.max_context_tokens)
            prompt = answer_prompt(q["question"], q.get("question_date", ""), text)
            response = answer_llm.complete("answer", ANSWER_SYSTEM, prompt, max_tokens=400)
            ok = judge(judge_llm, q["question_type"], q["question_id"], q["question"], q["answer"], response)
            out.append(QuestionResult(
                id=q["question_id"], type=q["question_type"], correct=ok, abstention=q["question_id"].endswith("_abs"),
                query_ms=round(q_ms, 2), context_tokens=tokenizer.count(text or ""), ingest_ms=round(st.wall_ms, 1),
                sessions=len(q.get("haystack_sessions") or []), messages=len(items), response_head=response[:120],
                namespace=q["namespace"], readiness=st.readiness or {"processingProfile": args.processing_profile, "verified": False},
            ))
            print(f"[{i + 1}/{len(questions)}] {q['question_id']} {q['question_type']} {'ok' if ok else 'miss'} "
                  f"ctx={tokenizer.count(text or '')}tok q={q_ms:.0f}ms", file=sys.stderr)
        except BudgetExceeded as e:
            result.partial = True
            result.partial_reason = str(e)
            print(f"stopping: {e}", file=sys.stderr)
            break
        except Exception as e:
            result.partial = True
            result.partial_reason = str(e)
            result.failures.append({"questionId": q["question_id"], "errorType": type(e).__name__, "detail": str(e)})
            break
        finally:
            if backend is not None:
                backend.close()
    result.finish(out, ingest_wall, write_calls, write_tokens)
    result.spend = spend.as_dict()
    result.cost["wallS"] = round(time.time() - t_start, 1)
    out_dir = args.out or default_results_dir()
    path = result.write(out_dir)
    print(f"wrote {path}", file=sys.stderr)
    return path


def default_results_dir() -> str:
    import os
    return os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "results"))
