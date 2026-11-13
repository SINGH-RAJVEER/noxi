{ pkgs }:
let
	config = import ../devenv.nix { inherit pkgs; };
in
pkgs.mkShell {
	packages = config.packages;
}
