# bench/ — the measurement program's harness

`docs/bench.md` states the budgets, the baseline and the kill criterion;
this directory is what produces the numbers.

| Path | What |
|---|---|
| `results/` | committed results files (`baseline.json` is CI's eval gate; dated files are the record of a measured day) |
| `common/` | shared by every runner: the `polis mcp` stdio client, the LLM client (Anthropic / OpenAI-compatible / stub) with the spend cap, the LongMemEval judge templates, the result schema + stratified subset, the table renderer |
| `longmemeval/run.py` | LongMemEval-S over Polis (`--config polis-default` / `polis-full`), one fresh `POLIS_HOME` per question over MCP stdio |
| `longmemeval/fetch.py` | downloads the dataset from HF to a local JSON |
| `mem0/run.py` | the same questions over Mem0 OSS (v3 line) — its write-path model calls are counted |
| `graphiti/run.py`, `graphiti/docker-compose.yml` | the same over Graphiti with FalkorDB |
| `../.github/workflows/bench.yml` | nightly 100-question job under a spend cap; skips without a key; uploads results, never commits numbers |

## Run it

```sh
cargo build --release -p polis-memory --features cli        # the binary the runner spawns
export POLIS_BIN=target/release/polis

# Keyless pipeline check (deterministic stubs; reachability only, no scores):
python3 bench/longmemeval/run.py --config polis-default --stub
python3 bench/longmemeval/run.py --config polis-full --stub
python3 bench/mem0/run.py --stub
python3 bench/graphiti/run.py --stub
python3 -m bench.common.report            # the joined table

# A scored run (spends money; the runner stops at --budget-tokens and marks the table partial):
pip install -r bench/requirements.txt
python3 bench/longmemeval/fetch.py --out /tmp/longmemeval_s.json
export ANTHROPIC_API_KEY=…                # or OPENAI_API_KEY with --provider openai
python3 bench/longmemeval/run.py --config polis-full --data /tmp/longmemeval_s.json --subset 100 \
    --provider anthropic --answer-model claude-sonnet-5 --judge-model claude-sonnet-5 --budget-tokens 6000000
docker compose -f bench/graphiti/docker-compose.yml up -d
python3 bench/graphiti/run.py --data /tmp/longmemeval_s.json --subset 100 --provider anthropic …
docker compose -f bench/graphiti/docker-compose.yml down -v
```

Every results file is `polis.bench/1`: system, subset, seed, stub flag,
commit, date, hardware, models, per-type accuracy, the cost columns, the
spend, and every question's verdict. `--limit N` runs the first N of the
subset for a smoke.

## The MCP round-trip row

`cargo build -p polis-memory --features cli && cargo test -p polis-memory --features eval -- --ignored eval_mcp_roundtrip --nocapture`
seeds B1's 10k synthetic corpus into a real home and times `memory_context`
over `polis mcp`'s stdio, end to end.

## Rules

- Nothing here publishes a number: a row reaches `docs/bench.md` by hand,
  next to the kill criterion, with commit / models / date / hardware.
- The judge templates and the answer prompt are byte-stable across the runs
  a table compares; change them and every row is stale.
- Competitors run under identical conditions or not at all.
