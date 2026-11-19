# Installs noxios from the ISO to an empty disk with `noxi install`, boots
# the installed system, and switches it to a second generation that noxi
# builds there.
#
# It follows Nixpkgs' nixos/tests/installer.nix for an offline install: the
# test has no network, so the system to install is in the image's store.
# The installer VM partitions the disk with parted, runs `noxi system init
# --root /mnt`, puts the flake of install-test/ next to the generated
# hardware configuration, and runs `noxi install`, which copies every path
# from the image's store with `auto?trusted=1` and builds nothing. The
# toplevel and the derivations it is assembled from set
# `allowSubstitutes = false`, which nixos-install and `noxi install` honour by
# building them; Nixpkgs' test therefore puts their build inputs in the
# installer's store. This test passes `--option always-allow-substitutes true`
# instead, so the install copies the installed system whole, and the real
# builds are the second generation's, in the installed system. The
# root file system gets a fixed UUID, so the generated hardware
# configuration gives the system this file evaluates on the build host with
# install-test/hardware-configuration.nix; the test checks that before it
# installs. BIOS and GRUB only; UEFI and systemd-boot are not covered yet.
{
	nixpkgs,
	rust-overlay,
	# The noxi repository source.
	source,
	# default.nix's function from extra modules to the image configuration.
	installer,
	# The noxi engine package.
	engine,
	pkgs,
}:
let
	inherit (pkgs) lib;
	system = "x86_64-linux";

	# The flake's outputs as the installed system evaluates them, from the
	# same sources its lock names.
	flake = (import ./install-test/flake.nix).outputs {
		self = { };
		inherit nixpkgs rust-overlay;
		noxi = source;
	};
	installed = flake.nixosConfigurations.noxios.config.system.build.toplevel;
	second = flake.nixosConfigurations.noxios-second.config.system.build.toplevel;

	testConfig =
		installer [
			"${nixpkgs}/nixos/modules/testing/test-instrumentation.nix"
			{
				# The system to install, which `noxi install` copies from the
				# image's store, and the flake the test puts in /mnt/etc/nixos.
				isoImage.storeContents = [
					installed
					files
				];
				nix.settings.substituters = lib.mkForce [ ];
			}
		];
	testIso = testConfig.config.system.build.isoImage;

	files = pkgs.runCommand "noxios-install-test-files" { } ''
		mkdir "$out"
		cp ${./install-test/flake.nix} "$out/flake.nix"
		cp ${./install-test/configuration.nix} "$out/configuration.nix"
		cp ${./install-test/generation-2.nix} "$out/generation-2.nix"
	'';

	qemu-common = import "${nixpkgs}/nixos/lib/qemu-common.nix" { inherit (pkgs) lib stdenv; };
	qemu = lib.concatStringsSep " " [
		(qemu-common.qemuBinary pkgs.qemu_test)
		"-m 4096"
		"-smp 2"
		"-netdev user,id=net0,restrict=on"
		"-device virtio-net-pci,netdev=net0"
	];
	testing = import "${nixpkgs}/nixos/lib/testing-python.nix" { inherit system pkgs; };
	parameters = {
		inherit qemu;
		iso = "${testIso}/iso/${testIso.isoName}";
		qemu_img = "${pkgs.qemu_test}/bin/qemu-img";
		files = "${files}";
		engine = engine.outPath;
		installed = builtins.unsafeDiscardStringContext installed.outPath;
		second = builtins.unsafeDiscardStringContext second.outPath;
		root_uuid = "5a1e2b3c-4d5e-4f60-8a7b-9c0d1e2f3a4b";
	};
in
testing.makeTest {
	name = "noxios-install";
	nodes = { };
	globalTimeout = 3600;
	# Indented strings strip spaces only, so the script lives in a .py file
	# and its parameters are plain strings.
	testScript =
		lib.concatStrings (lib.mapAttrsToList (name: value: "${name} = ${builtins.toJSON value}\n") parameters)
		+ builtins.readFile ./install-test.py;
	passthru = {
		inherit installed second testIso;
		# What the image's store holds, for checking offline builds on the build host.
		storeClosure = pkgs.closureInfo { rootPaths = testConfig.config.isoImage.storeContents; };
		installedClosure = pkgs.closureInfo { rootPaths = [ installed ]; };
	};
}
