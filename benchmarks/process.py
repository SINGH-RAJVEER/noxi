"""Owned process-tree execution and resource accounting on Linux."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import time
import uuid


def digest(path: Path) -> str:
	with path.open("rb") as stream:
		return hashlib.file_digest(stream, "sha256").hexdigest()


def absolute_program(path: Path) -> Path:
	# Resolve parents but keep argv[0]'s final command symlink intact.
	return path.absolute().parent.resolve() / path.name


def build_launcher(root: Path) -> Path:
	source = root / "benchmarks/exec.c"
	binary = root / "target/benchmarks/bin/bench-exec"
	stamp = binary.with_suffix(".sha256")
	identity = digest(source)
	if not binary.exists() or not stamp.exists() or stamp.read_text() != identity:
		binary.parent.mkdir(parents=True, exist_ok=True)
		subprocess.run(["cc", "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", str(source), "-o", str(binary)], check=True)
		stamp.write_text(identity)
	return binary


class Cgroup:
	"""A disposable child under an already delegated cgroup v2 parent."""

	def __init__(self, parent: Path | None = None):
		if parent is None:
			membership = Path("/proc/self/cgroup").read_text().strip().split("0::")[-1]
			current = Path("/sys/fs/cgroup") / membership.lstrip("/")
			for candidate in [current, *current.parents]:
				control = candidate / "cgroup.subtree_control"
				if control.exists() and os.access(candidate, os.W_OK):
					if {"cpu", "memory", "io"}.issubset(set(control.read_text().split())):
						parent = candidate
						break
		if parent is None:
			raise RuntimeError("no delegated cgroup v2 with cpu, memory, and io controllers; run in a delegated systemd scope")
		self.path = parent / f"noxi-bench-{os.getpid()}-{uuid.uuid4().hex}"
		self.path.mkdir()
		try:
			for required in ["cgroup.kill", "cpu.stat", "memory.peak", "io.stat"]:
				if not (self.path / required).exists():
					raise RuntimeError(f"required cgroup accounting unavailable: {required}")
		except BaseException:
			self.path.rmdir()
			raise

	def snapshot(self) -> dict:
		cpu = dict(line.split() for line in (self.path / "cpu.stat").read_text().splitlines())
		io = {key: 0 for key in ["rbytes", "wbytes", "rios", "wios"]}
		for line in (self.path / "io.stat").read_text().splitlines():
			for field in line.split()[1:]:
				key, value = field.split("=")
				if key in io:
					io[key] += int(value)
		return {
			"cpu_user_s": int(cpu.get("user_usec", 0)) / 1e6,
			"cpu_system_s": int(cpu.get("system_usec", 0)) / 1e6,
			"cpu_total_s": int(cpu.get("usage_usec", 0)) / 1e6,
			"process_tree_peak_bytes": int((self.path / "memory.peak").read_text()),
			"disk_read_bytes": io["rbytes"], "disk_write_bytes": io["wbytes"],
			"disk_read_ops": io["rios"], "disk_write_ops": io["wios"],
			"collector": "cgroup-v2", "memory_includes_page_cache": True,
		}

	def populated(self) -> bool:
		return "populated 1" in (self.path / "cgroup.events").read_text()

	def close(self) -> None:
		if self.populated():
			(self.path / "cgroup.kill").write_text("1")
		deadline = time.monotonic() + 5
		while self.populated() and time.monotonic() < deadline:
			time.sleep(0.01)
		# A sandbox may have created nested groups. Remove only our own tree.
		for path in sorted(self.path.rglob("*"), key=lambda p: len(p.parts), reverse=True):
			if path.is_dir():
				path.rmdir()
		self.path.rmdir()

	def __enter__(self):
		return self

	def __exit__(self, *exc):
		self.close()


def run_process(launcher: Path, argv: list[str], env: dict[str, str], cwd: Path,
		output: Path, timeout: float, group: Cgroup | None = None,
		expect_background: bool = False, stdin: Path | None = None) -> dict:
	"""Redirect to files, reap with wait4, and kill all owned work on timeout."""
	owned = group is None
	group = group or Cgroup()
	output.mkdir(parents=True, exist_ok=True)
	stdout_path, stderr_path = output / "stdout", output / "stderr"
	target_metrics = output / "target-resources.json"
	read_fd, write_fd = os.pipe()
	child = None
	try:
		with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
			with (stdin.open("rb") if stdin else open(os.devnull, "rb")) as input_stream:
				before = group.snapshot()
				spawn_ns = time.monotonic_ns()
				child = subprocess.Popen([str(launcher), str(group.path), str(write_fd), str(target_metrics), "--", *argv],
					env=env, cwd=cwd, stdin=input_stream, stdout=stdout, stderr=stderr,
					pass_fds=(write_fd,), start_new_session=True)
				os.close(write_fd)
				write_fd = -1
				started_ns = None
				deadline = time.monotonic() + timeout
				with selectors.DefaultSelector() as selector:
					selector.register(read_fd, selectors.EVENT_READ)
					if selector.select(timeout):
						with os.fdopen(read_fd, "r") as timing:
							line = timing.readline()
							if line:
								started_ns = int(line)
						read_fd = -1
				timed_out = False
				pid_fd = os.pidfd_open(child.pid)
				try:
					with selectors.DefaultSelector() as selector:
						selector.register(pid_fd, selectors.EVENT_READ)
						if not selector.select(max(0, deadline - time.monotonic())):
							timed_out = True
							(group.path / "cgroup.kill").write_text("1")
							# The launcher might still be outside the group.
							try:
								os.killpg(child.pid, signal.SIGKILL)
							except ProcessLookupError:
								pass
					pid, status, usage = os.wait4(child.pid, 0)
				finally:
					os.close(pid_fd)
				finished_ns = time.monotonic_ns()
				child.returncode = os.waitstatus_to_exitcode(status)
		resources = group.snapshot()
		for key in ["cpu_user_s", "cpu_system_s", "cpu_total_s", "disk_read_bytes", "disk_write_bytes", "disk_read_ops", "disk_write_ops"]:
			resources[key] -= before[key]
		resources["launcher_wait4_max_process_rss_bytes"] = usage.ru_maxrss * 1024
		if target_metrics.exists():
			metrics = json.loads(target_metrics.read_text())
			resources["target_wait4_max_process_rss_bytes"] = metrics["max_rss_bytes"]
			resources["target_wait4_cpu_user_s"] = metrics["cpu_user_s"]
			resources["target_wait4_cpu_system_s"] = metrics["cpu_system_s"]
		resources["wait4_cpu_user_s"] = usage.ru_utime
		resources["wait4_cpu_system_s"] = usage.ru_stime
		remaining = group.populated()
		return {
			"argv": argv, "exit_code": child.returncode, "timed_out": timed_out,
			"launcher_pid": child.pid, "cgroup_path": str(group.path),
			"elapsed_s": (finished_ns - (started_ns or spawn_ns)) / 1e9,
			"spawn_before_exec_s": (started_ns - spawn_ns) / 1e9 if started_ns else None,
			"launcher_ready": started_ns is not None,
			"resources": resources,
			"stdout_sha256": digest(stdout_path), "stderr_sha256": digest(stderr_path),
			"stdout_bytes": stdout_path.stat().st_size, "stderr_bytes": stderr_path.stat().st_size,
			"stdout_path": str(stdout_path), "stderr_path": str(stderr_path),
			"unexpected_survivors": remaining and not expect_background and not timed_out,
		}
	finally:
		if read_fd >= 0:
			os.close(read_fd)
		if write_fd >= 0:
			os.close(write_fd)
		if child is not None and child.returncode is None:
			try:
				(group.path / "cgroup.kill").write_text("1")
				os.killpg(child.pid, signal.SIGKILL)
			except ProcessLookupError:
				pass
			child.wait()
		if owned:
			group.close()


def start_background(launcher: Path, argv: list[str], env: dict[str, str], cwd: Path,
		output: Path, group: Cgroup) -> subprocess.Popen:
	output.mkdir(parents=True, exist_ok=True)
	with (output / "stdout").open("wb") as stdout, (output / "stderr").open("wb") as stderr:
		return subprocess.Popen([str(launcher), str(group.path), "-1", "-", "--", *argv], env=env,
			cwd=cwd, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, start_new_session=True)
