#!/usr/bin/env python3
"""Record CppNix 2.35.2 ATerm derivation oracles in a disposable chroot store.

Two kinds of case are recorded in cases.json:

- "json": a version-4 derivation JSON is given to `nix derivation add`; the
  printed path, the exact ATerm bytes written to the store and the
  `nix derivation show` JSON are recorded.
- "aterm": raw ATerm bytes are added with `nix-store --add` under a `.drv`
  name, which makes the reference parse them. The add result is recorded,
  and for accepted input, `nix derivation show` with every derivation
  feature enabled and with only `nix-command`.

Run inside the devenv shell from the repository root:

	python3 crates/compat-tests/fixtures/derivation-reference/generate.py \
		--reference /nix/store/...-nix-2.35.2/bin > /dev/null
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

ALL = "nix-command ca-derivations dynamic-derivations impure-derivations git-hashing"
DEFAULT = "nix-command"
HERE = Path(__file__).resolve().parent


def sri(algo, hex_digest):
	import base64

	return f"{algo}-" + base64.b64encode(bytes.fromhex(hex_digest)).decode()


SHA256_ABC = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
SHA1_ABC = "a9993e364706816aba3e25717850c26c9cd0d89d"
MD5_ABC = "900150983cd24fb0d6963f7d28e17f72"


def drv(name, outputs, env=None, srcs=(), drvs=None, args=("-c", "true"), extra=None):
	value = {
		"name": name,
		"version": 4,
		"outputs": outputs,
		"system": "x86_64-linux",
		"builder": "/bin/sh",
		"args": list(args),
		"env": env if env is not None else {k: "" for k in outputs},
		"inputs": {"srcs": list(srcs), "drvs": drvs or {}},
	}
	value.update(extra or {})
	return value


# Cases that refer to earlier ones use "@<case>" for that case's path.
JSON_CASES = [
	(
		"floating-nar-escapes",
		drv(
			"floating",
			{
				"out": {"method": "nar", "hashAlgo": "sha256"},
				"dev": {"method": "nar", "hashAlgo": "sha256"},
			},
			env={"out": "", "dev": "", "q": "a\"b\\c\nd\te\rf", "u": "café ✓"},
		),
	),
	(
		"floating-flat-sha512",
		drv("floating-flat", {"out": {"method": "flat", "hashAlgo": "sha512"}}),
	),
	(
		"floating-text",
		drv("producer.drv", {"out": {"method": "text", "hashAlgo": "sha256"}}),
	),
	(
		"floating-git",
		drv("floating-git", {"out": {"method": "git", "hashAlgo": "sha1"}}),
	),
	(
		"impure",
		drv("impure", {"out": {"method": "nar", "hashAlgo": "sha256", "impure": True}}),
	),
	(
		"fixed-text",
		drv("fixed-text", {"out": {"method": "text", "hash": sri("sha256", SHA256_ABC)}}),
	),
	(
		"fixed-flat-md5",
		drv("fixed-flat", {"out": {"method": "flat", "hash": sri("md5", MD5_ABC)}}),
	),
	(
		"fixed-nar-sha1",
		drv("fixed-nar", {"out": {"method": "nar", "hash": sri("sha1", SHA1_ABC)}}),
	),
	(
		"input-addressed",
		drv("input-addressed", {"out": {}, "lib": {}}, env={"out": "", "lib": "", "x": "1"}),
	),
	(
		"fixed-structured",
		drv(
			"fixed-structured",
			{"out": {"method": "flat", "hash": sri("md5", MD5_ABC)}},
			env={},
			extra={
				"structuredAttrs": {
					"float": 0.1,
					"big": 1e100,
					"small": 1.5e-7,
					"negzero": -0.0,
					"int": 12345678901234567,
					"neg": -42,
					"text": "café ✓ \"q\" \\ \n",
					"list": [True, False, None, 1.0, 2.5],
					"nested": {"b": 1, "a": {"y": [], "x": {}}},
				}
			},
		),
	),
	(
		"deferred",
		drv(
			"deferred",
			{"out": {}},
			drvs={"@floating-nar-escapes": {"outputs": ["out", "dev"], "dynamicOutputs": {}}},
		),
	),
	(
		"dynamic-input",
		drv(
			"dynamic",
			{"out": {"method": "nar", "hashAlgo": "sha256"}},
			drvs={
				"@floating-text": {
					"outputs": [],
					"dynamicOutputs": {"out": {"outputs": ["out"], "dynamicOutputs": {}}},
				}
			},
		),
	),
]

P = "/nix/store/g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q"
H = "0000000000000000000000000000000000000000000000000000000000000000"
ATERM_CASES = [
	("floating-nar", f'Derive([("out","","r:sha256","")],[],[],"s","b",[],[("out","")])'),
	(
		"quirks",
		'Derive([("out","","r:sha256",""),("out","/nix/store/g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-x","","")],'
		'[],[],"s","b",[,"a""b"],[("K","1")("K","2")])trailing',
	),
	("versioned-without-dynamic", 'DrvWithVersion("xp-dyn-drv",[("out","","","")],[],[],"s","b",[],[])'),
	("unknown-version", 'DrvWithVersion("v2",[],[],[],"s","b",[],[])'),
	("not-a-derivation", 'Dx'),
	("unterminated", 'Derive([("out","","","),'),
	("floating-with-path", f'Derive([("out","{P}-x","sha256","")],[],[],"s","b",[],[])'),
	("impure-with-path", f'Derive([("out","{P}-x","r:sha256","impure")],[],[],"s","b",[],[])'),
	("fixed-relative-path", f'Derive([("out","x","sha256","{H}")],[],[],"s","b",[],[])'),
	("unknown-hash-algorithm", 'Derive([("out","","r:sha3","")],[],[],"s","b",[],[])'),
	("fixed-wrong-length", f'Derive([("out","/x","sha256","abc")],[],[],"s","b",[],[])'),
	(
		"fixed-nix32",
		'Derive([("out","/anything","r:sha256","1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s")],[],[],"s","b",[],[])',
	),
	(
		"fixed-base64",
		'Derive([("out","/anything","sha256","ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=")],[],[],"s","b",[],[])',
	),
	(
		"fixed-upper-hex",
		f'Derive([("out","/anything","sha256","{SHA256_ABC.upper()}")],[],[],"s","b",[],[])',
	),
	(
		"output-path-canonicalised",
		'Derive([("out","/nix//store/./g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-x/","","ignored")],[],[],"s","b",[],[])',
	),
	("source-outside-store", 'Derive([("out","","","")],[],["/tmp/x"],"s","b",[],[])'),
	("source-relative", 'Derive([("out","","","")],[],["tmp/x"],"s","b",[],[])'),
	("source-bad-name", f'Derive([("out","","","")],[],["{P}-..-a"],"s","b",[],[])'),
	("source-bad-hash", 'Derive([("out","","","")],[],["/nix/store/e1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-x"],"s","b",[],[])'),
	("source-short", 'Derive([("out","","","")],[],["/nix/store/abc"],"s","b",[],[])'),
	(
		"dynamic-invalid-input-entry",
		f'DrvWithVersion("xp-dyn-drv",[("out","","r:sha256","")],[("{P}-x.drv","out")],[],"s","b",[],[])',
	),
	(
		"traditional-dynamic-input",
		f'Derive([("out","","r:sha256","")],[("{P}-x.drv",(["out"],[]))],[],"s","b",[],[])',
	),
	("text-floating", 'Derive([("out","","text:sha256","")],[],[],"s","b",[],[])'),
	("git-floating", 'Derive([("out","","git:sha1","")],[],[],"s","b",[],[])'),
	("blake3-floating", 'Derive([("out","","r:blake3","")],[],[],"s","b",[],[])'),
	("escapes", 'Derive([("out","","","")],[],[],"s","b",["\\a\\n\\t\\r\\\\\\""],[])'),
	("missing-comma", 'Derive([("out","","","")][],[],"s","b",[],[])'),
	("empty-outputs", 'Derive([],[],[],"s","b",[],[])'),
	("structured-invalid", 'Derive([("out","","","")],[],[],"s","b",[],[("__json","{")])'),
]


def run(args, stdin=None, features=ALL):
	env = {
		"HOME": os.environ["NOXI_GEN_HOME"],
		"PATH": "/usr/bin:/bin",
		"NIX_CONFIG": f"experimental-features = {features}",
		"NO_COLOR": "1",
	}
	proc = subprocess.run(args, input=stdin, capture_output=True, env=env)
	return {
		"exit_code": proc.returncode,
		"stdout": proc.stdout.decode(),
		"stderr": proc.stderr.decode(),
	}


def main():
	parser = argparse.ArgumentParser()
	parser.add_argument("--reference", required=True, help="the reference's bin directory")
	opts = parser.parse_args()
	nix = os.path.join(opts.reference, "nix")
	nix_store = os.path.join(opts.reference, "nix-store")

	with tempfile.TemporaryDirectory(prefix="drv-oracles-") as tmp:
		os.environ["NOXI_GEN_HOME"] = os.path.join(tmp, "home")
		os.mkdir(os.environ["NOXI_GEN_HOME"])
		root = os.path.join(tmp, "root")
		store = ["--store", f"local?root={root}"]
		paths = {}
		json_cases = []
		for name, value in JSON_CASES:
			text = json.dumps(value)
			for case, path in paths.items():
				text = text.replace(f'"@{case}"', json.dumps(os.path.basename(path)))
			added = run([nix, "derivation", "add", *store], stdin=text.encode())
			case = {"name": name, "input": json.loads(text), "add": added}
			if added["exit_code"] == 0:
				path = added["stdout"].strip()
				paths[name] = path
				case["aterm"] = Path(root + path).read_text()
				case["show"] = run([nix, "derivation", "show", *store, path])
			json_cases.append(case)

		aterm_cases = []
		for name, text in ATERM_CASES:
			source = os.path.join(tmp, "src", name)
			os.makedirs(source)
			file = os.path.join(source, f"{name}.drv")
			Path(file).write_bytes(text.encode())
			added = run([nix_store, *store, "--add", file])
			case = {"name": name, "aterm": text, "add": added}
			if added["exit_code"] == 0:
				path = added["stdout"].strip()
				case["show_all"] = run([nix, "derivation", "show", *store, path])
				case["show_default"] = run(
					[nix, "derivation", "show", *store, path], features=DEFAULT
				)
			aterm_cases.append(case)

		version = run([nix, "--version"])["stdout"].strip()

	result = {
		"reference_release": "2.35.2",
		"source_revision": "2c73b59da29606068c0c98db015dd3a66955525d",
		"source_revision_evidence": "flake.lock nodes.nix-reference.locked.rev",
		"platform": "x86_64-linux",
		"version_output": version,
		"generation_command": "python3 crates/compat-tests/fixtures/derivation-reference/generate.py --reference <nix-2.35.2>/bin",
		"store": "local?root=<temporary directory>, logical store directory /nix/store",
		"features": {"all": ALL, "default": DEFAULT},
		"json_cases": json_cases,
		"aterm_cases": aterm_cases,
	}
	(HERE / "cases.json").write_text(json.dumps(result, indent="\t", ensure_ascii=False) + "\n")


if __name__ == "__main__":
	main()
