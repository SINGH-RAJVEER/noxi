# Boots the noxios ISO and checks that noxi ran its boot units and serves the
# store, the noxios branding, the `noxi` live user, the noxios manual and the
# live system's flake. default.nix defines `start_command`, `engine` (the noxi
# package's store path) and `toplevel` (the uninstrumented image's system)
# ahead of this file.

machine = create_machine(start_command)
machine.start()
machine.wait_for_unit("multi-user.target")

with subtest("noxi registered the image's store and ran the channel unit"):
	machine.succeed("systemctl is-active register-nix-paths.service")
	machine.succeed("systemctl is-active nix-channel-init.service")
	machine.succeed("test -n \"$(readlink /nix/var/nix/profiles/system)\"")

with subtest("the nix commands and daemon are noxi's"):
	for command in ["nix", "nix-store", "nix-env", "nix-build", "nix-instantiate", "nix-daemon"]:
		target = machine.succeed(f"readlink -f /run/current-system/sw/bin/{command}").strip()
		assert target.startswith(f"{engine}/"), f"{command} resolves to {target}"
	machine.succeed("su noxi -c 'nix-store -q --hash /run/current-system'")
	pid = machine.succeed("systemctl show -p MainPID --value nix-daemon.service").strip()
	daemon = machine.succeed(f"readlink -f /proc/{pid}/exe").strip()
	assert daemon.startswith(f"{engine}/"), f"nix-daemon runs {daemon}"

with subtest("noxi-daemon.service and noxi-daemon.socket are aliases of the nix-daemon units"):
	machine.succeed("systemctl show -p Id --value noxi-daemon.service | grep -qx nix-daemon.service")
	machine.succeed("systemctl show -p Id --value noxi-daemon.socket | grep -qx nix-daemon.socket")
	machine.succeed("systemctl is-active noxi-daemon.socket")

with subtest("the system is branded noxios and derives from NixOS"):
	machine.succeed("grep -qx 'ID=noxios' /etc/os-release")
	machine.succeed("grep -qx 'ID_LIKE=nixos' /etc/os-release")
	machine.succeed("grep -qx 'NAME=noxios' /etc/os-release")
	machine.succeed("grep -q 'Welcome to noxios' /etc/issue")
	machine.succeed("grep -q 'noxios-help' /etc/issue")
	hostname = machine.succeed("hostname").strip()
	assert hostname == "noxios", f"host name is {hostname}"
	machine.succeed("ls /dev/disk/by-label | grep -qx 'noxios-minimal-[0-9.]*-x86_64'")

with subtest("the live user is noxi, logged in at the console"):
	machine.fail("id nixos")
	machine.succeed("id -nG noxi | tr ' ' '\\n' | grep -qx wheel")
	machine.succeed("su noxi -c 'sudo -n true'")
	machine.succeed("grep -E '^trusted-users = .*\\bnoxi\\b' /etc/nix/nix.conf")
	machine.wait_for_unit("getty@tty1.service")
	machine.wait_until_succeeds("pgrep -u noxi -t tty1")

with subtest("the noxios manual replaces the NixOS manual"):
	machine.fail("command -v nixos-help")
	machine.fail("test -e /run/current-system/sw/share/doc/nixos")
	machine.succeed("BROWSER=cat noxios-help | grep -q 'noxios manual'")
	machine.succeed("BROWSER=cat noxios-help system | grep -q 'noxios installer image'")
	machine.fail("noxios-help no-such-page")
	machine.succeed("man -w noxios-system")

with subtest("the live system's flake is in /etc/nixos and evaluates offline"):
	for name in ["flake.nix", "flake.lock", "configuration.nix"]:
		machine.succeed(f"test -f /etc/nixos/{name} && test ! -L /etc/nixos/{name} && test -w /etc/nixos/{name}")
	machine.succeed("grep -q 'noxios live system' /etc/nixos/flake.nix")
	label = machine.succeed(
		"nix --extra-experimental-features 'nix-command flakes' eval --raw "
		"/etc/nixos#nixosConfigurations.noxios.config.system.nixos.label"
	).strip()
	version = machine.succeed("cat /run/current-system/nixos-version").strip()
	assert label == version, f"the flake describes {label}, the running system is {version}"
	# The test image adds the driver's instrumentation; the flake describes
	# the image as published, so its system is that image's.
	evaluated = machine.succeed(
		"nix --extra-experimental-features 'nix-command flakes' eval --raw "
		"/etc/nixos#nixosConfigurations.noxios.config.system.build.toplevel.outPath"
	).strip()
	assert evaluated == toplevel, f"the flake's system is {evaluated}, the image's is {toplevel}"

# The checks NixOS's own ISO boot test makes.
machine.succeed("nix store verify --no-trust -r --option experimental-features nix-command /run/current-system")
with subtest("Check whether the channel got installed correctly"):
	machine.succeed("nix-instantiate --dry-run '<nixpkgs>' -A hello")
	machine.succeed("nix-env --dry-run -iA nixos.procps")

machine.shutdown()
