{ pkgs, ... }:
{
	languages.rust = {
		enable = true;
		channel = "stable";
		version = "1.95.0";
	};

	packages = [
		pkgs.git
		pkgs.just
	];
}
