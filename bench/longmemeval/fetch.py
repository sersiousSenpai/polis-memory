#!/usr/bin/env python3
"""Download LongMemEval-S (HF `xiaowu0162/longmemeval-cleaned`) to a local
JSON the runners take with `--data`. Needs `huggingface_hub` (requirements.txt).

  python3 bench/longmemeval/fetch.py --out /tmp/longmemeval_s.json
"""
import argparse
import json
import os
import sys


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("--repo", default="xiaowu0162/longmemeval-cleaned")
    p.add_argument("--file", default="longmemeval_s_cleaned.json")
    p.add_argument("--out", required=True)
    a = p.parse_args()
    try:
        from huggingface_hub import hf_hub_download  # type: ignore
    except ImportError:
        sys.exit("pip install huggingface_hub")
    path = hf_hub_download(repo_id=a.repo, filename=a.file, repo_type="dataset")
    with open(path) as f:
        data = json.load(f)
    with open(a.out, "w") as f:
        json.dump(data, f)
    print(f"{len(data)} questions → {a.out}", file=sys.stderr)


if __name__ == "__main__":
    main()
