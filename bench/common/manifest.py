"""Reproducibility and development/held-out partition contracts."""
import hashlib
import importlib.metadata
import json
import os
import platform
import subprocess
import uuid


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()).hexdigest()


def partition(questions, split, seed):
    # Assignment depends on ID, never question content or gold answers.
    def held(q):
        return int(hashlib.sha256(f"{seed}:{q['question_id']}".encode()).hexdigest()[:8], 16) % 5 == 0
    if split == "full":
        return list(questions)
    return [q for q in questions if held(q) == (split == "heldout")]


def build_manifest(args, dataset, questions, tokenizer):
    packages = {}
    for name in ("mem0ai", "graphiti-core", "tiktoken", "huggingface-hub"):
        try:
            packages[name] = importlib.metadata.version(name)
        except importlib.metadata.PackageNotFoundError:
            packages[name] = None
    dirty = subprocess.run(["git", "status", "--porcelain"], capture_output=True, text=True).stdout.strip()
    return {
        "runId": args.run_id, "invocationId": uuid.uuid4().hex, "datasetSha256": digest(dataset),
        "questionIds": [q["question_id"] for q in questions], "split": args.split,
        "splitSeed": args.seed, "captureProfile": args.capture_profile,
        "processingProfile": args.processing_profile, "contextTokenizer": tokenizer.name,
        "contextTokenCeiling": args.max_context_tokens, "timeoutSeconds": args.timeout,
        "packages": packages, "python": platform.python_version(), "platform": platform.platform(),
        "cpuCount": os.cpu_count(), "dirtyWorktree": bool(dirty),
        "sourceDatePolicy": "session timestamp shared by its turns; original date retained; no invented offsets",
        "usagePolicy": "provider-reported tokens only; unknown attempts reserve ceiling and stop",
        "scoreMeaning": "keyless pipeline fixture, not answer-quality evidence" if args.stub or args.provider == "stub" else "external answer quality",
        "ablations": {"capture": args.capture_profile, "processing": args.processing_profile},
    }
