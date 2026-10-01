{
	description = "Preview nix-rs command and compatibility workspace";

	inputs = {
		nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
		rust-overlay = {
			url = "github:oxalica/rust-overlay";
			inputs.nixpkgs.follows = "nixpkgs";
		};
		nix-reference.url = "github:NixOS/nix/2.35.2";
	};

	outputs = { self, nixpkgs, rust-overlay, nix-reference }:
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
					version = "0.1.1";
					src = pkgs.lib.fileset.toSource {
						root = ./.;
						fileset = pkgs.lib.fileset.unions [
							./.cargo
							./Cargo.lock
							./Cargo.toml
							./crates
						];
					};
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
				nix-reference = nix-reference.packages.${system}.nix;
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
				# Boots a NixOS machine running the pinned reference; see tests/vm/.
				vm = import ./tests/vm/test.nix {
					nixrs = self.packages.${system}.default;
					nixReference = self.packages.${system}.nix-reference;
					inherit (nixpkgs.legacyPackages.${system}) testers;
				};
			});
		};
}
