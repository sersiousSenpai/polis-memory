"""Keyless regression fixtures: configuration, dates, isolation, usage and holds."""
import argparse
import importlib.util
import json
from pathlib import Path
import tempfile
import sqlite3
from contextlib import closing
import types
import unittest
from unittest.mock import patch

from bench.common.polis_mcp import PolisMcp
from bench.common.budget import AggregateBudget, BudgetExceeded
from bench.common.llm import Llm, Spend
from bench.common.manifest import partition
from bench.common.runner import common_args, run
from bench.common.report import paired_interval
from bench.common.schema import builtin_sample, messages_of
from bench.common.tokenizer import ContextTokenizer
from bench.baselines.run import Baseline


class HarnessContract(unittest.TestCase):
    def test_session_dates_and_roles_are_not_fabricated(self):
        q = dict(builtin_sample()[0], namespace="fresh:q1")
        rows = messages_of(q, {"user", "assistant"})
        self.assertEqual(rows[0]["ts"], rows[1]["ts"])
        self.assertEqual(rows[1]["role"], "assistant")
        self.assertEqual(rows[0]["run"], "fresh:q1")
        self.assertEqual(rows[1]["turnIndex"], 1)

    def test_aggregate_budget_spans_systems_and_failed_retries(self):
        with tempfile.TemporaryDirectory() as directory:
            path = directory + "/spend.json"
            one, two = AggregateBudget(path, 100), AggregateBudget(path, 100)
            token = one.reserve("polis:answer", 70)
            one.settle(token, 30, 10)
            two.reserve("mem0:extraction", 50)  # unknown outcome retains 50
            with self.assertRaises(BudgetExceeded): one.reserve("retry", 11)
            with self.assertRaises(ValueError): AggregateBudget(path, 200).reserve("x", 1)

    def test_stub_records_zero_actual_usage(self):
        spend = Spend()
        Llm("stub", "stub", spend).complete("answer", "system", "<context>hello</context>")
        self.assertEqual(spend.tokens, 0)
        self.assertEqual(spend.calls, 0)
        self.assertGreater(spend.simulated_tokens, 0)

    def test_heldout_is_disjoint_and_independent_of_input_order(self):
        qs = [{"question_id": str(i)} for i in range(100)]
        dev, held = partition(qs, "development", 7), partition(qs, "heldout", 7)
        self.assertFalse({q["question_id"] for q in dev} & {q["question_id"] for q in held})
        self.assertEqual(len(dev) + len(held), 100)
        self.assertEqual(set(q["question_id"] for q in held), set(q["question_id"] for q in partition(qs[::-1], "heldout", 7)))

    def test_final_context_ceiling_counts_rendered_unicode(self):
        tokenizer = ContextTokenizer("stub", "stub", True)
        out = tokenizer.trim("[role=assistant] " + "🌍" * 100, 45)
        self.assertLessEqual(tokenizer.count(out), 45)
        self.assertNotIn("�", out)

    def test_hold_precedes_any_backend_or_model(self):
        args = common_args(argparse.ArgumentParser(), "test").parse_args([])
        with patch("bench.common.runner.Llm", side_effect=AssertionError("model invoked")):
            with self.assertRaisesRegex(SystemExit, "HOLD"):
                run(args, lambda _: self.fail("backend invoked"), {"user"})

    def test_profiles_namespaces_manifests_and_no_bogus_cost(self):
        with tempfile.TemporaryDirectory() as directory:
            parser = common_args(argparse.ArgumentParser(), "raw-history")
            args = parser.parse_args(["--stub", "--split", "full", "--capture-profile", "all-roles", "--out", directory])
            args.baseline = "raw-history"
            first = run(args, Baseline, {"user"})
            a = json.loads(Path(first).read_text())
            second = run(args, Baseline, {"user"})
            b = json.loads(Path(second).read_text())
            self.assertEqual(a["manifest"]["captureProfile"], "all-roles")
            self.assertEqual(a["cost"]["llmCallsWrite"], 0)
            self.assertEqual(a["spend"]["calls"], 0)
            self.assertEqual(len(a["manifest"]["datasetSha256"]), 64)
            self.assertEqual(a["accuracy"]["categoryN"]["multi-session"], 1)
            self.assertFalse({q["namespace"] for q in a["questions"]} & {q["namespace"] for q in b["questions"]})

    def test_processing_profiles_really_index_and_drain_organization(self):
        with tempfile.TemporaryDirectory() as directory:
            with closing(sqlite3.connect(directory + "/polis.db")) as db:
                db.executescript("CREATE TABLE ledger_events(seq INTEGER); INSERT INTO ledger_events VALUES(2); "
                    "CREATE TABLE prompts(body TEXT); INSERT INTO prompts VALUES('one'),('two'); "
                    "CREATE TABLE embeddings(target_id INTEGER,target_kind TEXT,model TEXT); "
                    "INSERT INTO embeddings VALUES(1,'prompt','model2vec/potion-base-8M'),(2,'prompt','model2vec/potion-base-8M');")
            client = PolisMcp.__new__(PolisMcp)
            client.home, client.timeout, client.env = directory, 10, {}
            client.polis_bin, client.binary_hash, client.asset_hashes = "/fixture/polis", "binary", {"model": "pinned"}
            with patch("bench.common.polis_mcp.subprocess.run") as invoke:
                invoke.side_effect = [types.SimpleNamespace(stdout='{"provider":"model2vec","embedded":2}'),
                                      types.SimpleNamespace(stdout='{"seqTo":2,"ran":true}')]
                result = client.process("organized")
                self.assertEqual(result["indexCoverage"], 1)
                self.assertEqual(invoke.call_args_list[0].args[0][-2:], ["reindex", "--all"])
                self.assertEqual(invoke.call_args_list[1].args[0][-1], "organize")
            client.asset_hashes = {}
            with self.assertRaisesRegex(RuntimeError, "pinned local assets"):
                client.process("indexed")
            with patch("bench.common.polis_mcp.subprocess.run", side_effect=AssertionError("processing unexpectedly ran")):
                self.assertEqual(client.process("immediate")["processingProfile"], "immediate")

    def test_paired_uncertainty_rejects_configuration_drift(self):
        result = {"models": {"answer": "same", "judge": "same"},
                  "manifest": {"split": "heldout", "questionIds": ["a", "b"]},
                  "questions": [{"id": "a", "type": "temporal", "correct": True}, {"id": "b", "type": "multi-session", "correct": False}]}
        interval = paired_interval(result, result, samples=100)
        self.assertEqual(interval["overall"]["paired95Pp"], [0, 0])
        self.assertEqual(interval["byType"]["temporal"]["n"], 1)
        with self.assertRaises(ValueError):
            paired_interval(result, dict(result, stub=True))

    def test_schema_contracts_generated_from_live_source(self):
        path = Path(__file__).resolve().parents[2] / "sdk/schema/generate.py"
        spec = importlib.util.spec_from_file_location("schema_generator", path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        schema = module.generate()
        self.assertIn("AnswerPack", schema["$defs"])
        self.assertIn("scope", schema["$defs"]["SearchRequest"]["properties"])
        self.assertIn("includeShared", schema["$defs"]["Scope"]["properties"])
        self.assertEqual(schema, json.loads(module.OUTPUT.read_text()))


if __name__ == "__main__": unittest.main()
