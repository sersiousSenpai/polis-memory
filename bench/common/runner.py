"""The per-question loop every runner shares: ingest the haystack into a fresh
memory, retrieve a grounding context for the question, answer with the fixed
model, judge with the fixed judge, and fold the numbers into one `Result`.

A backend is anything with `ingest(question, items) -> IngestStats`,
`context(question) -> (text | None, wall_ms)` and `close()`; the three
runners (Polis, Mem0, Graphiti) differ only there, so their tables join.
"""
from __future__ import annotations

import argparse
import sys
import time
from dataclasses import dataclass
from typing import Any, Callable, Protocol

from .judge import judge
from .llm import BudgetExceeded, Llm, Spend, approx_tokens
from .schema import (ANSWER_SYSTEM, QuestionResult, Result, answer_prompt, load_dataset,
                     messages_of, stamp, stratified_subset)


@dataclass
class IngestStats:
    wall_ms: float
    llm_calls: int = 0
    llm_tokens: int = 0


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
    return p


def run(args: argparse.Namespace, make_backend: Callable[[argparse.Namespace], Backend], roles: set[str]) -> str:
    stub = args.stub or args.provider == "stub"
    spend = Spend(budget_tokens=args.budget_tokens)
    provider = "stub" if stub else args.provider
    answer_llm = Llm(provider, args.answer_model, spend)
    judge_llm = Llm(provider, args.judge_model, spend)
    questions = stratified_subset(load_dataset(args.data, sample=stub), args.subset, args.seed)
    if args.limit:
        questions = questions[: args.limit]
    commit, date, hardware = stamp()
    result = Result(system=args.system, subset=args.subset, seed=args.seed, stub=stub, commit=commit, date=date,
                    hardware=hardware, models={"answer": args.answer_model if not stub else "stub", "judge": args.judge_model if not stub else "stub"})
    out: list[QuestionResult] = []
    ingest_wall = 0.0
    write_calls = 0
    write_tokens = 0
    t_start = time.time()
    for i, q in enumerate(questions):
        backend = make_backend(args)
        try:
            items = messages_of(q, roles)
            st = backend.ingest(q, items)
            ingest_wall += st.wall_ms / 1000.0
            write_calls += st.llm_calls
            write_tokens += st.llm_tokens
            text, q_ms = backend.context(q)
            prompt = answer_prompt(q["question"], q.get("question_date", ""), text)
            response = answer_llm.complete("answer", ANSWER_SYSTEM, prompt, max_tokens=400)
            ok = judge(judge_llm, q["question_type"], q["question_id"], q["question"], q["answer"], response)
            out.append(QuestionResult(
                id=q["question_id"], type=q["question_type"], correct=ok, abstention=q["question_id"].endswith("_abs"),
                query_ms=round(q_ms, 2), context_tokens=approx_tokens(text or ""), ingest_ms=round(st.wall_ms, 1),
                sessions=len(q.get("haystack_sessions") or []), messages=len(items), response_head=response[:120],
            ))
            print(f"[{i + 1}/{len(questions)}] {q['question_id']} {q['question_type']} {'ok' if ok else 'miss'} "
                  f"ctx={approx_tokens(text or '')}tok q={q_ms:.0f}ms", file=sys.stderr)
        except BudgetExceeded as e:
            result.partial = True
            result.partial_reason = str(e)
            print(f"stopping: {e}", file=sys.stderr)
            break
        finally:
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
