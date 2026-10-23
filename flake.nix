{
	description = "Preview nix-rs command and compatibility workspace";

	inputs = {
		nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
		rust-overlay = {
			url = "github:oxalica/rust-overlay";
			inputs.nixpkgs.follows = "nixpkgs";
		};
		home-manager = {
			url = "github:nix-community/home-manager/7b4c5ec4bedaf1e062bbc1bcaeddbc6bd242aa1b";
			inputs.nixpkgs.follows = "nixpkgs";
		};
		nix-reference.url = "github:NixOS/nix/2.35.2";
	};

	outputs = { self, nixpkgs, rust-overlay, nix-reference, home-manager }:
		let
			qualification = import ./tests/qualification/replacement.nix { inherit self nixpkgs home-manager nix-reference; };
			systems = [ "x86_64-linux" ];
			forAllSystems = f: nixpkgs.lib.genAttrs systems f;
			packageFor = system: replacement: import ./nix/package.nix {
				pkgs = nixpkgs.legacyPackages.${system};
				inherit rust-overlay replacement;
			};
		in
		{
			nixosConfigurations = qualification.configurations;
			homeConfigurations.qualification = home-manager.lib.homeManagerConfiguration {
				pkgs = nixpkgs.legacyPackages.x86_64-linux;
				modules = [ {
					home.username = "qualification";
					home.homeDirectory = "/home/qualification";
					home.stateVersion = "26.05";
					home.file."qualification-marker".text = "standalone";
				} ];
			};
			overlays.qualification = final: prev: {
				nixrs-replacement = packageFor prev.stdenv.hostPlatform.system true;
			};
			nixosModules.qualification = import ./nix/qualification-module.nix;
			packages = forAllSystems (system: {
				default = packageFor system false;
				nixrs = packageFor system false;
				nixrs-replacement = packageFor system true;
				qualification-nixos-report = qualification.reports.nixos;
				qualification-home-manager-report = qualification.reports.home-manager;
				qualification-distribution-report = qualification.reports.distribution;
				nix-reference = nix-reference.packages.${system}.nix;
				# A statically linked busybox with no references, used as the
				# builder in the build comparison tests.
				test-busybox = nixpkgs.legacyPackages.${system}.pkgsStatic.busybox;
			});
			apps = forAllSystems (system: {
				default = {
					type = "app";
					program = "${self.packages.${system}.default}/bin/nixrs";
					meta.description = "Run the nix-rs preview command";
				};
			});
			checks = forAllSystems (system: {
				nixrs = self.packages.${system}.default;
				qualification-nixos = qualification.checks.nixos;
				qualification-home-manager = qualification.checks.home-manager;
				qualification-distribution = qualification.checks.distribution;
				replacement = self.packages.${system}.nixrs-replacement;
				replacement-layout = nixpkgs.legacyPackages.${system}.runCommand "nixrs-replacement-layout" { } ''
					package=${self.packages.${system}.nixrs-replacement}
					for command in nix nix-build nix-instantiate nix-store nix-env nix-collect-garbage nix-daemon nix-shell nix-copy-closure nix-channel nix-prefetch-url nix-hash; do
						test -L "$package/bin/$command"
						test -x "$package/bin/$command"
					done
					"$package/bin/nixrs" eval --expr '1 + 2' | grep -x 3
					touch "$out"
				'';
				# Boots a NixOS machine running the pinned reference; see tests/vm/.
				vm = import ./tests/vm/test.nix {
					nixrs = self.packages.${system}.default;
					nixReference = self.packages.${system}.nix-reference;
					inherit (nixpkgs.legacyPackages.${system}) testers;
				};
			});
		};
}
