{
	description = "Preview nix-rs command and compatibility workspace";

	inputs = {
		nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
		rust-overlay = {
			url = "github:oxalica/rust-overlay";
			inputs.nixpkgs.follows = "nixpkgs";
		};
	};

	outputs = { self, nixpkgs, rust-overlay }:
		let
			systems = [ "x86_64-linux" ];
			forAllSystems = f: nixpkgs.lib.genAttrs systems f;
			packageFor = system:
				let
					pkgs = import nixpkgs {
						inherit system;
						overlays = [ (import rust-overlay) ];
					};
					toolchain = pkgs.rust-bin.nightly."2026-09-20".minimal;
					rustPlatform = pkgs.makeRustPlatform {
						cargo = toolchain;
						rustc = toolchain;
					};
				in
				rustPlatform.buildRustPackage {
					pname = "nixrs-preview";
					version = "0.1.0";
					src = pkgs.lib.cleanSource ./.;
					cargoLock.lockFile = ./Cargo.lock;
					cargoBuildFlags = [ "-p" "nix-rs-cli" ];
					cargoTestFlags = [ "-p" "nix-rs-cli" ];
					cargoInstallFlags = [ "-p" "nix-rs-cli" ];
				};
		in
		{
			packages = forAllSystems (system: {
				default = packageFor system;
				nixrs = packageFor system;
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
			});
		};
}
