{ lib, ... }:
{
	isoImage.edition = lib.mkForce "desktop";
	services.xserver.enable = true;
	services.xserver.desktopManager.xfce.enable = true;
	services.xserver.displayManager.lightdm.enable = true;
	services.displayManager.autoLogin = {
		enable = true;
		user = "noxi";
	};
	services.displayManager.defaultSession = "xfce";
}
