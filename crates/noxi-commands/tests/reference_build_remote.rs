//! Remote building compared with CppNix 2.35.2: `builders`, the build hook,
//! `nix __build-remote`, and the `ssh` and `ssh-ng` transports.
//!
//! No host is contacted. `ssh` is a script that records its arguments and
//! runs the command it is given on a chroot store of this machine. Both
//! engines use the same directory one after the other, removed in between.
//! The reference comes from `NOXI_REFERENCE_BIN`, else
//! `target/nix-reference/bin`; without it the tests print a notice and
//! return.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn reference_dir() -> Option<PathBuf> {
	let dir = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		});
	if dir.join("nix").exists() {
		Some(dir)
	} else {
		eprintln!("skipping: no reference nix in {}", dir.display());
		None
	}
}

fn busybox() -> Option<PathBuf> {
	let dir = std::env::var_os("NOXI_TEST_BUSYBOX")
		.map(PathBuf::from)
		.unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-busybox"));
	let file = dir.join("bin/busybox").canonicalize().ok();
	if file.is_none() {
		eprintln!("skipping: no busybox in {}", dir.display());
	}
	file
}

fn candidate_dir() -> &'static Path {
	Path::new(env!("CARGO_BIN_EXE_nix")).parent().unwrap()
}

const EXPR: &str = r#"
{ busybox ? /. + builtins.getEnv "BUSYBOX" }:
let
	mk = attrs: derivation ({
		system = "x86_64-linux";
		builder = "${busybox}";
	} // attrs // { args = [ "sh" "-e" "-c" attrs.script ]; });
	ca = { __contentAddressed = true; outputHashMode = "recursive"; outputHashAlgo = "sha256"; };
in rec {
	one = mk { name = "one"; script = "echo building one; echo one > $out"; };
	two = mk { name = "two"; script = "echo building two; read x < ${one}; echo $x two > $out"; };
	foo = mk { name = "foo"; script = "echo foo > $out"; requiredSystemFeatures = [ "foo" ]; };
	bar = mk { name = "bar"; script = "read x < ${foo}; echo $x bar > $out"; requiredSystemFeatures = [ "bar" ]; };
	outputs = mk { name = "outputs"; outputs = [ "out" "dev" ]; script = "echo out > $out; echo $out > $dev"; };
	local = mk { name = "local"; script = "echo local > $out"; preferLocalBuild = true; };
	other = mk { name = "other"; system = "riscv64-linux"; script = "echo other > $out"; };
	fail = mk { name = "fail"; script = "echo about to fail; echo kept > kept; exit 7"; };
	silent = mk { name = "silent"; script = "echo before; while :; do :; done"; };
	noisy = mk { name = "noisy"; script = "i=0; while [ $i -lt 200 ]; do echo 0123456789; i=$((i+1)); done; echo > $out"; };
	phases = mk { name = "phases"; script = "echo '@nix { \"action\": \"setPhase\", \"phase\": \"buildPhase\" }'; echo in phase; echo > $out"; };
	fixed = mk { name = "fixed"; script = "echo fixed > $out"; outputHashMode = "recursive"; outputHashAlgo = "sha256"; outputHash = "sha256-nvVUUt4LlZ5PmxSMzsxG1CY6JtRJ+D0NIWZ6ObFrpdU="; };
	wrong = mk { name = "wrong"; script = "echo wrong > $out"; outputHashMode = "recursive"; outputHashAlgo = "sha256"; outputHash = "sha256-nvVUUt4LlZ5PmxSMzsxG1CY6JtRJ+D0NIWZ6ObFrpdU="; };
	caOne = mk ({ name = "ca-one"; script = "echo one > $out"; } // ca);
	caTwo = mk ({ name = "ca-two"; script = "read x < ${caOne}; echo $x two > $out"; } // ca);
	structured = mk { name = "structured"; script = "${busybox} grep -o '\"outputs\":{[^}]*}' $NIX_ATTRS_JSON_FILE; exit 3"; __structuredAttrs = true; nested = { a = [ 1 "x\ny" ]; }; };
	odd = mk { name = "odd"; script = "printf %s \"$quote\" > $out"; quote = "' \" \\ $(touch $ROOT/pwned) `touch $ROOT/pwned` ; touch $ROOT/pwned; \n\t é"; };
	leafE = mk { name = "leaf-e"; script = "echo e > $out"; };
	leafC = mk { name = "leaf-c"; script = "echo c > $out"; };
	leafA = mk { name = "leaf-a"; script = "echo a > $out"; };
	leafD = mk { name = "leaf-d"; script = "echo d > $out"; };
	leafB = mk { name = "leaf-b"; script = "echo b > $out"; };
	leaves = mk { name = "leaves"; script = "cat ${leafE} ${leafC} ${leafA} ${leafD} ${leafB} > $out"; };
}
"#;

/// `ssh`: records its arguments, then runs the command in the store named
/// after the host.
const SSH: &str = r#"#!/bin/sh
echo "$(printf '%s|' "$@")SHELL=$SHELL DISPLAY=$DISPLAY" >> "$ROOT/ssh.log"
[ "$1" = -O ] && exit 255
[ -e "$ROOT/ssh.broken" ] && { echo "ssh: cannot connect" >&2; exit 255; }
host=$1
[ -e "$ROOT/ssh.motd" ] && echo "Welcome to the machine"
while [ "$1" != -- ]; do shift; done
shift
[ -e "$ROOT/ssh.motd" ] || echo started
NIX_REMOTE="local?root=$ROOT/machine-$host"
NIX_CONFIG="$(cat "$ROOT/machine.conf" 2> /dev/null)"
export NIX_REMOTE NIX_CONFIG
# As sshd does, the words are joined and given to the user's shell.
exec sh -c "$*"
"#;

fn script(path: &Path, text: &str) {
	fs::write(path, text).unwrap();
	fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Run the shell commands `steps` with one engine in a fresh directory,
/// returning each step's exit status, standard output and error.
fn session(bin: &Path, root: &Path, features: &str, steps: &[&str]) -> Vec<String> {
	noxi_test_support::remove_tree(root).unwrap();
	for dir in ["etc", "bin", "tmp", "home"] {
		fs::create_dir_all(root.join(dir)).unwrap();
	}
	fs::write(
		root.join("etc/nix.conf"),
		format!("experimental-features = {features}\nbuild-users-group =\nsubstituters =\n"),
	)
	.unwrap();
	fs::write(root.join("x.nix"), EXPR).unwrap();
	script(&root.join("bin/ssh"), SSH);
	steps
		.iter()
		.map(|step| {
			let output = Command::new("bash")
				.args(["-c", step])
				.current_dir(root)
				.env_clear()
				.env("HOME", root.join("home"))
				.env("TMPDIR", root.join("tmp"))
				.env("ROOT", root)
				.env("BIN", bin)
				.env("REF", reference_dir().unwrap_or_default())
				.env("BUSYBOX", busybox().unwrap())
				.env("DISPLAY", ":0")
				.env(
					"PATH",
					format!(
						"{}:{}:/run/current-system/sw/bin:/usr/bin:/bin",
						root.join("bin").display(),
						bin.display()
					),
				)
				.env("NIX_CONF_DIR", root.join("etc"))
				.env("NIX_STATE_DIR", root.join("state"))
				.output()
				.unwrap();
			let errors: String = String::from_utf8_lossy(&output.stderr)
				.lines()
				.filter(|line| !line.starts_with("warning: you don't have Internet access"))
				.map(|line| format!("{line}\n"))
				.collect();
			format!(
				"$ {step}\n{:?}\n{}{errors}",
				output.status.code(),
				String::from_utf8_lossy(&output.stdout)
			)
		})
		.collect()
}

fn same_with(features: &str, steps: &[&str]) {
	let (Some(reference), Some(_)) = (reference_dir(), busybox()) else {
		return;
	};
	let scratch = noxi_test_support::tempdir().unwrap();
	let root = scratch.path().join("s");
	let expected = session(&reference, &root, features, steps);
	let actual = session(candidate_dir(), &root, features, steps);
	noxi_test_support::remove_tree(&root).unwrap();
	let mut differences = String::new();
	for (expected, actual) in expected.iter().zip(&actual) {
		if expected != actual {
			differences += &format!("--- reference\n{expected}\n--- noxi\n{actual}\n");
		}
	}
	assert!(differences.is_empty(), "{differences}");
}

fn same(steps: &[&str]) {
	same_with("nix-command", steps)
}

/// The store's builds go to machines that are local stores.
#[test]
fn local_machines_build_for_a_local_store() {
	same(&[
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --builders $ROOT/m1; cat $ROOT/m0/nix/store/*-one",
		"nix path-info --store $ROOT/m1 --all | sort",
		"nix path-info --store $ROOT/m0 --json --all | sed 's|\"registrationTime\":[0-9]*|T|g'",
		"nix log --store $ROOT/m0 -f x.nix one",
		"nix-build x.nix -A outputs --no-out-link --store $ROOT/m0 -j0 --builders $ROOT/m1; ls $ROOT/m1/nix/store | grep outputs",
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --builders $ROOT/m1",
		"nix-build x.nix -A phases --no-out-link --store $ROOT/m0 -j0 --builders $ROOT/m1",
		"nix log --store $ROOT/m0 -f x.nix phases",
		"nix-build x.nix -A odd --no-out-link --store $ROOT/m0 -j0 --builders $ROOT/m1 2> /dev/null; od -c $ROOT/m0/nix/store/*-odd; ls $ROOT/pwned",
		"nix-build x.nix -A structured --no-out-link --store $ROOT/m0 -j0 --builders $ROOT/m1",
		"nix-build x.nix -A two --no-out-link --store $ROOT/n0 -j0 --builders $ROOT/n1 2> /dev/null; cat $ROOT/n0/nix/store/*-two",
	]);
}

#[test]
fn features_choose_machines() {
	same(&[
		"nix-build x.nix -A bar --no-out-link --store $ROOT/m0 -j0 --builders \"$ROOT/m1 - - 1 1 foo; $ROOT/m2 - - 1 1 bar\" 2> /dev/null; cat $ROOT/m0/nix/store/*-bar",
		"ls $ROOT/m1/nix/store | grep -v drv; ls $ROOT/m2/nix/store | grep -v drv",
		"nix-build x.nix -A foo --no-out-link --store $ROOT/n0 -j0 --builders \"$ROOT/m1 - - 1 1 - foo\" 2> /dev/null",
		"nix-build x.nix -A one --no-out-link --store $ROOT/n0 -j0 --builders \"$ROOT/m1 - - 1 1 - foo\"",
		"nix-build x.nix -A foo --no-out-link --store $ROOT/o0 -j0 --builders \"$ROOT/m1 - - 1 1 bar,baz qux\"",
		"nix-build x.nix -A foo --no-out-link --store $ROOT/o0 -j1 --builders \"$ROOT/m1 - - 1 1 bar,baz qux\"",
		"nix-build x.nix -A other --no-out-link --store $ROOT/o0 -j1 --builders \"$ROOT/m1 x86_64-linux,i686-linux\"",
		"nix-build x.nix -A other --no-out-link --store $ROOT/o0 -j0 --builders \"$ROOT/m1 x86_64-linux\"",
		"nix-build x.nix -A one --no-out-link --store $ROOT/o0 -j1 -vvvv --builders \"$ROOT/m1 riscv64-linux - 2\" 2>&1 | grep -A6 'Failed to find'",
		"nix-build x.nix -A one --no-out-link --store $ROOT/p0 -j0 --builders ''",
		"nix-build x.nix -A other --no-out-link --store $ROOT/p0 -j0 --builders ''",
	]);
}

#[test]
fn local_builds_are_preferred_or_replaced_as_configured() {
	same(&[
		"nix-build x.nix -A local --no-out-link --store $ROOT/m0 -j1 --builders $ROOT/m1; ls $ROOT/m1",
		"nix-build x.nix -A local --no-out-link --store $ROOT/n0 -j0 --builders $ROOT/m1",
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j1 --builders $ROOT/m1",
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j1 --builders $ROOT/m1 --check",
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --builders $ROOT/m1 --check",
		"nix-build x.nix -A one --no-out-link --store $ROOT/o0 -j1 --builders $ROOT/m1 --build-hook ''",
		"nix-build x.nix -A one --no-out-link --store $ROOT/p0 -j1 --option builders ''",
	]);
}

#[test]
fn machines_files_are_read() {
	same(&[
		"echo \"$ROOT/m1 - - 1 1 # a comment\" > etc/machines; nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0",
		"printf '# nothing\\n\\n' > etc/machines; nix-build x.nix -A one --no-out-link --store $ROOT/n0 -j0",
		"echo \"$ROOT/m2\" > list; nix-build x.nix -A one --no-out-link --store $ROOT/n0 -j0 --builders @$ROOT/list",
		"echo \"$ROOT/m3\" > list; NIX_REMOTE_SYSTEMS=$ROOT/list nix-build x.nix -A one --no-out-link --store $ROOT/o0 -j0",
		"nix-build x.nix -A one --no-out-link --store $ROOT/p0 -j0 --builders @$ROOT/missing",
		"nix-build x.nix -A one --no-out-link --store $ROOT/p0 -j0 --builders \"$ROOT/m1 - - many\"",
		"nix-build x.nix -A one --no-out-link --store $ROOT/p0 -j0 --builders \"$ROOT/m1 - - 1 fast\"",
		"nix-build x.nix -A one --no-out-link --store $ROOT/p0 -j0 --builders \"$ROOT/m1 - - 1 -2\"",
		"nix-build x.nix -A one --no-out-link --store $ROOT/p0 -j0 --builders \"$ROOT/m1 - - 1 1 - - not*base64\"",
	]);
}

/// Machines are chosen by load relative to speed, and busy slots are locks
/// other processes hold.
#[test]
fn load_and_speed_choose_machines() {
	same(&[
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --builders \"$ROOT/a - - 1 1; $ROOT/b - - 1 2\" 2>&1 | grep ' on '",
		"ls $ROOT/m0/nix/var/nix/current-load",
		// Both engines agree on the names of the locks of a slot.
		"cd $ROOT/m0/nix/var/nix/current-load; exec 9> local:____$(echo $ROOT/b | tr / _ | cut -c2-)-0; flock 9; cd $ROOT; nix-build x.nix -A outputs --no-out-link --store $ROOT/m0 -j0 --builders \"$ROOT/a - - 1 1; $ROOT/b - - 1 2\" 2>&1 | grep ' on '",
		"cd $ROOT/m0/nix/var/nix/current-load; exec 9> local:____$(echo $ROOT/b | tr / _ | cut -c2-)-0; flock 9; cd $ROOT; nix-build x.nix -A foo --no-out-link --store $ROOT/m0 -j1 --option system-features foo --builders \"$ROOT/b - - 1 2 foo\"; ls $ROOT/b/nix/store | grep foo",
		"nix-build x.nix -A two --no-out-link --store $ROOT/n0 -j0 --builders \"$ROOT/a - - 4 3; $ROOT/b - - 1 2; $ROOT/c - - 1 3\" 2>&1 | grep ' on ' | sort",
		"nix-build x.nix -A fixed --no-out-link --store $ROOT/n0 -j0 --builders \"$ROOT/a - - 0 3; $ROOT/b - - 1 2\" 2>&1 | grep ' on '",
	]);
}

/// A build waits for a machine whose slot is taken.
#[test]
fn a_busy_machine_postpones_the_build() {
	same(&[
		"mkdir -p $ROOT/m0/nix/var/nix/current-load",
		"f=$ROOT/m0/nix/var/nix/current-load/local:____$(echo $ROOT/b | tr / _ | cut -c2-)-0; (exec 9> $f; flock 9; sleep 4) & sleep 1; nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --option build-poll-interval 1 --builders $ROOT/b",
	]);
}

#[test]
fn failures_on_machines_are_reported() {
	same(&[
		"nix-build x.nix -A fail --no-out-link --store $ROOT/m0 -j0 --builders $ROOT/m1",
		"nix build -f x.nix fail --no-link --store $ROOT/m0 -j0 --builders $ROOT/m1",
		"nix build -L -f x.nix fail --no-link --store $ROOT/m0 -j0 --builders $ROOT/m1",
		"nix log --store $ROOT/m0 -f x.nix fail",
		"nix-build x.nix -A fail --no-out-link --store $ROOT/m0 -j0 --builders $ROOT/m1 --keep-failed 2>&1 | sed -E 's|/nix-[0-9]+-[0-9]+|/nix-N|'",
		"nix-build x.nix -A wrong --no-out-link --store $ROOT/m0 -j0 --builders $ROOT/m1",
		"nix-build x.nix -A silent --no-out-link --store $ROOT/m0 -j0 --builders $ROOT/m1 --max-silent-time 2",
		"nix-build x.nix -A silent --no-out-link --store $ROOT/m0 -j0 --builders $ROOT/m1 --timeout 2",
	]);
}

#[test]
fn post_build_hooks_run_for_builds_on_machines() {
	same(&[
		"printf '#!/bin/sh\\necho ran $OUT_PATHS >> $ROOT/ran\\n' > hook; chmod +x hook; nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --builders $ROOT/m1 --post-build-hook $PWD/hook; cat ran",
		"printf '#!/bin/sh\\nexit 3\\n' > hook; nix-build x.nix -A outputs --no-out-link --store $ROOT/m0 -j0 --builders $ROOT/m1 --post-build-hook $PWD/hook",
	]);
}

#[test]
fn content_addressed_outputs_come_back_with_their_realisations() {
	same_with(
		"nix-command ca-derivations",
		&[
			"nix-build x.nix -A caTwo --no-out-link --store $ROOT/m0 -j0 --builders \"$ROOT/m1 - - 1 1 ca-derivations\" 2> /dev/null; cat $ROOT/m0/nix/store/*-ca-two",
			"nix-build x.nix -A caTwo --no-out-link --store $ROOT/m0 -j0 --builders \"$ROOT/m1 - - 1 1 ca-derivations\"",
			"nix path-info --store $ROOT/m0 --all | grep -v drv | sort; nix path-info --store $ROOT/m1 --all | sort",
			"nix-build x.nix -A caTwo --no-out-link --store $ROOT/n0 -j0 --builders \"$ROOT/m1\"",
			"nix-build x.nix -A caTwo --no-out-link --store $ROOT/o0 -j0 --builders \"ssh://host - - 1 1 ca-derivations\" 2> /dev/null; cat $ROOT/o0/nix/store/*-ca-two",
		],
	);
}

/// `ssh://`: the arguments of `ssh`, and building over the serve protocol.
#[test]
fn legacy_ssh_machines_build() {
	same(&[
		"nix-build x.nix -A two --no-out-link --store $ROOT/m0 -j0 --builders host 2> /dev/null; cat $ROOT/m0/nix/store/*-two; sort ssh.log",
		"nix path-info --store $ROOT/machine-host --all | sort; nix log --store $ROOT/m0 -f x.nix two",
		"rm ssh.log; NIX_SSHOPTS=\"-o 'Foo=a b' -v\" nix-build x.nix -A one --no-out-link --store $ROOT/n0 -j0 --builders 'ssh://user@host:2222?compress=true - '$ROOT'/key 1 1 - - c3NoLWVkMjU1MTkgQUFBQUMz' 2>&1 | grep -v '^copying path.*busybox'; sed -E \"s|$ROOT/tmp/[^|]*/host-key|HOSTKEY|\" ssh.log | sort",
		"nix-build x.nix -A one --no-out-link --store $ROOT/o0 -j0 --builders 'ssh://host?remote-program=nix-store%20-v'",
		"nix-build x.nix -A outputs --no-out-link --store $ROOT/m0 -j0 --builders ssh://host",
		"nix-build x.nix -A odd --no-out-link --store $ROOT/m0 -j0 --builders ssh://host 2> /dev/null; od -c $ROOT/m0/nix/store/*-odd; ls $ROOT/pwned",
		"nix-build x.nix -A phases --no-out-link --store $ROOT/m0 -j0 --builders ssh://host; nix log --store $ROOT/m0 -f x.nix phases",
		"nix-build x.nix -A fixed --no-out-link --store $ROOT/m0 -j0 --builders ssh://host",
	]);
}

#[test]
fn legacy_ssh_machines_fail() {
	same(&[
		"nix-build x.nix -A fail --no-out-link --store $ROOT/m0 -j0 --builders ssh://host",
		"nix build -f x.nix fail --no-link --store $ROOT/m0 -j0 --builders ssh://host",
		"nix-build x.nix -A wrong --no-out-link --store $ROOT/m0 -j0 --builders ssh://host",
		"nix-build x.nix -A silent --no-out-link --store $ROOT/m0 -j0 --builders ssh://host --max-silent-time 2",
		"nix-build x.nix -A silent --no-out-link --store $ROOT/m0 -j0 --builders ssh://host --timeout 2",
		// How much output comes before the limit depends on how it is read.
		"nix-build x.nix -A noisy --no-out-link --store $ROOT/l0 --option max-build-log-size 100 --builders '' 2>&1 | grep -v ^0123",
		"nix-build x.nix -A noisy --no-out-link --store $ROOT/m0 -j0 --builders $ROOT/m1 --option max-build-log-size 100 > out 2>&1; echo $?; tail -3 out",
		"nix-build x.nix -A noisy --no-out-link --store $ROOT/m0 -j0 --builders ssh://host --option max-build-log-size 100 > out 2>&1; echo $?; tail -3 out",
		"nix-build x.nix -A foo --no-out-link --store $ROOT/m0 -j0 --builders 'ssh://host - - 1 1 foo'",
		"touch ssh.broken; nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --builders ssh://host",
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j1 --builders ssh://host",
		"rm ssh.broken; touch ssh.motd; nix-build x.nix -A two --no-out-link --store $ROOT/m0 -j0 --builders ssh://host",
	]);
}

#[test]
fn ssh_ng_machines_build() {
	same(&[
		"nix-build x.nix -A two --no-out-link --store $ROOT/m0 -j0 --builders ssh-ng://host 2> /dev/null; cat $ROOT/m0/nix/store/*-two; sort ssh.log",
		"nix path-info --store $ROOT/machine-host --all | sort; nix log --store $ROOT/m0 -f x.nix two",
		"nix-build x.nix -A one --no-out-link --store $ROOT/n0 -j0 --builders ssh-ng://host",
		"nix-build x.nix -A odd --no-out-link --store $ROOT/m0 -j0 --builders ssh-ng://host 2> /dev/null; od -c $ROOT/m0/nix/store/*-odd; ls $ROOT/pwned",
		"nix-build x.nix -A fail --no-out-link --store $ROOT/m0 -j0 --builders ssh-ng://host",
		"nix-build x.nix -A wrong --no-out-link --store $ROOT/m0 -j0 --builders ssh-ng://host",
		"touch ssh.broken; nix-build x.nix -A fixed --no-out-link --store $ROOT/m0 -j0 --builders ssh-ng://host",
		"nix-build x.nix -A fixed --no-out-link --store $ROOT/m0 -j1 --builders ssh-ng://host",
	]);
}

/// A machine that substitutes needs fewer paths sent.
#[test]
fn machines_substitute_if_asked_to() {
	same(&[
		"nix-build x.nix -A one --no-out-link --store $ROOT/c0 2> /dev/null | xargs nix copy --to file://$ROOT/cache --from $ROOT/c0 2> /dev/null",
		"printf 'substituters = file://%s/cache\nrequire-sigs = false\n' $ROOT > machine.conf; nix-build x.nix -A two --no-out-link --store $ROOT/m0 --option substituters file://$ROOT/cache --option require-sigs false -j0 --builders ssh://host --builders-use-substitutes 2>&1 | grep -v '^downloading\\|^querying' | sort",
		"nix-build x.nix -A two --no-out-link --store $ROOT/n0 --option substituters file://$ROOT/cache --option require-sigs false -j0 --builders ssh-ng://host2 --builders-use-substitutes 2>&1 | grep '^copying'",
		"nix-build x.nix -A two --no-out-link --store $ROOT/o0 --option substituters file://$ROOT/cache --option require-sigs false -j0 --builders ssh://host3 2>&1 | grep '^copying'",
	]);
}

/// A daemon offers its clients' builds to its own hook.
#[test]
fn daemons_build_on_machines() {
	let client = |user: &str, build: &str| {
		format!(
			"echo \"trusted-users = {user}\" >> etc/nix.conf; nix-store --init --store $ROOT/m0; rm -f sock; NIX_DAEMON_SOCKET_PATH=$ROOT/sock nix-daemon --store local?root=$ROOT/m0 > daemon.log 2>&1 & p=$!; while [ ! -S sock ]; do sleep 0.1; done; sleep 0.2; {build}; kill $p"
		)
	};
	let steps = [
		client(
			"$(id -un)",
			"nix-build x.nix -A two --no-out-link --store \"unix://$ROOT/sock?root=$ROOT/m0\" -j0 --builders ssh://host 2>&1 | grep -v '^copying'",
		),
		"cat m0/nix/store/*-two; nix log --store $ROOT/m0 -f x.nix two".to_owned(),
		client(
			"$(id -un)",
			"nix-build x.nix -A fail --no-out-link --store \"unix://$ROOT/sock?root=$ROOT/m0\" -j0 --builders ssh://host",
		),
		// An untrusted client's machines are ignored.
		client(
			"nobody",
			"nix-build x.nix -A fixed --no-out-link --store \"unix://$ROOT/sock?root=$ROOT/m0\" -j0 --builders ssh://evil; ls -d machine-evil",
		),
	];
	let steps: Vec<&str> = steps.iter().map(String::as_str).collect();
	same(&steps);
}

/// A host or user `ssh` would read as an option never reaches it.
#[test]
fn option_like_hosts_are_refused() {
	same(&[
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --builders 'ssh://-oProxyCommand=touch%20pwned'; ls pwned ssh.log",
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --builders 'ssh://%2doProxyCommand=x'; ls ssh.log",
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --builders 'ssh-ng://-E@host'; ls ssh.log",
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --builders '-oProxyCommand=x'; ls ssh.log",
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --builders 'ssh://@host'; ls ssh.log",
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --builders 'ssh://host;touch pwned'; ls pwned",
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --builders 'nosuch://host'",
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --builders \"ssh://$(printf 'h%.0s' $(seq 300))\" > out 2>&1; echo $?; tail -1 out",
	]);
}

/// What comes back is checked against what the machine claims.
#[test]
fn tampered_outputs_are_refused() {
	for (builder, store) in [
		("$ROOT/m1", "m1"),
		("ssh://host", "machine-host"),
		("ssh-ng://host", "machine-host"),
	] {
		let build = format!(
			"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --builders {builder} 2>&1 | grep -vE '^(copying|building)'; nix path-info --store $ROOT/m0 $(echo $ROOT/{store}/nix/store/*-one | sed 's|.*/nix|/nix|')"
		);
		same(&[
			&format!("nix-build x.nix -A one --no-out-link --store $ROOT/{store} 2> /dev/null"),
			&format!(
				"f=$(echo $ROOT/{store}/nix/store/*-one); chmod +w $f; echo evil > $f; chmod -w $f"
			),
			&build,
			&format!(
				"f=$(echo $ROOT/{store}/nix/store/*-one); chmod +w $f; echo much more evil > $f; chmod -w $f"
			),
			&build,
			&format!("rm -f $ROOT/{store}/nix/store/*-one"),
			&format!(
				"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -j0 --builders {builder} > /dev/null 2>&1; echo $?; ls $ROOT/m0/nix/store | grep -c -- '-one$'"
			),
		]);
	}
}

/// A machine that does not trust its client builds only what it can verify.
#[test]
fn untrusting_machines_check_what_they_get() {
	let machine =
		"--builders 'ssh-ng://host?remote-program=nix-daemon%20--force-untrusted - - 1 1 foo,bar'";
	let step = |s: &str| s.replace("MACHINE", machine);
	let steps = [
		step(
			"echo 'extra-experimental-features = daemon-trust-override\nsystem-features = foo bar' > machine.conf; nix-build x.nix -A two --no-out-link --store $ROOT/m0 -j0 MACHINE 2>&1 | grep -v '^copying path'; cat $ROOT/m0/nix/store/*-two",
		),
		step("nix path-info --store $ROOT/machine-host --all | sort"),
		step(
			"nix-build x.nix -A fixed --no-out-link --store $ROOT/m0 -j0 MACHINE 2>&1 | grep -v '^copying path'",
		),
		step(
			"nix-build x.nix -A foo --no-out-link --store $ROOT/m0 --option system-features foo --builders '' 2> /dev/null",
		),
		step(
			"nix-build x.nix -A bar --no-out-link --store $ROOT/m0 -j0 MACHINE 2>&1 | grep -vE '^(copying|building)'",
		),
		step(
			"nix-build x.nix -A fail --no-out-link --store $ROOT/m0 -j0 MACHINE 2>&1 | grep -vE '^(copying|building)'",
		),
	];
	let steps: Vec<&str> = steps.iter().map(String::as_str).collect();
	same(&steps);
}

const RECORD: &str = r##"#!/bin/sh
echo "$@" > $ROOT/hook.args
pwd >> $ROOT/hook.args
cat <&0 > $ROOT/hook.in &
sleep 1
echo '@nix {"action":"msg","level":0,"msg":"a message"}' >&2
echo 'not json' >&2
echo "# decline" >&2
wait
"##;

/// The hook gets the settings and nothing else.
#[test]
fn the_hook_is_sent_the_settings() {
	let dump = r#"perl -e 'undef $/; my $d=<STDIN>; my $p=0; sub n { my $v=unpack("Q<",substr($d,$p,8)); $p+=8; $v } sub s_ { my $n=n(); my $v=substr($d,$p,$n); $p+=int(($n+7)/8)*8; $v } while (n()) { my $k=s_(); my $v=s_(); print "$k = $v\n" } print s_(), "|", n(), "|", s_(), "|", s_(), "|"; my $c=n(); print s_(), "," while $c--; print "|", length($d)-$p, "\n"' < hook.in"#;
	let hook = format!("cat > hook <<'X'\n{RECORD}X\nchmod +x hook");
	let dump = format!(
		// Where the executables and the shell for sandboxes are differs.
		"cat hook.args; {dump} | grep -vE '^(sandbox-paths|system-features) ='"
	);
	same(&[
		&hook,
		"nix-build x.nix -A foo --no-out-link --store $ROOT/m0 --build-hook \"$ROOT/hook a b\" --access-tokens 'x.org=secret' --cores 3 --keep-failed --max-silent-time 9 --option secret-key-files $ROOT/key 2>&1",
		&dump,
		"nix-build x.nix -A one --no-out-link --store \"local?root=$ROOT/m1\" -vv --build-hook $ROOT/hook -j3 2> /dev/null",
		&dump,
	]);
}

#[test]
fn custom_hooks_are_obeyed() {
	let hook = |body: &str| {
		format!(
			"printf '#!/bin/sh\\n{body}' > hook; chmod +x hook; nix-build x.nix -A one --no-out-link --store $ROOT/m0 --build-hook $ROOT/hook"
		)
	};
	let steps = [
		hook("echo \"# frobnicate\" >&2; sleep 9\\n"),
		hook("echo oops >&2\\nexit 1\\n"),
		hook("exec 0<&-\\nsleep 1\\nexit 1\\n"),
		hook(
			"echo \"# accept\" >&2; echo nowhere >&2; cat > /dev/null; echo a line >&4; echo stray >&2; exit 4\\n",
		),
		hook("echo \"# accept\" >&2; echo nowhere >&2; cat > /dev/null; exit 0\\n"),
		hook("echo \"# accept\" >&2\\n"),
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 --build-hook nix-no-such-hook"
			.to_owned(),
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 --build-hook $ROOT/nix-no-such-hook"
			.to_owned(),
		"nix-store --store $ROOT/m0 -l $ROOT/m0/nix/store/*-one.drv 2>&1 | sed 's|.*/nix|/nix|'"
			.to_owned(),
		hook("echo \"# decline-permanently\" >&2\\n"),
	];
	let steps: Vec<&str> = steps.iter().map(String::as_str).collect();
	same(&steps);
}

/// Each engine's worker can use the other's hook.
#[test]
fn hooks_are_interchangeable() {
	let (Some(reference), Some(_)) = (reference_dir(), busybox()) else {
		return;
	};
	let scratch = noxi_test_support::tempdir().unwrap();
	let root = scratch.path().join("s");
	for (worker, hook) in [
		(reference.as_path(), candidate_dir()),
		(candidate_dir(), reference.as_path()),
	] {
		for builder in ["$ROOT/m1", "ssh://host", "ssh-ng://host"] {
			let step = format!(
				"nix-build x.nix -A two --no-out-link --store $ROOT/m0 -j0 --builders {builder} --build-hook '{}/nix __build-remote' 2> log || cat log; cat $ROOT/m0/nix/store/*-two",
				hook.display()
			);
			let output = session(worker, &root, "nix-command", &[&step]).remove(0);
			assert!(output.contains("\nSome(0)\n"), "{output}");
			assert!(output.contains("one two\n"), "{output}");
		}
	}
	noxi_test_support::remove_tree(&root).unwrap();
}

/// Each engine's machines can be used by the other's hook.
#[test]
fn serve_and_daemon_machines_are_interchangeable() {
	let (Some(reference), Some(_)) = (reference_dir(), busybox()) else {
		return;
	};
	let scratch = noxi_test_support::tempdir().unwrap();
	let root = scratch.path().join("s");
	for (client, server) in [
		(reference.as_path(), candidate_dir()),
		(candidate_dir(), reference.as_path()),
	] {
		for (scheme, program) in [("ssh", "nix-store"), ("ssh-ng", "nix-daemon")] {
			for attr in ["two", "odd", "fixed", "caTwo"] {
				let step = format!(
					"nix-build x.nix -A {attr} --no-out-link --store $ROOT/m0 -j0 --builders '{scheme}://host?remote-program={}/{program} - - 1 1 ca-derivations' 2> log || cat log",
					server.display()
				);
				let output =
					session(client, &root, "nix-command ca-derivations", &[&step]).remove(0);
				assert!(output.contains("\nSome(0)\n"), "{attr}: {output}");
			}
			let step = format!(
				"nix-build x.nix -A fail --no-out-link --store $ROOT/m0 -j0 --builders '{scheme}://host?remote-program={}/{program}'",
				server.display()
			);
			let output = session(client, &root, "nix-command", &[&step]).remove(0);
			assert!(output.contains("failed with exit code 7"), "{output}");
		}
	}
	noxi_test_support::remove_tree(&root).unwrap();
}

/// Without machines, goals still build here in the reference's order.
#[test]
fn local_builds_start_in_the_reference_order() {
	same(&[
		"nix-build x.nix -A leaves --no-out-link --store $ROOT/m0 -j1 --builders \"$ROOT/m1 riscv64-linux\" 2>&1 | grep '^building'",
		"nix-build x.nix -A leaves --no-out-link --store $ROOT/n0 -j1 --builders '' 2>&1 | grep '^building'",
	]);
}

/// With no machine, the default hook is started and declines.
#[test]
fn the_default_hook_declines_without_machines() {
	same(&[
		"nix-build x.nix -A one --no-out-link --store $ROOT/m0 -vvvvv 2>&1 | grep -E \"^(starting build hook|cannot find machines file|got [0-9]+ remote builders|hook reply is)\" | sed \"s|'[^ ]*/nix __build-remote'|NIX|\"",
		"echo > etc/machines; nix-build x.nix -A two --no-out-link --store $ROOT/n0 -vvvvv 2>&1 | grep -cE '^(starting build hook|got 0 remote builders)'",
	]);
}

/// `nix-store --serve` and `nix-daemon --stdio` of both engines answer the
/// reference's client alike.
#[test]
fn servers_answer_the_reference_client() {
	let serve = "ssh://host?remote-program=$BIN/nix-store";
	let daemon = "ssh-ng://host?remote-program=$BIN/nix-daemon";
	let mut steps = vec![
		"nix-build x.nix -A two --no-out-link --store $ROOT/m0 2> /dev/null; nix-instantiate x.nix -A outputs --store $ROOT/m0 > drv 2> /dev/null".to_owned(),
		"printf '#!/bin/sh\\nexec %s/nix-store --serve\\n' \"$BIN\" > bin/ro; chmod +x bin/ro".to_owned(),
	];
	for store in [serve, daemon] {
		let q = |s: &str| s.replace("STORE", store);
		steps.extend([
			q("$REF/nix copy --store $ROOT/m0 --to \"STORE\" $(echo $ROOT/m0/nix/store/*-two | sed 's|.*/nix|/nix|')"),
			q("$REF/nix path-info --store \"STORE\" --json $(echo $ROOT/m0/nix/store/*-two | sed 's|.*/nix|/nix|') | sed 's|\"registrationTime\":[0-9]*|T|g'"),
			q("$REF/nix-store --store \"STORE\" -qR $(echo $ROOT/m0/nix/store/*-two | sed 's|.*/nix|/nix|')"),
			q("$REF/nix copy --store $ROOT/m0 --to \"STORE\" $(cat drv); $REF/nix-store --store \"STORE\" -qR --include-outputs $(cat drv)"),
			q("$REF/nix-store --store \"STORE\" -r $(cat drv); $REF/nix-store --store \"STORE\" -qR --include-outputs $(cat drv)"),
			q("$REF/nix copy --from \"STORE\" --to $ROOT/c1 $(echo $ROOT/m0/nix/store/*-two | sed 's|.*/nix|/nix|'); ls $ROOT/c1/nix/store | sort"),
			q("$REF/nix store cat --store \"STORE\" /nix/store/00000000000000000000000000000000-missing/x"),
			"rm -rf machine-host c1".to_owned(),
		]);
	}
	steps.extend([
		"$REF/nix copy --store $ROOT/m0 --to 'ssh://host?remote-program=ro' $(echo $ROOT/m0/nix/store/*-two | sed 's|.*/nix|/nix|')".to_owned(),
		"$REF/nix-store --store \"$serve\" -r /nix/store/00000000000000000000000000000000-missing.drv".replace("$serve", serve),
		// The reference's transport is a pair of pipes, noxi's a socket, so
		// the client's error about the refused upload differs; see
		// docs/verification.md.
		"nix-build x.nix -A one --no-out-link --store $ROOT/n0 -j0 --builders 'ssh://host?remote-program=ro' 2>&1 | grep -m1 '^error'".to_owned(),
	]);
	let steps: Vec<&str> = steps.iter().map(String::as_str).collect();
	same(&steps);
}

/// Interrupting a build leaves nothing running.
#[test]
fn interrupted_remote_builds_stop() {
	// What is left is reported and then killed, so that a failure cannot leave
	// a builder running.
	let left = "for e in /proc/[0-9]*/environ; do p=${e#/proc/}; p=${p%/environ}; [ $p = $$ ] && continue; grep -qz \"^ROOT=$ROOT\\$\" $e 2> /dev/null && { cat /proc/$p/comm 2> /dev/null; kill -9 $p 2> /dev/null; }; done | sort";
	let steps = [
		format!(
			"nix-build x.nix -A silent --no-out-link --store $ROOT/m0 -j0 --builders ssh://host > out 2>&1 & p=$!; sleep 3; kill -INT $p; timeout 30 tail --pid=$p -f /dev/null; kill -9 $p 2> /dev/null; wait $p; echo $?; sleep 2; {left}"
		),
		format!(
			"nix-build x.nix -A silent --no-out-link --store $ROOT/m0 -j0 --builders ssh-ng://host2 > out 2>&1 & p=$!; sleep 3; kill -INT $p; timeout 30 tail --pid=$p -f /dev/null; kill -9 $p 2> /dev/null; wait $p; echo $?; sleep 2; {left}"
		),
		format!(
			"nix-build x.nix -A silent --no-out-link --store $ROOT/m0 -j0 --builders $ROOT/m1 > out 2>&1 & p=$!; sleep 3; kill -INT $p; timeout 30 tail --pid=$p -f /dev/null; kill -9 $p 2> /dev/null; wait $p; echo $?; sleep 2; {left}"
		),
	];
	let steps: Vec<&str> = steps.iter().map(String::as_str).collect();
	same(&steps);
}
