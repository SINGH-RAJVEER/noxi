# The machine the VM test boots: a NixOS system whose Nix is the pinned
# CppNix reference, with the packaged nixrs installed beside it.
{
	nixrs,
	nixReference,
	...
}:
{
	# Every comparison in the test script is against this binary and the
	# daemon it starts, never the Nix that happens to be in nixpkgs.
	nix.package = nixReference;

	environment.systemPackages = [ nixrs ];

	# An unprivileged user, so evaluation runs the way it does on a real
	# machine: through the daemon, with the system's nix.conf.
	users.users.tester.isNormalUser = true;

	# A case file that lives in the store, so `--file` is also checked against
	# a read-only store path rather than only a home directory.
	environment.etc."nixrs-vm/case.nix".text = ''
		let
			double = x: x * 2;
		in
		{
			doubled = map double [ 1 2 3 ];
			system = builtins.currentSystem;
			digest = builtins.hashString "sha256" "abc";
		}
	'';
}
