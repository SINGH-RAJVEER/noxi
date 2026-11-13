{ pkgs, ... }:
let
	rustOverlayLock = (builtins.fromJSON (builtins.readFile ./flake.lock)).nodes.rust-overlay.locked;
	rustOverlay = import (builtins.fetchTree {
		inherit (rustOverlayLock) type owner repo rev narHash;
	});
	rustPkgs = pkgs.extend rustOverlay;
	rustToolchain = rustPkgs.rust-bin.selectLatestNightlyWith (t: t.minimal.override {
		extensions = [ "clippy" "rust-analyzer" "rustfmt" ];
	});
in
{
	dotenv.disableHint = true;
	cachix.pull = [ "noxios" ];
	packages = [
		# Rust and native code compiled by Cargo dependencies.
		rustToolchain
		pkgs.cargo-nextest
		pkgs.stdenv.cc
		pkgs.gnumake
		pkgs.cmake
		pkgs.meson
		pkgs.ninja
		pkgs.pkg-config

		# Repository workflows, fixture scripts and release inspection.
		pkgs.bash
		pkgs.coreutils
		pkgs.findutils
		pkgs.gawk
		pkgs.gnugrep
		pkgs.gnused
		pkgs.gnutar
		pkgs.gzip
		pkgs.bzip2
		pkgs.xz
		pkgs.curl
		pkgs.git
		pkgs.jujutsu
		pkgs.just
		pkgs.jq
		pkgs.ripgrep
		pkgs.nix
		pkgs.cachix
		pkgs.actionlint
		pkgs.python3
		pkgs.uv
		pkgs.sqlite
		pkgs.zstd
		pkgs.cargo-deny
		pkgs.cargo-about

		# Evaluator profiling.
		pkgs.perf

		# VM disk inspection and the reference VM's monitor connection.
		pkgs.qemu_kvm
		pkgs.socat
	];
}
