# The installed system's configuration in the install test. It replaces the
# configuration.nix `noxi system init` writes and imports the
# hardware-configuration.nix it generates, as Nixpkgs' installer tests do.
{ lib, modulesPath, ... }:
{
	imports = [
		./hardware-configuration.nix
		(modulesPath + "/testing/test-instrumentation.nix")
	];

	boot.loader.grub.device = "/dev/vda";
	boot.loader.grub.extraConfig = "serial; terminal_output serial";

	# The guest's virtualisation extensions follow the build host's CPU; the
	# installed system must be the same on every host.
	boot.kernelModules = lib.mkForce [ ];

	documentation.enable = false;
	hardware.enableAllFirmware = lib.mkForce false;

	# The test has no network.
	nix.settings.substituters = lib.mkForce [ ];

	system.stateVersion = "26.11";
}
