# The live system's flake: nix/noxios/live/flake.nix and configuration.nix,
# with a flake.lock generated from the sources this image was built from,
# copied to /etc/nixos at boot.
#
# Every locked node is a `path` input at the source's store path, with its
# NAR hash, modification time and revision, so the flake evaluates offline
# from the image's store. The `original` of each node is the GitHub
# reference `noxi flake update` follows. The lock's text refers to the
# source paths, which puts them in the image's closure.
#
# The root `nixos` channel stays: NixOS's own ISO check evaluates
# `nix-env -iA nixos.procps` through it, and `nixos-rebuild --upgrade`
# updates it by that name. NIX_PATH is unchanged.
{
	nixpkgs,
	rust-overlay,
	# The noxi repository source the image was built from.
	source,
}:
{ config, lib, pkgs, ... }:
let
	node =
		input: original:
		{
			locked = {
				type = "path";
				path = input.outPath;
				inherit (input) narHash lastModified;
			} // lib.optionalAttrs (input ? rev) { inherit (input) rev; };
			original = { type = "github"; } // original;
		};
	lock = {
		nodes = {
			root.inputs = {
				nixpkgs = "nixpkgs";
				noxi = "noxi";
				rust-overlay = "rust-overlay";
			};
			nixpkgs = node nixpkgs {
				owner = "NixOS";
				repo = "nixpkgs";
				ref = "nixos-unstable";
			};
			noxi = node source {
				owner = "SINGH-RAJVEER";
				repo = "nix-rs";
			} // { flake = false; };
			rust-overlay = node rust-overlay {
				owner = "oxalica";
				repo = "rust-overlay";
			} // { flake = false; };
		};
		root = "root";
		version = 7;
	};
	flake = pkgs.runCommand "noxios-live-flake" { nativeBuildInputs = [ pkgs.jq ]; } ''
		mkdir "$out"
		cp ${./live/flake.nix} "$out/flake.nix"
		cp ${./live/configuration.nix} "$out/configuration.nix"
		jq . ${pkgs.writeText "flake.lock" (builtins.toJSON lock)} > "$out/flake.lock"
	'';
in
{
	system.build.noxiosFlake = flake;

	# The cloned /etc/nixos/configuration.nix of clone-config.nix rebuilds a
	# stock NixOS installer, without noxi. The flake replaces it: with
	# /etc/nixos/flake.nix present, `nixos-rebuild` and `noxi system` both
	# build the flake. clone-config.nix still creates /mnt.
	installer.cloneConfig = false;

	# Writable copies, so `noxi flake update` can rewrite the lock; an
	# existing flake is kept, as clone-config.nix keeps configuration.nix.
	boot.postBootCommands = ''
		if ! [ -e /etc/nixos/flake.nix ]; then
			mkdir -p /etc/nixos
			install -m 0644 ${flake}/flake.nix ${flake}/flake.lock ${flake}/configuration.nix /etc/nixos/
		fi
	'';
}
