# The standalone user's second Home Manager generation, for the profile,
# rollback and generation-removal comparison with CppNix.
{ ... }:
{
	home.username = "standalone";
	home.homeDirectory = "/home/standalone";
	home.stateVersion = "26.05";
	home.file."qualification-marker".text = "standalone-b";
}
