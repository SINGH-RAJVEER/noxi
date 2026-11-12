"""Mechanism reports must contain measurements as well as correct Nix output."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("eval_profile", Path(__file__).with_name("eval-profile.py"))
profile = importlib.util.module_from_spec(spec)
spec.loader.exec_module(profile)


class CounterReportTests(unittest.TestCase):
	def test_missing_and_malformed_counters_never_pass(self):
		with tempfile.TemporaryDirectory() as temp:
			path = Path(temp) / "counters.json"
			for mode in ["diagnostics", "allocation"]:
				self.assertFalse(profile.validate_counters(mode, path)[0])
				for text in ["{", "[]", "{}"]:
					path.write_text(text)
					self.assertFalse(profile.validate_counters(mode, path)[0])
				path.unlink()

	def test_diagnostic_schema_requires_executed_work(self):
		with tempfile.TemporaryDirectory() as temp:
			path = Path(temp) / "counters.json"
			value = {"schema_version": 1, "scope": "current_thread_since_reset", "opcodes": {"0": 1}, "opcode_pairs": {}, "frame_transitions": {}, "thunk_creations": {}, "layouts": {"Value": 16}}
			path.write_text(json.dumps(value))
			self.assertTrue(profile.validate_counters("diagnostics", path)[0])
			value["opcodes"] = {}
			path.write_text(json.dumps(value))
			self.assertFalse(profile.validate_counters("diagnostics", path)[0])

	def test_allocation_schema_does_not_accept_evaluator_counters(self):
		with tempfile.TemporaryDirectory() as temp:
			path = Path(temp) / "counters.json"
			path.write_text(json.dumps({"schema_version": 1, "scope": "current_thread_since_reset"}))
			self.assertFalse(profile.validate_counters("allocation", path)[0])

	def test_allocation_report_requires_numeric_measurements(self):
		with tempfile.TemporaryDirectory() as temp:
			path = Path(temp) / "counters.json"
			fields = ["allocation_count", "allocation_failure_count", "allocation_requested_bytes", "reallocation_count", "reallocation_failure_count", "reallocation_old_requested_bytes", "reallocation_new_requested_bytes", "deallocation_count", "deallocation_requested_bytes", "current_live_requested_bytes", "peak_live_requested_bytes"]
			value = dict.fromkeys(fields, 0)
			value.update(schema=1, scope="process_requested_allocation_bytes", allocator="mimalloc", diagnostic_overhead=True, allocation_count=1, allocation_requested_bytes=16, current_live_requested_bytes=16, peak_live_requested_bytes=16)
			path.write_text(json.dumps(value))
			self.assertTrue(profile.validate_counters("allocation", path)[0])
			value["allocation_count"] = True
			path.write_text(json.dumps(value))
			self.assertFalse(profile.validate_counters("allocation", path)[0])


if __name__ == "__main__":
	unittest.main()
