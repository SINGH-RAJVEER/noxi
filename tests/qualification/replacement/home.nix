# The standalone Home Manager configuration the qualification VM's
# `standalone` user builds and switches to.
{ ... }:
{
	home.username = "standalone";
	home.homeDirectory = "/home/standalone";
	home.stateVersion = "26.05";
	home.file."qualification-marker".text = "standalone";
}
