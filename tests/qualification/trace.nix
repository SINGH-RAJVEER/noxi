# Traces every Nix command that NixOS and Home Manager run on the pinned
# reference system: at boot, during a flake rebuild and switch, Home Manager
# activation, the garbage-collection and store-optimisation services, a
# rollback, and `nix-collect-garbage -d`.
#
# The kernel audits every execve, so callers that bypass `nix.package` are
# seen too. The rebuild target is prebuilt and every locked flake input is in
# the store, so the guest evaluates it offline to the same derivations and
# builds nothing. The decoded calls are written to the check's output.
{
	self,
	nixpkgs,
	home-manager,
	nix-reference,
}:
let
	system = "x86_64-linux";
	pkgs = nixpkgs.legacyPackages.${system};

	# Every locked input, so the guest's evaluation finds each one by its
	# NAR hash instead of fetching it.
	inputSources = [
		nixpkgs
		home-manager
		nix-reference
	] ++ builtins.attrValues nix-reference.inputs;

	machine =
		{ lib, ... }:
		{
			imports = [ home-manager.nixosModules.home-manager ];

			system.stateVersion = "26.05";
			virtualisation.memorySize = 4096;
			virtualisation.cores = 2;
			virtualisation.diskSize = 8192;
			# Test machines leave out switch-to-configuration by default.
			system.switch.enable = true;
			# The test disk has no room for GRUB; boot-loader installers do not
			# run Nix.
			boot.loader.grub.enable = false;

			nix.package = nix-reference.packages.${system}.nix;
			nix.settings = {
				experimental-features = [
					"nix-command"
					"flakes"
				];
				# The guest is offline.
				substituters = lib.mkForce [ ];
				flake-registry = "";
			};
			nix.gc.automatic = true;
			nix.optimise.automatic = true;

			users.users.qualification.isNormalUser = true;
			home-manager.useGlobalPkgs = true;
			home-manager.users.qualification = { ... }: {
				home.stateVersion = "26.05";
			};

			security.auditd = {
				enable = true;
				# One unrotated log, written through before the check reads it.
				settings = {
					max_log_file = 1024;
					max_log_file_action = "ignore";
					flush = "data";
				};
			};
			security.audit = {
				enable = true;
				backlogLimit = 8192;
				rules = [ "-a exit,always -F arch=b64 -S execve" ];
			};

			# The flake the guest rebuilds from.
			environment.etc.qual.source = self;
		};

	traceTest = extra: {
		name = "noxi-reference-trace";
		nodes.machine = {
			imports = [
				machine
				extra
			];
		};
		testScript = builtins.readFile ./trace-script.py;
	};

	# The generation the guest switches to: the traced machine without its
	# prebuilt dependencies, plus a marker file. A test's node is its
	# configuration, which nixos-rebuild reads as `config`.
	targetConfig = (pkgs.testers.runNixOSTest (traceTest {
		environment.etc."noxi-trace-generation".text = "target";
	})).config.nodes.machine;
in
{
	target.config = targetConfig;
	check = pkgs.testers.runNixOSTest (traceTest {
		system.extraDependencies = [ targetConfig.system.build.toplevel ] ++ inputSources;
	});
}
