# The noxios installer profile.
#
# A fork of two Nixpkgs modules that name the live user `nixos`:
# `nixos/modules/profiles/installation-device.nix` and
# `nixos/modules/installer/cd-dvd/installation-cd-base.nix`, both at Nixpkgs
# revision e94cb152ed51bd6e24eb4a41f1460252beb52cd2 (the qualification row's
# pin, tests/qualification/flake.lock). Compare against that revision when the
# pin moves.
#
# The changes from upstream:
# - the live user is `noxi`, with the same groups, empty password, console
#   auto-login, passwordless sudo through `wheel` and `trusted-users` entry;
# - the getty help line names `noxi` and points at `noxios-help`;
# - the NixOS manual is no longer forced on (branding.nix turns it off);
# - the `live.noxios.passwd=` kernel parameter sets the live user's password,
#   and `live.nixos.passwd=` is still accepted for existing boot setups.
# Everything else, including the root `nixos` channel and the cloned
# `/etc/nixos/configuration.nix`, is upstream's behaviour.
{
	config,
	lib,
	options,
	pkgs,
	modulesPath,
	...
}:
let
	inherit (lib) mkDefault mkImageMediaOverride optionalString;
	user = "noxi";
in
{
	disabledModules = [
		"installer/cd-dvd/installation-cd-base.nix"
		"profiles/installation-device.nix"
	];

	imports = [
		# installation-cd-base.nix
		"${modulesPath}/installer/cd-dvd/iso-image.nix"
		"${modulesPath}/profiles/base.nix"

		# installation-device.nix: enable devices which are usually scanned,
		# because we don't know the target system.
		"${modulesPath}/installer/scan/detected.nix"
		"${modulesPath}/installer/scan/not-detected.nix"
		# Allow "nixos-rebuild" to work properly by providing
		# /etc/nixos/configuration.nix.
		"${modulesPath}/profiles/clone-config.nix"
		# Include a copy of Nixpkgs so that nixos-install works out of the box.
		"${modulesPath}/installer/cd-dvd/channel.nix"
	];

	config = {
		# installation-cd-base.nix

		hardware.enableAllHardware = true;

		# Adds terminus_font for people with HiDPI displays
		console.packages = options.console.packages.default ++ [ pkgs.terminus_font ];

		# EFI booting
		isoImage.makeEfiBootable = true;

		# USB booting
		isoImage.makeUsbBootable = true;

		# Add Memtest86+ to the CD.
		boot.loader.grub.memtest86.enable = true;

		# An installation media cannot tolerate a host config defined file
		# system layout on a fresh machine, before it has been formatted.
		swapDevices = mkImageMediaOverride [ ];
		fileSystems = mkImageMediaOverride config.lib.isoFileSystems;
		boot.initrd.luks.devices = mkImageMediaOverride { };

		boot.postBootCommands = ''
			for o in $(</proc/cmdline); do
				case "$o" in
					live.noxios.passwd=*|live.nixos.passwd=*)
						set -- $(IFS==; echo $o)
						echo "${user}:$2" | ${pkgs.shadow}/bin/chpasswd
						;;
				esac
			done
		'';

		environment.defaultPackages = with pkgs; [
			rsync
		];

		programs.git.enable = mkDefault true;

		system.stateVersion = mkDefault lib.trivial.release;

		# installation-device.nix

		system.nixos.variant_id = mkDefault "installer";

		# Enable in installer, even if the minimal profile disables it.
		documentation.enable = mkImageMediaOverride true;

		# Use the less privileged live user.
		users.users.${user} = {
			isNormalUser = true;
			description = "noxios live user";
			extraGroups = [
				"wheel"
				"networkmanager"
				"video"
			];
			# Allow the user to log in without a password.
			initialHashedPassword = "";
		};

		# Allow the user to log in as root without a password.
		users.users.root.initialHashedPassword = "";

		# Don't require sudo/root to `reboot` or `poweroff`.
		security.polkit.enable = true;

		# Allow passwordless sudo from the live user.
		security.sudo = {
			enable = mkDefault true;
			wheelNeedsPassword = mkImageMediaOverride false;
		};

		# Automatically log in at the virtual consoles.
		services.getty.autologinUser = user;

		# Some more help text. Plain strings: indented strings keep the tabs.
		services.getty.helpLine =
			lib.concatLines [
				"The \"${user}\" and \"root\" accounts have empty passwords."
				""
				"To log in over ssh you must set a password for either \"${user}\" or \"root\""
				"with `passwd` (prefix with `sudo` for \"root\"), or add your public key to"
				"/home/${user}/.ssh/authorized_keys or /root/.ssh/authorized_keys."
				""
				"To set up a wireless connection, run `nmtui`."
				""
				"Run `noxi settings tui` for system settings or `noxi settings --help` for commands."
				""
				"Run `noxios-help` for the noxios manual. The live system's flake is in"
				"/etc/nixos; `sudo noxi system switch` rebuilds the running system from it."
			]
			+ optionalString config.services.xserver.enable (lib.concatLines [
				""
				"Type `sudo systemctl start display-manager' to"
				"start the graphical user interface."
			]);

		# We run sshd by default. Login is only possible after adding a
		# password via "passwd" or by adding a ssh key to ~/.ssh/authorized_keys.
		# The latter one is particular useful if keys are manually added to
		# installation device for head-less systems i.e. arm boards by manually
		# mounting the storage in a different system.
		services.openssh = {
			enable = mkDefault true;
			settings.PermitRootLogin = mkDefault "yes";
		};

		# Provide networkmanager for easy network configuration.
		networking.networkmanager.enable = true;

		# Tell the Nix evaluator to garbage collect more aggressively.
		# This is desirable in memory-constrained environments that don't
		# (yet) have swap set up.
		environment.variables.GC_INITIAL_HEAP_SIZE = "1M";

		# Make the installer more likely to succeed in low memory
		# environments. The kernel's overcommit heustistics bite us
		# fairly often, preventing processes such as nix-worker or
		# download-using-manifests.pl from forking even if there is
		# plenty of free memory.
		boot.kernel.sysctl."vm.overcommit_memory" = "1";

		# To speed up installation a little bit, include the complete
		# stdenvNoCC in the Nix store on the CD.
		system.extraDependencies =
			with pkgs;
			[
				stdenvNoCC # for runCommand
				busybox
				# For boot.initrd.systemd
				makeInitrdNGTool
			]
			++ jq.all; # for closureInfo

		boot.swraid.enable = true;
		# remove warning about unset mail
		boot.swraid.mdadmConf = "PROGRAM ${pkgs.coreutils}/bin/true";

		# Show all debug messages from the kernel but don't log refused packets
		# because we have the firewall enabled. This makes installs from the
		# console less cumbersome if the machine has a public IP.
		networking.firewall.logRefusedConnections = mkDefault false;

		# Prevent installation media from evacuating persistent storage, as their
		# var directory is not persistent and it would thus result in deletion of
		# those entries.
		environment.etc."systemd/pstore.conf".text = "[PStore]\nUnlink=no\n";

		# allow nix-copy to live system
		nix.settings.trusted-users = [ user ];

		# Install less voices for speechd to save some space
		nixpkgs.overlays = [
			(_: prev: {
				mbrola-voices = prev.mbrola-voices.override {
					# only ship with one voice per language
					languages = [ "*1" ];
				};
			})
		];
	};
}
