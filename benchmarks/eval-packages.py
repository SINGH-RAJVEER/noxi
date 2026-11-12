#!/usr/bin/env python3
"""Ordinary locked Nixpkgs evaluation diagnostics with recorded independent oracles."""

import functools
import json
from pathlib import Path
import subprocess

import fixtures
import run


PATHS = {
	"hello.drvPath": "/nix/store/gdxx5ilpli16l40mg2z9vs4b0w6wa1gf-hello-2.12.3.drv",
	"hello.outPath": "/nix/store/xl1h9i29pgq2q5cszjhm5wpfxfbbqwyi-hello-2.12.3",
	"stdenv.drvPath": "/nix/store/r5ssr70bc8l2hmjn56mnpbpgr9dizn40-stdenv-linux.drv",
	"ripgrep.drvPath": "/nix/store/1hiqcb1xg6k56wnv6b5d9djgwg5dl9lh-ripgrep-15.2.0.drv",
	"python3.drvPath": "/nix/store/a477qxskr7lac8a9ya5wdxd4263ldp4z-python3-3.14.7.drv",
	"git.outPath": "/nix/store/msr1v91ybfw6j12rs5mfl8ghb2rqsnsr-git-2.55.0",
}
CASES = {
	"EVAL-PKG-HELLO": "Locked hello derivation path",
	"EVAL-PKG-PATHS": "Six locked ordinary Nixpkgs derivation and output paths",
	"EVAL-PKG-NAMES": "Three ordinary package name projections",
}


@functools.cache
def nixpkgs() -> str:
	lock = json.loads((run.ROOT / "flake.lock").read_text())
	name = lock["nodes"][lock["root"]]["inputs"]["nixpkgs"]
	locked = lock["nodes"][name]["locked"]
	# Fetch the exact locked input, without copying the workspace's target tree.
	expression = f'(builtins.fetchTree (builtins.fromJSON {json.dumps(json.dumps(locked))})).outPath'
	return subprocess.run(["nix", "eval", "--raw", "--impure", "--expr", expression], check=True,
		stdout=subprocess.PIPE, timeout=600).stdout.decode()



original_evaluation = fixtures.evaluation


def evaluation(case: str, size: int) -> tuple[str, object]:
	if case not in CASES:
		return original_evaluation(case, size)
	prefix = f'let pkgs = import {nixpkgs()} {{ system = "x86_64-linux"; }}; in '
	if case == "EVAL-PKG-HELLO":
		return prefix + "pkgs.hello.drvPath", PATHS["hello.drvPath"]
	if case == "EVAL-PKG-PATHS":
		return prefix + "[ " + " ".join("pkgs." + attr for attr in PATHS) + " ]", list(PATHS.values())
	return prefix + "[ pkgs.hello.name pkgs.ripgrep.name pkgs.git.name ]", ["hello-2.12.3", "ripgrep-15.2.0", "git-2.55.0"]


def main() -> int:
	original_catalog = run.catalog
	original_identity = run.source_identity
	def catalog() -> list[dict]:
		return original_catalog() + [{"id": case, "scenario": scenario, "suite": "research", "requires": "evaluator"}
			for case, scenario in CASES.items()]
	def source_identity(root: Path = run.ROOT) -> dict:
		return {**original_identity(root), "eval_packages_wrapper_sha256": run.digest(Path(__file__))}
	fixtures.evaluation = evaluation
	run.ADAPTERS.update(CASES)
	run.catalog = catalog
	run.source_identity = source_identity
	return run.main()


if __name__ == "__main__":
	raise SystemExit(main())
