"""Deterministic offline inputs and equivalent legacy-command adapters."""

from __future__ import annotations

from dataclasses import dataclass, field
import functools
import hashlib
import http.server
import json
import os
from pathlib import Path
import random
import shutil
import subprocess
import threading
import time

from process import Cgroup, absolute_program, digest, start_background


CONFIG = "\n".join([
	"experimental-features = nix-command", "build-users-group =", "substituters =",
	"sandbox = true", "sandbox-fallback = false", "sandbox-paths =", "cores = 1",
	"max-jobs = 1", "system-features = benchmark big-parallel kvm nixos-test uid-range",
	"connect-timeout = 2", "download-attempts = 1", "show-trace = false", "",
])


def environment(home: Path, config: str = CONFIG) -> dict[str, str]:
	for name in ["conf", "cache", "config", "state", "data", "tmp"]:
		(home / name).mkdir(parents=True, exist_ok=True)
	return {
		"HOME": str(home), "NIX_CONF_DIR": str(home / "conf"), "NIX_CONFIG": config,
		"NIX_USER_CONF_FILES": str(home / "conf/nonexistent"), "NIX_PATH": "",
		"NIX_REMOTE": "dummy://", "NIX_STORE_DIR": "/nix/store",
		"XDG_CACHE_HOME": str(home / "cache"), "XDG_CONFIG_HOME": str(home / "config"),
		"XDG_CONFIG_DIRS": str(home / "conf"), "XDG_STATE_HOME": str(home / "state"),
		"XDG_DATA_HOME": str(home / "data"), "TMPDIR": str(home / "tmp"),
		"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C", "TZ": "UTC",
	}


def remove_tree(path: Path) -> None:
	"""Remove only runner-owned roots, including canonical read-only store dirs."""
	if not path.exists():
		return
	for directory, _, _ in os.walk(path, followlinks=False):
		os.chmod(directory, 0o700)
	def fix(function, filename, _error):
		os.chmod(filename, 0o700)
		function(filename)
	shutil.rmtree(path, onerror=fix)


def make_tree(path: Path, count: int, payload_bytes: int, seed: int) -> dict:
	path.mkdir(parents=True)
	rng = random.Random(seed)
	for i in range(count):
		folder = path / f"d{i % 16:02d}"
		folder.mkdir(exist_ok=True)
		file = folder / f"f{i:06d}"
		file.write_bytes(rng.randbytes(payload_bytes))
		if i % 7 == 0:
			file.chmod(0o755)
	(path / "link").symlink_to("d00/f000000")
	(path / "empty").mkdir()
	return {"generator_version": 1, "seed": seed, "files": count,
		"payload_bytes": count * payload_bytes, "tree_sha256": tree_digest(path)}


def tree_digest(root: Path) -> str:
	hash_ = hashlib.sha256()
	for path in sorted(root.rglob("*"), key=lambda p: os.fsencode(str(p.relative_to(root)))):
		name = os.fsencode(str(path.relative_to(root)))
		if path.is_symlink():
			kind, data = b"link", os.fsencode(os.readlink(path))
		elif path.is_dir():
			kind, data = b"dir", b""
		else:
			kind = b"exe" if path.stat().st_mode & 0o100 else b"file"
			data = bytes.fromhex(digest(path))
		for value in [name, kind, data]:
			hash_.update(len(value).to_bytes(8, "little"))
			hash_.update(value)
	return hash_.hexdigest()


# The derivation paths the reference computed for the qualification system,
# its VM, its Home Manager module generation and a standalone generation.
QUALIFICATION_DRVS = [
	"/nix/store/f7rbcys5ki5q0bvnalm75sq15cfn7n3b-nixos-system-nixrs-reference-26.11pre-git.drv",
	"/nix/store/d111q3a19x7bfpw1hrry7dcyx60awhly-nixos-vm.drv",
	"/nix/store/ks2l014wl7n9pm5h58y94wj03mbbmjin-home-manager-generation.drv",
	"/nix/store/09p2niv7bmc293ji0wsdqdihgia3jbjc-home-manager-generation.drv",
]


@functools.cache
def qualification_inputs() -> tuple[str, str]:
	"""Locked Nixpkgs and Home Manager sources of the qualification flake, resolved by the host's Nix."""
	flake = Path(__file__).resolve().parent.parent / "tests/qualification"
	def source(name: str) -> str:
		expr = f'(builtins.getFlake "path:{flake}").inputs.{name}.outPath'
		return subprocess.run(["nix", "eval", "--raw", "--impure", "--expr", expr], check=True,
			stdout=subprocess.PIPE, timeout=600).stdout.decode()
	return source("nixpkgs"), source("home-manager")


def evaluation(case: str, size: int) -> tuple[str, object]:
	attrs = " ".join(f"a{i:06d} = {i};" for i in range(size))
	if case in {"EVAL-01", "START-03"}:
		return '{ answer = 42; nested = [ 1 2 3 ]; }', {"answer": 42, "nested": [1, 2, 3]}
	if case == "EVAL-02":
		return f'let a = {{ {attrs} }}; in {{ count = builtins.length (builtins.attrNames a); last = a.a{size - 1:06d}; }}', {"count": size, "last": size - 1}
	if case == "EVAL-03":
		return f'let a = {{ {attrs} }}; in [ (a // {{ extra = 7; }}) (builtins.intersectAttrs {{ a000000 = 0; }} a) ]', [{**{f"a{i:06d}": i for i in range(size)}, "extra": 7}, {"a000000": 0}]
	if case == "EVAL-04":
		return f'let xs = builtins.genList (i: {size} - i) {size}; in {{ sorted = builtins.sort builtins.lessThan xs; sum = builtins.foldl\' (a: b: a + b) 0 xs; }}', {"sorted": list(range(1, size + 1)), "sum": size * (size + 1) // 2}
	if case == "EVAL-05":
		return f'let shared = builtins.genList (i: i) {size}; in builtins.genList (_: builtins.length shared) {size}', [size] * size
	if case == "EVAL-06":
		return f'builtins.genList (i: (import ./shared.nix) + i) {size}', list(range(42, 42 + size))
	raise ValueError(f"no evaluation fixture for {case}")


class CacheServer:
	def __init__(self, directory: Path):
		self.lock = threading.Lock()
		self.requests = []
		self.body_bytes = 0
		self.latency_s = 0.0
		self.fault: str | None = None
		owner = self
		class Handler(http.server.SimpleHTTPRequestHandler):
			protocol_version = "HTTP/1.1"
			def copyfile(self, source, output):
				while data := source.read(65536):
					output.write(data)
					with owner.lock:
						owner.body_bytes += len(data)
			def log_message(self, *_args):
				pass
			def do_GET(self):
				with owner.lock:
					owner.requests.append(self.path)
				if owner.latency_s:
					time.sleep(owner.latency_s)
				if owner.fault == "unavailable" and self.path.endswith(".narinfo"):
					self.send_error(503)
				else:
					super().do_GET()
		self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Handler, directory=str(directory)))
		self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
		self.thread.start()
		self.url = f"http://127.0.0.1:{self.server.server_port}"

	def reset(self):
		with self.lock:
			self.requests.clear()
			self.body_bytes = 0

	def counters(self):
		with self.lock:
			paths = list(self.requests)
			body_bytes = self.body_bytes
		return {"http_requests": len(paths), "narinfo_requests": sum(p.endswith(".narinfo") for p in paths), "request_paths": paths,
			"successful_file_response_body_bytes": body_bytes}

	def close(self):
		self.server.shutdown()
		self.server.server_close()
		self.thread.join()


@dataclass
class Operation:
	program: str
	args: list[str]
	env: dict[str, str]
	cwd: Path
	oracle: str = "reference-bytes"
	expected: object = None
	expected_exit: int = 0
	root: Path | None = None
	paths: list[str] = field(default_factory=list)
	group: Cgroup | None = None
	daemon: subprocess.Popen | None = None


# These are concrete subsets/size variants of the broader catalog rows.
# Unsupported dimensions remain listed in each fixture's metadata.
ADAPTERS = {
	"START-01", "START-03", "START-04",
	"EVAL-01", "EVAL-02", "EVAL-03", "EVAL-04", "EVAL-05", "EVAL-06", "EVAL-08",
	"FETCH-01", "PLAN-01", "DAG-01",
	"CACHE-01", "CACHE-02", "CACHE-03", "CACHE-06", "CACHE-08", "CACHE-10",
	"NAR-01", "NAR-02", "NAR-03", "NAR-04", "NAR-05",
	"DB-01", "DB-02", "DB-06", "BUILD-01", "BUILD-03", "BUILD-06", "BUILD-07",
	"RPC-01", "PROFILE-06", "GC-01", "GC-02", "GC-05", "GC-06",
	"FAIL-02", "FAIL-03",
}


class Fixtures:
	def __init__(self, root: Path, reference: Path, busybox: Path | None, size: int, seed: int, timeout: float, case: str):
		self.root, self.reference, self.size, self.seed, self.timeout = root, reference, size, seed, timeout
		self.case = case
		self.busybox = busybox.resolve() if busybox else None
		self.home = root / "fixture-home"
		self.env = environment(self.home)
		self.seed_store = root / "seed-store"
		self.paths: list[str] = []
		self.drvs: list[str] = []
		self.outputs: list[str] = []
		self.seeds: dict[str, Path] = {}
		self.cache_server: CacheServer | None = None
		self.metadata: dict = {"generator_version": 1, "seed": seed, "size": size,
			"page_cache": "uncontrolled", "process_state": "cold CLI", "build_jobs": 1, "build_cores": 1}
		root.mkdir(parents=True, exist_ok=True)
		self.tree = root / "tree"
		count, payload = (1, size * 1024 * 1024) if case in {"NAR-03", "NAR-05", "CACHE-01", "CACHE-08"} else (size, 256)
		self.metadata["tree"] = make_tree(self.tree, count, payload, seed)
		(root / "shared.nix").write_text("42\n")

	def ref(self, program: str, args: list[str], env: dict | None = None, input_: bytes | None = None) -> bytes:
		result = subprocess.run([str(absolute_program(self.reference / program)), *args],
			env=env or self.env, cwd=self.root, input=input_, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
			timeout=self.timeout)
		if result.returncode:
			raise RuntimeError(f"fixture {program} failed: {result.stderr.decode(errors='replace')[-4000:]}")
		return result.stdout

	def store_uri(self, path: Path) -> str:
		return f"local?root={path}"

	def ensure_store(self):
		if self.paths:
			return
		self.seed_store.mkdir()
		if self.case in {"CACHE-01", "CACHE-08"}:
			self.paths = self.ref("nix-store", ["--store", self.store_uri(self.seed_store), "--add", str(self.tree)]).decode().splitlines()
			self.metadata["store_seed"] = {"paths": self.paths, "shape": "single tree", "helper_engine": "CppNix", "timed_work": "substitution only"}
			return
		for i in range(self.size):
			# A chain with one shared predecessor creates known metadata/closure work.
			text = f'"payload-{self.seed}-{i}"'
			if self.paths:
				refs = self.paths if self.case in {"CACHE-02", "CACHE-10"} and i == self.size - 1 else self.paths[-1:]
				if self.case in {"CACHE-02", "CACHE-10"} and i < self.size - 1:
					refs = []
				for reference in refs:
					text += f' + "${{builtins.storePath {reference}}}"'
			expr = f'builtins.toFile "node-{i:06d}" ({text})'
			value = self.ref("nix-instantiate", ["--store", self.store_uri(self.seed_store), "--eval", "--strict", "--json", "--read-write-mode", "--expr", expr])
			self.paths.append(json.loads(value))
		self.metadata["store_seed"] = {"paths": self.paths, "shape": "wide" if self.case in {"CACHE-02", "CACHE-10"} else "chain", "helper_engine": "CppNix", "timed_work": "commands against preseeded stores"}

	def ensure_builds(self, kind: str):
		if self.drvs:
			return
		if not self.busybox:
			raise FileNotFoundError("static BusyBox is required; pass --busybox target/test-busybox")
		self.metadata["busybox_sha256"] = digest(self.busybox / "bin/busybox")
		self.ensure_store()
		# The host daemon is a read-only source for this untimed fixture copy.
		self.ref("nix", ["copy", "--store", "daemon", "--no-check-sigs", "--to", self.store_uri(self.seed_store), str(self.busybox)])
		nodes = []
		count = self.size if kind in {"PLAN-01", "DAG-01", "DB-06"} else 1
		for i in range(count):
			dependency = f' + "\n# ${{n{i - 1}}}"' if i else ""
			script = f"printf payload-{i} > $out"
			options = ""
			if kind == "BUILD-03":
				script = f'{self.busybox}/bin/mkdir $out; i=0; while [ $i -lt {self.size} ]; do printf payload > $out/f$i; i=$((i+1)); done'
			if kind == "BUILD-06":
				script = f'i=0; while [ $i -lt {self.size} ]; do echo log-line-$i; i=$((i+1)); done; printf payload > $out'
			if kind == "BUILD-07":
				options = 'outputHashMode = "recursive"; outputHashAlgo = "sha256"; outputHash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";'
			nodes.append(f'n{i} = derivation {{ name = "bench-{i:06d}"; system = "x86_64-linux"; builder = "${{builtins.storePath {self.busybox}}}/bin/sh"; args = [ "-c" ({json.dumps(script)}{dependency}) ]; {options} }};')
		expr = 'let ' + " ".join(nodes) + ' in [ ' + " ".join(f'n{i}' for i in range(count)) + ' ]'
		(self.root / "graph.nix").write_text(expr)
		self.drvs = self.ref("nix-instantiate", ["--store", self.store_uri(self.seed_store), "--no-gc-warning", str(self.root / "graph.nix")]).decode().splitlines()
		self.outputs = self.ref("nix-store", ["--store", self.store_uri(self.seed_store), "--query", "--outputs", *self.drvs]).decode().splitlines()
		self.metadata["build_graph"] = {"shape": "chain", "derivations": self.drvs, "outputs": self.outputs, "busybox": str(self.busybox), "helper_engine": "CppNix", "timed_work": "realization/planning only", "kind": kind}

	def ensure_cache(self, compression: str = "none"):
		if self.cache_server:
			return
		if self.case == "FAIL-02":
			self.ensure_builds("BUILD-01")
			self.ref("nix-store", ["--store", self.store_uri(self.seed_store), "--realise", "--no-gc-warning", self.drvs[-1]])
			self.paths = self.outputs.copy()
		else:
			self.ensure_store()
		if self.busybox:
			self.metadata["busybox_sha256"] = digest(self.busybox / "bin/busybox")
		secret = self.ref("nix", ["key", "generate-secret", "--key-name", "benchmark-fixture"])
		self.public_key = self.ref("nix", ["key", "convert-secret-to-public"], input_=secret).decode().strip()
		secret_path = self.root / "secret-key"
		secret_path.write_bytes(secret)
		secret_path.chmod(0o600)
		cache = self.root / "cache"
		self.ref("nix", ["copy", "--store", self.store_uri(self.seed_store), "--to", f"file://{cache}?compression={compression}&secret-key={secret_path}", self.paths[-1]])
		secret_path.unlink()
		self.cache_server = CacheServer(cache)
		self.metadata["cache"] = {"compression": compression, "public_key": self.public_key,
			"fixture_server": "Python ThreadingHTTPServer HTTP/1.1 on loopback", "server_resources": "not included in client cgroup", "seed_paths": self.paths}

	def clone(self, target: Path):
		self.ensure_store()
		shutil.copytree(self.seed_store, target, symlinks=True)

	def prepare(self, case: str, state: Path, engine: Path, launcher: Path, variant: str = "default") -> Operation:
		state.mkdir(parents=True)
		env = environment(state / "home")
		root = state / "store"
		if case == "START-01":
			return Operation("nix-store", ["--version"], env, self.root, "version")
		if case == "EVAL-08":
			nixpkgs, home_manager = qualification_inputs()
			self.metadata["qualification_inputs"] = {"nixpkgs": nixpkgs, "home-manager": home_manager}
			file = Path(__file__).resolve().parent / "eval-08.nix"
			self.metadata["expression_sha256"] = digest(file)
			return Operation("nix-instantiate", ["--eval", "--strict", "--json", str(file), "--argstr", "nixpkgs", nixpkgs,
				"--argstr", "homeManager", home_manager], env, self.root, "json", QUALIFICATION_DRVS)
		if case.startswith("EVAL-") or case == "START-03":
			expr, expected = evaluation(case, self.size)
			file = self.root / f"{case}.nix"
			file.write_text(expr)
			return Operation("nix-instantiate", ["--eval", "--strict", "--json", str(file)], env, self.root, "json", expected)
		if case in {"NAR-01", "NAR-02", "NAR-03", "NAR-04"}:
			root.mkdir()
			path = self.ref("nix-store", ["--store", self.store_uri(root), "--add", str(self.tree)]).decode().strip()
			# CppNix dumps a physical filesystem path; nix-rs maps a logical store path.
			dump_path = str(root / path.lstrip("/")) if engine == self.reference else path
			return Operation("nix-store", ["--store", self.store_uri(root), "--dump", dump_path], env, self.root)
		if case in {"FETCH-01", "NAR-05"}:
			root.mkdir()
			expr = f'builtins.path {{ path = {self.tree}; name = "benchmark-source"; }}'
			return Operation("nix-instantiate", ["--store", self.store_uri(root), "--eval", "--read-write-mode", "--strict", "--json", "--expr", expr], env, self.root, "import", root=root)
		if case.startswith("CACHE-") or case in {"FAIL-02", "FAIL-03"}:
			self.ensure_cache(variant if case == "CACHE-08" else "none")
			root.mkdir()
			self.cache_server.reset()
			self.cache_server.latency_s = 0.02 if case == "CACHE-03" else 0
			self.cache_server.fault = "unavailable" if case == "FAIL-03" else None
			url = self.cache_server.url if case != "CACHE-10" or variant != "file" else f"file://{self.root / 'cache'}"
			if case == "CACHE-06":
				url = f"{url}/missing {url}"
			config = CONFIG + f"trusted-public-keys = {self.public_key}\nsubstituters = {url}\n"
			env = environment(state / "home", config)
			if case == "FAIL-02":
				# Rotate the trusted key rather than corrupting the shared seed.
				env = environment(state / "home", CONFIG + f"trusted-public-keys =\nsubstituters = {url}\n")
			return Operation("nix-store", ["--store", self.store_uri(root), "--realise", "--no-gc-warning", self.paths[-1]], env, self.root,
				"failure" if case.startswith("FAIL") else "closure", expected_exit=1 if case.startswith("FAIL") else 0, root=root, paths=self.paths)
		if case.startswith("PLAN-") or case.startswith("DAG-") or case.startswith("BUILD-") or case == "DB-06":
			self.ensure_builds(case)
			self.clone(root)
			if case == "PLAN-01":
				return Operation("nix-store", ["--store", self.store_uri(root), "--realise", "--dry-run", "--no-gc-warning", self.drvs[-1]], env, self.root, "dry-run", root=root, paths=self.outputs)
			if case == "DB-06":
				return Operation("nix-store", ["--store", self.store_uri(root), "--query", "--outputs", *self.drvs], env, self.root)
			if case == "BUILD-07":
				return Operation("nix-store", ["--store", self.store_uri(root), "--realise", "--no-gc-warning", self.drvs[-1]], env, self.root, "hash-failure", expected_exit=102, root=root, paths=self.outputs)
			return Operation("nix-store", ["--store", self.store_uri(root), "--realise", "--no-gc-warning", self.drvs[-1]], env, self.root, "build", root=root, paths=self.outputs)
		self.clone(root)
		if case == "RPC-01":
			server_role = variant
			server_bin = self.server_bins[server_role]
			daemon_program = absolute_program(server_bin / "nix-daemon")
			if not daemon_program.exists():
				 raise FileNotFoundError(f"missing {server_role} daemon {daemon_program}")
			group = Cgroup()
			self.metadata["rpc_server"] = {"role": server_role, "program": str(daemon_program), "sha256": digest(daemon_program), "resources": "included with client; memory peak includes daemon startup"}
			socket = state / "daemon.sock"
			daemon_env = dict(env, NIX_DAEMON_SOCKET_PATH=str(socket))
			daemon = start_background(launcher, [str(daemon_program), "--store", self.store_uri(root)], daemon_env, self.root, state / "daemon-log", group)
			deadline = time.monotonic() + self.timeout
			while not socket.exists() and daemon.poll() is None and time.monotonic() < deadline:
				time.sleep(0.005)
			if not socket.exists():
				group.close()
				daemon.wait(timeout=5)
				raise RuntimeError("daemon did not create its socket")
			return Operation("nix-store", ["--store", f"unix://{socket}?root={root}", "--query", "--hash", *self.paths], env, self.root, group=group, daemon=daemon)
		if case == "START-04":
			return Operation("nix-store", ["--store", self.store_uri(root), "--query", "--size", self.paths[-1]], env, self.root)
		if case == "DB-01":
			return Operation("nix-store", ["--store", self.store_uri(root), "--query", "--hash", *self.paths], env, self.root)
		if case in {"DB-02", "PLAN-02"}:
			return Operation("nix-store", ["--store", self.store_uri(root), "--query", "--requisites", self.paths[-1]], env, self.root, "path-set", sorted(self.paths))
		if case == "NAR-06":
			return Operation("nix-store", ["--store", self.store_uri(root), "--verify", "--check-contents"], env, self.root, "verify", root=root, paths=self.paths)
		if case == "GC-01":
			return Operation("nix-store", ["--store", self.store_uri(root), "--gc", "--print-dead"], env, self.root, "dead-set", sorted(self.paths))
		if case == "GC-02":
			return Operation("nix-store", ["--store", self.store_uri(root), "--gc"], env, self.root, "gc", root=root, paths=self.paths)
		if case == "GC-05":
			return Operation("nix-store", ["--store", self.store_uri(root), "--verify", "--check-contents"], env, self.root, "verify", root=root, paths=self.paths)
		if case == "GC-06":
			return Operation("nix-store", ["--store", self.store_uri(root), "--optimise"], env, self.root, "verify", root=root, paths=self.paths)
		if case == "PROFILE-06":
			profile = state / "profile"
			for path in self.paths[:2]:
				result = subprocess.run([str(absolute_program(engine / "nix-env")), "--store", self.store_uri(root), "--profile", str(profile), "--set", path], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=self.timeout)
				if result.returncode:
					raise RuntimeError(result.stderr.decode(errors="replace"))
			return Operation("nix-env", ["--store", self.store_uri(root), "--profile", str(profile), "--rollback"], env, self.root, "profile", {"profile": str(profile), "target": self.paths[0]}, root=root)
		raise ValueError(f"no adapter for {case}")

	def close(self):
		if self.cache_server:
			self.cache_server.close()
