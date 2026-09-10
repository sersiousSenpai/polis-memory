"""Run after installing sdk/python and starting polis serve.
POLIS_TOKEN and POLIS_PRINCIPAL come from the local daemon's identity/config.
"""
import os
from polis_memory import Client, Scope

base = os.environ.get("POLIS_URL", "http://127.0.0.1:7677")
principal = os.environ["POLIS_PRINCIPAL"]
token = os.environ["POLIS_TOKEN"]
writer = Client(base, token=token, scope=Scope(principal=principal, agent="planner"))
reader = Client(base, token=token, scope=Scope(principal=principal))
receipt = writer.ingest([{"body": "We decided to use SQLite for the project database.", "role": "user"}],
                        idempotency_key="two-agents-database-v1")
if receipt["recorded"]:
    writer.decide(receipt["recorded"][0])
context = reader.context("Which database did we decide to use?", trace_id="two-agents-read")
print(context)
trace_id = context.get("retrieval", {}).get("traceId")
if trace_id:
    print(reader.traces(id=trace_id))
