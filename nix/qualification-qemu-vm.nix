# The pinned VM module hardcodes an auto-sized boot image with no spare
# space. Its inode budget is too small for the offline flake source trees.
# Patch that one construction argument for qualification only, preserving
# upstream imports and runtime behavior. Never patch the host's Nixpkgs.
{ nixpkgs }:
let
	source = builtins.readFile "${nixpkgs}/nixos/modules/virtualisation/qemu-vm.nix";
	needle = "diskSize = \"auto\";";
	patched = builtins.replaceStrings [
		needle
		"../../lib/"
		"../profiles/qemu-guest.nix"
		"./disk-size-option.nix"
		"./credentials-options.nix"
	] [
		"diskSize = 16384;"
		"${nixpkgs}/nixos/lib/"
		"${nixpkgs}/nixos/modules/profiles/qemu-guest.nix"
		"${nixpkgs}/nixos/modules/virtualisation/disk-size-option.nix"
		"${nixpkgs}/nixos/modules/virtualisation/credentials-options.nix"
	] source;
in
assert builtins.length (builtins.split needle source) == 3;
builtins.toFile "qualification-qemu-vm.nix" patched
