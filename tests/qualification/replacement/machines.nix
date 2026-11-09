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
	# The source of the engine release before the candidate.
	previous,
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
	# The previous engine release, built from its own source with the same
	# pins, for the release-to-release upgrade.
	previousPackage = import (previous + "/nix/package.nix") {
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
	homeConfigurationOf =
		confPath:
		import (home-manager + "/home-manager/home-manager.nix") {
			inherit pkgs confPath;
			confAttr = "";
		};
	homeConfiguration = homeConfigurationOf "${./home.nix}";
	homeConfigurationB = homeConfigurationOf "${./home-b.nix}";

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
				# Authorization boundaries the daemon mode probes; root stays the
				# only trusted user.
				allowed-users = [
					"root"
					"qualification"
					"standalone"
				];
				trusted-users = [ "root" ];
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
			# A user the daemon must refuse: not in allowed-users.
			users.users.outsider = {
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

	# A generation whose new derivations the guest builds itself with noxi:
	# changing the marker yields a new etc file, etc tree, activation script
	# and toplevel. Everything else these four derivations need, including
	# build-only inputs such as the no-compiler stdenv and the builder
	# scripts, is preloaded as a binary cache would provide it; the four
	# outputs themselves never reach the disk.
	fresh = generation "fresh" candidate { };
	freshDerivations =
		let
			config = fresh;
			activate =
				(pkgs.writeShellApplication {
					name = "activate";
					text = config.system.activationScripts.script;
					checkPhase = "";
					bashOptions = [ ];
				}).overrideAttrs
					{ preferLocalBuild = true; };
			inputsOf = drv: builtins.attrNames (builtins.getContext (builtins.toJSON drv.drvAttrs));
		in
		assert lib.assertMsg (builtins.elem activate.drvPath (inputsOf config.system.build.toplevel))
			"the fresh generation's activation script no longer matches activatable-system.nix";
		[
			config.environment.etc."qualification-generation".source
			config.system.build.etc
			activate
			config.system.build.toplevel
		];
	freshInputs =
		let
			outputs = map (drv: drv.outPath) freshDerivations;
			attrs = builtins.toJSON (map (drv: drv.drvAttrs) freshDerivations);
		in
		pkgs.runCommand "noxi-qualification-fresh-inputs"
			{
				# Every store path the four derivations name, less their own
				# outputs, so the closure holds their inputs only.
				inputs = builtins.replaceStrings outputs (map (_: "") outputs) attrs;
				passAsFile = [ "inputs" ];
			}
			''
				cp "$inputsPath" "$out"
			'';
in
{
	inherit
		pkgs
		candidate
		upgrade
		previousPackage
		common
		freshDerivations
		freshInputs
		;
	generations = {
		# CppNix switches the machine to this one.
		one = generation "one" candidate { };
		# noxi rebuilds to these.
		two = generation "two" candidate { };
		three = generation "three" candidate { };
		upgrade = generation "upgrade" upgrade { };
		# The previous release, which the candidate upgrades from.
		previous = generation "previous" previousPackage { };
		# Built in the guest by noxi; see `fresh` above.
		inherit fresh;
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
		# The second generation of the profile and rollback comparison.
		b = {
			config = "${./home-b.nix}";
			configuration = homeConfigurationB;
			activation = homeConfigurationB.activationPackage;
		};
	};
}
