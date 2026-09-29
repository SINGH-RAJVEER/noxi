{
	description = "Pinned CppNix replacement qualification VM";

	inputs = {
		nixpkgs.url = "github:NixOS/nixpkgs/e94cb152ed51bd6e24eb4a41f1460252beb52cd2";
		home-manager = {
			url = "github:nix-community/home-manager/7b4c5ec4bedaf1e062bbc1bcaeddbc6bd242aa1b";
			inputs.nixpkgs.follows = "nixpkgs";
		};
		nix-reference.url = "github:NixOS/nix/2c73b59da29606068c0c98db015dd3a66955525d";
	};

	outputs = { self, nixpkgs, home-manager, nix-reference }:
		let
			system = "x86_64-linux";
			reference = nixpkgs.lib.nixosSystem {
				inherit system;
				modules = [
					"${nixpkgs}/nixos/modules/virtualisation/qemu-vm.nix"
					home-manager.nixosModules.home-manager
					({ ... }: {
						system.stateVersion = "26.05";
						networking.hostName = "nixrs-reference";
						boot.kernelParams = [ "console=ttyS0" ];
						boot.loader.grub.efiSupport = true;
						boot.loader.grub.efiInstallAsRemovable = true;
						virtualisation.memorySize = 2048;
						virtualisation.useBootLoader = true;
						virtualisation.useEFIBoot = true;
						nix.package = nix-reference.packages.${system}.nix;
						nix.settings.experimental-features = [ "nix-command" "flakes" ];
						users.users.qualification.isNormalUser = true;
						home-manager.useGlobalPkgs = true;
						home-manager.users.qualification = { ... }: {
							home.stateVersion = "26.05";
						};
					})
				];
			};
		in
		{
			nixosConfigurations.reference = reference;
			packages.${system}.reference-vm = reference.config.system.build.vm;
		};
}
