# The flake of this noxios live system, copied to /etc/nixos at boot.
#
# `noxi system switch` (and `nixos-rebuild switch`) rebuild the running
# system from nixosConfigurations.noxios, named after the default host name.
# `noxi flake update` moves the inputs forward; `noxi system switch --update`
# does both. Put changes to the system in configuration.nix.
#
# The lock pins the sources the image was built from. They are in the image's
# store, so the flake evaluates offline until an input is updated.
{
	description = "noxios live system";

	inputs = {
		nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
		# noxi and its Rust toolchain overlay, imported as plain sources.
		noxi = {
			url = "github:SINGH-RAJVEER/nix-rs";
			flake = false;
		};
		rust-overlay = {
			url = "github:oxalica/rust-overlay";
			flake = false;
		};
	};

	outputs = { self, nixpkgs, noxi, rust-overlay }:
		let
			system = "x86_64-linux";
			pkgs = import nixpkgs {
				inherit system;
				config = { };
				overlays = [ ];
			};
			noxios = import "${noxi}/nix/noxios" {
				inherit nixpkgs rust-overlay;
				source = noxi;
				package = import "${noxi}/nix/package.nix" {
					inherit pkgs rust-overlay;
					replacement = true;
				};
			};
		in
		{
			nixosConfigurations.noxios = noxios.iso.extendModules {
				modules = [ ./configuration.nix ];
			};
		};
}
