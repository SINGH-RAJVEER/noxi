# Replaces the Nix package manager of a NixOS system with noxi and changes
# nothing else.
#
# The package takes the place of `nix.package`, so every NixOS caller of it
# runs noxi: the nix-daemon units through systemd.packages, nixos-rebuild,
# nixos-install, nixos-option, the boot-loader installers, the installer
# image's store registration and channel units, and the nix-gc and
# nix-optimise services. Every other option keeps its NixOS default.
#
# The daemon units keep their nix-daemon names and gain noxi-daemon aliases.
# NixOS ignores the Alias= lines of packaged units, so the aliases are set
# here; see nix/package.nix.
#
# The engine is unqualified for hosts; see docs/system.md#noxios-installer-image.
{ package }:
{ config, lib, ... }:
{
	options.noxios.package = lib.mkOption {
		type = lib.types.package;
		default = package;
		defaultText = lib.literalExpression "noxi.packages.x86_64-linux.noxi-replacement";
		description = "The noxi engine package that replaces `nix.package`.";
	};

	config = {
		nix.package = import ../as-nix-package.nix config.noxios.package;
		systemd.services.nix-daemon.aliases = lib.mkIf config.nix.enable [ "noxi-daemon.service" ];
		systemd.sockets.nix-daemon.aliases = lib.mkIf config.nix.enable [ "noxi-daemon.socket" ];
	};
}
