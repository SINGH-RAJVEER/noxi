"""Validity and process-lifecycle checks for the differential timing runner."""

import json
import os
import sqlite3
from pathlib import Path
import sys
import tempfile
import time
import unittest

from fixtures import Operation, environment, make_tree, remove_tree, tree_digest
from process import Cgroup, build_launcher, run_process
from run import ROOT, paired_summary, verify


class OracleTests(unittest.TestCase):
	def result(self, root, stdout=b"42", stderr=b"", exit_code=0):
		(root / "stdout").write_bytes(stdout)
		(root / "stderr").write_bytes(stderr)
		return {"stdout_path": str(root / "stdout"), "stderr_path": str(root / "stderr"),
			"exit_code": exit_code, "timed_out": False, "launcher_ready": True, "unexpected_survivors": False}

	def test_fast_failure_and_wrong_forcing_are_never_success(self):
		with tempfile.TemporaryDirectory() as temp:
			root = Path(temp)
			fixture = type("Fixture", (), {"root": root})()
			op = Operation("nix-instantiate", [], {}, root, "json", {"nested": [1, 2, 3]})
			failed = verify(self.result(root, b"", b"parse error", 1), op, fixture, root / "state", "")
			self.assertEqual(failed["status"], "failed")
			unforced = verify(self.result(root, b'{"nested":null}'), op, fixture, root / "state", "")
			self.assertEqual(unforced["status"], "mismatch")
			valid = verify(self.result(root, b'{"nested":[1,2,3]}'), op, fixture, root / "state", "")
			self.assertEqual(valid["status"], "passed")

	def test_signal_and_surviving_processes_are_not_success(self):
		with tempfile.TemporaryDirectory() as temp:
			root = Path(temp)
			fixture = type("Fixture", (), {"root": root})()
			op = Operation("program", [], {}, root, "json", 42)
			result = self.result(root, exit_code=-9)
			self.assertEqual(verify(result, op, fixture, root, "")["status"], "failed")
			result = self.result(root)
			result["unexpected_survivors"] = True
			self.assertEqual(verify(result, op, fixture, root, "")["status"], "invalid_sample")

	def test_summary_requires_both_successful_sides(self):
		rows = [{"pair_id": "a", "role": "reference", "elapsed_s": 2, "correctness": {"status": "passed"}},
			{"pair_id": "a", "role": "candidate", "elapsed_s": 1, "correctness": {"status": "passed"}},
			{"pair_id": "b", "role": "reference", "elapsed_s": 2, "correctness": {"status": "failed"}},
			{"pair_id": "b", "role": "candidate", "elapsed_s": 0.001, "correctness": {"status": "passed"}}]
		rows += [{"pair_id": "preflight", "stage": "preflight", "role": role, "elapsed_s": 100, "correctness": {"status": "passed"}} for role in ["reference", "candidate"]]
		summary = paired_summary(rows, 1, bootstrap=100)
		self.assertEqual(summary["reference"]["successful_pairs"], 1)
		self.assertEqual(summary["reference"]["paired_geometric_speedup"], 2)
		self.assertTrue(summary["reference"]["exploratory"])

	def test_failure_cannot_leave_an_absent_output_registered(self):
		with tempfile.TemporaryDirectory() as temp:
			root = Path(temp)
			fixture = type("Fixture", (), {"root": root})()
			path = "/nix/store/00000000000000000000000000000000-output"
			database = root / "nix/var/nix/db/db.sqlite"
			database.parent.mkdir(parents=True)
			with sqlite3.connect(database) as db:
				db.execute("CREATE TABLE ValidPaths (path TEXT)")
				db.execute("INSERT INTO ValidPaths VALUES (?)", (path,))
			op = Operation("nix-store", [], {}, root, "failure", expected_exit=1, root=root, paths=[path])
			result = self.result(root, b"", b"signature rejected", 1)
			self.assertEqual(verify(result, op, fixture, root, "")["status"], "mismatch")
			with sqlite3.connect(database) as db:
				db.execute("DELETE FROM ValidPaths")
			self.assertEqual(verify(result, op, fixture, root, "")["status"], "passed")

	def test_fixture_identity_includes_content_modes_and_links(self):
		with tempfile.TemporaryDirectory() as temp:
			root = Path(temp)
			a = make_tree(root / "a", 20, 1024, 17)
			b = make_tree(root / "b", 20, 1024, 17)
			self.assertEqual(a, b)
			file = root / "b/d00/f000000"
			file.chmod(0o644)
			self.assertNotEqual(tree_digest(root / "b"), a["tree_sha256"])

	def test_cleanup_handles_canonical_readonly_directories(self):
		with tempfile.TemporaryDirectory() as temp:
			root = Path(temp) / "owned"
			(root / "nested").mkdir(parents=True)
			(root / "nested/data").write_text("data")
			(root / "nested").chmod(0o555)
			root.chmod(0o555)
			remove_tree(root)
			self.assertFalse(root.exists())


class ProcessTests(unittest.TestCase):
	@classmethod
	def setUpClass(cls):
		try:
			with Cgroup():
				pass
		except (RuntimeError, OSError) as error:
			raise unittest.SkipTest(str(error))
		cls.launcher = build_launcher(ROOT)

	def test_child_cpu_memory_and_streams_are_accounted(self):
		with tempfile.TemporaryDirectory() as temp:
			root = Path(temp)
			child_code = 'data = bytearray(32 * 1024 * 1024); print(sum(range(1000000)))'
			code = f'import subprocess, sys; subprocess.run([sys.executable, "-c", {child_code!r}], check=True)'
			result = run_process(self.launcher, [sys.executable, "-c", code], environment(root / "home"), root, root / "output", 10)
			self.assertEqual(result["exit_code"], 0)
			self.assertGreater(result["resources"]["process_tree_peak_bytes"], 32 * 1024 * 1024)
			self.assertGreater(result["resources"]["cpu_total_s"], 0)
			self.assertGreater(result["resources"]["target_wait4_max_process_rss_bytes"], 32 * 1024 * 1024)
			self.assertEqual(Path(result["stdout_path"]).read_text().strip(), "499999500000")
			self.assertFalse(result["unexpected_survivors"])

	def test_timeout_kills_child_that_escapes_process_group(self):
		with tempfile.TemporaryDirectory() as temp:
			root = Path(temp)
			marker = root / "escaped-pid"
			code = f'import subprocess,sys,time; p=subprocess.Popen([sys.executable,"-c","import time; time.sleep(60)"],start_new_session=True); open({str(marker)!r},"w").write(str(p.pid)); time.sleep(60)'
			result = run_process(self.launcher, [sys.executable, "-c", code], environment(root / "home"), root, root / "output", 0.3)
			self.assertTrue(result["timed_out"])
			self.assertLess(result["elapsed_s"], 2)
			pid = int(marker.read_text())
			stat = Path(f"/proc/{pid}/stat")
			if stat.exists():
				self.assertEqual(stat.read_text().split(") ")[1].split()[0], "Z")

	def test_stdin_file_is_not_buffered_into_command(self):
		with tempfile.TemporaryDirectory() as temp:
			root = Path(temp)
			input_ = root / "input"
			input_.write_bytes(b"payload" * 1000)
			result = run_process(self.launcher, [sys.executable, "-c", "import sys; print(len(sys.stdin.buffer.read()))"], environment(root / "home"), root, root / "output", 10, stdin=input_)
			self.assertEqual(Path(result["stdout_path"]).read_text().strip(), "7000")

	def test_target_rss_does_not_inherit_large_fixture_generator(self):
		with tempfile.TemporaryDirectory() as temp:
			root = Path(temp)
			# Keep the parent resident allocation alive across launcher creation.
			payload = bytearray(96 * 1024 * 1024)
			result = run_process(self.launcher, [sys.executable, "-c", "print(42)"], environment(root / "home"), root, root / "output", 10)
			self.assertEqual(len(payload), 96 * 1024 * 1024)
			self.assertLess(result["resources"]["target_wait4_max_process_rss_bytes"], 48 * 1024 * 1024)


if __name__ == "__main__":
	unittest.main()
