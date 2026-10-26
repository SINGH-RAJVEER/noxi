# Import only in disposable qualification test nodes. This is deliberately
# not a public host migration module, and makes no compatibility claim.
#
# With `nixrs.qualification.enable`, the package replaces `nix.package`, so
# every NixOS caller of the package (the nix-daemon units through
# systemd.packages, nixos-rebuild's PATH, the boot-loader installers,
# register-nix-paths, the nix-gc and nix-optimise services and Home Manager's
# activation) runs nix-rs. Exactly one daemon unit exists: the package's.
{ config, lib, pkgs, ... }:
let
	cfg = config.nixrs.qualification;
	metadata = {
		pname = "nixrs-replacement-provisional";
		version = "2.35.2";
	};
in
{
	options.nixrs.qualification = {
		enable = lib.mkEnableOption "the unqualified nix-rs engine in a disposable VM";
		package = lib.mkOption {
			type = lib.types.package;
			default = pkgs.nixrs-replacement;
			defaultText = lib.literalExpression "pkgs.nixrs-replacement";
			description = "The provisional replacement package, `packages.x86_64-linux.nixrs-replacement`.";
		};
	};
	config = lib.mkIf cfg.enable {
		# `pkgs.formats.nixConf` asserts a Nix version of at least 2.2 even
		# with checkConfig off. The VM states the reference format version it
		# targets; the executables keep their own crate versions.
		nix.package = lib.mkForce (cfg.package // metadata // { out = cfg.package.out // metadata; });
		# Nixpkgs validates the generated nix.conf with `nix config show` while
		# building the system, and nix-rs has no `nix` executable. Recorded as a
		# gap; the guest checks the command directly.
		nix.checkConfig = false;
	};
}
