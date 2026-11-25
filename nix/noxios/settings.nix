{ rust-overlay }:
{ config, lib, pkgs, ... }:
let
	cfg = config.noxios.settings;
	console = import ./settings-package.nix { inherit pkgs rust-overlay; };
	desktop = import ./settings-package.nix {
		inherit pkgs rust-overlay;
		desktop = true;
	};
	packages = [ console ] ++ lib.optional cfg.graphical desktop;
	env = [
		"--set NOXIOS_SETTINGS_ENGINE_DIR ${lib.escapeShellArg "${config.nix.package}/bin"}"
		"--set NOXIOS_SETTINGS_HOST ${lib.escapeShellArg config.networking.hostName}"
		"--prefix PATH : ${lib.escapeShellArg (lib.makeBinPath [ config.nix.package pkgs.git pkgs.jujutsu pkgs.gh ])}"
		"--run ${lib.escapeShellArg ''export NIX_CONFIG="${"$"}{NIX_CONFIG:-}"$'\nextra-experimental-features = nix-command flakes' ''}"
	];
	settings = pkgs.symlinkJoin {
		name = "noxios-settings";
		paths = packages;
		nativeBuildInputs = [ pkgs.makeWrapper ];
		postBuild = ''
			for executable in noxios-settings noxios-settings-tui ${lib.optionalString cfg.graphical "noxios-settings-gui"}; do
				wrapProgram "$out/bin/$executable" ${lib.concatStringsSep " " env}
			done
		'';
	};
in
{
	options.noxios.settings = {
		enable = lib.mkEnableOption "the built-in noxios Settings interfaces" // { default = true; };
		graphical = lib.mkOption {
			type = lib.types.bool;
			default = config.services.xserver.enable || config.services.displayManager.sessionPackages != [ ];
			defaultText = lib.literalExpression "config.services.xserver.enable || config.services.displayManager.sessionPackages != [ ]";
			description = "Install the settings window and desktop launcher. Enable explicitly for an independently started compositor.";
		};
	};
	config = lib.mkIf cfg.enable {
		environment.systemPackages = [ settings ];
		system.build.noxiosSettings = settings;
	};
}
