# Installs noxios from the ISO to an empty disk with `noxi install`, boots the
# installed system, and switches it to a second generation built by noxi.
# install-test.nix defines `qemu` (the QEMU command without drives), `iso`,
# `qemu_img`, `files` (the install-test flake), `engine` (the noxi package),
# `installed` and `second` (the two systems as the build host evaluates them)
# and `root_uuid` ahead of this file.

import os
import subprocess

disk = os.path.join(os.environ.get("TMPDIR", "/tmp"), "noxios-target.qcow2")
subprocess.run([qemu_img, "create", "-f", "qcow2", disk, "12G"], check=True)
drive = f"-drive file={disk},if=virtio,cache=unsafe,werror=report"
nix = "nix --extra-experimental-features 'nix-command flakes'"
cppnix = "^/nix/store/[a-z0-9]{32}-nix-([0-9]|util-|store-|expr-|main-|cmd-|fetchers-|flake-)"
# The installed system's toplevel and the derivations it is assembled from
# set allowSubstitutes = false; see install-test.nix.
install_options = "--option always-allow-substitutes true"


def toplevel_of(machine, link):
	return machine.succeed(f"readlink -f {link}").strip()


installer = create_machine(f"{qemu} -cdrom {iso} {drive}", name="installer")
installer.start()
installer.wait_for_unit("multi-user.target")

with subtest("partition and format the disk"):
	installer.succeed(
		"udevadm settle",
		"flock /dev/vda parted --script /dev/vda -- mklabel msdos mkpart primary ext4 1M -1s",
		"udevadm settle",
		f"mkfs.ext4 -F -L noxios -U {root_uuid} /dev/vda1",
		"udevadm settle",
		"mount /dev/disk/by-label/noxios /mnt",
	)

with subtest("noxi system init generates the configuration"):
	installer.succeed("noxi system init --root /mnt")
	hardware = installer.succeed("cat /mnt/etc/nixos/hardware-configuration.nix")
	print(hardware)
	assert f"/dev/disk/by-uuid/{root_uuid}" in hardware, "the root file system is not in the hardware configuration"
	installer.succeed("test -s /mnt/etc/nixos/configuration.nix")

with subtest("noxi system init --flake keeps noxi as the package manager"):
	installer.succeed("noxi system init --root /mnt --dir /tmp/init-flake --flake")
	installer.succeed("grep -q 'noxios.modules' /tmp/init-flake/flake.nix")
	installer.succeed("cmp /etc/nixos/flake.lock /tmp/init-flake/flake.lock")
	identity = installer.succeed(
		f"{nix} eval --raw /tmp/init-flake#nixosConfigurations.noxios.config "
		"--apply 'c: \"${c.nix.package.pname} ${c.system.nixos.distroId}\"'"
	).strip()
	assert identity == "noxi-replacement-provisional noxios", f"the generated flake's system is {identity}"

with subtest("the noxios flake evaluates offline to the preloaded system"):
	for name in ["flake.nix", "configuration.nix", "generation-2.nix"]:
		installer.succeed(f"install -m 0644 {files}/{name} /mnt/etc/nixos/{name}")
	installer.succeed("install -m 0644 /etc/nixos/flake.lock /mnt/etc/nixos/flake.lock")
	evaluated = installer.succeed(
		f"{nix} eval --raw /mnt/etc/nixos#nixosConfigurations.noxios.config.system.build.toplevel.outPath"
	).strip()
	assert evaluated == installed, (
		f"the generated hardware configuration gives {evaluated}, the build host's "
		f"install-test/hardware-configuration.nix gives {installed}"
	)

with subtest("noxi install copies the system from the image's store and installs GRUB"):
	output = installer.succeed(
		f"noxi install --root /mnt --flake /mnt/etc/nixos#noxios --no-root-passwd {install_options} < /dev/null 2>&1"
	)
	print(output)
	assert "will be built" not in output, "noxi install built paths instead of copying them"
	assert "auto?trusted=1" in output, "the plan does not name the substituter"
	installer.succeed("test \"$(readlink /mnt/nix/var/nix/profiles/system)\" = system-1-link")
	installer.succeed(f"test \"$(readlink /mnt/nix/var/nix/profiles/system-1-link)\" = {installed}")
	installer.succeed("test -e /mnt/etc/NIXOS")
	installer.succeed("grep -q noxios /mnt/boot/grub/grub.cfg")
	installer.fail(f"nix-store --store /mnt -qR {installed} | grep -E '{cppnix}'")

with subtest("noxi install is idempotent"):
	installer.succeed(f"noxi install --root /mnt --flake /mnt/etc/nixos#noxios --no-root-passwd {install_options} < /dev/null >&2")
	installer.succeed("test ! -e /mnt/nix/var/nix/profiles/system-2-link")

with subtest("noxi system enter runs commands in the installed system"):
	entered = installer.succeed("noxi system enter --root /mnt -c 'readlink -f /run/current-system'").strip()
	assert entered == installed, f"noxi system enter activated {entered}"
	installer.succeed("noxi system enter --root /mnt -- grep -qx ID=noxios /etc/os-release")

with subtest("shut the installer down"):
	installer.succeed("umount -R /mnt", "sync")
	installer.shutdown()

target = create_machine(f"{qemu} {drive}", name="target")
target.start()
target.wait_for_unit("multi-user.target")

with subtest("the installed system booted the installed generation"):
	assert toplevel_of(target, "/run/booted-system") == installed
	target.succeed("grep -qx 'ID=noxios' /etc/os-release")
	target.succeed("grep -qx 'ID_LIKE=nixos' /etc/os-release")

with subtest("noxi is the installed system's package manager"):
	for command in ["nix", "nix-store", "nix-env", "nix-build", "nix-daemon"]:
		resolved = toplevel_of(target, f"/run/current-system/sw/bin/{command}")
		assert resolved.startswith(f"{engine}/"), f"{command} resolves to {resolved}"
	# The daemon is socket-activated; root reaches it only through NIX_REMOTE.
	target.succeed("NIX_REMOTE=daemon nix-store -q --hash /run/current-system")
	pid = target.succeed("systemctl show -p MainPID --value nix-daemon.service").strip()
	daemon = toplevel_of(target, f"/proc/{pid}/exe")
	assert daemon.startswith(f"{engine}/"), f"nix-daemon runs {daemon}"
	target.succeed("systemctl show -p Id --value noxi-daemon.socket | grep -qx nix-daemon.socket")
	target.fail(f"nix-store -qR /run/current-system | grep -E '{cppnix}'")

with subtest("noxi builds and switches to a second generation"):
	output = target.succeed("noxi system switch --flake /etc/nixos#noxios-second 2>&1")
	print(output)
	assert "-etc.drv" in output, "noxi did not build the second generation's /etc"
	assert toplevel_of(target, "/run/current-system") == second
	assert toplevel_of(target, "/nix/var/nix/profiles/system-2-link") == second
	target.succeed("grep -qx 2 /etc/noxios-generation")

with subtest("the installed system boots the second generation"):
	target.shutdown()
	target.start()
	target.wait_for_unit("multi-user.target")
	assert toplevel_of(target, "/run/booted-system") == second
	target.succeed("grep -qx 2 /etc/noxios-generation")
	target.succeed("grep -qx 'ID=noxios' /etc/os-release")

target.shutdown()
