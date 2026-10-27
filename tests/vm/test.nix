# Automated VM check for the packaged noxi command.
#
# The script it runs lives in ./test-script.py: the test driver type-checks
# it, so keeping it in a real .py file means editors and the checker see the
# same thing.
{
	noxi,
	nixReference,
	testers,
}:
testers.runNixOSTest {
	name = "noxi";

	node.specialArgs = { inherit noxi nixReference; };

	nodes.machine = ./machine.nix;

	testScript = builtins.readFile ./test-script.py;
}
