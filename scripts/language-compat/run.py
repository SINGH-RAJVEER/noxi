"""Run the deterministic 10,210-case, byte-exact CppNix language comparison.

Run through devenv: uv run --no-project python scripts/language-compat/run.py
--reference /path/to/reference/bin --candidate target/debug --out /tmp/language-check
The selected output directory must be empty to keep fixture contents controlled.
"""

import argparse
import base64
import collections
import concurrent.futures
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import time


BATCHES = {
	"semantics": ("semantics", 3714),
	"generated-strings": ("strings", 2972),
	"contexts": ("contexts", 1296),
	"builtins": ("builtin_cases", 2228),
}

EXTRA_BATCHES = {"builtin-matrix": ("builtin_matrix", 9456)}


# Optional per-case "mode" key selecting how nix-instantiate prints the result.
MODES = {
	"strict": ["--eval", "--strict"],
	"lazy": ["--eval"],
	"json": ["--eval", "--strict", "--json"],
	"xml": ["--eval", "--strict", "--xml"],
	"parse": ["--parse"],
}


def write_json(path, value):
	path.write_text(json.dumps(value, indent=2) + "\n")


def hashes(binaries):
	result = {}
	for label, binary in binaries.items():
		with binary.open("rb") as stream:
			result[label] = hashlib.file_digest(stream, "sha256").hexdigest()
	return result


def executable(value):
	path = Path(value).expanduser().absolute()
	if path.is_dir():
		path /= "nix-instantiate"
	if not path.is_file() or not os.access(path, os.X_OK):
		raise argparse.ArgumentTypeError(f"not an executable: {path}")
	return path


def positive(value):
	number = int(value)
	if number <= 0:
		raise argparse.ArgumentTypeError("must be positive")
	return number


def run_batch(name, cases, root, binaries, workers, timeout):
	working = root / name
	working.mkdir()
	home = working / "home"
	home.mkdir()
	env = {
		"HOME": str(home), "NIX_CONF_DIR": str(home), "NIX_USER_CONF_FILES": "",
		"NIX_CONFIG": "", "NIX_PATH": "", "NIX_REMOTE": "dummy://",
		"PATH": os.environ.get("PATH", ""), "LC_ALL": "C", "TZ": "UTC",
	}
	before = hashes(binaries)
	started = time.monotonic()

	def evaluate(binary, case):
		config = env | {"NIX_CONFIG": case.get("config", "")}
		args = ["nix-instantiate", "--store", "dummy://", *MODES[case.get("mode", "strict")],
			"--expr", case["expression"], *case.get("flags", [])]
		try:
			process = subprocess.run(args, executable=str(binary), env=config,
				cwd=working, capture_output=True, timeout=timeout)
			return {
				"exit_code": process.returncode, "timeout": False,
				"stdout_b64": base64.b64encode(process.stdout).decode(),
				"stderr_b64": base64.b64encode(process.stderr).decode(),
			}
		except subprocess.TimeoutExpired as error:
			return {
				"exit_code": None, "timeout": True,
				"stdout_b64": base64.b64encode(error.stdout or b"").decode(),
				"stderr_b64": base64.b64encode(error.stderr or b"").decode(),
			}
		except OSError as error:
			return {"exit_code": None, "timeout": False, "invocation_error": str(error)}

	def compare(entry):
		index, case = entry
		actual = evaluate(binaries["candidate"], case)
		expected = evaluate(binaries["reference"], case)
		normal = all(not result["timeout"] and result["exit_code"] in (0, 1)
			for result in (actual, expected))
		if not normal:
			outcome = "timeout-or-signal-or-invocation-error"
		elif actual["exit_code"] != expected["exit_code"] or actual["stdout_b64"] != expected["stdout_b64"]:
			outcome = "value-or-status-mismatch"
		elif actual["stderr_b64"] != expected["stderr_b64"]:
			outcome = "diagnostic-mismatch"
		else:
			outcome = "match"
		return {"index": index, **case, "outcome": outcome,
			"candidate": actual, "reference": expected}

	with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
		results = list(pool.map(compare, enumerate(cases)))
	after = hashes(binaries)
	counts = dict(collections.Counter(result["outcome"] for result in results))
	report = {
		"name": name, **{label: str(binary) for label, binary in binaries.items()},
		"executable_sha256": before, "executable_sha256_after": after,
		"executables_unchanged": before == after,
		"environment": env, "workers": workers, "timeout_seconds": timeout,
		"elapsed_seconds": round(time.monotonic() - started, 2),
		"counts": counts, "cases": results,
	}
	write_json(working / "report.json", report)
	write_json(working / "mismatches.json", [result for result in results if result["outcome"] != "match"])
	brief = {key: report[key] for key in ("name", "counts", "elapsed_seconds", "executables_unchanged")}
	brief["report"] = str(working / "report.json")
	print(json.dumps(brief), flush=True)
	return brief


def main():
	parser = argparse.ArgumentParser(description=__doc__)
	parser.add_argument("--reference", type=executable, default=os.environ.get("NOXI_REFERENCE_BIN"))
	parser.add_argument("--candidate", type=executable, default=os.environ.get("NOXI_CANDIDATE_BIN"))
	parser.add_argument("--out", type=Path, default=Path("target/generated-language"))
	parser.add_argument("--batch", action="append", choices=BATCHES | EXTRA_BATCHES)
	parser.add_argument("--jobs", type=positive, default=1, help="concurrent batches")
	parser.add_argument("--workers", type=positive, default=1, help="concurrent cases per batch")
	parser.add_argument("--timeout", type=positive, default=5, help="seconds per engine and case")
	parser.add_argument("--generate-only", action="store_true", help="write cases and fixtures without running engines")
	args = parser.parse_args()
	if not args.generate_only and (args.reference is None or args.candidate is None):
		parser.error("--reference and --candidate are required unless --generate-only is set")
	root = args.out.expanduser().absolute()
	if not re.fullmatch(r"[A-Za-z0-9_./+\-]+", str(root)):
		parser.error("--out must use only letters, digits, underscores, dots, slashes, plus or minus signs for Nix path literals")
	if root.exists() and (not root.is_dir() or any(root.iterdir())):
		parser.error(f"--out must be an empty directory: {root}")
	root.mkdir(parents=True, exist_ok=True)
	names = list(dict.fromkeys(args.batch or BATCHES))
	generated = {}
	for name in names:
		module_name, expected_count = (BATCHES | EXTRA_BATCHES)[name]
		spec = importlib.util.spec_from_file_location(
			f"language_compat_{module_name}", Path(__file__).with_name(f"{module_name}.py"))
		module = importlib.util.module_from_spec(spec)
		spec.loader.exec_module(module)
		cases = module.generate(root)
		if len(cases) != expected_count:
			raise RuntimeError(f"{name}: generated {len(cases)} cases, expected {expected_count}")
		generated[name] = cases
		write_json(root / f"{name}-cases.json", cases)
	if args.generate_only:
		print(json.dumps({"generated_cases": {name: len(cases) for name, cases in generated.items()}}))
		return 0
	binaries = {"reference": args.reference, "candidate": args.candidate}
	before = hashes(binaries)
	with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
		futures = [pool.submit(run_batch, name, cases, root, binaries, args.workers, args.timeout)
			for name, cases in generated.items()]
		batches = [future.result() for future in futures]
	after = hashes(binaries)
	counts = dict(sum((collections.Counter(batch["counts"]) for batch in batches), collections.Counter()))
	unchanged = before == after and all(batch["executables_unchanged"] for batch in batches)
	summary = {
		"case_executions": sum(len(cases) for cases in generated.values()),
		"counts": counts, "executable_sha256": before,
		"executable_sha256_after": after, "executables_unchanged": unchanged,
		"batches": batches,
	}
	write_json(root / "summary.json", summary)
	print(json.dumps(summary, indent=2))
	return 0 if unchanged and set(counts) == {"match"} else 1


if __name__ == "__main__":
	raise SystemExit(main())
