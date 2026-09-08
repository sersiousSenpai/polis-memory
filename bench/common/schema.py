"""The shared result schema (`polis.bench/1`), the stratified subset selector,
the dataset loader (with a tiny built-in sample so `--stub` runs offline), and
the git/hardware stamps every results file carries.
"""
from __future__ import annotations

import datetime as _dt
import hashlib
import json
import os
import platform
import random
import subprocess
from dataclasses import dataclass, field, asdict
from typing import Any

SCHEMA = "polis.bench/1"
BENCHMARK = "longmemeval-s"
TYPES = [
    "single-session-user", "single-session-assistant", "single-session-preference",
    "temporal-reasoning", "knowledge-update", "multi-session",
]

ANSWER_SYSTEM = (
    "You are a helpful assistant with access to the user's past conversations, "
    "given below as retrieved context. Answer the question using that context. "
    "If the context does not contain the answer, say that the information is not available."
)


def answer_prompt(question: str, question_date: str, context: str | None) -> str:
    return (
        f"<context>\n{context or ''}\n</context>\n\n"
        f"Today's date: {question_date}\n\nQuestion: {question}\n\n"
        "Answer concisely, citing the relevant past conversation when you can."
    )


@dataclass
class QuestionResult:
    id: str
    type: str
    correct: bool
    abstention: bool
    query_ms: float
    context_tokens: int
    ingest_ms: float
    sessions: int
    messages: int
    response_head: str = ""


@dataclass
class Result:
    system: str
    subset: int
    seed: int
    stub: bool
    commit: str
    date: str
    hardware: str
    models: dict[str, str]
    n: int = 0
    accuracy: dict[str, Any] = field(default_factory=dict)
    cost: dict[str, Any] = field(default_factory=dict)
    spend: dict[str, Any] = field(default_factory=dict)
    partial: bool = False
    partial_reason: str | None = None
    questions: list[dict[str, Any]] = field(default_factory=list)
    schema: str = SCHEMA
    benchmark: str = BENCHMARK

    def finish(self, results: list[QuestionResult], ingest_wall_s: float, write_calls: int, write_tokens: int) -> None:
        self.n = len(results)
        by_type: dict[str, list[bool]] = {}
        for r in results:
            by_type.setdefault(r.type, []).append(r.correct)
        self.accuracy = {
            "overall": _mean([r.correct for r in results]),
            "byType": {t: _mean(v) for t, v in sorted(by_type.items())},
            "abstention": _mean([r.correct for r in results if r.abstention]),
        }
        q_ms = sorted(r.query_ms for r in results)
        ctx = sorted(r.context_tokens for r in results)
        self.cost = {
            "ingestWallS": round(ingest_wall_s, 3),
            "llmCallsWrite": write_calls,
            "tokensWrite": write_tokens,
            "queryP50Ms": _pct(q_ms, 0.5),
            "queryP95Ms": _pct(q_ms, 0.95),
            "contextTokensP50": _pct(ctx, 0.5),
            "sessionsIngested": sum(r.sessions for r in results),
            "messagesIngested": sum(r.messages for r in results),
        }
        self.questions = [asdict(r) for r in results]

    def write(self, out_dir: str) -> str:
        os.makedirs(out_dir, exist_ok=True)
        name = f"{self.date}-{self.commit[:12]}-longmemeval-{self.system}.json"
        path = os.path.join(out_dir, name)
        with open(path, "w") as f:
            json.dump(asdict(self), f, indent=2)
        return path


def _mean(xs: list[bool]) -> float | None:
    return round(sum(1 for x in xs if x) / len(xs), 4) if xs else None


def _pct(xs: list[float], p: float) -> float | None:
    if not xs:
        return None
    k = min(len(xs) - 1, max(0, int(round(p * (len(xs) - 1)))))
    return round(xs[k], 2)


def stamp() -> tuple[str, str, str]:
    """(commit, date, hardware) the way the Rust instruments stamp results."""
    try:
        commit = subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip() or "unknown"
    except Exception:
        commit = "unknown"
    date = _dt.date.today().isoformat()
    hw = f"{platform.system().lower()}-{platform.machine()} {platform.processor() or ''}".strip()
    return commit, date, hw


def stratified_subset(questions: list[dict[str, Any]], n: int, seed: int) -> list[dict[str, Any]]:
    """`n` questions with each type's share preserved (seeded, stable order)."""
    if n >= len(questions):
        return list(questions)
    rng = random.Random(seed)
    by_type: dict[str, list[dict[str, Any]]] = {}
    for q in questions:
        by_type.setdefault(q["question_type"], []).append(q)
    total = len(questions)
    picked: list[dict[str, Any]] = []
    shares = {t: len(v) * n / total for t, v in by_type.items()}
    counts = {t: int(s) for t, s in shares.items()}
    # Distribute the rounding remainder to the largest fractional shares.
    remainder = n - sum(counts.values())
    for t in sorted(shares, key=lambda t: shares[t] - counts[t], reverse=True)[:remainder]:
        counts[t] += 1
    for t in sorted(by_type):
        pool = sorted(by_type[t], key=lambda q: q["question_id"])
        rng.shuffle(pool)
        picked.extend(pool[:counts[t]])
    picked.sort(key=lambda q: q["question_id"])
    return picked


def load_dataset(path: str | None, sample: bool) -> list[dict[str, Any]]:
    """The LongMemEval-S JSON (a list of question objects). `path` may be the
    file `huggingface_hub` downloaded (see README) or any local copy; with
    `sample=True` and no path, a six-question built-in set covering every
    type is used so the pipeline runs offline."""
    if path:
        with open(path) as f:
            data = json.load(f)
        if isinstance(data, dict) and "data" in data:
            data = data["data"]
        return list(data)
    if not sample:
        raise SystemExit("no dataset: pass --data <longmemeval_s.json> (see bench/README.md) or --stub for the built-in sample")
    return builtin_sample()


def builtin_sample() -> list[dict[str, Any]]:
    """Six synthetic questions, one per type, shaped exactly like the dataset."""
    def sess(sid: str, turns: list[tuple[str, str]]) -> list[dict[str, str]]:
        return [{"role": r, "content": c} for r, c in turns]
    base = "2023/05/20 (Sat) 02:21"
    return [
        {
            "question_id": "sample_ssu_1", "question_type": "single-session-user",
            "question": "Which database did I say we chose for the grep index?", "answer": "sqlite with fts5 trigram",
            "question_date": base, "haystack_session_ids": ["s1", "s2"], "haystack_dates": ["2023/05/01 (Mon) 10:00", "2023/05/03 (Wed) 11:00"],
            "haystack_sessions": [sess("s1", [("user", "we chose sqlite fts5 trigram for the grep index, it keeps grep under 30 ms"), ("assistant", "Noted: sqlite FTS5 trigram for grep.")]),
                                  sess("s2", [("user", "the weather is nice today"), ("assistant", "Enjoy it!")])],
            "answer_session_ids": ["s1"],
        },
        {
            "question_id": "sample_ssa_1", "question_type": "single-session-assistant",
            "question": "What port did the assistant suggest for the standalone daemon?", "answer": "7677",
            "question_date": base, "haystack_session_ids": ["s3"], "haystack_dates": ["2023/05/04 (Thu) 09:00"],
            "haystack_sessions": [sess("s3", [("user", "what port should the standalone daemon use?"), ("assistant", "Use 7677 by default; 7676 is taken by the host app.")])],
            "answer_session_ids": ["s3"],
        },
        {
            "question_id": "sample_ssp_1", "question_type": "single-session-preference",
            "question": "Recommend a note-taking setup for me.", "answer": "The user prefers plain markdown files in a folder synced by git, no proprietary formats.",
            "question_date": base, "haystack_session_ids": ["s4"], "haystack_dates": ["2023/05/05 (Fri) 12:00"],
            "haystack_sessions": [sess("s4", [("user", "I only keep notes as plain markdown files in a git-synced folder; I refuse proprietary formats"), ("assistant", "Got it.")])],
            "answer_session_ids": ["s4"],
        },
        {
            "question_id": "sample_tr_1", "question_type": "temporal-reasoning",
            "question": "How many days after I started the migration did I finish it?", "answer": "4 days",
            "question_date": base, "haystack_session_ids": ["s5", "s6"], "haystack_dates": ["2023/05/06 (Sat) 08:00", "2023/05/10 (Wed) 18:00"],
            "haystack_sessions": [sess("s5", [("user", "I started the schema migration today"), ("assistant", "Good luck.")]),
                                  sess("s6", [("user", "the schema migration is finished today"), ("assistant", "Congratulations.")])],
            "answer_session_ids": ["s5", "s6"],
        },
        {
            "question_id": "sample_ku_1", "question_type": "knowledge-update",
            "question": "Which embedder are we using now?", "answer": "model2vec (previously the apple sentence embedder)",
            "question_date": base, "haystack_session_ids": ["s7", "s8"], "haystack_dates": ["2023/05/07 (Sun) 08:00", "2023/05/12 (Fri) 18:00"],
            "haystack_sessions": [sess("s7", [("user", "we use the apple sentence embedder for the semantic arm"), ("assistant", "Ok.")]),
                                  sess("s8", [("user", "update: we switched the semantic arm to model2vec"), ("assistant", "Noted the switch to model2vec.")])],
            "answer_session_ids": ["s8"],
        },
        {
            "question_id": "sample_ms_1_abs", "question_type": "multi-session",
            "question": "What did I decide about the org node's oauth provider?", "answer": "The user never discussed an oauth provider; the information is not available.",
            "question_date": base, "haystack_session_ids": ["s9", "s10"], "haystack_dates": ["2023/05/08 (Mon) 08:00", "2023/05/09 (Tue) 08:00"],
            "haystack_sessions": [sess("s9", [("user", "the org node relays envelopes over https with a bearer token"), ("assistant", "Understood.")]),
                                  sess("s10", [("user", "peers re-verify every segment on import"), ("assistant", "Right.")])],
            "answer_session_ids": [],
        },
    ]


def messages_of(question: dict[str, Any], roles: set[str]) -> list[dict[str, Any]]:
    """The haystack as ingest items: one per turn of the chosen roles, with
    the session's date as `ts` and the session id as `session`; the question
    id is the `run` (dedup key) so replays record nothing twice."""
    items: list[dict[str, Any]] = []
    sids = question.get("haystack_session_ids") or []
    dates = question.get("haystack_dates") or []
    for i, session in enumerate(question.get("haystack_sessions") or []):
        sid = sids[i] if i < len(sids) else f"session-{i}"
        ts = parse_date_ms(dates[i]) if i < len(dates) else None
        for j, turn in enumerate(session):
            role = turn.get("role", "user")
            if role not in roles:
                continue
            items.append({
                "body": turn.get("content", ""),
                "ts": (ts + j * 60_000) if ts is not None else None,
                "role": "user" if role == "user" else "agent",
                "session": sid,
                "run": question["question_id"],
            })
    return items


def parse_date_ms(s: str) -> int | None:
    # "2023/05/20 (Sat) 02:21" → unix ms
    try:
        head, _, tail = s.partition(" (")
        clock = tail.split(") ")[1] if ") " in tail else "00:00"
        dt = _dt.datetime.strptime(f"{head} {clock}", "%Y/%m/%d %H:%M").replace(tzinfo=_dt.timezone.utc)
        return int(dt.timestamp() * 1000)
    except Exception:
        return None


def content_hash(items: list[dict[str, Any]]) -> str:
    h = hashlib.sha256()
    for it in items:
        h.update((it.get("body") or "").encode())
    return h.hexdigest()[:12]
