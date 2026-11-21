# What `noxi system init --root /mnt` generates in the install test's
# installer VM, for the build host's evaluation of the installed system. The
# test formats the root file system with this UUID, and checks that the
# generated file gives the same system before it installs.
{ lib, modulesPath, ... }:
{
	imports = [ (modulesPath + "/profiles/qemu-guest.nix") ];

	boot.initrd.availableKernelModules = [ "ata_piix" "virtio_pci" "floppy" "sr_mod" "virtio_blk" ];
	boot.initrd.kernelModules = [ ];
	boot.kernelModules = [ ];
	boot.extraModulePackages = [ ];

	fileSystems."/" = {
		device = "/dev/disk/by-uuid/5a1e2b3c-4d5e-4f60-8a7b-9c0d1e2f3a4b";
		fsType = "ext4";
	};

	swapDevices = [ ];

	nixpkgs.hostPlatform = lib.mkDefault "x86_64-linux";
}
