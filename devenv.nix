{ pkgs, ... }:
let
	rustOverlayLock = (builtins.fromJSON (builtins.readFile ./flake.lock)).nodes.rust-overlay.locked;
	rustOverlay = import (builtins.fetchTree {
		inherit (rustOverlayLock) type owner repo rev narHash;
	});
	rustPkgs = pkgs.extend rustOverlay;
	rustToolchain = rustPkgs.rust-bin.nightly."2026-09-20".minimal.override {
		extensions = [ "clippy" "rust-analyzer" "rustfmt" ];
	};
in
{
	packages = [
		rustToolchain
		pkgs.bash
		pkgs.cmake
		pkgs.coreutils
		pkgs.gawk
		pkgs.git
		pkgs.just
		pkgs.jq
		pkgs.meson
		pkgs.ninja
		pkgs.nix
		pkgs.pkg-config
		pkgs.zstd
	];
}
