#!/usr/bin/env python3
"""Print reference NAR hex and provenance without opening a Nix store."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


REVISION = "2c73b59da29606068c0c98db015dd3a66955525d"


def file(contents, executable=False):
	return {"type": "regular", "executable": executable, "contents_hex": contents.hex()}


def symlink(target):
	return {"type": "symlink", "target_hex": target.hex()}


def directory(entries):
	return {
		"type": "directory",
		"entries": [
			{"name_hex": name.hex(), "node": node}
			for name, node in sorted(entries, key=lambda entry: entry[0])
		],
	}


def materialize(path, node):
	if node["type"] == "regular":
		with open(path, "wb") as source:
			source.write(bytes.fromhex(node["contents_hex"]))
		os.chmod(path, 0o755 if node["executable"] else 0o644)
	elif node["type"] == "symlink":
		os.symlink(bytes.fromhex(node["target_hex"]), path)
	else:
		os.mkdir(path, 0o755)
		# Deliberately create entries in reverse byte order. nix-store must sort them.
		for entry in reversed(node["entries"]):
			materialize(path + b"/" + bytes.fromhex(entry["name_hex"]), entry["node"])


def main():
	repo = Path(__file__).resolve().parents[4]
	parser = argparse.ArgumentParser(description=__doc__)
	parser.add_argument("--reference", type=Path, default=repo / "target/nix-reference/bin/nix-store")
	parser.add_argument("--temp-dir", type=Path, help="Parent for temporary sources; defaults to the system temporary directory")
	args = parser.parse_args()
	# nix-store is a multicall symlink: preserve its name when resolving the package.
	reference = str(args.reference.parent.resolve(strict=True) / args.reference.name)
	version_command = [reference, "--version"]
	version = subprocess.check_output(version_command, text=True).strip()
	assert version == "nix-store (Nix) 2.35.2", version
	lock = json.loads((repo / "flake.lock").read_text())
	assert lock["nodes"]["nix-reference"]["locked"]["rev"] == REVISION
	binary = file(b"NAR\x00\xff\x80\r\n\x01binary\x00end")
	executable = file(b"#!/bin/sh\nprintf 'reference\\n'\n", True)
	target = symlink(b"../missing/target")
	cases = {
		"regular": binary,
		"executable": executable,
		"symlink": target,
		"directory": directory([
			(b".empty", file(b"")),
			(b"A-executable", executable),
			(b"a-binary", binary),
			(b"nested", directory([
				(b"empty-dir", directory([])),
				(b"link", symlink(b"../a-binary")),
				(b"z-text", file(b"nested\n")),
			])),
			(b"z-last", file(b"last\n")),
			(b"\xff-last", file(b"high byte name\n")),
		]),
	}
	provenance = {
		"reference_release": "2.35.2",
		"source_url": "https://github.com/NixOS/nix",
		"source_revision": REVISION,
		"source_tag": "2.35.2",
		"source_revision_evidence": "flake.lock nodes.nix-reference.locked.rev",
		"platform": "x86_64-linux",
		"version_command": version_command,
		"version_output": version,
		"generation_command": f"python3 crates/compat-tests/fixtures/nar-reference/generate.py --reference {reference}",
		"hex_encoding": "lowercase hex of unmodified nix-store --dump stdout; whitespace ignored",
		"source_modes": {"regular": "0644", "executable": "0755", "directory": "0755"},
		"creation_order": "directory entries created in descending raw byte order",
		"scope": "Filesystem dumps only; no store open, import, export, or registration.",
		"cases": {},
	}
	with tempfile.TemporaryDirectory(prefix="nar-oracles-", dir=args.temp_dir) as root:
		for name, node in cases.items():
			path = os.fsencode(root) + b"/" + name.encode()
			materialize(path, node)
			command = [reference, "--dump", os.fsdecode(path)]
			result = subprocess.run(command, check=True, capture_output=True)
			assert not result.stderr, result.stderr
			archive = result.stdout
			provenance["cases"][name] = {
				"command": command,
				"exit_code": result.returncode,
				"stderr": "",
				"nar_size": len(archive),
				"nar_sha256": hashlib.sha256(archive).hexdigest(),
				"source": node,
			}
			print(f"=== {name}.nar.hex ===")
			for offset in range(0, len(archive), 32):
				print(archive[offset:offset + 32].hex())
	print("=== provenance.json ===")
	print(json.dumps(provenance, indent="\t"))


if __name__ == "__main__":
	main()
