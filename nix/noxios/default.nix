# The noxios installer image and its checks.
#
# The image is the NixOS minimal installation ISO of the pinned Nixpkgs with
# noxi as `nix.package` (module.nix), the noxios name on what people see
# (branding.nix), a `noxi` live user (installer.nix), the noxios manual
# (manual.nix) and a locked flake of the live system in /etc/nixos
# (live.nix). The kernel, channel, installer tools and every name other
# software calls stay as NixOS ships them.
#
# The live flake evaluates this file again from its own inputs, so every
# argument must be reachable from there; see live/flake.nix. After `noxi
# flake update`, a published image calls this file and nix/package.nix of the
# newest noxi revision with the arguments live/flake.nix passes today. Both
# argument sets are therefore an interface of released images: add arguments
# only with defaults, and never remove or rename one.
{
	nixpkgs,
	# The engine package, `packages.x86_64-linux.noxi-replacement`.
	package,
	# The rust-overlay source the engine package was built with.
	rust-overlay,
	# The noxi repository source this file belongs to.
	source,
}:
let
	system = "x86_64-linux";
	module = import ./module.nix { inherit package; };

	# Evaluated with eval-config.nix, as Hydra evaluates the official ISO in
	# nixos/release.nix. nixosSystem would set `nixpkgs.flake.source`, which
	# puts `nixpkgs=flake:nixpkgs` ahead of the channel in NIX_PATH, and that
	# entry needs the `flakes` feature the image leaves off. The version module
	# mirrors what the Nixpkgs flake's lib gives nixosSystem, so the label stays
	# 26.11.<date>.<rev>.
	installer =
		extraModules:
		import "${nixpkgs}/nixos/lib/eval-config.nix" {
			inherit system;
			modules = [
				"${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix"
				module
				./branding.nix
				./installer.nix
				./manual.nix
				(import ./live.nix { inherit nixpkgs rust-overlay source; })
				{
					system.nixos.versionSuffix = ".${builtins.substring 0 8 (nixpkgs.lastModifiedDate or "19700101")}.${nixpkgs.shortRev or "dirty"}";
					system.nixos.revision = nixpkgs.rev or null;
				}
				(
					{ config, lib, pkgs, ... }:
					{
						image.baseName = lib.mkForce "${config.system.nixos.distroId}${
							lib.optionalString (config.isoImage.edition != "") "-${config.isoImage.edition}"
						}-${config.system.nixos.label}-${pkgs.stdenv.hostPlatform.system}";
						# nixos-$EDITION-$RELEASE-$ARCH upstream; at most 32 characters,
						# which iso-image.nix asserts. The label also names the root file
						# system the initrd mounts, so a successful boot checks it.
						isoImage.volumeID = "noxios${
							lib.optionalString (config.isoImage.edition != "") "-${config.isoImage.edition}"
						}-${config.system.nixos.release}-${pkgs.stdenv.hostPlatform.uname.processor}";
					}
				)
			] ++ extraModules;
		};

	iso = installer [ ];
	inherit (iso) pkgs;
	inherit (pkgs) lib;
	engine = iso.config.nix.package;
in
{
	inherit module iso;
	image = iso.config.system.build.isoImage;

	# The modules of an installed noxios system: noxi as the package manager
	# and the noxios name. A system's flake adds its own configuration.
	modules = [
		module
		./branding.nix
	];

	# Installs the image to a disk with `noxi install`, boots the installed
	# system and switches it to a second generation that noxi builds there.
	install = import ./install-test.nix {
		inherit nixpkgs rust-overlay source installer engine pkgs;
	};

	# Fails when a CppNix package is in the image's store, and when the engine
	# is not. With the NixOS manual dropped (branding.nix), CppNix is not in
	# the image's build graph either: the manual's option list was the only
	# thing Nixpkgs evaluated with its own build-platform `nix`.
	closure = pkgs.runCommand "noxios-iso-closure" {
		closure = pkgs.closureInfo { rootPaths = iso.config.isoImage.storeContents; };
	} ''
		if grep -E '^/nix/store/[a-z0-9]{32}-nix-([0-9]|util-|store-|expr-|main-|cmd-|fetchers-|flake-)' "$closure/store-paths"; then
			echo "CppNix is in the noxios image's store" >&2
			exit 1
		fi
		grep -qx '${engine.outPath}' "$closure/store-paths"
		touch "$out"
	'';

	# Boots the image as NixOS's own ISO boot test does, with the test driver's
	# instrumentation added, and checks that noxi ran the boot units and serves
	# the store, the branding, the `noxi` live user, the manual, and that the
	# live flake evaluates offline to the published image's system.
	boot =
		let
			testIso = (installer [ "${nixpkgs}/nixos/modules/testing/test-instrumentation.nix" ]).config.system.build.isoImage;
			qemu-common = import "${nixpkgs}/nixos/lib/qemu-common.nix" { inherit (pkgs) lib stdenv; };
			startCommand = lib.concatStringsSep " " [
				(qemu-common.qemuBinary pkgs.qemu_test)
				"-m 3072"
				"-netdev user,id=net0"
				"-device virtio-net-pci,netdev=net0"
				"-cdrom ${testIso}/iso/${testIso.isoName}"
			];
			testing = import "${nixpkgs}/nixos/lib/testing-python.nix" { inherit system pkgs; };
		in
		testing.makeTest {
			name = "noxios-iso-boot";
			nodes = { };
			globalTimeout = 1200;
			# Indented strings strip spaces only, so the script lives in a .py
			# file and its parameters are a plain string. The live flake evaluates
			# the published image's system, which the test image only extends.
			testScript =
				"start_command = ${builtins.toJSON startCommand}\nengine = ${builtins.toJSON engine.outPath}\n"
				+ "toplevel = ${builtins.toJSON (builtins.unsafeDiscardStringContext iso.config.system.build.toplevel.outPath)}\n"
				+ builtins.readFile ./boot-test.py;
		};
}
