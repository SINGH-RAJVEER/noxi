# Record one VM qualification round: copy each mode's report and write a
# manifest with the exact revision, the candidate's installed executable
# hashes, the harness hashes at that revision and each report's derivation,
# output and build exit codes.
#
# uv run --no-project --no-config python scripts/qualification-manifest.py \
#	--revision REV --candidate STORE_PATH --out tests/qualification/results/DATE-REV \
#	--note TEXT MODE=REPORT_LINK:REPORT_EXIT:GATE_EXIT ...

import argparse
import hashlib
import json
import pathlib
import subprocess


def sha256(data: bytes) -> str:
	return hashlib.sha256(data).hexdigest()


def at_revision(revision: str, path: str) -> bytes:
	return subprocess.run(["git", "show", f"{revision}:{path}"], check=True, capture_output=True).stdout


def deriver(output: str) -> str:
	result = subprocess.run(["nix-store", "--query", "--deriver", output], capture_output=True, text=True)
	return result.stdout.strip() if result.returncode == 0 else ""


def main() -> None:
	parser = argparse.ArgumentParser()
	parser.add_argument("--revision", required=True)
	parser.add_argument("--candidate", required=True)
	parser.add_argument("--out", required=True)
	parser.add_argument("--note", default="")
	parser.add_argument("modes", nargs="+")
	args = parser.parse_args()

	out = pathlib.Path(args.out)
	out.mkdir(parents=True, exist_ok=True)
	candidate = pathlib.Path(args.candidate)
	executables = {
		path.name: sha256(path.resolve().read_bytes())
		for path in sorted((candidate / "bin").iterdir())
	}
	reports = {}
	for spec in args.modes:
		mode, rest = spec.split("=", 1)
		link, report_exit, gate_exit = rest.rsplit(":", 2)
		output = str(pathlib.Path(link).resolve())
		data = (pathlib.Path(output) / "report.json").read_bytes()
		(out / f"{mode}.json").write_bytes(data)
		report = json.loads(data)
		reports[mode] = {
			"file": f"{mode}.json",
			"sha256": sha256(data),
			"drv_path": deriver(output),
			"report_output": output,
			"report_build_exit_code": int(report_exit),
			"strict_gate_build_exit_code": int(gate_exit),
			"counts": report["summary"],
		}
	manifest = {
		"schema": 3,
		"revision": args.revision,
		"candidate": str(candidate),
		"note": args.note,
		"installed_executable_sha256": executables,
		"test_script_sha256": sha256(at_revision(args.revision, "tests/qualification/replacement/script.py")),
		"qualification_flake_lock_sha256": sha256(at_revision(args.revision, "tests/qualification/flake.lock")),
		"guest_command_timeout_seconds": 1800,
		"driver_execute_timeout_seconds": 900,
		"reports": reports,
	}
	(out / "manifest.json").write_text(json.dumps(manifest, indent="\t") + "\n")


if __name__ == "__main__":
	main()
