# The system the install test installs, copied to /mnt/etc/nixos with the
# live image's flake.lock, so its inputs resolve offline from the image's
# store. It is a noxios system: the noxios modules from the noxi input, so
# noxi stays the package manager after installation, and configuration.nix
# with the hardware-configuration.nix `noxi system init` generates.
#
# nixosConfigurations.noxios is the system `noxi install` installs and
# nixosConfigurations.noxios-second the second generation the installed
# system builds and switches to. The installed system carries the build
# inputs of the second generation's /etc and toplevel, so noxi builds them
# offline instead of finding them in the store.
#
# install-test.nix evaluates the same outputs on the build host, with
# hardware-configuration.nix next to this file standing in for the generated
# one, and preloads the installed system into the test image.
{
	description = "noxios install test system";

	inputs = {
		nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
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
			pkgs = import nixpkgs {
				system = "x86_64-linux";
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
			base = nixpkgs.lib.nixosSystem {
				modules = noxios.modules ++ [ ./configuration.nix ];
			};
			second = base.extendModules { modules = [ ./generation-2.nix ]; };
			installed = base.extendModules {
				modules = [
					(
						{ pkgs, ... }:
						{
							# The toplevel and the derivations it is assembled from set
							# `allowSubstitutes = false`, and the store holds only the
							# system's runtime closure. The second generation's /etc and
							# toplevel are therefore built from these inputs: those of its
							# /etc derivation, and those of this system's toplevel, such as
							# the wrapper check and the stage 2 script, which both
							# generations share.
							system.extraDependencies = [
								pkgs.stdenvNoCC
								second.config.system.build.etc.inputDerivation
								base.config.system.build.toplevel.inputDerivation
							];
						}
					)
				];
			};
		in
		{
			nixosConfigurations.noxios = installed;
			nixosConfigurations.noxios-second = installed.extendModules { modules = [ ./generation-2.nix ]; };
		};
}
