"""Whole-run host validity checks, independent of command correctness."""

import unittest

from host import delta, verdict


class HostTests(unittest.TestCase):
	def snapshot(self, time, stall=0, throttled=0):
		return {"recorded_monotonic_s": time, "affinity": [0], "pressure": {"memory": f"full avg10=0 total={stall}"},
			"cgroup_hierarchy": {"/parent/leaf": {"memory.high": "max", "memory.max": "max", "memory.current": "20", "cpu.stat": f"nr_throttled {throttled}"},
				"/parent": {"memory.high": "100", "memory.max": "150", "memory.current": "90", "memory.pressure": "full avg10=0 total=0", "cpu.stat": "nr_throttled 0"}}}

	def policy(self):
		return {"max_memory_full_fraction": 0.01, "max_nr_throttled_delta": 0, "expected_affinity": [0]}

	def test_finite_ancestor_limit_is_visible_with_unlimited_leaf(self):
		value = delta(self.snapshot(1), self.snapshot(11))
		self.assertEqual(value["effective_memory_high_bytes"], 100)
		self.assertEqual(len(value["finite_memory_limits"]), 2)
		self.assertEqual(verdict(self.snapshot(1), self.snapshot(11), self.policy(), [])["status"], "passed")

	def test_pressure_is_whole_run_delta_and_does_not_filter_samples(self):
		value = verdict(self.snapshot(1), self.snapshot(11, 200000), self.policy(), [])
		self.assertEqual(value["status"], "invalid")
		self.assertEqual(value["metrics"]["memory_full_fraction"], 0.02)
		self.assertEqual(verdict(self.snapshot(1), self.snapshot(11, 100000), self.policy(), [])["status"], "passed")

	def test_throttling_and_sample_affinity_invalidate_lane(self):
		self.assertEqual(verdict(self.snapshot(1), self.snapshot(11, throttled=1), self.policy(), [])["status"], "invalid")
		self.assertEqual(verdict(self.snapshot(1), self.snapshot(11), self.policy(),
			[{"host_resources": {"affinity_before": [1], "affinity_after": [0]}}])["status"], "invalid")

	def test_ancestor_pressure_invalidates_even_when_system_pressure_is_zero(self):
		before, after = self.snapshot(1), self.snapshot(11)
		before["cgroup_hierarchy"]["/parent"]["memory.pressure"] = "full avg10=0 total=0"
		after["cgroup_hierarchy"]["/parent"]["memory.pressure"] = "full avg10=0 total=200000"
		self.assertEqual(verdict(before, after, self.policy(), [])["status"], "invalid")

	def test_counter_regression_cannot_pass(self):
		self.assertEqual(verdict(self.snapshot(1, 100), self.snapshot(11), self.policy(), [])["status"], "invalid")
		self.assertEqual(verdict(self.snapshot(1, throttled=1), self.snapshot(11), self.policy(), [])["status"], "invalid")

	def test_ancestor_fault_reclaim_and_throttled_time_deltas(self):
		before, after = self.snapshot(1), self.snapshot(11)
		for snap, count in [(before, 10), (after, 15)]:
			parent = snap["cgroup_hierarchy"]["/parent"]
			parent["memory.stat"] = f"pgfault {count}\npgmajfault {count}\npgscan_direct {count}\npgsteal_direct {count}\nanon 100"
			parent["cpu.stat"] += f"\nthrottled_usec {count}"
		value = delta(before, after)
		self.assertEqual(value["ancestor_reclaim_fault_deltas"]["/parent"],
			{"pgfault": 5, "pgmajfault": 5, "pgscan_direct": 5, "pgsteal_direct": 5})
		self.assertEqual(value["ancestor_throttled_usec_deltas"]["/parent"], 5)
		self.assertIsNone(value["ancestor_throttled_usec_deltas"]["/parent/leaf"])

	def test_observe_policy_retains_pressure_but_rejects_cap_events(self):
		before, after = self.snapshot(1), self.snapshot(11, 9000000)
		policy = {**self.policy(), "memory_pressure_policy": "observe", "reject_memory_events": ["high", "max", "oom", "oom_kill"]}
		for snap in [before, after]:
			for values in snap["cgroup_hierarchy"].values():
				values["memory.events"] = "high 0\nmax 0\noom 0\noom_kill 0"
		self.assertEqual(verdict(before, after, policy, [])["status"], "passed")
		after["cgroup_hierarchy"]["/parent"]["memory.events"] = "high 1\nmax 0\noom 0\noom_kill 0"
		self.assertEqual(verdict(before, after, policy, [])["status"], "invalid")
		del after["cgroup_hierarchy"]["/parent"]["memory.events"]
		self.assertEqual(verdict(before, after, policy, [])["status"], "invalid")

	def test_unknown_pressure_policy_fails_closed(self):
		self.assertEqual(verdict(self.snapshot(1), self.snapshot(11),
			{**self.policy(), "memory_pressure_policy": "typo"}, [])["status"], "invalid")

	def test_required_target_accounting_cannot_be_omitted(self):
		policy = {**self.policy(), "require_target_accounting": True}
		self.assertEqual(verdict(self.snapshot(1), self.snapshot(11), policy,
			[{"host_resources": {"affinity_before": [0], "affinity_after": [0]}}])["status"], "invalid")

	def test_every_expected_target_path_requires_events_and_throttling(self):
		policy = {**self.policy(), "require_target_accounting": True,
			"reject_memory_events": ["high", "max", "oom", "oom_kill"]}
		before, after = self.snapshot(1), self.snapshot(11)
		for snap in [before, after]:
			for values in snap["cgroup_hierarchy"].values():
				values["memory.events"] = "high 0\nmax 0\noom 0\noom_kill 0"
		valid = {"memory_accounting_paths": ["/target"],
			"ancestor_memory_event_deltas": {"/target": dict.fromkeys(policy["reject_memory_events"], 0)},
			"ancestor_nr_throttled_deltas": {"/target": 0}}
		import copy
		self.assertEqual(verdict(before, after, policy, [{"target_host_resources": valid}])["status"], "passed")
		for field in ["ancestor_memory_event_deltas", "ancestor_nr_throttled_deltas"]:
			for missing in ["dictionary", "path", "counter"]:
				broken = copy.deepcopy(valid)
				if missing == "dictionary":
					del broken[field]
				elif missing == "path":
					broken[field] = {}
				elif field == "ancestor_memory_event_deltas":
					broken[field]["/target"]["high"] = None
				else:
					broken[field]["/target"] = None
				with self.subTest(field=field, missing=missing):
					self.assertEqual(verdict(before, after, policy, [{"target_host_resources": broken}])["status"], "invalid")
