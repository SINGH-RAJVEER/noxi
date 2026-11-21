"""Run, collect and summarize noxi-compat-result/1 records.

Subcommands:

	run --tier TIER --dir DIR -- COMMAND...
		Record the run's provenance in DIR/run.json, run COMMAND with
		NOXI_RESULTS_DIR=DIR and NOXI_TIER=TIER, turn nextest's JUnit report
		for the tier's profile into DIR/nextest.jsonl, and write
		DIR/summary.json. Exits with COMMAND's status.
	junit --tier TIER --dir DIR JUNIT.xml
		Only the JUnit conversion.
	qualification --dir DIR RESULT_DIR...
		Turn disposable-VM qualification reports
		(tests/qualification/results/*/MODE.json) into DIR/qualification.jsonl.
	summary DIR...
		Write DIR/summary.json for each record directory and print the counts.
	keep DIR NAME
		Copy a record directory to tests/compat-results/NAME as committed
		evidence; refuses records of a dirty source tree.
	rows
		List the compatibility matrix row ids.
	evidence [--check]
		Regenerate docs/compatibility-evidence.md from tests/compat-results,
		or fail when it is stale, a matrix row is unmapped, or a link is broken.

The record format is documented in docs/compatibility-results.md.
"""

import argparse
import datetime
import json
import os
import pathlib
import platform
import re
import shutil
import subprocess
import sys
import time
import tomllib
import xml.etree.ElementTree as ElementTree

SCHEMA = "noxi-compat-result/1"
SUMMARY_SCHEMA = "noxi-compat-summary/1"
VERDICTS = ("pass", "fail", "known-difference", "unrun")
TIERS = ("fast", "store", "daemon", "system")
ROOT = pathlib.Path(__file__).resolve().parents[1]
MATRIX = ROOT / "docs/compatibility-matrix.md"
EVIDENCE = ROOT / "docs/compatibility-evidence.md"
KEPT = ROOT / "tests/compat-results"
ROWS = KEPT / "rows.toml"
INLINE_LIMIT = 32 * 1024


def slug(text):
	return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")


def matrix_rows(path=MATRIX):
	"""The (id, area) pairs of the matrix's `| Area | ... |` table."""
	rows = []
	in_table = False
	for line in pathlib.Path(path).read_text().splitlines():
		if line.startswith("| Area |"):
			in_table = True
			continue
		if not in_table:
			continue
		if not line.startswith("|"):
			break
		cells = [cell.strip() for cell in line.strip("|").split("|")]
		if set(cells[0]) <= set("-: "):
			continue
		rows.append((slug(cells[0]), cells[0]))
	return rows


def load_rows(path=ROWS):
	with open(path, "rb") as file:
		data = tomllib.load(file)
	return data.get("rows", {}), data.get("qualification", {})


def row_for(binary_id, test_name, rows):
	"""The row an entry of rows.toml assigns to a test, or None."""
	by_test, by_binary, by_package = [], {}, {}
	for row, entries in rows.items():
		for entry in entries:
			if " " in entry:
				binary, pattern = entry.split(" ", 1)
				by_test.append((binary, re.compile(pattern), row))
			elif "::" in entry:
				by_binary[entry] = row
			else:
				by_package[entry] = row
	for binary, pattern, row in by_test:
		if binary == binary_id and pattern.search(test_name):
			return row
	if binary_id in by_binary:
		return by_binary[binary_id]
	return by_package.get(binary_id.split("::", 1)[0])


def git(*args):
	try:
		return subprocess.run(["git", "-C", str(ROOT), *args], check=True, capture_output=True, text=True).stdout
	except (OSError, subprocess.CalledProcessError):
		return None


def noxi_revision():
	revision = git("rev-parse", "HEAD")
	if revision is not None:
		status = git("status", "--porcelain", "--untracked-files=no")
		return {"revision": revision.strip(), "dirty": None if status is None else bool(status.strip()), "source": "git"}
	if "NOXI_SOURCE_REV" in os.environ:
		return {"revision": os.environ["NOXI_SOURCE_REV"], "dirty": os.environ.get("NOXI_SOURCE_DIRTY") == "1", "source": "NOXI_SOURCE_REV"}
	return {"revision": "unknown", "dirty": None, "source": None}


def reference_revision():
	fields = {}
	lock = ROOT / "tests/nix-official.lock"
	if lock.exists():
		for line in lock.read_text().splitlines():
			key, _, value = line.partition("=")
			fields[key] = value
	return {"release": fields.get("ref"), "revision": fields.get("commit"), "repository": fields.get("repository")}


def provenance(tier, command):
	settings = {name: os.environ[name] for name in sorted(os.environ) if name.startswith(("NOXI_", "CARGO_BUILD_", "NEXTEST_"))}
	return {
		"schema": SCHEMA,
		"tier": tier,
		"command": command,
		"started": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
		"platform": f"{platform.machine()}-{platform.system().lower()}",
		"kernel": platform.release(),
		"noxi": noxi_revision(),
		"reference": reference_revision(),
		"settings": settings,
	}


def text_field(text):
	data = (text or "").encode()
	field = {"size": len(data)}
	if len(data) <= INLINE_LIMIT:
		field["text"] = text or ""
	else:
		field["text"] = data[:INLINE_LIMIT].decode(errors="replace")
		field["truncated"] = True
	return field


def junit_records(path, tier, run, rows):
	"""One record per test case of a nextest JUnit report."""
	records = []
	for suite in ElementTree.parse(path).getroot().iter("testsuite"):
		for case in suite.iter("testcase"):
			binary_id = case.get("classname") or suite.get("name")
			name = case.get("name")
			failure = case.find("failure")
			if failure is None:
				failure = case.find("error")
			skipped = case.find("skipped")
			if failure is not None:
				verdict, reason = "fail", (failure.get("message") or failure.get("type") or "failed")
			elif skipped is not None:
				verdict, reason = "unrun", (skipped.get("message") or "skipped")
			else:
				verdict, reason = "pass", None
			output = "".join((element.text or "") for tag in ("system-out", "system-err") for element in case.iter(tag))
			records.append({
				"schema": SCHEMA,
				"case": f"nextest:{binary_id}:{name}",
				"runner": "nextest",
				"tier": tier,
				"matrix_row": row_for(binary_id, name, rows),
				"inventory": [],
				"run": run,
				"settings": {"command": run.get("command")},
				"engines": {"reference": None, "noxi": None},
				"test": {"binary_id": binary_id, "name": name, "seconds": float(case.get("time") or 0), "output": text_field(output) if verdict == "fail" else None},
				"verdict": verdict,
				"classification": None,
				"reason": reason,
				"differences": [],
			})
	return records


def qualification_records(result_dir, modes):
	"""One record per step of each disposable-VM scenario report in result_dir."""
	result_dir = pathlib.Path(result_dir)
	manifest_path = result_dir / "manifest.json"
	manifest = json.loads(manifest_path.read_text()) if manifest_path.exists() else {}
	records = []
	for report_path in sorted(result_dir.glob("*.json")):
		if report_path.name == "manifest.json":
			continue
		report = json.loads(report_path.read_text())
		mode = report.get("mode", report_path.stem)
		if "steps" not in report:
			continue
		expected = report.get("expected", {})
		run = {
			"schema": SCHEMA,
			"tier": "system",
			"command": f"just qualification-report {mode}",
			"platform": "x86_64-linux",
			"noxi": {"revision": manifest.get("revision", "unknown"), "dirty": None, "source": str(manifest_path.relative_to(ROOT)) if manifest_path.is_relative_to(ROOT) else str(manifest_path), "executables": manifest.get("installed_executable_sha256"), "candidate": manifest.get("candidate") or expected.get("candidate")},
			"reference": {**reference_revision(), "executables": expected.get("reference")},
			"report": {"file": report_path.name, "schema": report.get("schema"), "manifest_schema": manifest.get("schema"), "note": manifest.get("note")},
		}
		for step in report["steps"]:
			status = step.get("status")
			verdict = {"pass": "pass", "fail": "fail"}.get(status, "unrun")
			exit_status = step.get("exit_status")
			records.append({
				"schema": SCHEMA,
				"case": f"qualification-{mode}:{step.get('name')}",
				"runner": f"qualification-{mode}",
				"tier": "system",
				"matrix_row": modes.get(mode),
				"inventory": [],
				"run": run,
				"settings": {"user": step.get("user"), "expected_status": step.get("expected_status"), "timeout_seconds": manifest.get("guest_command_timeout_seconds")},
				"engines": {
					"reference": None,
					"noxi": {
						"argv": ["sh", "-c", step.get("command")],
						"exit_code": int(exit_status) if str(exit_status).lstrip("-").isdigit() else None,
						"signal": None,
						"timed_out": None,
						"stdout": text_field(step.get("stdout")),
						"stderr": text_field(step.get("stderr")),
						"structured_output": None,
						"store": None,
						"daemon_exchange": None,
					},
				},
				"verdict": verdict,
				"classification": status,
				"reason": step.get("detail") if verdict != "pass" else None,
				"differences": [],
			})
	return records


def write_jsonl(path, records):
	with open(path, "w") as file:
		for record in records:
			file.write(json.dumps(record, sort_keys=True) + "\n")


def read_records(directory):
	records = []
	for path in sorted(pathlib.Path(directory).glob("*.jsonl")):
		for number, line in enumerate(path.read_text().splitlines(), 1):
			if not line.strip():
				continue
			record = json.loads(line)
			if record.get("schema") != SCHEMA:
				raise ValueError(f"{path}:{number}: schema {record.get('schema')!r}, expected {SCHEMA}")
			if record.get("verdict") not in VERDICTS:
				raise ValueError(f"{path}:{number}: verdict {record.get('verdict')!r}")
			records.append((path.name, record))
	return records


def summarize(directory):
	directory = pathlib.Path(directory)
	by_row, by_runner, by_tier = {}, {}, {}
	failures = []
	runs = {}
	for file, record in read_records(directory):
		verdict = record["verdict"]
		row = record.get("matrix_row") or "unmapped"
		for table, key in ((by_row, row), (by_runner, record["runner"]), (by_tier, record["tier"])):
			counts = table.setdefault(key, dict.fromkeys(VERDICTS, 0))
			counts[verdict] += 1
		if verdict == "fail":
			failures.append({"case": record["case"], "file": file, "reason": (record.get("reason") or "")[:300]})
		run = record.get("run") or {}
		runs.setdefault(record["runner"], {
			"file": file,
			"noxi": {key: (run.get("noxi") or {}).get(key) for key in ("revision", "dirty")},
			"reference": {key: (run.get("reference") or {}).get(key) for key in ("release", "revision")},
			"platform": run.get("platform"),
		})
	header = directory / "run.json"
	summary = {
		"schema": SUMMARY_SCHEMA,
		"run": json.loads(header.read_text()) if header.exists() else None,
		"runners": runs,
		"rows": dict(sorted(by_row.items())),
		"runner_counts": dict(sorted(by_runner.items())),
		"tiers": dict(sorted(by_tier.items())),
		"failures": failures,
	}
	(directory / "summary.json").write_text(json.dumps(summary, indent="\t", sort_keys=True) + "\n")
	return summary


def print_summary(directory, summary):
	print(f"{directory}:")
	for row, counts in summary["rows"].items():
		print(f"  {row:45} " + " ".join(f"{verdict}={counts[verdict]}" for verdict in VERDICTS))
	for failure in summary["failures"][:20]:
		print(f"  FAIL {failure['case']}: {failure['reason'][:160]}")


def evidence_markdown(rows, matrix, kept=KEPT):
	"""docs/compatibility-evidence.md, from rows.toml and the kept summaries."""
	summaries = []
	for path in sorted(kept.glob("*/summary.json")):
		summaries.append((path.parent.name, json.loads(path.read_text())))
	lines = [
		"# Compatibility matrix evidence",
		"",
		"Generated by `just compat-evidence` from `tests/compat-results/rows.toml` and the result sets kept in `tests/compat-results/`; `just compat-evidence-check` fails when this page is stale. Each count is a number of `noxi-compat-result/1` records ([format](compatibility-results.md)) for the [matrix](compatibility-matrix.md) row. A result set is evidence for the source revision it names, not for later revisions. `known-difference` and `unrun` are never passes.",
		"",
	]
	for name, summary in summaries:
		run = summary.get("run") or {}
		revisions = sorted({(value["noxi"]["revision"], value["noxi"]["dirty"]) for value in summary.get("runners", {}).values()})
		described = ", ".join(f"`{revision[:12]}`" + (" (dirty)" if dirty else "") for revision, dirty in revisions if revision)
		lines.append(f"- [`{name}`](../tests/compat-results/{name}/summary.json): tier {run.get('tier', 'mixed')}, noxi {described or 'unknown'}, runners {', '.join(sorted(summary.get('runners', {})))}")
	lines += ["", "| Row | Mapped tests | Result set | pass | fail | known-difference | unrun |", "|---|---|---|---|---|---|---|"]
	for row, area in matrix:
		entries = rows.get(row, [])
		mapped = str(len(entries)) if entries else "none"
		printed = False
		for name, summary in summaries:
			counts = summary.get("rows", {}).get(row)
			if not counts:
				continue
			cells = " | ".join(str(counts[verdict]) for verdict in VERDICTS)
			lines.append(f"| {area} | {mapped} | [`{name}`](../tests/compat-results/{name}/) | {cells} |")
			printed = True
		if not printed:
			lines.append(f"| {area} | {mapped} | no recorded results | | | | |")
	unmapped = [(name, summary["rows"]["unmapped"]) for name, summary in summaries if "unmapped" in summary.get("rows", {})]
	if unmapped:
		lines += ["", "Records whose test is not mapped to a row in `rows.toml`:", ""]
		for name, counts in unmapped:
			lines.append(f"- `{name}`: " + ", ".join(f"{counts[verdict]} {verdict}" for verdict in VERDICTS))
	return "\n".join(lines) + "\n"


def check_evidence(rows, matrix, kept=KEPT, evidence=EVIDENCE):
	problems = []
	ids = {row for row, _ in matrix}
	for row in ids - set(rows):
		problems.append(f"matrix row {row} has no entry in rows.toml")
	for row in set(rows) - ids:
		problems.append(f"rows.toml names {row}, which is not a matrix row")
	for path in sorted(kept.glob("*/summary.json")):
		summary = json.loads(path.read_text())
		for row in summary.get("rows", {}):
			if row != "unmapped" and row not in ids:
				problems.append(f"{path.parent.name} has records for unknown row {row}")
	expected = evidence_markdown(rows, matrix, kept)
	if not evidence.exists() or evidence.read_text() != expected:
		problems.append(f"{evidence.name} is stale; run just compat-evidence")
	for link in re.findall(r"\]\(\.\./([^)]+)\)", expected):
		if not (ROOT / link).exists():
			problems.append(f"broken evidence link {link}")
	return problems


def command_run(args):
	directory = pathlib.Path(args.dir).resolve()
	if directory.exists():
		shutil.rmtree(directory)
	directory.mkdir(parents=True)
	command = args.command[1:] if args.command[:1] == ["--"] else args.command
	run = provenance(args.tier, command)
	(directory / "run.json").write_text(json.dumps(run, indent="\t", sort_keys=True) + "\n")
	environment = dict(os.environ, NOXI_RESULTS_DIR=str(directory), NOXI_TIER=args.tier)
	started = time.time()
	status = subprocess.run(command, env=environment).returncode
	run["exit_status"] = status
	(directory / "run.json").write_text(json.dumps(run, indent="\t", sort_keys=True) + "\n")
	junit = pathlib.Path(args.junit or ROOT / "target/nextest" / args.tier / "junit.xml")
	if junit.exists() and junit.stat().st_mtime >= started:
		rows, _ = load_rows()
		write_jsonl(directory / "nextest.jsonl", junit_records(junit, args.tier, run, rows))
	print_summary(directory, summarize(directory))
	return status


def main(argv=None):
	parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
	commands = parser.add_subparsers(dest="action", required=True)
	run = commands.add_parser("run")
	run.add_argument("--tier", choices=TIERS, required=True)
	run.add_argument("--dir", required=True)
	run.add_argument("--junit")
	run.add_argument("command", nargs=argparse.REMAINDER)
	junit = commands.add_parser("junit")
	junit.add_argument("--tier", choices=TIERS, required=True)
	junit.add_argument("--dir", required=True)
	junit.add_argument("report")
	qualification = commands.add_parser("qualification")
	qualification.add_argument("--dir", required=True)
	qualification.add_argument("results", nargs="+")
	summary = commands.add_parser("summary")
	summary.add_argument("dirs", nargs="+")
	keep = commands.add_parser("keep")
	keep.add_argument("dir")
	keep.add_argument("name")
	keep.add_argument("--allow-dirty", action="store_true")
	commands.add_parser("rows")
	evidence = commands.add_parser("evidence")
	evidence.add_argument("--check", action="store_true")
	args = parser.parse_args(argv)

	if args.action == "run":
		return command_run(args)
	if args.action == "junit":
		directory = pathlib.Path(args.dir)
		directory.mkdir(parents=True, exist_ok=True)
		header = directory / "run.json"
		run = json.loads(header.read_text()) if header.exists() else provenance(args.tier, None)
		rows, _ = load_rows()
		write_jsonl(directory / "nextest.jsonl", junit_records(args.report, args.tier, run, rows))
		print_summary(directory, summarize(directory))
		return 0
	if args.action == "qualification":
		directory = pathlib.Path(args.dir)
		directory.mkdir(parents=True, exist_ok=True)
		_, modes = load_rows()
		records = [record for result in args.results for record in qualification_records(result, modes)]
		write_jsonl(directory / "qualification.jsonl", records)
		print_summary(directory, summarize(directory))
		return 0
	if args.action == "summary":
		for directory in args.dirs:
			print_summary(directory, summarize(directory))
		return 0
	if args.action == "keep":
		source = pathlib.Path(args.dir)
		dirty = [record["case"] for _, record in read_records(source) if ((record.get("run") or {}).get("noxi") or {}).get("dirty")]
		if dirty and not args.allow_dirty:
			print(f"{len(dirty)} records come from a dirty source tree, for example {dirty[0]}; commit first or pass --allow-dirty", file=sys.stderr)
			return 1
		target = KEPT / args.name
		if target.exists():
			shutil.rmtree(target)
		shutil.copytree(source, target)
		summarize(target)
		print(target)
		return 0
	if args.action == "rows":
		rows, _ = load_rows()
		for row, area in matrix_rows():
			print(f"{row}\t{area}\t{len(rows.get(row, []))} mapped")
		return 0
	if args.action == "evidence":
		rows, _ = load_rows()
		matrix = matrix_rows()
		if args.check:
			problems = check_evidence(rows, matrix)
			for problem in problems:
				print(problem, file=sys.stderr)
			return 1 if problems else 0
		EVIDENCE.write_text(evidence_markdown(rows, matrix))
		print(EVIDENCE)
		return 0
	return 2


if __name__ == "__main__":
	sys.exit(main())
