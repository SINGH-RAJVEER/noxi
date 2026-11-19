# The noxios name on everything people see, for the installer image and for
# installed systems alike. The image's volume label is set in default.nix,
# because only the ISO configuration has the isoImage options.
#
# NixOS's derivative-distribution options carry the name: `/etc/os-release`
# reads `ID=noxios` and, written by NixOS itself, `ID_LIKE=nixos`; the boot
# menu entries, the stage 1 and 2 banners, `nixos-version`, the getty
# greeting and the default host name follow `distroName` and `distroId`.
#
# Names other software calls stay as NixOS ships them: the `nix*` commands,
# `/nix/store`, `/nix/var/nix`, `nix.conf`, `NIX_PATH`, the `nixbld` users,
# the `nix-daemon` units, the `nix.*` and `system.nixos.*` options, the
# `nixos-*` tools and the root `nixos` channel.
{ config, lib, pkgs, ... }:
let
	home = "https://github.com/SINGH-RAJVEER/nix-rs";
in
{
	system.nixos = {
		distroName = "noxios";
		distroId = "noxios";
		vendorName = "noxios";
		vendorId = "noxios";
		extraOSReleaseArgs = {
			HOME_URL = home;
			VENDOR_URL = home;
			DOCUMENTATION_URL = "${home}/tree/main/docs";
			SUPPORT_URL = "${home}/issues";
			BUG_REPORT_URL = "${home}/issues";
		};
	};

	services.getty.greetingLine = ''<<< Welcome to noxios ${config.system.nixos.label} (\m) - \l >>>'';

	# The NixOS manual and `nixos-help`; noxios ships its own manual
	# (manual.nix). Nixpkgs evaluates the manual's option list with its own
	# CppNix, so dropping it also removes CppNix from the image's build graph.
	# installation-device.nix forced it on with mkImageMediaOverride; the
	# noxios installer profile does not, and mkForce wins over both.
	documentation.nixos.enable = lib.mkForce false;
}
