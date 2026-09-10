# Polis HTTP clients

These dependency-free clients are implemented and tested locally. They have **not**
been published to PyPI/npm. Install from this checkout:

```sh
python3 -m pip install ./sdk/python
npm install ./sdk/typescript
```

Run `polis init`, then `polis serve`. Read its local bearer token from
`$POLIS_HOME/token` (default `~/.polis/token`) and supply it to the client.
Never place that token in source control. The daemon authenticates reads and
writes. A principal selects memory; it is not a replacement for authentication.

```python
from polis_memory import Client, Scope

writer = Client(token=token, scope=Scope(principal=principal, agent="planner"))
reader = Client(token=token, scope=Scope(principal=principal))
receipt = writer.ingest(
    [{"body": "We decided to use SQLite for the project database.", "role": "user"}],
    idempotency_key="database-decision-1",
)
if receipt["recorded"]:
    writer.decide(receipt["recorded"][0])
print(reader.context("Which database did we decide to use?", trace_id="database-check"))
```

The second agent leaves `scope.agent` unset when reading the principal's shared
memory: specifying the first or second agent is an evidence filter, not a label
for the caller. `sdk/examples/two_agents.py` and `.mjs` implement this scenario.

Caller-supplied `trace_id`/`traceId` is a correlation prefix. Use the returned
`context.retrieval.traceId` or `pack.retrieval.traceId` for exact trace lookup.

Both clients propagate scope and retrieval trace IDs, offer typed errors and configurable
timeouts, and expose capture, decision writes, temporal claims, retrieval,
evidence lookup, explicit forgetting and traces. TypeScript uses asynchronous `fetch` with abort and
caller cancellation. Python provides sync methods and `AsyncClient` via bounded
socket calls in worker threads; cancelling a coroutine does not cancel an active
worker. No client automatically retries writes.

Forgetting requires the literal confirmation word and defaults to a returned
ledger citation sequence, avoiding confusion with internal source row IDs:

```python
reader.forget(source_seq, confirm="forget")
```

```javascript
await reader.forget(sourceSeq, {confirm: 'forget'});
```

The default `ledger_event` target resolves a returned citation to its prompt,
page or standalone annotation source. When using an internal note row ID instead,
specify `target_kind="note"` / `targetKind: 'note'`; `user_note` is an alias.
`prompt` and `browse_event` likewise accept source row IDs, not ledger sequences.
Scope and confirmation propagate with every request, including async calls.

```python
reader.forget(note_row_id, target_kind="note", confirm="forget")
```

```javascript
await reader.forget(noteRowId, {targetKind: 'user_note', confirm: 'forget'});
```

The SDK wire tests verify confirmation and target/scope propagation. For lifecycle,
peer redaction and restore validation, consult the server/store test results for
this revision.

Ingestion idempotency uses the server's body/run/role/project/principal/device/agent/org namespace key. Supply the same
`idempotency_key`/`idempotencyKey` for each retry of the same operation. Different
item run values are rejected; if `scope.run` is set it must equal the operation key. Repeated identical bodies within a key can collapse;
this API does not promise an arbitrary HTTP `Idempotency-Key` header contract.

`GET /v1/memory/schema` publishes the JSON contracts, including all AnswerPack
and claim/evidence/trace dependencies. The constrained generator reads Rust
serde source and rejects unsupported forms. It handles current structs, unit and
adjacently tagged enums, flattening, maps, optionals and rename/default attributes.
It is not a general Rust parser. GET query mappings are explicit; JSON uses
camelCase while query parameters use snake_case and flat scope/filter fields.

```sh
python3 sdk/schema/generate.py
python3 sdk/schema/types.py
python3 sdk/schema/generate.py --check
python3 sdk/schema/types.py --check
python3 -m unittest discover -s sdk/python/tests -v
node --test sdk/typescript/test/*.test.mjs
```

The Python tests use a real loopback HTTP fixture. TypeScript tests use an
injected fetch transport to verify wire requests, typed failures and cancellation.
The Rust server/MCP suites verify the actual store behavior. The five-minute
clean-install scenario across macOS/Linux/Windows remains an acceptance target.
