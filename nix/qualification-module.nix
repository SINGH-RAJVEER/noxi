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
in
{
	# The harness tiers, off unless noxi.qualification.harness.enable is set.
	imports = [ ./harness-tiers.nix ];
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
		nix.package = lib.mkForce (import ./as-nix-package.nix cfg.package);
		# Nixpkgs' default `nix.checkConfig` stays on: the generated nix.conf
		# is validated with the candidate's own `nix config show` while the
		# system builds.
	};
}
