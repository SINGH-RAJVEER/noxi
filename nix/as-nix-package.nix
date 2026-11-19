# Presents a noxi engine package to NixOS as a `nix.package` value.
#
# `pkgs.formats.nixConf` and other Nixpkgs callers compare `nix.package`'s
# version against CppNix releases, even with `nix.checkConfig` off. The
# package states the reference release its commands follow; the executables
# keep their own crate versions.
package:
let
	metadata = {
		pname = "noxi-replacement-provisional";
		version = "2.35.2";
	};
in
package // metadata // { out = package.out // metadata; }
