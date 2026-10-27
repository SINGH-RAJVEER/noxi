# The qualification machine and every generation it switches to.
#
# The host builds these generations into the VM's disk; the guest evaluates
# this same file again with `nixos-rebuild --file`, so a rebuild is real
# evidence only when the guest reaches the host's derivations. For that, every
# argument is a store path string without context, identical on both sides,
# and everything else is reached through relative paths inside a copied source
# tree. Files copied to the store keep their content-addressed paths whichever
# tree they come from.
{
	nixpkgs,
	home-manager,
	rust-overlay,
}:
let
	system = "x86_64-linux";
	# Equal to the guest's `import <nixpkgs> { }` when no user configuration
	# exists, which Home Manager's standalone command evaluates.
	pkgs = import nixpkgs {
		inherit system;
		config = { };
		overlays = [ ];
	};
	inherit (pkgs) lib;

	candidate = import ../../../nix/package.nix {
		inherit pkgs rust-overlay;
		replacement = true;
	};
	# A distinct package with the same engine binaries, for the upgrade step.
	upgrade = pkgs.symlinkJoin {
		name = "noxi-replacement-upgrade-fixture";
		paths = [ candidate ];
		postBuild = ''
			echo upgrade > "$out/share/noxi/qualification-upgrade"
			rm "$out/lib/systemd/system/nix-daemon.service"
			sed "s|${candidate}|$out|g" ${candidate}/lib/systemd/system/nix-daemon.service > "$out/lib/systemd/system/nix-daemon.service"
		'';
	};
	homeManagerPackage = pkgs.callPackage (home-manager + "/home-manager") { path = home-manager; };
	homeConfiguration = import (home-manager + "/home-manager/home-manager.nix") {
		inherit pkgs;
		confPath = "${./home.nix}";
		confAttr = "";
	};

	common =
		{ lib, ... }:
		{
			disabledModules = [ "virtualisation/qemu-vm.nix" ];
			imports = [
				(import ../../../nix/qualification-qemu-vm.nix { inherit nixpkgs; })
				../../../nix/qualification-module.nix
				(home-manager + "/nixos")
			];
			system.stateVersion = "26.05";
			# Test machines leave out switch-to-configuration by default.
			system.switch.enable = true;
			virtualisation = {
				memorySize = 4096;
				cores = 2;
				diskSize = 16384;
				# Boot from the disk's GRUB, so a reboot starts the generation the
				# profile selects rather than the test's own system.
				useBootLoader = true;
				mountHostNixStore = false;
				writableStore = false;
			};
			boot.loader.grub.enable = true;
			nix.settings = {
				experimental-features = [
					"nix-command"
					"flakes"
				];
				# The guest is offline.
				substituters = lib.mkForce [ ];
				flake-registry = "";
			};
			# Classic-mode lookups for Home Manager's standalone command.
			nix.nixPath = [
				"nixpkgs=${nixpkgs}"
				"home-manager=${home-manager}"
			];
			# Home Manager as a NixOS module.
			users.users.qualification = {
				isNormalUser = true;
				password = "";
			};
			home-manager.useGlobalPkgs = true;
			home-manager.users.qualification.home.stateVersion = "26.05";
			# Standalone Home Manager, kept apart from the module's user.
			users.users.standalone = {
				isNormalUser = true;
				password = "";
			};
			environment.systemPackages = [ homeManagerPackage ];
		};

	# A test node's configuration. Its toplevel depends only on the modules,
	# not on the test's name or script.
	node =
		module:
		(pkgs.testers.runNixOSTest {
			name = "noxi-qualification-generation";
			nodes.machine.imports = [
				common
				module
			];
			testScript = "";
		}).config.nodes.machine;

	generation =
		marker: package: module:
		node {
			imports = [ module ];
			noxi.qualification = {
				enable = true;
				inherit package;
			};
			environment.etc."qualification-generation".text = marker;
		};
in
{
	inherit
		pkgs
		candidate
		upgrade
		common
		;
	generations = {
		# CppNix switches the machine to this one.
		one = generation "one" candidate { };
		# noxi rebuilds to these.
		two = generation "two" candidate { };
		three = generation "three" candidate { };
		upgrade = generation "upgrade" upgrade { };
		# A failed upgrade: evaluation stops at a failing assertion, so nothing
		# is built or activated. Never built on the host.
		broken = generation "broken" upgrade {
			assertions = [
				{
					assertion = false;
					message = "noxi qualification: deliberately failing upgrade";
				}
			];
		};
	};
	# The standalone generation `home-manager -f home.nix build` evaluates.
	home = {
		config = "${./home.nix}";
		configuration = homeConfiguration;
		activation = homeConfiguration.activationPackage;
	};
}
