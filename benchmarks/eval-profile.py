#!/usr/bin/env python3
"""Isolated, oracle-checked EVAL-08 mechanism runs, separate from headline timing."""

import argparse
import json
import shutil
from pathlib import Path
import sys

from fixtures import QUALIFICATION_DRVS, environment, qualification_args, qualification_inputs
from process import absolute_program, build_launcher, digest, run_process
from run import ROOT, hardware, write_json


def validate_counters(mode: str, path: Path) -> tuple[bool, str | None]:
	try:
		value = json.loads(path.read_text())
	except (OSError, ValueError) as error:
		return False, f"missing or invalid {mode} counters: {error}"
	if not isinstance(value, dict):
		return False, "counter report must be an object"
	if mode == "diagnostics":
		if value.get("schema_version") != 1 or value.get("scope") != "current_thread_since_reset":
			return False, "unsupported evaluator counter schema/scope"
		for field in ["opcodes", "opcode_pairs", "frame_transitions", "thunk_creations", "layouts"]:
			counts = value.get(field)
			if not isinstance(counts, dict) or any(type(n) is not int or n < 0 for n in counts.values()):
				return False, f"invalid {field} counter map"
		if not sum(value["opcodes"].values()) or not value["layouts"]:
			return False, "evaluator counter report contains no executed workload/layouts"
	else:
		if value.get("schema") != 1 or value.get("scope") != "process_requested_allocation_bytes" or value.get("allocator") != "mimalloc" or value.get("diagnostic_overhead") is not True:
			return False, "unsupported allocation counter schema/scope/allocator"
		for field in ["allocation_count", "allocation_failure_count", "allocation_requested_bytes", "reallocation_count", "reallocation_failure_count", "reallocation_old_requested_bytes", "reallocation_new_requested_bytes", "deallocation_count", "deallocation_requested_bytes", "current_live_requested_bytes", "peak_live_requested_bytes"]:
			if type(value.get(field)) is not int or value[field] < 0:
				return False, f"invalid {field} allocation counter"
		if not value["allocation_count"]:
			return False, "allocation counter report contains no allocations"
	return True, None


def main() -> int:
	parser = argparse.ArgumentParser(description=__doc__)
	parser.add_argument("--binary", type=Path, required=True)
	parser.add_argument("--mode", choices=["stat", "flat", "dwarf", "lbr", "diagnostics", "allocation"], required=True)
	parser.add_argument("--output", type=Path, required=True)
	parser.add_argument("--timeout", type=float, default=600)
	parser.add_argument("--eval08-output", choices=["system", "vm", "module", "standalone"],
		help="diagnose one lazily selected output against its independent recorded path")
	args = parser.parse_args()
	out = args.output.resolve()
	out.mkdir(parents=True, exist_ok=False)
	env = environment(out / "home")
	nixpkgs, home_manager = qualification_inputs()
	arguments, expected = qualification_args(nixpkgs, home_manager, args.eval08_output or "default")
	command = [str(absolute_program(args.binary)), *arguments]
	if args.mode == "stat":
		command = [shutil.which("perf"), "stat", "-x", ";", "-o", str(out / "perf-stat.txt"), "-e", "cycles:u,instructions:u", "--", *command]
	elif args.mode in {"flat", "dwarf", "lbr"}:
		prefix = [shutil.which("perf"), "record", "-e", "cycles:u", "-c", "10000000", "-o", str(out / "perf.data")]
		if args.mode != "flat":
			prefix += ["--user-callchains", "--call-graph", "dwarf,8192" if args.mode == "dwarf" else "lbr"]
		command = [*prefix, "--", *command]
	else:
		name = "NOXI_EVAL_DIAGNOSTICS" if args.mode == "diagnostics" else "NOXI_EVAL_ALLOCATION_DIAGNOSTICS"
		env[name] = str(out / "counters.json")
	identity = args.binary.resolve().parent.parent / "source.json"
	write_json(out / "metadata.json", {"command": command, "environment": env, "mode": args.mode,
		"collector_sources": {name: digest(ROOT / "benchmarks" / name) for name in ["eval-profile.py", "run.py", "fixtures.py", "process.py", "exec.c", "host.py"]},
		"binary_sha256": digest(args.binary), "source_identity": json.loads(identity.read_text()) if identity.exists() else None,
		"hardware_before": hardware(), "expression_sha256": digest(ROOT / "benchmarks/eval-08.nix"),
		"expected": expected, "qualification_output": args.eval08_output or "combined", "limitations": ["mechanism run includes profiler/instrumentation overhead; excluded from production timings"]})
	result = run_process(build_launcher(ROOT), command, env, ROOT, out / "process", args.timeout)
	try:
		actual = json.loads(Path(result["stdout_path"]).read_text())
	except (ValueError, OSError):
		actual = None
	result["oracle_passed"] = (result["exit_code"] == 0 and actual == expected
		and not result["timed_out"] and result["launcher_ready"] and not result["unexpected_survivors"])
	result["collection_passed"] = True
	if args.mode in {"diagnostics", "allocation"}:
		result["collection_passed"], result["collection_error"] = validate_counters(args.mode, out / "counters.json")
	result["hardware_after"] = hardware()
	write_json(out / "result.json", result)
	write_json(out / "manifest.json", {str(p.relative_to(out)): digest(p) for p in out.rglob("*") if p.is_file()})
	print(json.dumps({"oracle_passed": result["oracle_passed"], "collection_passed": result["collection_passed"], "elapsed_s": result["elapsed_s"], "output": str(out)}))
	return 0 if result["oracle_passed"] and result["collection_passed"] else 1


if __name__ == "__main__":
	sys.exit(main())
