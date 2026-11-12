#!/usr/bin/env python3
"""Matched evaluator hotspot experiments using the ordinary benchmark runner.

These synthetic cases are additional diagnostics, not catalog qualification.
All ordinary runner options apply; --size is the iteration/recursion count.
"""

from pathlib import Path
import sys

import fixtures
import run


CASES = {
	"EVAL-HOT-MERGE": "Repeated 256-binding literal updates with two selections",
	"EVAL-HOT-CONTEXT": "Successful nested addErrorContext boundaries",
}


def evaluation(case: str, size: int) -> tuple[str, object]:
	if case == "EVAL-HOT-MERGE":
		attrs = " ".join(f"a{i:04d} = {i};" for i in range(256))
		return (
			f"let mk = i: ({{ {attrs} }} // {{ extra = i; }}); "
			f"in builtins.foldl' (a: b: a + b.extra + b.a0255) 0 (builtins.genList mk {size})",
			size * (size - 1) // 2 + size * 255,
		)
	if case == "EVAL-HOT-CONTEXT":
		return (
			'let f = n: if n == 0 then 42 else builtins.addErrorContext "context" '
			f"(f (n - 1)); in f {size}",
			42,
		)
	return original_evaluation(case, size)


original_evaluation = fixtures.evaluation


def main() -> int:
	original_catalog = run.catalog
	original_identity = run.source_identity

	def catalog() -> list[dict]:
		return original_catalog() + [
			{"id": case, "scenario": scenario, "suite": "research", "requires": "evaluator"}
			for case, scenario in CASES.items()
		]

	def source_identity(root: Path = run.ROOT) -> dict:
		return {
			**original_identity(root),
			"eval_hotspots_wrapper_sha256": run.digest(Path(__file__)),
		}

	fixtures.evaluation = evaluation
	run.ADAPTERS.update(CASES)
	run.catalog = catalog
	run.source_identity = source_identity
	if not any(arg == "--case" or arg.startswith("--case=") for arg in sys.argv[1:]):
		for case in CASES:
			sys.argv.extend(["--case", case])
	return run.main()


if __name__ == "__main__":
	raise SystemExit(main())
