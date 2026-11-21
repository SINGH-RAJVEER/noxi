import importlib.util
import json
import pathlib
import tempfile
import unittest


spec = importlib.util.spec_from_file_location("compat_results", pathlib.Path(__file__).resolve().parents[2] / "scripts/compat-results.py")
results = importlib.util.module_from_spec(spec)
spec.loader.exec_module(results)

JUNIT = """<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="3" failures="1">
	<testsuite name="noxi-compat-tests::upstream_nars" tests="1">
		<testcase name="reads_all" classname="noxi-compat-tests::upstream_nars" time="0.5"/>
	</testsuite>
	<testsuite name="noxi-hash" tests="2">
		<testcase name="blake3" classname="noxi-hash" time="0.1">
			<failure message="assertion failed" type="test failure"/>
			<system-out>left: 1</system-out>
		</testcase>
		<testcase name="later" classname="noxi-hash" time="0"><skipped/></testcase>
	</testsuite>
</testsuites>
"""

ROWS = {
	"nar-archive-format": ["noxi-compat-tests::upstream_nars"],
	"hash-algorithms": ["noxi-hash"],
	"nixos-and-home-manager-evaluation": ["noxi-commands::instantiate matches_reference_nixos"],
	"cli-and-configuration": ["noxi-commands::instantiate"],
}


class CompatResultsTests(unittest.TestCase):
	def test_rows_prefer_test_then_binary_then_package(self):
		self.assertEqual(results.row_for("noxi-commands::instantiate", "matches_reference_nixos_paths", ROWS), "nixos-and-home-manager-evaluation")
		self.assertEqual(results.row_for("noxi-commands::instantiate", "evaluates", ROWS), "cli-and-configuration")
		self.assertEqual(results.row_for("noxi-hash", "anything", ROWS), "hash-algorithms")
		self.assertIsNone(results.row_for("noxi-store", "anything", ROWS))

	def test_junit_cases_become_records_with_verdicts(self):
		with tempfile.TemporaryDirectory() as directory:
			report = pathlib.Path(directory) / "junit.xml"
			report.write_text(JUNIT)
			records = results.junit_records(report, "fast", {"command": ["cargo", "nextest"]}, ROWS)
		by_case = {record["case"]: record for record in records}
		passed = by_case["nextest:noxi-compat-tests::upstream_nars:reads_all"]
		self.assertEqual((passed["verdict"], passed["matrix_row"], passed["tier"]), ("pass", "nar-archive-format", "fast"))
		failed = by_case["nextest:noxi-hash:blake3"]
		self.assertEqual((failed["verdict"], failed["reason"]), ("fail", "assertion failed"))
		self.assertEqual(failed["test"]["output"]["text"], "left: 1")
		self.assertEqual(by_case["nextest:noxi-hash:later"]["verdict"], "unrun")
		self.assertTrue(all(record["schema"] == results.SCHEMA for record in records))

	def test_summary_counts_rows_and_rejects_unknown_verdicts(self):
		with tempfile.TemporaryDirectory() as directory:
			directory = pathlib.Path(directory)
			record = {"schema": results.SCHEMA, "case": "r:a", "runner": "r", "tier": "fast", "matrix_row": "hash-algorithms", "verdict": "pass", "run": {"noxi": {"revision": "abc", "dirty": False}}}
			failing = dict(record, case="r:b", verdict="fail", reason="differs")
			results.write_jsonl(directory / "r.jsonl", [record, failing, dict(record, case="r:c", matrix_row=None, verdict="unrun")])
			summary = results.summarize(directory)
			self.assertEqual(summary["rows"]["hash-algorithms"], {"pass": 1, "fail": 1, "known-difference": 0, "unrun": 0})
			self.assertEqual(summary["rows"]["unmapped"]["unrun"], 1)
			self.assertEqual(summary["failures"], [{"case": "r:b", "file": "r.jsonl", "reason": "differs"}])
			self.assertEqual(summary["runners"]["r"]["noxi"], {"revision": "abc", "dirty": False})
			self.assertTrue((directory / "summary.json").exists())
			results.write_jsonl(directory / "r.jsonl", [dict(record, verdict="maybe")])
			with self.assertRaises(ValueError):
				results.summarize(directory)

	def test_qualification_steps_become_system_records(self):
		with tempfile.TemporaryDirectory() as directory:
			directory = pathlib.Path(directory)
			(directory / "manifest.json").write_text(json.dumps({"schema": 3, "revision": "b6a0", "guest_command_timeout_seconds": 1800}))
			(directory / "nixos.json").write_text(json.dumps({"schema": 2, "mode": "nixos", "expected": {"reference": "/nix/store/x-nix-2.35.2"}, "steps": [
				{"name": "boot", "status": "pass", "command": "true", "user": "root", "exit_status": "0", "stdout": "", "stderr": ""},
				{"name": "switch", "status": "blocked", "command": "x", "detail": "earlier step failed"},
			]}))
			records = results.qualification_records(directory, {"nixos": "nixos-activation"})
		self.assertEqual([record["verdict"] for record in records], ["pass", "unrun"])
		self.assertEqual(records[0]["case"], "qualification-nixos:boot")
		self.assertEqual((records[0]["tier"], records[0]["matrix_row"]), ("system", "nixos-activation"))
		self.assertEqual(records[0]["run"]["noxi"]["revision"], "b6a0")
		self.assertEqual(records[0]["engines"]["noxi"]["exit_code"], 0)
		self.assertEqual(records[1]["reason"], "earlier step failed")

	def test_matrix_rows_and_evidence_check(self):
		with tempfile.TemporaryDirectory() as directory:
			directory = pathlib.Path(directory)
			matrix = directory / "matrix.md"
			matrix.write_text("| Field | x |\n|---|---|\n| a | b |\n\n| Area | Current status | Evidence |\n|---|---|---|\n| Hash algorithms | ok | more |\n| C and C++ libraries | Unrun | all |\n\nafter\n")
			rows = results.matrix_rows(matrix)
			self.assertEqual(rows, [("hash-algorithms", "Hash algorithms"), ("c-and-c-libraries", "C and C++ libraries")])
			kept = directory / "kept"
			(kept / "set").mkdir(parents=True)
			(kept / "set/summary.json").write_text(json.dumps({"run": {"tier": "fast"}, "runners": {"nextest": {"noxi": {"revision": "abc", "dirty": False}}}, "rows": {"hash-algorithms": {"pass": 2, "fail": 0, "known-difference": 0, "unrun": 1}}}))
			mapping = {"hash-algorithms": ["noxi-hash"], "c-and-c-libraries": []}
			text = results.evidence_markdown(mapping, rows, kept)
			self.assertIn("| Hash algorithms | 1 | [`set`](../tests/compat-results/set/) | 2 | 0 | 0 | 1 |", text)
			self.assertIn("| C and C++ libraries | none | no recorded results | | | | |", text)
			evidence = directory / "evidence.md"
			problems = results.check_evidence({"hash-algorithms": []}, rows, kept, evidence)
			self.assertIn("matrix row c-and-c-libraries has no entry in rows.toml", problems)
			self.assertTrue(any("stale" in problem for problem in problems))

	def test_repository_rows_cover_the_matrix(self):
		rows, modes = results.load_rows()
		ids = {row for row, _ in results.matrix_rows()}
		self.assertEqual(set(rows), ids)
		self.assertTrue(set(modes.values()) <= ids)


if __name__ == "__main__":
	unittest.main()
