"""Read host resource constraints and evaluate a predeclared whole-run lane policy."""

import os
import hashlib
from pathlib import Path
import time


VALIDATOR_SOURCE_SHA256 = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()


def read(path):
	try:
		return Path(path).read_text().strip()
	except OSError:
		return None


def snapshot(cgroup_path: Path | None = None) -> dict:
	hierarchy = {}
	path = cgroup_path or Path("/sys/fs/cgroup") / Path("/proc/self/cgroup").read_text().split(":", 2)[2].strip().lstrip("/")
	while path.is_relative_to("/sys/fs/cgroup"):
		hierarchy[str(path)] = {name: read(path / name) for name in
			["memory.current", "memory.high", "memory.max", "memory.events", "memory.events.local", "memory.stat", "memory.pressure", "cpu.max", "cpu.stat"]}
		if path == Path("/sys/fs/cgroup"):
			break
		path = path.parent
	return {"recorded_monotonic_s": time.monotonic(), "affinity": sorted(os.sched_getaffinity(0)),
		"cgroup_hierarchy": hierarchy, "pressure": {name: read(f"/proc/pressure/{name}") for name in ["cpu", "memory", "io"]}}


def field(text: str | None, name: str) -> int | None:
	for line in (text or "").splitlines():
		parts = line.split()
		if parts and parts[0] == name:
			return int(parts[1])
	return None


def full_total(text: str | None) -> int | None:
	for line in (text or "").splitlines():
		if line.startswith("full "):
			return int(next(value.split("=", 1)[1] for value in line.split() if value.startswith("total=")))
	return None


def delta(before: dict, after: dict) -> dict:
	elapsed = after["recorded_monotonic_s"] - before["recorded_monotonic_s"]
	first, last = [full_total(value.get("pressure", {}).get("memory")) for value in [before, after]]
	stall = None if first is None or last is None else last - first
	hierarchy = after.get("cgroup_hierarchy", {})
	throttling = {}
	ancestor_pressure = {}
	memory_events = {}
	reclaim_faults = {}
	throttled_time = {}
	limits = []
	for path, values in hierarchy.items():
		initial = before.get("cgroup_hierarchy", {}).get(path, {})
		reclaim_faults[path] = {}
		for line in (values.get("memory.stat") or "").splitlines():
			name = line.split()[0]
			if name.startswith(("pgfault", "pgmajfault", "pgscan", "pgsteal")):
				start = field(initial.get("memory.stat"), name)
				reclaim_faults[path][name] = None if start is None else field(values.get("memory.stat"), name) - start
		start_us, end_us = field(initial.get("cpu.stat"), "throttled_usec"), field(values.get("cpu.stat"), "throttled_usec")
		throttled_time[path] = None if start_us is None or end_us is None else end_us - start_us
		memory_events[path] = {name: None if field(initial.get("memory.events"), name) is None or field(values.get("memory.events"), name) is None
			else field(values.get("memory.events"), name) - field(initial.get("memory.events"), name)
			for name in ["high", "max", "oom", "oom_kill"]}
		first_stall, last_stall = full_total(initial.get("memory.pressure")), full_total(values.get("memory.pressure"))
		ancestor_stall = None if first_stall is None or last_stall is None else last_stall - first_stall
		ancestor_pressure[path] = {"full_stall_us": ancestor_stall,
			"full_fraction": ancestor_stall / (elapsed * 1e6) if ancestor_stall is not None and elapsed > 0 else None}
		start, end = field(initial.get("cpu.stat"), "nr_throttled"), field(values.get("cpu.stat"), "nr_throttled")
		throttling[path] = None if start is None or end is None else end - start
		for kind in ["memory.high", "memory.max"]:
			value = values.get(kind)
			if value and value != "max":
				limits.append({"path": path, "kind": kind, "bytes": int(value), "current_bytes": int(values.get("memory.current") or 0)})
	return {"elapsed_s": elapsed, "memory_full_stall_us": stall,
		"memory_full_fraction": stall / (elapsed * 1e6) if stall is not None and elapsed > 0 else None,
		"memory_accounting_paths": [path for path, values in hierarchy.items() if values.get("memory.high") is not None],
		"ancestor_reclaim_fault_deltas": reclaim_faults, "ancestor_throttled_usec_deltas": throttled_time,
		"ancestor_nr_throttled_deltas": throttling, "ancestor_memory_pressure": ancestor_pressure, "ancestor_memory_event_deltas": memory_events, "finite_memory_limits": limits,
		"effective_memory_high_bytes": min((row["bytes"] for row in limits if row["kind"] == "memory.high"), default=None),
		"affinity_before": before.get("affinity"), "affinity_after": after.get("affinity")}


def verdict(before: dict, after: dict, policy: dict | None, samples: list[dict]) -> dict:
	metrics = delta(before, after)
	if not policy:
		return {"validator_source_sha256": VALIDATOR_SOURCE_SHA256, "status": "not_requested", "metrics": metrics, "reasons": ["no predeclared host policy supplied"]}
	reasons = []
	if policy.get("memory_pressure_policy", "threshold") not in {"threshold", "observe"}:
		reasons.append("unknown memory pressure policy")
	if metrics["elapsed_s"] <= 0:
		reasons.append("nonpositive host observation interval")
	if metrics["memory_full_stall_us"] is not None and metrics["memory_full_stall_us"] < 0:
		reasons.append("host memory pressure counter regressed")
	for path, values in metrics["ancestor_memory_pressure"].items():
		stall = values["full_stall_us"]
		if stall is not None and stall < 0:
			reasons.append(f"ancestor memory pressure counter regressed: {path}")
		elif policy.get("memory_pressure_policy", "threshold") == "threshold" and values["full_fraction"] is not None and values["full_fraction"] > policy["max_memory_full_fraction"]:
			reasons.append(f"ancestor memory full PSI exceeds declared fraction: {path}")
	if any(value is not None and value < 0 for value in metrics["ancestor_nr_throttled_deltas"].values()):
		reasons.append("ancestor CPU throttling counter regressed")
	for limit in metrics["finite_memory_limits"]:
		if metrics["ancestor_memory_pressure"][limit["path"]]["full_fraction"] is None:
			reasons.append(f"limited ancestor memory PSI unavailable: {limit['path']}")
	if not metrics["ancestor_nr_throttled_deltas"] or any(value is None for value in metrics["ancestor_nr_throttled_deltas"].values()):
		reasons.append("ancestor CPU throttling accounting unavailable")
	fraction = metrics["memory_full_fraction"]
	if fraction is None:
		reasons.append("memory full PSI unavailable")
	elif policy.get("memory_pressure_policy", "threshold") == "threshold" and fraction > policy["max_memory_full_fraction"]:
		reasons.append("whole-run memory full PSI exceeds declared fraction")
	if any(value is not None and value > policy["max_nr_throttled_delta"] for value in metrics["ancestor_nr_throttled_deltas"].values()):
		reasons.append("ancestor CPU throttling exceeds declared delta")
	observations = [metrics] + [row.get("target_host_resources", {}) for row in samples]
	if policy.get("require_target_accounting") and any(not row.get("target_host_resources", {}).get("memory_accounting_paths") for row in samples):
		reasons.append("sample target memory accounting unavailable")
	for observation in observations:
		for path in observation.get("memory_accounting_paths", []):
			events = observation.get("ancestor_memory_event_deltas", {}).get(path)
			for name in policy.get("reject_memory_events", []):
				if not isinstance(events, dict) or events.get(name) is None:
					reasons.append(f"required memory event accounting unavailable for {name}: {path}")
			if observation.get("ancestor_nr_throttled_deltas", {}).get(path) is None:
				reasons.append(f"required CPU throttling accounting unavailable: {path}")
		for path, events in observation.get("ancestor_memory_event_deltas", {}).items():
			for name in policy.get("reject_memory_events", []):
				value = events.get(name)
				# The root cgroup has no memory controller; constrained descendants must report counters.
				if value is None and path in observation.get("memory_accounting_paths", []):
					reasons.append(f"memory event accounting unavailable for {name}: {path}")
				elif value is not None and value != 0:
					reasons.append(f"memory event delta {name}={value}: {path}")
		if any(value is not None and value != 0 for value in observation.get("ancestor_nr_throttled_deltas", {}).values()):
			reasons.append("sample ancestor CPU throttling changed")
	expected = policy["expected_affinity"]
	if any(value != expected for value in [metrics["affinity_before"], metrics["affinity_after"]]):
		reasons.append("run affinity differs from declared lane")
	if any(row.get("host_resources", {}).get(key, expected) != expected for row in samples for key in ["affinity_before", "affinity_after"]):
		reasons.append("sample affinity differs from declared lane")
	return {"validator_source_sha256": VALIDATOR_SOURCE_SHA256, "status": "invalid" if reasons else "passed", "reasons": reasons, "metrics": metrics,
		"external_activity_observation": policy.get("external_activity_observation", "manual observation required"),
		"sample_policy": "per-sample deltas retained; PSI observed without sample filtering" if policy.get("memory_pressure_policy") == "observe" else "per-sample deltas retained; pressure threshold applies to the full run, no individual sample filtering"}
