# The test driver injects machine, start_all and constants from replacement.nix.
# A successful report derivation means the probes and recovery ran. Only the
# strict gate decides whether all compatibility steps passed.
import json
import shlex

steps = []


def probe(name, command, expected=0):
	code, output = machine.execute(f"timeout 180 {command} 2>&1")
	step = {
		"name": name,
		"command": command,
		"exit_code": code,
		"expected_exit_code": expected,
		"status": "pass" if code == expected else "fail",
		"output": output,
	}
	steps.append(step)
	print(json.dumps(step))
	return code == expected


def blocked(name, reason):
	steps.append({"name": name, "status": "blocked", "reason": reason})


def shell(command):
	return "bash -c " + shlex.quote(command)


def record(name, condition, detail):
	steps.append({"name": name, "status": "pass" if condition else "fail", "output": detail})
	print(json.dumps(steps[-1]))


def handoff():
	# Stop service and socket together, including all reference workers. No
	# CppNix writer remains while Rust opens the existing guest store.
	machine.succeed("systemctl stop nix-daemon.socket nix-daemon.service")
	probe("handoff-no-reference-writer", shell("! pgrep -x nix-daemon"))
	# Bootstrap the VM-only nix.package/module with a prebuilt generation.
	# This tests activation wiring, not a generation built by nix-rs.
	probe("bootstrap-candidate-module", paths["bootstrap"] + "/bin/switch-to-configuration test")
	machine.succeed("systemctl stop nix-daemon.socket nix-daemon.service; systemctl reset-failed nix-daemon.service")
	probe("module-daemon-unit", "systemctl cat nix-daemon.service nix-daemon.socket")
	# Use the packaged clients for every subsequent command, including
	# children spawned by nixos-rebuild and Home Manager.
	machine.succeed("mkdir -p /run/qualification-bin; cp -as " + CANDIDATE + "/bin/. /run/qualification-bin/")
	probe("daemon-start", shell("systemctl start nix-daemon.service; sleep 1; systemctl is-active nix-daemon.service"))
	probe("daemon-journal", "journalctl -u nix-daemon.service --no-pager -n 35")
	probe("client-nix", CANDIDATE + "/bin/nix --version")
	probe("client-nix-instantiate", CANDIDATE + "/bin/nix-instantiate --eval --strict --expr '1 + 2'")


def restore():
	machine.succeed("systemctl stop nix-daemon.socket nix-daemon.service")
	probe("restore-reference-generation", shell("$(cat /root/reference-system)/bin/switch-to-configuration test"))
	machine.succeed(
		"rm -rf /run/qualification-bin; systemctl daemon-reload; systemctl reset-failed nix-daemon.service; "
		"systemctl start nix-daemon.socket nix-daemon.service"
	)
	machine.wait_for_unit("nix-daemon.service")
	probe("restore-system-profile", shell(REFERENCE + "/bin/nix-env -p /nix/var/nix/profiles/system --set $(cat /root/reference-system)"))
	probe("return-to-cppnix", REFERENCE + "/bin/nix --version")
	# Full metadata/content verification, against this guest's disk only.
	probe("cppnix-store-verification", REFERENCE + "/bin/nix-store --verify --check-contents")
	probe("reference-daemon-executable", shell("test $(readlink -f /proc/$(systemctl show -p MainPID --value nix-daemon.service)/exe) = $(readlink -f " + REFERENCE + "/bin/nix)"))


start_all()
machine.wait_for_unit("multi-user.target")
machine.wait_for_unit("nix-daemon.service")
machine.wait_for_unit("home-manager-qualification.service")
probe("reference-boot", REFERENCE + "/bin/nix --version")
probe("seed-system-profile", REFERENCE + "/bin/nix-env -p /nix/var/nix/profiles/system --set /run/current-system")
machine.succeed("readlink -f /run/current-system > /root/reference-system")
paths = json.loads(machine.succeed("cat /etc/qualification/paths.json"))
SOURCE = machine.succeed("readlink -f /etc/qualification/source").strip()
probe("artifact-hashes", shell("sha256sum " + CANDIDATE + "/libexec/nix-rs/*"))

if MODE == "distribution":
	profile = "/root/qualification-profile"
	# CppNix installs the first candidate into a separate package profile.
	probe("install", REFERENCE + f"/bin/nix-env -p {profile} --set {CANDIDATE}")
	probe("installed-evaluator", profile + "/bin/nix-instantiate --eval --strict --expr '1 + 2'")
	probe("installed-modern-cli", profile + "/bin/nix --version")
	probe("upgrade", CANDIDATE + f"/bin/nix-env -p {profile} --set {UPGRADE}")
	probe("upgrade-selected", f"test -f {profile}/qualification-upgrade")
	before = machine.succeed(f"readlink {profile}").strip()
	failed_code, failed_output = machine.execute(CANDIDATE + f"/bin/nix-env -p {profile} --set /nix/store/00000000000000000000000000000000-missing 2>&1")
	after = machine.succeed(f"readlink {profile}").strip()
	record("failed-upgrade-preserves-profile", failed_code != 0 and before == after, failed_output + f"\nbefore={before} after={after}")
	probe("rollback", CANDIDATE + f"/bin/nix-env -p {profile} --rollback")
	probe("rollback-selected", shell(f"test $(readlink -f {profile}) = {CANDIDATE}"))
	probe("removal", CANDIDATE + f"/bin/nix-env -p {profile} --uninstall '.*'")
	probe("removal-empty", shell(f"test ! -e {profile}/bin/nix-instantiate"))
	probe("cppnix-store-verification", REFERENCE + "/bin/nix-store --verify --check-contents")
else:
	handoff()
	env = "env PATH=/run/qualification-bin:/run/current-system/sw/bin NIX_PATH=nixpkgs=" + NIXPKGS
	if MODE == "home-manager":
		# Pass the candidate PATH inside the login user shell too.
		for action in ["build", "switch"]:
			command = env + " home-manager -f /etc/qualification/home.nix -I home-manager=" + HM + " " + action
			probe("standalone-" + action, "su - qualification -c " + shlex.quote(command))
		for action in ["build", "switch"]:
			command = env + " home-manager --flake path:" + SOURCE + "#qualification " + action
			probe("standalone-flake-" + action, "su - qualification -c " + shlex.quote(command))
		probe("standalone-activation-marker", "test -f /home/qualification/qualification-marker")
		probe("module-activation", shell("systemctl restart home-manager-qualification.service; systemctl is-active home-manager-qualification.service"))
	else:
		flake = "path:" + SOURCE
		results = {}
		for action in ["build", "dry-activate", "test", "switch", "boot"]:
			results[action] = probe("rebuild-" + action, env + " nixos-rebuild " + action + " --flake " + flake + "#qualification-one --no-reexec")
		probe("selected-first-generation", "grep -x one /etc/qualification-generation")
		if results["boot"]:
			machine.reboot()
			machine.wait_for_unit("multi-user.target")
			probe("reboot-new-generation", "grep -x one /etc/qualification-generation")
			probe("second-change", env + " nixos-rebuild switch --flake " + flake + "#qualification-two --no-reexec")
			probe("second-generation-marker", "grep -x two /etc/qualification-generation")
			probe("rollback", env + " nixos-rebuild switch --rollback --no-reexec")
			probe("rollback-marker", "grep -x one /etc/qualification-generation")
		else:
			for step in ["reboot-new-generation", "second-change", "second-generation-marker", "rollback", "rollback-marker"]:
				blocked(step, "rebuild-boot failed; no candidate boot generation exists")
	restore()

report = {
	"schema": 1,
	"mode": MODE,
	"candidate": CANDIDATE,
	"source": SOURCE,
	"reference": REFERENCE,
	"nixpkgs": NIXPKGS,
	"home_manager": HM,
	"upgrade_fixture": UPGRADE,
	"steps": steps,
}
(machine.out_dir / "report.json").write_text(json.dumps(report, indent="\t") + "\n")
