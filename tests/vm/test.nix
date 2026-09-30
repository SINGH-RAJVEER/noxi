# Automated VM check for the packaged nixrs command.
#
# The script it runs lives in ./test-script.py: the test driver type-checks
# it, so keeping it in a real .py file means editors and the checker see the
# same thing.
{
	nixrs,
	nixReference,
	testers,
}:
testers.runNixOSTest {
	name = "nixrs";

	node.specialArgs = { inherit nixrs nixReference; };

	nodes.machine = ./machine.nix;

	testScript = builtins.readFile ./test-script.py;
}
