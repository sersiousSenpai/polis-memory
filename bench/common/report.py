"""Render the joined table (every `*-longmemeval-*.json` in a results dir) as
markdown, one row per system, the columns `docs/bench.md` shows."""
from __future__ import annotations

import glob
import json
import os
import sys

from .schema import TYPES


def load_all(results_dir: str) -> list[dict]:
    rows = []
    for path in sorted(glob.glob(os.path.join(results_dir, "*-longmemeval-*.json"))):
        with open(path) as f:
            rows.append(json.load(f))
    return rows


def fmt(x) -> str:
    if x is None:
        return "—"
    if isinstance(x, float):
        return f"{x * 100:.1f}" if x <= 1.0 else f"{x:.1f}"
    return str(x)


def render(rows: list[dict]) -> str:
    short = {"single-session-user": "ss-user", "single-session-assistant": "ss-asst", "single-session-preference": "ss-pref",
             "temporal-reasoning": "temporal", "knowledge-update": "kn-update", "multi-session": "multi-sess"}
    head = ["system", "n", "overall"] + [short[t] for t in TYPES] + ["abstain", "write calls", "write tokens", "query p50 ms", "ctx tokens p50", "models", "date", "commit", "hardware", "note"]
    lines = ["| " + " | ".join(head) + " |", "|" + "---|" * len(head)]
    for r in rows:
        acc = {} if r.get("stub") else r.get("accuracy", {})
        by = acc.get("byType", {})
        cost = r.get("cost", {})
        note = ("STUB — reachability only, not a scored run" if r.get("stub") else "") + (f" PARTIAL: {r.get('partial_reason')}" if r.get("partial") else "")
        cells = [r.get("system"), r.get("n"), fmt(acc.get("overall"))] + [fmt(by.get(t)) for t in TYPES] + [
            fmt(acc.get("abstention")), cost.get("llmCallsWrite"), cost.get("tokensWrite"), f"{cost['queryP50Ms']:.2f}" if cost.get("queryP50Ms") is not None else "—",
            cost.get("contextTokensP50"), "/".join(f"{k}={v}" for k, v in (r.get("models") or {}).items()),
            r.get("date"), (r.get("commit") or "")[:7], r.get("hardware"), (note.strip() + " categories=" + str(r.get("accuracy", {}).get("categoryN", {}))),
        ]
        lines.append("| " + " | ".join(str(c) if c is not None else "—" for c in cells) + " |")
    return "\n".join(lines)


if __name__ == "__main__":
    d = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "..", "results")
    print(render(load_all(d)))


def paired_interval(left: dict, right: dict, *, seed=20260909, samples=4000) -> dict:
    """Paired bootstrap over identical held-out/full question sets; percentage points."""
    import random
    if left.get("stub") or right.get("stub"):
        raise ValueError("keyless fixtures are not answer-quality comparisons")
    for key in ("datasetSha256", "split", "questionIds", "contextTokenizer", "contextTokenCeiling"):
        if left.get("manifest", {}).get(key) != right.get("manifest", {}).get(key):
            raise ValueError(f"incomparable manifests: {key}")
    if left.get("manifest", {}).get("split") not in {"heldout", "full"}:
        raise ValueError("development questions cannot support release comparisons")
    if left.get("models") != right.get("models"):
        raise ValueError("answer/judge models differ")
    a = {q["id"]: q for q in left.get("questions", [])}
    b = {q["id"]: q for q in right.get("questions", [])}
    if not a or a.keys() != b.keys() or left.get("partial") or right.get("partial"):
        raise ValueError("paired release intervals require complete identical question sets")
    rng = random.Random(seed)
    def interval(ids):
        deltas = [int(a[id]["correct"]) - int(b[id]["correct"]) for id in ids]
        boot = sorted(sum(rng.choice(deltas) for _ in deltas) / len(deltas) * 100 for _ in range(samples))
        return {"n": len(deltas), "differencePp": sum(deltas) / len(deltas) * 100,
                "paired95Pp": [boot[int(samples * .025)], boot[min(samples - 1, int(samples * .975))]]}
    return {"method": "paired percentile bootstrap", "seed": seed, "samples": samples,
            "overall": interval(sorted(a)), "byType": {category: interval([id for id in sorted(a) if a[id]["type"] == category])
            for category in sorted({q["type"] for q in a.values()})}}
