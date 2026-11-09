# Driven by tests/qualification/replacement/test.nix, which defines MODE.
#
# Every scenario step is recorded with its command, exit status and output,
# and a step that cannot run because an earlier one failed is recorded as
# blocked, so one run yields the full list of gaps. When a nixos-rebuild action
# fails, its component commands are run as separate `component-*` steps so the
# scenario can continue and show which part of the command set is missing.
# Those never count towards the action itself.

import json
import shlex

SYSTEM = "/etc/qualification/system.nix"
FLAKE = "/etc/qualification/flake"
PROFILE = "/nix/var/nix/profiles/system"
TIMEOUT = 1800
FAILED_BUILD = (
	"nix-build --no-out-link --expr 'let pkgs = import <nixpkgs> {}; in derivation {"
	' name = "qualification-failing-build"; system = "x86_64-linux"; builder = "${pkgs.bash}/bin/bash";'
	' args = [ "-c" "echo deliberate build failure >&2; exit 1" ]; }\''
)

steps: list[dict] = []
expected: dict = {}
# CppNix outputs the candidate must reproduce, recorded before the swap.
controls: dict[str, str] = {}
PREFETCH = "printf 'noxi qualification\\n' > /tmp/prefetch-input && nix-prefetch-url file:///tmp/prefetch-input"


def record(name: str, status: str, **fields) -> bool:
	step = {"name": name, "status": status, **fields}
	steps.append(step)
	print(json.dumps(step, indent="\t"))
	return status == "pass"


def run(name: str, command: str, expected_status: int = 0, user: str | None = None, check=None) -> bool:
	"""Run a step; `check(stdout)` may add a condition on its output."""
	workdir = '"$HOME"' if user is not None else "/tmp"
	shell = f"cd {workdir} && timeout {TIMEOUT} bash -c {shlex.quote(command)}"
	if user is not None:
		shell = f"su -l {user} -c {shlex.quote(shell)}"
	status, out = machine.execute(f"{shell} 2>/tmp/qualification-stderr")
	err = machine.succeed("cat /tmp/qualification-stderr")
	ok = status == expected_status
	detail = ""
	if ok and check is not None:
		ok, detail = check(out)
	return record(
		name,
		"pass" if ok else "fail",
		command=command,
		user=user or "root",
		exit_status=status,
		expected_status=expected_status,
		stdout=out,
		stderr=err,
		detail=detail,
	)


def verify(name: str, condition: bool, detail: str) -> bool:
	return record(name, "pass" if condition else "fail", detail=detail)


def blocked(name: str, reason: str) -> bool:
	return record(name, "blocked", reason=reason)


def text(command: str) -> str:
	return machine.succeed(command).strip()


def current() -> str:
	return text("readlink -f /run/current-system")


def profile() -> str:
	return text(f"readlink -f {PROFILE}")


def generation(name: str) -> dict:
	return expected["generations"][name]


def last_line(out: str) -> str:
	lines = out.strip().splitlines()
	return lines[-1] if lines else ""


def equals(value: str):
	return lambda out: (last_line(out) == value, f"expected {value}, got {last_line(out)!r}")


def is_generation(name: str) -> tuple[bool, str]:
	now = current()
	return now == generation(name)["out"], f"current system {now}, expected {name} {generation(name)['out']}"


def daemons(label: str, package: str) -> None:
	"""Exactly one daemon unit, and every running daemon from `package`."""
	run(f"{label}-socket-active", "systemctl is-active nix-daemon.socket")
	run(
		f"{label}-daemon-start",
		"systemctl start nix-daemon.service; sleep 2; systemctl is-active nix-daemon.service",
	)
	run(f"{label}-daemon-journal", "journalctl -b -u nix-daemon.service -u nix-daemon.socket --no-pager -n 40")
	exes = text(
		"for pid in $(pgrep -x nix-daemon || true); do readlink -f /proc/$pid/exe || true; done | sort -u"
	).split()
	main_pid = text("systemctl show nix-daemon.service -p MainPID --value")
	expected_exe = text(f"readlink -f {package}/bin/nix-daemon")
	units = text("systemctl list-unit-files 'nix-daemon*' --no-legend | awk '{print $1}' | sort").split()
	verify(
		f"{label}-single-store-daemon",
		units == ["nix-daemon.service", "nix-daemon.socket"] and main_pid != "0"
		and exes == [expected_exe],
		f"units {units}, MainPID {main_pid}, daemon executables {exes}, expected executable {expected_exe}",
	)


def instantiate(label: str, name: str, user: str | None = None) -> bool:
	return run(
		label,
		f"nix-instantiate {SYSTEM} -A {name}.config.system.build.toplevel --add-root /tmp/{label}.drv && readlink -f /tmp/{label}.drv",
		user=user,
		check=equals(generation(name)["drv"]),
	)


def rebuild(label: str, action: str, name: str | None, extra: str = "--no-reexec") -> bool:
	target = f"--file {SYSTEM} --attr {name}" if name else ""
	return run(label, f"nixos-rebuild {action} {target} {extra}")


def components(label: str, name: str, activation: str) -> bool:
	"""nixos-rebuild's own steps, run one at a time with noxi."""
	out = generation(name)["out"]
	ok = instantiate(f"component-{label}-instantiate", name)
	ok = run(f"component-{label}-realise", f"nix-store --realise {generation(name)['drv']}", check=equals(out)) and ok
	if not ok:
		return blocked(f"component-{label}-activate", "evaluation or realization failed; do not activate the host-prebuilt closure")
	if activation in ("switch", "boot"):
		ok = run(f"component-{label}-set-profile", f"nix-env -p {PROFILE} --set {out}") and ok
	ok = run(f"component-{label}-activate", f"{out}/bin/switch-to-configuration {activation}") and ok
	return ok


def reboot(label: str, name: str) -> bool:
	machine.reboot()
	machine.wait_for_unit("multi-user.target")
	booted = text("readlink -f /run/booted-system")
	return verify(
		label,
		booted == generation(name)["out"] if name in expected["generations"] else booted == name,
		f"booted {booted}, expected {name}",
	)


def swap_to_noxi(label: str) -> bool:
	"""CppNix rebuilds the machine into generation `one`, which uses noxi."""
	instantiate("control-cppnix-instantiate-one", "one")
	run(
		"control-cppnix-rebuild-build-one",
		f"cd /tmp && nixos-rebuild build --file {SYSTEM} --attr one --no-reexec && readlink -f result",
		check=equals(generation("one")["out"]),
	)
	run(label, f"nixos-rebuild switch --file {SYSTEM} --attr one --no-reexec")
	# Activation can select the generation and fail starting the daemon.
	# Keep that failure, but probe the selected engine's remaining commands.
	selected = verify(f"{label}-generation", *is_generation("one"))
	run(
		f"{label}-commands-on-path",
		"readlink -f $(command -v nix-instantiate)",
		check=lambda out: (out.strip().startswith(expected["candidate"]), out.strip()),
	)
	run("noxi-version", "nix-instantiate --version", check=equals("nix-instantiate (Nix) 2.35.2"))
	run("noxi-modern-cli", "nix --version")
	run("noxi-config-show", "nix config show --json")
	# Smoke probes for the newly packaged commands against the CppNix
	# control. They do not qualify channel updates or generations.
	for name, command in [("channel-list", "nix-channel --list"), ("prefetch-url", PREFETCH)]:
		want = controls[name]
		run(f"noxi-{name}", command, check=lambda out, want=want: (out == want, f"expected {want!r}, got {out!r}"))
	daemons("noxi", expected["candidate"])
	return selected


def return_to_cppnix(label: str) -> None:
	"""The documented return: switch the system profile back to the CppNix
	generation, with noxi's nix-env, else with CppNix's from that generation."""
	ok = run(f"{label}-switch-generation", f"nix-env -p {PROFILE} --switch-generation 1")
	if not ok:
		run(f"recovery-{label}-switch-generation", f"{BASE}/sw/bin/nix-env -p {PROFILE} --switch-generation 1")
	if not run(f"{label}-activate", f"{PROFILE}/bin/switch-to-configuration switch") or current() != BASE:
		# Explicit rescue uses only the saved CppNix closure, never the host.
		run(f"recovery-{label}-stop-writers", "systemctl stop nix-daemon.socket nix-daemon.service")
		run(f"recovery-{label}-activate", f"{BASE}/bin/switch-to-configuration switch")
		run(f"recovery-{label}-profile", f"{BASE}/sw/bin/nix-env -p {PROFILE} --set {BASE}")
	verify(f"{label}-generation", current() == BASE, f"current {current()}, CppNix generation {BASE}")
	reboot(f"{label}-reboot", BASE)
	reference = expected["reference"]
	run(f"{label}-version", "nix-instantiate --version", check=equals("nix-instantiate (Nix) 2.35.2"))
	daemons("cppnix", reference)
	run(f"{label}-list-generations", f"nix-env -p {PROFILE} --list-generations")
	# CppNix reads and reuses the store noxi wrote: verification, then a
	# rebuild of a noxi generation that has nothing to build.
	run(f"{label}-verify-store", "nix-store --verify --check-contents")
	run(
		f"{label}-reuses-store",
		f"cd /tmp && nixos-rebuild build --file {SYSTEM} --attr two --no-reexec && readlink -f result",
		check=equals(generation("two")["out"]),
	)


def nixos() -> None:
	"""TODO section 7: rebuild, reboot, a second change and rollback."""
	instantiate("noxi-instantiate-two", "two")
	run("noxi-find-file", "nix-instantiate --find-file nixpkgs")
	# nixos-rebuild's default re-execution first builds the target's own
	# nixos-rebuild; the actions below skip it so each is measured alone.
	rebuild("rebuild-build-reexec", "build", "two", extra="")
	actions = {}
	for action in ["build", "dry-activate", "test", "switch", "boot"]:
		before = current()
		actions[action] = rebuild(f"rebuild-{action}", action, "two")
		if action == "build":
			run("rebuild-build-result", "readlink -f /tmp/result", check=equals(generation("two")["out"]))
		elif action == "dry-activate":
			verify("rebuild-dry-activate-unchanged", current() == before, f"current {current()}, before {before}")
		elif action == "test":
			verify("rebuild-test-activated", *is_generation("two"))
			verify("rebuild-test-profile-unchanged", profile() == generation("one")["out"], f"profile {profile()}")
		elif action == "switch":
			verify("rebuild-switch-activated", *is_generation("two"))
			verify("rebuild-switch-profile", profile() == generation("two")["out"], f"profile {profile()}")
		elif action == "boot":
			verify("rebuild-boot-running-unchanged", current() == before, f"current {current()}, before {before}")
			verify("rebuild-boot-profile", profile() == generation("two")["out"], f"profile {profile()}")
			run(
				"rebuild-boot-entry",
				"grep -m1 -o '/nix/store/[a-z0-9]*-nixos-system-[^/]*' /boot/grub/grub.cfg",
				check=equals(generation("two")["out"]),
			)
	run("rebuild-list-generations", "nixos-rebuild list-generations")
	run("rebuild-flake-build", f"nixos-rebuild build --flake {FLAKE}#two --no-write-lock-file --no-reexec")
	if profile() != generation("two")["out"]:
		components("two", "two", "boot")

	if profile() == generation("two")["out"]:
		reboot("reboot-into-two", "two")
		run("reboot-generation-marker", "cat /etc/qualification-generation", check=equals("two"))
	else:
		blocked("reboot-into-two", "no noxi-built generation is in the system profile")

	rebuild("second-change-switch", "switch", "three")
	verify("second-change-generation", *is_generation("three"))
	if current() == generation("two")["out"]:
		components("three", "three", "switch")

	if current() == generation("three")["out"]:
		rebuild("rollback", "switch --rollback", None)
		verify("rollback-generation", *is_generation("two"))
		if current() != generation("two")["out"]:
			run("component-rollback-profile", f"nix-env -p {PROFILE} --rollback")
			run("component-rollback-activate", f"{PROFILE}/bin/switch-to-configuration switch")
			verify("component-rollback-generation", *is_generation("two"))
	else:
		blocked("rollback", "the second change did not reach generation three")


def home_manager() -> None:
	"""Standalone Home Manager in classic and flake mode, and the NixOS module."""
	home = expected["home"]
	command = f"home-manager -f {home['config']}"
	run("standalone-build", f"{command} build && readlink -f result", user="standalone", check=equals(home["out"]))
	if not run("standalone-switch", f"{command} switch", user="standalone"):
		# The generation's own activation, which `switch` runs after building.
		run("component-standalone-activate", f"{home['out']}/activate", user="standalone")
	run("standalone-marker", "cat ~/qualification-marker", user="standalone", check=equals("standalone"))
	run("standalone-generations", "home-manager generations", user="standalone")
	run("standalone-flake-build", f"home-manager --flake {FLAKE}#standalone --no-write-lock-file build", user="standalone")
	run("standalone-flake-switch", f"home-manager --flake {FLAKE}#standalone --no-write-lock-file switch", user="standalone")
	run(
		"module-activation",
		"systemctl restart home-manager-qualification.service && systemctl is-active home-manager-qualification.service",
	)
	run("module-activation-journal", "journalctl -b -u home-manager-qualification.service --no-pager -n 40")


def lifecycle() -> None:
	"""TODO section 8: upgrade, failed upgrade, rollback; removal follows."""
	rebuild("upgrade", "switch", "upgrade")
	verify("upgrade-generation", *is_generation("upgrade"))
	if current() != generation("upgrade")["out"]:
		components("upgrade", "upgrade", "switch")
	run("upgrade-package-selected",
		f"systemctl cat nix-daemon.service | grep -F 'ExecStart=@{expected['upgrade']}/bin/nix-daemon nix-daemon --daemon'"
		f" && cat {expected['upgrade']}/share/noxi/qualification-upgrade", check=equals("upgrade"))
	before, before_profile = current(), profile()
	run(
		"failed-upgrade",
		f"nixos-rebuild switch --file {SYSTEM} --attr broken --no-reexec",
		expected_status=1,
	)
	verify(
		"failed-upgrade-reports-assertion",
		"deliberately failing upgrade" in steps[-1].get("stderr", ""),
		"the assertion message must reach the user",
	)
	verify(
		"failed-upgrade-unchanged",
		current() == before and profile() == before_profile,
		f"current {current()} profile {profile()}, before {before} {before_profile}",
	)
	run(
		"failed-build",
		FAILED_BUILD,
		expected_status=100,
	)
	verify(
		"failed-build-reports-log",
		"deliberate build failure" in steps[-1].get("stderr", ""),
		"the builder's log must reach the user",
	)
	if current() == generation("upgrade")["out"]:
		rebuild("rollback", "switch --rollback", None)
		verify("rollback-generation", *is_generation("one"))
		if current() != generation("one")["out"]:
			run("component-rollback-profile", f"nix-env -p {PROFILE} --rollback")
			run("component-rollback-activate", f"{PROFILE}/bin/switch-to-configuration switch")
			verify("component-rollback-generation", *is_generation("one"))
	else:
		blocked("rollback", "the upgrade did not reach its generation")


def removal() -> None:
	"""After the return to CppNix, remove every noxi generation."""
	run("removal-delete-generations", f"nix-env -p {PROFILE} --delete-generations old")
	run("removal-refresh-boot-menu", f"{BASE}/bin/switch-to-configuration boot")
	for name in expected["generations"]:
		run(f"removal-no-{name}-boot-entry", f"! grep -F {generation(name)['out']} /boot/grub/grub.cfg")
	run("removal-collect-garbage", "nix-collect-garbage")
	run(
		"removal-no-noxi-generation",
		f"! ls -d {PROFILE}-*-link | grep -v '^{PROFILE}-1-link$'"
		f" && test $(readlink -f $(command -v nix-instantiate)) = $(readlink -f {expected['reference']}/bin/nix-instantiate)"
		f" && ! nix-store --query --requisites /run/current-system/sw | grep -Fx {expected['candidate']}",
	)
	run("removal-verify-store", "nix-store --verify --check-contents")


machine.start(allow_reboot=True)
machine.wait_for_unit("multi-user.target")
machine.wait_for_unit("nix-daemon.socket")
expected = json.loads(machine.succeed("cat /etc/qualification/expected.json"))
# Candidate generations replace /etc. Keep the immutable store targets of
# these fixtures rather than referring to the base generation's /etc links.
SYSTEM = text(f"readlink -f {SYSTEM}")
FLAKE = text(f"readlink -f {FLAKE}")
run("reference-boot", "nix-instantiate --version", check=equals("nix-instantiate (Nix) 2.35.2"))
# Test machines boot without a system profile. Record the booted CppNix
# system as generation 1, the one the return switches back to.
run("seed-system-profile", f"nix-env -p {PROFILE} --set /run/current-system")
BASE = current()
run("candidate-artifacts", f"sha256sum {expected['candidate']}/libexec/noxi/*")
for name, command in [("channel-list", "nix-channel --list"), ("prefetch-url", PREFETCH)]:
	run(f"control-cppnix-{name}", command)
	controls[name] = steps[-1]["stdout"]
run(
	"control-cppnix-flake-build",
	f"nix build --no-link --no-write-lock-file --print-out-paths {FLAKE}#nixosConfigurations.two.config.system.build.toplevel",
	check=equals(generation("two")["out"]),
)
if MODE == "lifecycle":
	run("control-cppnix-failed-build", FAILED_BUILD, expected_status=100)
	verify("control-cppnix-failed-build-log", "deliberate build failure" in steps[-1].get("stderr", ""),
		"the control builder must start and print its deliberate failure")
	run("control-cppnix-failed-upgrade", f"nixos-rebuild build --file {SYSTEM} --attr broken --no-reexec", expected_status=1)
	verify("control-cppnix-failed-upgrade-assertion", "deliberately failing upgrade" in steps[-1].get("stderr", ""),
		"the control must report the intentional assertion")

if MODE == "home-manager":
	machine.wait_for_unit("home-manager-qualification.service")
	# The same standalone build under CppNix, which must find it prebuilt.
	run(
		"control-cppnix-standalone-build",
		f"home-manager -f {expected['home']['config']} build && readlink -f result",
		user="standalone",
		check=equals(expected["home"]["out"]),
	)
	machine.succeed("rm -f /home/standalone/result /tmp/result")
	run(
		"control-cppnix-standalone-flake-build",
		f"home-manager --flake {FLAKE}#standalone --no-write-lock-file build && readlink -f result",
		user="standalone",
		check=equals(expected["home"]["out"]),
	)

swap = swap_to_noxi("lifecycle-install" if MODE == "lifecycle" else "swap-to-noxi")
if swap:
	{"nixos": nixos, "home-manager": home_manager, "lifecycle": lifecycle}[MODE]()
else:
	blocked(f"{MODE}-scenario", "CppNix could not switch the machine to the noxi generation")
return_to_cppnix("return-to-cppnix")
if MODE == "lifecycle":
	removal()

summary: dict[str, int] = {}
for step in steps:
	summary[step["status"]] = summary.get(step["status"], 0) + 1
report = {"schema": 2, "mode": MODE, "expected": expected, "summary": summary, "steps": steps}
(machine.out_dir / "report.json").write_text(json.dumps(report, indent="\t") + "\n")
lines = [f"{step['status']}\t{step['name']}" for step in steps]
(machine.out_dir / "steps.txt").write_text("\n".join(lines) + "\n")
print("\n".join(lines))
