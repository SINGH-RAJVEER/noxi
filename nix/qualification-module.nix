# Import only in disposable qualification test nodes. This is deliberately
# not a public host migration module, and makes no compatibility claim.
#
# With `noxi.qualification.enable`, the package replaces `nix.package`, so
# every NixOS caller of the package (the nix-daemon units through
# systemd.packages, nixos-rebuild's PATH, the boot-loader installers,
# register-nix-paths, the nix-gc and nix-optimise services and Home Manager's
# activation) runs noxi. Exactly one daemon unit exists: the package's.
{ config, lib, pkgs, ... }:
let
	cfg = config.noxi.qualification;
	metadata = {
		pname = "noxi-replacement-provisional";
		version = "2.35.2";
	};
in
{
	options.noxi.qualification = {
		enable = lib.mkEnableOption "the unqualified noxi engine in a disposable VM";
		package = lib.mkOption {
			type = lib.types.package;
			default = pkgs.noxi-replacement;
			defaultText = lib.literalExpression "pkgs.noxi-replacement";
			description = "The provisional replacement package, `packages.x86_64-linux.noxi-replacement`.";
		};
	};
	config = lib.mkIf cfg.enable {
		# `pkgs.formats.nixConf` asserts a Nix version of at least 2.2 even
		# with checkConfig off. The VM states the reference format version it
		# targets; the executables keep their own crate versions.
		nix.package = lib.mkForce (cfg.package // metadata // { out = cfg.package.out // metadata; });
		# Nixpkgs' default `nix.checkConfig` stays on: the generated nix.conf
		# is validated with the candidate's own `nix config show` while the
		# system builds.
	};
}
