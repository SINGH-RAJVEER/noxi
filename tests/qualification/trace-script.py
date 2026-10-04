# Driven by tests/qualification/trace.nix. Runs each scenario between phase
# markers, then decodes the kernel's execve audit records into the Nix
# commands each phase ran and who ran them.

import json
import re
from dataclasses import asdict, dataclass, field

MARKER = "nixrs-trace-phase"
# A Nix executable from any package, the reference or not.
NIX_EXE = re.compile(r"^/nix/store/[a-z0-9]{32}-(?:nix|lix)-[0-9][^/]*/(?:bin|libexec)/")
STORE_HASH = re.compile(r"/nix/store/[a-z0-9]{32}-")


def phase(name: str) -> None:
	machine.succeed(f"/run/current-system/sw/bin/true {MARKER} {name}")


def run(name: str, command: str) -> None:
	phase(name)
	status, output = machine.execute(command + " 2>&1")
	results[name] = Result(command, status, output)
	print(f"{name}: exit {status}\n{output}")


@dataclass
class Result:
	command: str
	status: int
	output: str


@dataclass
class Call:
	serial: int
	pid: str
	ppid: str
	uid: str
	exe: str
	argv: list[str]
	phase: str = ""
	parents: list[str] = field(default_factory=list)


results: dict[str, Result] = {}

start_all()
machine.wait_for_unit("multi-user.target")
machine.wait_for_unit("auditd.service")
machine.wait_for_unit("home-manager-qualification.service")

# Test machines boot without a system profile. Record the booted system as
# generation 1 so the switch makes generation 2 and the rollback has a target.
run("setup", "nix-env -p /nix/var/nix/profiles/system --set /run/current-system")
# /etc/qual is a symlink, which a `path:` flake reference must not be.
FLAKE = '--flake "path:$(readlink -f /etc/qual)#trace-target"'
run("rebuild-switch", f"nixos-rebuild switch {FLAKE}")
switched = machine.execute("test -e /etc/nixrs-trace-generation")[0] == 0
run("home-manager-restart", "systemctl restart home-manager-qualification.service")
run("nix-gc-service", "systemctl start --wait nix-gc.service")
# The guest's store overlays the host's, so a full optimisation would run for
# minutes. The trace needs only the command the unit runs.
run(
	"nix-optimise-service",
	"systemctl start nix-optimise.service"
	" && until pgrep -x nix-store; do sleep 0.1; done"
	" && systemctl stop nix-optimise.service",
)
run("list-generations", "nixos-rebuild list-generations")
run("rollback", f"nixos-rebuild switch --rollback {FLAKE}")
run("collect-garbage", "nix-collect-garbage -d")
phase("end")

machine.copy_from_machine("/var/log/audit/audit.log")
audit = (machine.out_dir / "audit.log").read_text()


def decode(value: str) -> str:
	if value.startswith('"'):
		return value[1:-1]
	return bytes.fromhex(value).decode(errors="replace")


# Group records by event serial; an execve event has EXECVE and SYSCALL records.
events: dict[str, dict[str, str]] = {}
order: list[str] = []
record = re.compile(r"^type=(\w+) msg=audit\([0-9.]+:(\d+)\): (.*)$")
for line in audit.splitlines():
	match = record.match(line)
	if not match:
		continue
	kind, serial, body = match.groups()
	if serial not in events:
		events[serial] = {}
		order.append(serial)
	events[serial][kind] = events[serial].get(kind, "") + " " + body

pair = re.compile(r'(\S+?)=("[^"]*"|\S+)')
calls: list[Call] = []
for serial in order:
	event = events[serial]
	if "EXECVE" not in event or "SYSCALL" not in event:
		continue
	args: dict[int, list[tuple[int, str]]] = {}
	for key, value in pair.findall(event["EXECVE"]):
		part = re.fullmatch(r"a(\d+)(?:\[(\d+)\])?", key)
		if part:
			args.setdefault(int(part[1]), []).append((int(part[2] or 0), decode(value)))
	argv = ["".join(text for _, text in sorted(args[i])) for i in sorted(args)]
	syscall = dict(pair.findall(event["SYSCALL"]))
	calls.append(
		Call(
			serial=int(serial),
			pid=syscall.get("pid", ""),
			ppid=syscall.get("ppid", ""),
			uid=syscall.get("uid", ""),
			exe=decode(syscall.get("exe", '""')),
			argv=argv,
		)
	)

# Attribute each Nix call to its phase and to the commands that started it.
latest: dict[str, Call] = {}
current = "boot"
traced: list[Call] = []
for call in calls:
	if len(call.argv) >= 3 and call.argv[1] == MARKER:
		current = call.argv[2]
		continue
	latest[call.pid] = call
	if not NIX_EXE.match(call.exe):
		continue
	call.phase = current
	pid = call.ppid
	while pid in latest and len(call.parents) < 4:
		parent = latest[pid]
		call.parents.append(parent.argv[0].rsplit("/", 1)[-1] if parent.argv else "?")
		pid = parent.ppid
	traced.append(call)

with open(machine.out_dir / "calls.json", "w") as out:
	report = {
		"results": {name: asdict(result) for name, result in results.items()},
		"calls": [asdict(call) for call in traced],
	}
	json.dump(report, out, indent="\t")


def short(text: str) -> str:
	return STORE_HASH.sub("/nix/store/…-", text)


lines = []
for call in traced:
	argv = " ".join(short(arg) for arg in call.argv)
	lines.append(f"{call.phase}\tuid={call.uid}\t{' <- '.join(call.parents)}\t{short(call.exe)}\t{argv}")
(machine.out_dir / "calls.txt").write_text("\n".join(lines) + "\n")

assert results["rebuild-switch"].status == 0, results["rebuild-switch"].output
assert switched, "the rebuild did not switch to the target generation"
assert any(call.phase == "rebuild-switch" for call in traced), "no Nix calls traced during the rebuild"
