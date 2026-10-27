# Driven by tests/vm/test.nix against the machine that tests/vm/machine.nix
# describes.
#
# noxi evaluates pure expressions and owns no store, and the host-side tests
# already compare it with the reference in an isolated environment. What only
# a real machine adds is checked here: the packaged binary, the reference Nix
# the machine actually runs, the guest's own platform, an unprivileged user
# under a real daemon, and a store that noxi has to leave alone.

import shlex

# The flake's nix-reference input. Moving the pin is a deliberate change that
# also moves the fixture expectations, so it is spelled out rather than read.
REFERENCE_VERSION = "2.35.2"
STORE_CASE = "/etc/noxi-vm/case.nix"

start_all()
machine.wait_for_unit("multi-user.target")
machine.wait_for_unit("nix-daemon.socket")


def as_tester(command: str) -> str:
	return "su -l tester -c " + shlex.quote(command)


def compare(reference_args: str, noxi_args: str) -> tuple[int, str]:
	# Exit status and stdout only: noxi renders diagnostics differently, and
	# the host-side runner is where stderr is compared.
	expected = machine.execute(as_tester("nix-instantiate --eval --strict " + reference_args))
	actual = machine.execute(as_tester("noxi eval " + noxi_args))
	assert actual == expected, f"reference {expected!r}, noxi {actual!r}"
	return actual


def compare_expr(expression: str) -> tuple[int, str]:
	quoted = shlex.quote(expression)
	return compare("--expr " + quoted, "--expr " + quoted)


with subtest("the machine runs the pinned reference"):
	version = machine.succeed(as_tester("nix-instantiate --version"))
	assert version.strip() == f"nix-instantiate (Nix) {REFERENCE_VERSION}", version

with subtest("the packaged noxi is on PATH"):
	version = machine.succeed(as_tester("noxi --version"))
	assert version.startswith("noxi ") and "(preview)" in version, version

with subtest("evaluation agrees with the reference on this machine"):
	# The guest's platform, not the one the test was built on.
	arch = machine.succeed("uname -m").strip()
	assert compare_expr("builtins.currentSystem") == (0, f'"{arch}-linux"\n')
	compare_expr("builtins.langVersion")
	compare_expr('builtins.toJSON { a = [ 1 "x" ]; }')

with subtest("an evaluation error fails in both"):
	status, _ = compare_expr('throw "boom"')
	assert status == 1, status

with subtest("a file is evaluated from the store and from a home directory"):
	store_path = machine.succeed(f"realpath {STORE_CASE}").strip()
	assert store_path.startswith("/nix/store/"), store_path
	status, _ = compare(store_path, "--file " + store_path)
	assert status == 0, status
	machine.succeed(as_tester(f"cp {STORE_CASE} ~/case.nix"))
	status, _ = compare("~/case.nix", "--file ~/case.nix")
	assert status == 0, status

with subtest("noxi needs no daemon and leaves the store untouched"):
	machine.succeed("systemctl stop nix-daemon.socket nix-daemon.service")
	store_state = "ls /nix/store | sha256sum && find /nix/var/nix/db -type f -exec sha256sum {} + | sort"
	before = machine.succeed(store_state)
	assert "/nix/var/nix/db/db.sqlite" in before, before
	# As root, which could write to the store directly, rather than as a user
	# who could not have changed it anyway.
	machine.succeed(f"noxi eval --file {STORE_CASE}")
	machine.succeed("noxi eval --expr " + shlex.quote('builtins.toJSON { a = 1; }'))
	after = machine.succeed(store_state)
	assert after == before, f"before:\n{before}\nafter:\n{after}"
