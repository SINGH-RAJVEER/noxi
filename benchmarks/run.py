#!/usr/bin/env python3
"""Paired CppNix/nix-rs benchmarks with verified outputs and owned stores."""

from __future__ import annotations

import argparse
from collections import defaultdict
import csv
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import statistics
import sqlite3
import subprocess
import sys
import tempfile
import time

from fixtures import ADAPTERS, CONFIG, Fixtures, Operation, environment, remove_tree, tree_digest
from process import Cgroup, absolute_program, build_launcher, digest, run_process


ROOT = Path(__file__).resolve().parent.parent
SCHEMA_VERSION = 1
PINNED_REVISION = "2c73b59da29606068c0c98db015dd3a66955525d"


def write_json(path: Path, value: object):
	path.write_text(json.dumps(value, indent="\t", sort_keys=True) + "\n")


def source_identity(root: Path = ROOT) -> dict:
	hash_ = hashlib.sha256()
	paths = [root / name for name in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml", "flake.nix", "flake.lock"]]
	paths += list((root / "crates").rglob("*.rs")) + list((root / "crates").rglob("Cargo.toml"))
	for path in sorted(paths):
		hash_.update(str(path.relative_to(root)).encode())
		hash_.update(bytes.fromhex(digest(path)))
	result = subprocess.run(["jj", "log", "-r", "@", "--no-graph", "-T", 'commit_id ++ "\\n"'], cwd=root, capture_output=True, text=True)
	return {"source_tree_sha256": hash_.hexdigest(), "jj_revision": result.stdout.strip() if result.returncode == 0 else None,
		"cargo_lock_sha256": digest(root / "Cargo.lock"), "flake_lock_sha256": digest(root / "flake.lock"),
		"rust_toolchain": (root / "rust-toolchain.toml").read_text(), "cargo_config": (root / ".cargo/config.toml").read_text()}


def artifact(directory: Path, role: str, work: Path, timeout: float) -> dict:
	result = {"role": role, "bin_directory": str(directory), "executables": {}}
	for program in ["nix-store", "nix-instantiate", "nix-env", "nix-build", "nix-daemon", "nixrs", "nix"]:
		path = absolute_program(directory / program)
		if not path.is_file():
			continue
		version = subprocess.run([str(path), "--version"], cwd=work, env=environment(work / f"version-{role}"),
			capture_output=True, timeout=timeout)
		result["executables"][program] = {"path": str(path), "sha256": digest(path),
			"version_stdout": version.stdout.decode(errors="replace"), "version_stderr": version.stderr.decode(errors="replace"), "version_exit": version.returncode}
	return result


def hardware() -> dict:
	cpu = Path("/proc/cpuinfo").read_text().split("\n\n")[0]
	def read(path):
		try:
			return Path(path).read_text().strip()
		except OSError:
			return None
	return {"platform": platform.platform(), "kernel": platform.release(), "machine": platform.machine(),
		"cpu_first_processor": cpu, "affinity": sorted(os.sched_getaffinity(0)), "load_average": os.getloadavg(),
		"meminfo": read("/proc/meminfo"), "mountinfo": read("/proc/self/mountinfo"),
		"cpu_governor": read("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
		"cgroup_membership": read("/proc/self/cgroup")}


def normalize(data: bytes, state: Path, fixture: Fixtures) -> bytes:
	# Only physical fixture/state names may differ; retain every other byte.
	return data.replace(str(state).encode(), b"STATE").replace(str(fixture.root).encode(), b"FIXTURE")


def verify(result: dict, operation: Operation, fixture: Fixtures, state: Path, version: str, exact_output: bool = False) -> dict:
	stdout = Path(result["stdout_path"]).read_bytes()
	stderr = Path(result["stderr_path"]).read_bytes()
	actual = {"stdout": normalize(stdout, state, fixture).hex(), "stderr": normalize(stderr, state, fixture).hex(),
		"exit_code": result["exit_code"]}
	if result["timed_out"]:
		return {"status": "timed_out", "reason": "process-tree timeout", "actual": actual}
	if not result["launcher_ready"] or result["unexpected_survivors"]:
		return {"status": "invalid_sample", "reason": "launcher failed or target left unexpected background work", "actual": actual}
	if result["exit_code"] != operation.expected_exit:
		return {"status": "failed", "reason": f"expected exit {operation.expected_exit}, got {result['exit_code']}", "actual": actual}
	try:
		if operation.oracle == "version":
			if stdout.decode() != version or stderr:
				raise ValueError("version output differs from the recorded artifact")
			actual = {"valid_version": True}
		elif operation.oracle == "json":
			value = json.loads(stdout)
			if value != operation.expected:
				raise ValueError("forced JSON differs from independently generated expected value")
			if stderr:
				raise ValueError("unexpected evaluator diagnostic")
			actual = {"json": value, "exit_code": result["exit_code"]}
		elif operation.oracle in {"closure", "build", "verify"}:
			actual["closure"] = closure_records(fixture, operation.root, operation.paths)
			if operation.oracle == "build":
				kind = fixture.metadata["build_graph"]["kind"]
				for i, path in enumerate(operation.paths):
					physical = operation.root / path.lstrip("/")
					if kind == "BUILD-03":
						if len(list(physical.iterdir())) != fixture.size or any(p.read_bytes() != b"payload" for p in physical.iterdir()):
							raise ValueError("I/O builder produced the wrong file tree")
					elif physical.read_bytes() != (b"payload" if kind == "BUILD-06" else f"payload-{i}".encode()):
						raise ValueError("builder output is missing or has wrong content")
			# Progress diagnostics differ between maintenance/substitution engines.
			# Preserve raw logs; the primary comparison is verified artifacts/status.
			actual = {"stdout": actual["stdout"], "closure": actual["closure"], "exit_code": result["exit_code"]}
		elif operation.oracle == "import":
			path = json.loads(stdout)
			physical = operation.root / path.lstrip("/")
			if tree_digest(physical) != fixture.metadata["tree"]["tree_sha256"]:
				raise ValueError("source import changed its bytes, names, symlinks, or modes")
			actual = {"path": path, "tree_sha256": tree_digest(physical),
				"closure": closure_records(fixture, operation.root, [path]), "exit_code": result["exit_code"]}
		elif operation.oracle in {"path-set", "dead-set"}:
			value = sorted(stdout.decode().splitlines())
			if value != operation.expected or (stderr and operation.oracle == "path-set"):
				raise ValueError("incorrect path set or unexpected diagnostic")
			actual = {"paths": value, "exit_code": result["exit_code"]}
		elif operation.oracle == "dry-run":
			if any((operation.root / path.lstrip("/")).exists() for path in operation.paths):
				raise ValueError("planning realized an output")
			if not all(path.encode() in stderr for path in fixture.drvs):
				raise ValueError("dry-run did not report the complete planned graph")
		elif operation.oracle in {"failure", "hash-failure"}:
			if operation.oracle == "hash-failure" and b"hash mismatch" not in stderr:
				raise ValueError("failure did not report the expected hash mismatch")
			if operation.oracle == "failure" and not any(term in stderr.lower() for term in [b"substitut", b"signed", b"signature", b"unable to download"]):
				raise ValueError("failure does not match the injected cache fault")
			if any((operation.root / path.lstrip("/")).exists() for path in operation.paths):
				raise ValueError("failed operation published a requested output")
			assert_unregistered(operation.root, operation.paths)
			actual = {"expected_failure": True, "exit_code": result["exit_code"]}
		elif operation.oracle == "gc":
			if any((operation.root / path.lstrip("/")).exists() for path in operation.paths):
				raise ValueError("GC left unrooted fixture paths behind")
			assert_unregistered(operation.root, operation.paths)
			actual = {"deleted_paths": sorted(operation.paths), "exit_code": result["exit_code"]}
		elif operation.oracle == "profile":
			profile = Path(operation.expected["profile"])
			if str(profile.resolve()) != operation.expected["target"]:
				raise ValueError("rollback selected the wrong generation")
			actual = {"target": operation.expected["target"], "exit_code": result["exit_code"]}
	except (ValueError, OSError, RuntimeError, sqlite3.Error, subprocess.SubprocessError) as error:
		return {"status": "mismatch", "reason": str(error), "actual": actual}
	if exact_output:
		# Preserve raw bytes even when an artifact/JSON oracle discards diagnostics
		# or accepts multiple serializations. Do not normalize physical paths here.
		actual["exact_output"] = {"stdout": stdout.hex(), "stderr": stderr.hex(), "exit_code": result["exit_code"]}
	return {"status": "passed", "oracle": operation.oracle, "actual": actual,
		"digest": hashlib.sha256(json.dumps(actual, sort_keys=True).encode()).hexdigest()}


def assert_unregistered(root: Path, paths: list[str]) -> None:
	database = root / "nix/var/nix/db/db.sqlite"
	if not database.exists():
		return
	with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as db:
		for path in paths:
			if db.execute("SELECT 1 FROM ValidPaths WHERE path = ?", (path,)).fetchone():
				raise ValueError(f"requested absent output remains registered: {path}")


def closure_records(fixture: Fixtures, root: Path, paths: list[str]) -> dict:
	if not paths:
		return {}
	# Inspect candidate-published paths with stock CppNix, outside the timed region.
	data = fixture.ref("nix", ["path-info", "--store", fixture.store_uri(root), "--json", *paths])
	values = json.loads(data)
	for info in values.values():
		# These record publication events, not immutable artifact identity.
		for field in ["registrationTime", "ultimate"]:
			info.pop(field, None)
	return values


def paired_summary(records: list[dict], seed: int, bootstrap: int = 2000) -> dict:
	groups = defaultdict(dict)
	for sample in records:
		if sample.get("stage", "samples") == "samples" and sample["correctness"]["status"] == "passed":
			groups[sample["pair_id"]][sample["role"]] = sample
	result = {}
	for baseline in sorted({role for group in groups.values() for role in group} - {"candidate"}):
		pairs = [(v[baseline], v["candidate"]) for v in groups.values() if baseline in v and "candidate" in v]
		if not pairs:
			continue
		logs = [math.log(a["elapsed_s"] / b["elapsed_s"]) for a, b in pairs]
		rng = random.Random(seed)
		estimates = sorted(math.exp(statistics.mean(rng.choices(logs, k=len(logs)))) for _ in range(bootstrap))
		result[baseline] = {"successful_pairs": len(pairs), "paired_geometric_speedup": math.exp(statistics.mean(logs)),
			"speedup_ci95": [estimates[int(bootstrap * 0.025)], estimates[int(bootstrap * 0.975)]],
			"baseline_median_s": statistics.median(a["elapsed_s"] for a, _ in pairs),
			"candidate_median_s": statistics.median(b["elapsed_s"] for _, b in pairs),
			"absolute_median_saved_s": statistics.median(a["elapsed_s"] - b["elapsed_s"] for a, b in pairs),
			"exploratory": len(pairs) < 30, "p95": None}
		for side, position in [("baseline", 0), ("candidate", 1)]:
			resources = [pair[position].get("resources", {}) for pair in pairs]
			keys = set.intersection(*(set(row) for row in resources))
			result[baseline][f"{side}_resource_medians"] = {key: statistics.median(row[key] for row in resources)
				for key in keys if isinstance(resources[0][key], (float, int)) and not isinstance(resources[0][key], bool)}
			counters = [pair[position].get("counters", {}) for pair in pairs]
			keys = set.intersection(*(set(row) for row in counters))
			result[baseline][f"{side}_counter_medians"] = {key: statistics.median(row[key] for row in counters)
				for key in keys if isinstance(counters[0][key], (float, int))}
	return result


def catalog() -> list[dict]:
	with (ROOT / "benchmarks/catalog.tsv").open(newline="") as stream:
		return list(csv.DictReader(stream, delimiter="\t"))


def variants(case: str) -> list[str]:
	if case == "RPC-01":
		return ["cppnix-server", "nixrs-server"]
	if case == "CACHE-08":
		return ["none", "xz", "zstd", "gzip", "bzip2", "br"]
	if case == "CACHE-10":
		return ["http", "file"]
	return ["default"]


def execute_case(args, case: str, variant: str, engines: dict, artifacts: dict, launcher: Path,
		work: Path, output: Path, samples_stream) -> tuple[dict, list[dict]]:
	fixture = Fixtures(work / f"{case}-{variant}", engines["reference"], args.busybox, args.size, args.seed, args.timeout, case,
		http_protocol=args.http_protocol)
	fixture.server_bins = {"cppnix-server": engines["reference"], "nixrs-server": engines["candidate"]}
	coverage = {"case_id": case, "variant_id": variant, "status": "running", "adapter_scope": "offline subset; selected size, cold CLI, uncontrolled OS page cache"}
	records = []
	expected = None
	try:
		for stage, count in [("preflight", 1), ("samples", args.samples)]:
			for index in range(count):
				roles = list(engines)
				if stage == "samples":
					random.Random(f"{args.seed}/{case}/{variant}/{index}").shuffle(roles)
				pair_records = []
				for role in roles:
					state = fixture.root / f"state-{stage}-{index}-{role}"
					operation = None
					try:
						operation = fixture.prepare(case, state, engines[role], launcher, variant)
						if args.exact_output:
							operation.args.insert(0, "--quiet")
						path = absolute_program(engines[role] / operation.program)
						if not path.is_file():
							raise FileNotFoundError(f"{role} is missing {operation.program}")
						result_dir = output / stage / f"{case}-{variant}" / f"{index:04d}-{role}"
						utc_start = datetime.now(timezone.utc).isoformat()
						result = run_process(launcher, [str(path), *operation.args], operation.env, operation.cwd,
							result_dir, args.timeout, group=operation.group, expect_background=operation.daemon is not None)
						result.update({"schema_version": SCHEMA_VERSION, "run_id": output.name, "case_id": case, "variant_id": variant,
							"sample_index": index, "pair_id": f"{case}/{variant}/{stage}/{index}", "role": role, "stage": stage,
							"order_seed": args.seed, "utc_start": utc_start,
							"environment": operation.env, "fixture": fixture.metadata, "milestone": "verified command completion",
							"artifact_sha256": artifacts[role]["executables"][operation.program]["sha256"]})
						if fixture.cache_server:
							result["counters"] = fixture.cache_server.counters()
						result["correctness"] = verify(result, operation, fixture, state,
							artifacts[role]["executables"][operation.program]["version_stdout"], exact_output=args.exact_output)
						if result["correctness"]["status"] == "passed":
							if expected is None:
								expected = result["correctness"]["actual"]
							elif result["correctness"]["actual"] != expected:
								result["correctness"].update(status="mismatch", reason="result/effects differ from the verified reference preflight")
						result["cleanup"] = "owned processes and state removed"
						pair_records.append(result)
					except FileNotFoundError as error:
						coverage.update(status="blocked", reason=str(error))
						return coverage, records
					finally:
						if operation and operation.group:
							operation.group.close()
							operation.daemon.wait(timeout=5)
						remove_tree(state)
				# Never count either side of a mismatched/failed pair.
				if not all(r["correctness"]["status"] == "passed" for r in pair_records):
					for record in pair_records:
						if record["correctness"]["status"] == "passed":
							record["correctness"].update(status="invalid_sample", reason="another engine in this matched block failed its oracle")
				for record in pair_records:
					samples_stream.write(json.dumps(record, sort_keys=True) + "\n")
					samples_stream.flush()
				if stage == "samples":
					records += pair_records
				if stage == "preflight" and any(r["correctness"]["status"] != "passed" for r in pair_records):
					coverage.update(status="mismatch", reason="preflight oracle failed; inspect preserved outputs")
					return coverage, records
		coverage["status"] = "passed" if all(r["correctness"]["status"] == "passed" for r in records) else "failed"
		return coverage, records
	except (RuntimeError, ValueError, OSError, subprocess.SubprocessError) as error:
		coverage.update(status="failed", reason=str(error))
		return coverage, records
	finally:
		fixture.close()
		remove_tree(fixture.root)


def parser() -> argparse.ArgumentParser:
	result = argparse.ArgumentParser(description=__doc__)
	result.add_argument("--reference-bin", type=Path)
	result.add_argument("--candidate-bin", type=Path, default=ROOT / "target/release")
	result.add_argument("--baseline-bin", type=Path)
	result.add_argument("--comparison-bin", action="append", default=[], metavar="NAME=DIR",
		help="additional preserved release artifact, repeatable; every engine must pass each matched block")
	result.add_argument("--busybox", type=Path)
	result.add_argument("--case", action="append", default=[], help="case ID, repeatable; default is all offline adapters")
	result.add_argument("--suite", choices=["pr", "nightly", "release", "research"])
	result.add_argument("--samples", type=int, default=30)
	result.add_argument("--size", type=int, default=32, help="scale point; large-tree cases interpret this as MiB")
	result.add_argument("--seed", type=int, default=1729)
	result.add_argument("--timeout", type=float, default=60)
	result.add_argument("--http-protocol", choices=["HTTP/1.0", "HTTP/1.1"], default="HTTP/1.1",
		help="loopback cache response protocol; HTTP/1.0 closes without an explicit Connection header")
	result.add_argument("--exact-output", action="store_true",
		help="add --quiet and compare raw stdout, stderr and exit status in addition to the artifact oracle")
	result.add_argument("--output", type=Path)
	result.add_argument("--list", action="store_true")
	result.add_argument("--record-source", type=Path, help="write source identity beside a preserved baseline artifact")
	return result


def main() -> int:
	args = parser().parse_args()
	if args.record_source:
		write_json(args.record_source, source_identity())
		return 0
	rows = catalog()
	if args.list:
		for row in rows:
			print(f"{row['id']}\t{'adapter' if row['id'] in ADAPTERS else 'planned'}\t{row['scenario']}")
		return 0
	if not args.reference_bin:
		parser().error("--reference-bin is required for paired runs")
	if args.samples <= 0 or args.size <= 0 or args.timeout <= 0:
		parser().error("samples, size, and timeout must be positive")
	known = {row["id"] for row in rows}
	if set(args.case) - known:
		parser().error(f"unknown cases: {sorted(set(args.case) - known)}")
	selected = set(args.case) if args.case else {row["id"] for row in rows if not args.suite or row["suite"] == args.suite}
	output = (args.output or ROOT / "target/benchmarks" / datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")).absolute()
	output.mkdir(parents=True, exist_ok=False)
	launcher = build_launcher(ROOT)
	# Fail before promising resource-qualified timings if delegation is absent.
	with Cgroup():
		pass
	engines = {"reference": args.reference_bin.absolute(), "candidate": args.candidate_bin.absolute()}
	if args.baseline_bin:
		engines["baseline"] = args.baseline_bin.absolute()
	for entry in args.comparison_bin:
		name, separator, directory = entry.partition("=")
		if not separator or not directory or not name or not name.replace("-", "").replace("_", "").isalnum():
			parser().error("--comparison-bin must be NAME=DIR with an alphanumeric, hyphen or underscore name")
		if name in engines:
			parser().error(f"duplicate comparison engine: {name}")
		engines[name] = Path(directory).absolute()
	coverage, summaries = [], {}
	with tempfile.TemporaryDirectory(prefix="nixrs-benchmark-") as temp:
		work = Path(temp)
		artifacts = {role: artifact(path, role, work, args.timeout) for role, path in engines.items()}
		ref_version = artifacts["reference"]["executables"].get("nix-store", {})
		if ref_version.get("version_exit") != 0 or not ref_version.get("version_stdout", "").strip().endswith("2.35.2"):
			raise RuntimeError("reference must be the pinned CppNix 2.35.2 artifact")
		identity = source_identity()
		for role, directory in engines.items():
			if role in {"reference", "candidate"}:
				continue
			baseline_identity = directory.parent / "source.json"
			artifacts[role]["source_identity"] = json.loads(baseline_identity.read_text()) if baseline_identity.exists() else {"unverified_source": True}
		metadata = {"schema_version": SCHEMA_VERSION, "run_id": output.name,
			"runner_sources": {name: digest(ROOT / "benchmarks" / name) for name in ["run.py", "fixtures.py", "process.py", "exec.c", "catalog.tsv"]},
			"collector_runtime": {"python": sys.version, "cc": subprocess.run(["cc", "--version"], capture_output=True, text=True, check=True).stdout.splitlines()[0],
				"rustc": subprocess.run(["rustc", "--version", "--verbose"], capture_output=True, text=True, check=True).stdout},
			"reference_revision_declared_by_project_lock": PINNED_REVISION,
			"reference_binary_revision_not_inferred_from_version": True,
			"artifacts": artifacts, "candidate_source_identity": identity,
			"hardware": hardware(), "arguments": {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
			"launcher": {"sha256": digest(launcher), "source_sha256": digest(ROOT / "benchmarks/exec.c")},
			"limits": ["offline adapters cover selected dimensions only", "OS page cache is uncontrolled", "elapsed time includes target exec, native wait4 metric-file write, and collector wake/reap latency", "target wait4 RSS excludes Python's inherited high-water mark; launcher RSS does not", "cgroup memory includes file cache", "local HTTP server CPU is outside client accounting", "phase traces and hardware counters are not implemented"]}
		write_json(output / "metadata.json", metadata)
		with (output / "samples.jsonl").open("w") as samples_stream:
			for row in rows:
				case = row["id"]
				if case not in selected:
					coverage.append({"case_id": case, "status": "not_selected"})
					continue
				if case not in ADAPTERS:
					coverage.append({"case_id": case, "status": "blocked", "reason": f"adapter not implemented; requires {row['requires']}"})
					continue
				for variant in variants(case):
					print(f"{case}/{variant}", flush=True)
					status, records = execute_case(args, case, variant, engines, artifacts, launcher, work, output, samples_stream)
					coverage.append(status)
					summaries[f"{case}/{variant}"] = paired_summary(records, args.seed)
					print(f"  {status['status']}: {status.get('reason', str(len(records)) + ' samples')}", flush=True)
		write_json(output / "coverage.json", coverage)
		write_json(output / "summary.json", summaries)
		lines = ["# Benchmark run", "", f"Run `{output.name}`. Selected scale {args.size}, {args.samples} paired samples, seed {args.seed}. Ratios above 1 favor the candidate. Runs below 30 pairs are exploratory; no p95 or general speed claim is made.", "", "| Case | Baseline | Pairs | Baseline ms | Candidate ms | Speedup | 95% interval |", "| --- | --- | --- | --- | --- | --- | --- |"]
		for case, comparisons in summaries.items():
			for role, summary in comparisons.items():
				lo, hi = summary["speedup_ci95"]
				lines.append(f"| {case} | {role} | {summary['successful_pairs']} | {summary['baseline_median_s'] * 1e3:.3f} | {summary['candidate_median_s'] * 1e3:.3f} | {summary['paired_geometric_speedup']:.3f} | {lo:.3f} to {hi:.3f} |")
		lines += ["", "Coverage and raw samples are in `coverage.json` and `samples.jsonl`. Resource metrics, output digests, fixture identity, exact arguments, and isolated environment maps are preserved per sample. The metadata file records limits and hardware. No aggregate drop-in score is computed.", "", "## Incomplete or unsuccessful cases", ""]
		lines += [f"- {item['case_id']}/{item.get('variant_id', 'default')}: {item['status']}, {item.get('reason', '')}" for item in coverage if item["status"] not in {"passed", "not_selected"}]
		(output / "report.md").write_text("\n".join(lines) + "\n")
		manifest = {str(path.relative_to(output)): digest(path) for path in output.rglob("*") if path.is_file()}
		write_json(output / "manifest.json", manifest)
	print(f"Report: {output / 'report.md'}")
	return 1 if any(item["status"] in {"failed", "mismatch"} for item in coverage) else 0


if __name__ == "__main__":
	raise SystemExit(main())
