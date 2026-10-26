# The qualification system of matches_reference_nixos_paths in
# crates/nix-rs-commands/tests/instantiate.rs (QUALIFICATION_SYSTEM); keep both equal.
{ nixpkgs, homeManager }:
let
	system = import "${nixpkgs}/nixos/lib/eval-config.nix" {
		system = "x86_64-linux";
		modules = [
			"${nixpkgs}/nixos/modules/virtualisation/qemu-vm.nix"
			(import "${homeManager}/nixos")
			({ ... }: {
				system.stateVersion = "26.05";
				networking.hostName = "nixrs-reference";
				boot.kernelParams = [ "console=ttyS0" ];
				boot.loader.grub.efiSupport = true;
				boot.loader.grub.efiInstallAsRemovable = true;
				virtualisation.memorySize = 2048;
				virtualisation.diskSize = 8192;
				virtualisation.useBootLoader = true;
				virtualisation.useEFIBoot = true;
				nix.settings.experimental-features = [ "nix-command" "flakes" ];
				users.users.qualification.isNormalUser = true;
				home-manager.useGlobalPkgs = true;
				home-manager.users.qualification = { ... }: {
					home.stateVersion = "26.05";
				};
			})
		];
	};
	standalone = import "${homeManager}/modules" {
		pkgs = import nixpkgs { system = "x86_64-linux"; };
		configuration = { pkgs, ... }: {
			home.username = "qualification";
			home.homeDirectory = "/home/qualification";
			home.stateVersion = "26.05";
			home.packages = [ pkgs.hello ];
			programs.git.enable = true;
		};
	};
in
[
	system.config.system.build.toplevel.drvPath
	system.config.system.build.vm.drvPath
	system.config.home-manager.users.qualification.home.activationPackage.drvPath
	standalone.activationPackage.drvPath
]
