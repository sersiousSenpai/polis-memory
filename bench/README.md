# Reproducible local-memory evaluation

**Paid external benchmarks remain on HOLD.** `bench/hold.json` is checked before
constructing a backend or model. The workflow runs keyless fixtures only; adding
a model key cannot activate a scored run. No competitor or leadership scores
have been produced by this change.

Three complementary suites are available/prepared:

| Suite | Location and status |
|---|---|
| Deterministic correctness/reachability | Rust scope, dense-class, temporal, forgetting, citation tests plus `bench/tests`: keyless regression gates. |
| Realistic Polis tasks | `realistic/tasks.json` supplies four development and four separate heldout coding-agent tasks with expected/forbidden evidence and citation checks. The LongMemEval-shaped built-in six remain pipeline fixtures. These synthetic cases are not real-user answer-quality evidence. |
| External answer quality | LongMemEval-100 iteration / 500 full comparison run specification and shared report format prepared; scored execution held. |

Capture and processing are independent settings:

- `--capture-profile user-only|all-roles` controls source roles. The legacy
  `polis-default` and `polis-full` labels only supply capture defaults.
- `--processing-profile immediate|indexed|organized` controls readiness. Immediate
  disables embeddings and does no background work. Indexed requires complete
  prompt-target coverage. Organized indexes and runs deterministic filing with
  no model CLI available. Capture and processing times/calls are separate.
- Indexed/organized require `--embedding-assets /path/to/models` containing
  `potion-base-8M/{model.safetensors,tokenizer.json,config.json}`. Every file must
  match the provider's committed SHA-256 and size pins before it is copied into
  the fresh question home. The runner never downloads assets.

```sh
python3 -m unittest discover -s bench/tests -v
cargo build -p polis-memory --features cli --locked
python3 bench/longmemeval/run.py --polis target/debug/polis --config polis-full \
  --capture-profile all-roles --processing-profile immediate --stub --split full
python3 bench/realistic/run.py --polis target/debug/polis --split all
python3 bench/baselines/run.py --baseline raw-history --stub --split full
python3 bench/baselines/run.py --baseline lexical --stub --split full
python3 bench/mem0/run.py --stub --split full
python3 bench/graphiti/run.py --stub --split full
python3 -m bench.common.report
```

The real local-model readiness fixture verifies semantic-only paraphrases through
MCP, then measures 50 captures during normal daemon indexing with actual HTTP
searches. Supply already-downloaded pinned assets; the runner makes no external
model calls and does not download files:

```sh
cargo build -p polis-memory --release --features cli --locked
python3 bench/baselines/local_model_readiness.py --polis target/release/polis \
  --assets /path/to/models --binary-profile 'release; features cli' \
  --output /path/to/local-readiness.json
```

Its manifest includes raw capture/readiness samples, asset and binary hashes,
source roles, exact-citation checks, and zero local LLM usage. The short normal-load
fixture does not measure overload, sustained backfill or answer-quality leadership.

The independent no-model filing measurement initializes ten fresh homes,
processes identity bookkeeping, then times filing 400 captured user/assistant
sources per sample. It includes CLI startup/store opening and checks every source
link, with no embeddings or LLM calls:

```sh
python3 bench/baselines/local_filing.py --polis target/release/polis \
  --binary-profile 'release; features cli' --output /path/to/local-filing.json
```

This measures deterministic inbox filing; it does not measure semantic
classification precision or model-backed filing.

Mem0/Graphiti stub modes are simple lexical fixtures, **not measurements of those
products**. Their real adapters are deliberately unavailable until extraction,
embedding and retry usage can be measured and empty external namespaces verified.
The former fabricated write-call/token estimates have been removed.

Every invocation writes `polis.bench/2` with a unique artifact ID and campaign
and question namespaces. A supplied campaign ID does not reuse question stores.
The manifest records normalized dataset SHA-256, exact question IDs, split,
package versions, Python/platform/CPU metadata, dirty worktree flag, model names,
processing/timeout settings, tokenizer and final context ceiling. Per-question
readiness includes asset hashes, indexed targets, processing time and failures.
Original session dates apply equally to all turns; no invented one-minute offsets
are added. Capture role and session relationships remain source metadata.

The final rendered context is bounded after every adapter. Keyless runs use a
conservative UTF-8 byte ceiling and label it as an upper bound, not actual tokens.
Scored OpenAI runs require the answer model's verified `tiktoken` encoding; a
scored Anthropic tokenizer integration remains outstanding. Stub simulated usage
is separate from zero actual model calls/tokens. Unknown provider usage is never
reported as zero.

When the hold is explicitly lifted, all systems/retries must share the same
`--budget-ledger` path and `--budget-tokens` value. The atomic ledger reserves a
conservative input/output ceiling before dispatch and settles against actual
provider usage; failed attempts retain their reservation. There is no blind
HTTP retry. This is an aggregate **token** limit, not a dollar cap; release
comparison also requires reviewed per-model prices and an explicit money budget.

Use `--split development` for tuning, `--split heldout` for held-out evidence,
and `--split full --subset 500` for declared full release comparisons. Selection
hashes question ID plus seed; expected answers never drive the partition. Publish
per-category sample sizes. `bench.common.report.paired_interval` computes seeded
paired bootstrap intervals and rejects mismatched datasets, question sets, models,
context ceilings, development runs, stubs and partial runs. Raw history and lexical
baselines share the answer/judge prompts and final-context ceiling. Capture and
processing profiles supply initial ablations; additional fusion/neighbor/claim
ablations remain to be measured before making architectural claims.
