//! Rebuilding, checking and repairing derivations compared with CppNix 2.35.2,
//! as the upstream functional tests `build-delete.sh`, `check.sh`, `repair.sh`
//! and `post-hook.sh` use them.
//!
//! Both engines use the same store directory one after the other, removed in
//! between. The reference comes from `NOXI_REFERENCE_BIN`, else
//! `target/nix-reference/bin`; without it the tests print a notice and return.
use std::fs;
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

fn candidate_dir() -> &'static Path {
	Path::new(env!("CARGO_BIN_EXE_nix")).parent().unwrap()
}

/// `state` is a file in the test's root that builders read, so a rebuild can
/// produce something else.
const EXPR: &str = r#"
let
	mk = attrs: derivation ({
		system = "x86_64-linux";
		builder = "/bin/sh";
		PATH = "/run/current-system/sw/bin:/usr/bin:/bin";
	} // attrs);
	ca = { __contentAddressed = true; outputHashMode = "recursive"; outputHashAlgo = "sha256"; };
	script = text: [ "-c" text ];
	pair = script "mkdir $first $second; echo first > $first/file; ln -s $first $second/link; echo self $second > $second/file";
in rec {
	two = mk { name = "two"; outputs = [ "first" "second" ]; args = pair; };
	twoCa = mk ({ name = "two-ca"; outputs = [ "first" "second" ]; args = pair; } // ca);
	use = mk { name = "use"; args = script "cat ${two.first}/file ${two.second}/file > $out"; };
	stable = mk { name = "stable"; args = script "echo $out > $out"; };
	stableCa = mk ({ name = "stable-ca"; args = script "echo $out > $out"; } // ca);
	fixed = mk { name = "fixed"; args = script "cat ${builtins.getEnv "ROOT"}/state > $out"; outputHash = builtins.hashString "sha256" "one\n"; outputHashAlgo = "sha256"; outputHashMode = "flat"; };
	stateful = mk { name = "stateful"; args = script "cat ${builtins.getEnv "ROOT"}/state > $out"; };
	statefulCa = mk ({ name = "stateful-ca"; args = script "cat ${builtins.getEnv "ROOT"}/state > $out"; } // ca);
	failing = mk { name = "failing"; args = script "echo kept > leftover; exit 1"; };
	parent = mk { name = "parent"; args = script "mkdir $out; ln -s ${stable} $out/stable"; };
}
"#;

/// Run the shell commands `steps` with one engine in a fresh diverted store,
/// returning each step's exit status, standard output and error lines.
fn session(bin: &Path, root: &Path, features: &str, steps: &[&str]) -> Vec<String> {
	noxi_test_support::remove_tree(root).unwrap();
	fs::create_dir_all(root.join("etc")).unwrap();
	fs::write(
		root.join("etc/nix.conf"),
		format!(
			"experimental-features = {features}\nsandbox = false\nbuild-users-group =\nsubstituters =\n"
		),
	)
	.unwrap();
	fs::write(root.join("x.nix"), EXPR).unwrap();
	fs::write(root.join("state"), "one\n").unwrap();
	steps
		.iter()
		.map(|step| {
			let output = Command::new("bash")
				.args(["-c", step])
				.current_dir(root)
				.env_clear()
				.env("HOME", root.join("home"))
				.env("ROOT", root)
				.env(
					"PATH",
					format!("{}:/run/current-system/sw/bin:/usr/bin:/bin", bin.display()),
				)
				.env("NIX_STORE_DIR", root.join("store"))
				.env("NIX_STATE_DIR", root.join("var"))
				.env("NIX_LOG_DIR", root.join("log"))
				.env("NIX_CONF_DIR", root.join("etc"))
				.env("NIX_REMOTE", "")
				.output()
				.unwrap();
			let errors: String = String::from_utf8_lossy(&output.stderr)
				.lines()
				.filter(|line| {
					!line.starts_with("warning: you don't have Internet access")
						&& !line.contains("keeping build directory")
				})
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

fn same(steps: &[&str]) {
	same_with("nix-command ca-derivations", steps)
}

fn same_with(features: &str, steps: &[&str]) {
	let Some(reference) = reference_dir() else {
		return;
	};
	let dir = noxi_test_support::tempdir().unwrap();
	let root = dir.path().join("engine");
	let expected = session(&reference, &root, features, steps);
	let actual = session(candidate_dir(), &root, features, steps);
	let different: Vec<String> = expected
		.iter()
		.zip(&actual)
		.filter(|(expected, actual)| expected != actual)
		.map(|(expected, actual)| format!("--- reference\n{expected}--- noxi\n{actual}"))
		.collect();
	assert!(different.is_empty(), "{}", different.join("\n"));
}

#[test]
fn rebuilding_some_outputs_matches_the_reference() {
	same(&[
		"nix-build x.nix -A use --no-out-link",
		"nix-store --delete $(nix-build x.nix -A use --no-out-link) > /dev/null 2>&1; nix-store --delete store/*-two-second > /dev/null 2>&1; ls store",
		"nix-build x.nix -A use --no-out-link; cat store/*-use store/*-two-second/link/file; nix-store -q --hash store/*-two-second",
		"nix-store -q --references store/*-two-second; ls store",
		"nix-build x.nix -A two.first -A two.second --no-out-link 2>/dev/null; nix-store --verify --check-contents",
		"nix-build x.nix -A twoCa.first -A twoCa.second --no-out-link",
		"nix-store --delete store/*.drv > /dev/null 2>&1; nix-store --delete store/*-two-ca-second > /dev/null 2>&1; nix-build x.nix -A twoCa.second --no-out-link; cat store/*-two-ca-second/link/file; ls store",
		"nix-store --verify --check-contents",
	]);
}

#[test]
fn checking_builds_match_the_reference() {
	same(&[
		"nix-build x.nix -A stable --no-out-link --check",
		"for a in stable stableCa stateful statefulCa fixed two.first; do nix-build x.nix -A $a --no-out-link; done",
		"nix-build x.nix -A stable --no-out-link --check",
		"nix-build x.nix -A stableCa --no-out-link --check",
		"nix-build x.nix -A two.first --no-out-link --check; ls store | grep two",
		"nix-build x.nix -A fixed -A stateful --no-out-link --check",
		"nix-store -r --check $(nix-instantiate x.nix -A stable)",
		"echo two > state",
		"nix-build x.nix -A stateful --no-out-link --check; ls store | grep check",
		"nix-build x.nix -A stateful --no-out-link --check --keep-failed; cat store/*-stateful.check; ls store | grep check",
		"nix-build x.nix -A statefulCa --no-out-link --check",
		"nix-build x.nix -A fixed --no-out-link --check",
		"nix-build x.nix -A stateful -A fixed --no-out-link --check --keep-going 2> /dev/null",
		"cat $(nix-store -q $(nix-instantiate x.nix -A stateful -A fixed 2> /dev/null)); nix-store --verify --check-contents",
		"nix-store --delete store/*-two-second > /dev/null 2>&1; nix-build x.nix -A two.first --no-out-link --check; ls store | grep two",
		"nix-build x.nix -A two.second --no-out-link --check",
	]);
}

/// The reference's `diff-hook` fails as an ordinary user because it calls
/// `setgroups`, so only noxi's run of it is checked.
#[test]
fn nondeterministic_outputs_are_shown_to_the_diff_hook() {
	let dir = noxi_test_support::tempdir().unwrap();
	let steps = session(
		candidate_dir(),
		&dir.path().join("engine"),
		"nix-command",
		&[
			"nix-build x.nix -A stateful --no-out-link; echo two > state",
			"printf '#!/bin/sh\\necho hook ${1##*-} ${2##*-} ${3##*-} $#\\n' > hook; chmod +x hook; nix-build x.nix -A stateful --no-out-link --check --diff-hook $PWD/hook",
			"nix-build x.nix -A stateful --no-out-link --check --run-diff-hook --diff-hook $PWD/hook",
			"printf '#!/bin/sh\\nexit 3\\n' > hook; nix-build x.nix -A stateful --no-out-link --check --run-diff-hook --diff-hook $PWD/hook",
		],
	);
	assert!(!steps[1].contains("hook stateful"), "{}", steps[1]);
	assert!(
		steps[2].contains("\nhook stateful stateful.check stateful.drv 4\n"),
		"{}",
		steps[2]
	);
	assert!(
		steps[3].contains("error: diff hook execution failed: diff-hook program")
			&& steps[3].contains("failed with exit code 3"),
		"{}",
		steps[3]
	);
	for step in &steps[1..] {
		assert!(step.contains("may not be deterministic"), "{step}");
		assert!(step.contains("\nSome(104)\n"), "{step}");
	}
}

#[test]
fn failed_build_directories_are_kept_like_the_reference() {
	same(&[
		"mkdir builds; nix-build x.nix -A failing --no-out-link --option build-dir $PWD/builds; ls builds",
		"nix-build x.nix -A failing --no-out-link --keep-failed --option build-dir $PWD/builds 2> log; echo $?; cd builds/*; stat -c %a . build 2> /dev/null; find . -name leftover -exec cat {} +; grep -c 'note: keeping build directory \"' ../../log",
	]);
}

/// Without `ca-derivations`: with it, 2.35.2 takes a registered realisation
/// as proof that the output is good and repairs nothing, and noxi repairs.
#[test]
fn repairs_match_the_reference() {
	same_with(
		"nix-command",
		&[
			"nix-build x.nix -A parent --no-out-link",
			"chmod u+w store/*-stable; echo bad > store/*-stable; nix-store --verify --check-contents",
			"nix-store --verify --check-contents --repair; cat store/*-stable",
			"chmod u+w store/*-stable; echo bad > store/*-stable; nix-store --repair-path store/*-stable; cat store/*-stable; stat -c %a store/*-stable",
			"chmod u+w store/*-stable; echo bad > store/*-stable; nix-build x.nix -A stable --no-out-link --repair; cat store/*-stable",
			"chmod u+w store/*-stable; echo bad > store/*-stable; nix-build x.nix -A parent --no-out-link --repair; cat store/*-stable",
			"chmod u+w store/*-stable; echo bad > store/*-stable; nix-store -r --repair $(nix-instantiate x.nix -A stable) > /dev/null; cat store/*-stable; ls store",
			"chmod u+w store/*-stable; rm store/*-stable; nix-store --verify --repair; cat store/*-stable",
			"nix-store --delete $(nix-store -q --referrers-closure $(nix-store -qd store/*-stable)) > /dev/null 2>&1; chmod u+w store/*-stable; echo bad > store/*-stable; nix-store --verify --check-contents --repair; nix-store --repair-path store/*-stable; cat store/*-stable",
		],
	);
}

#[test]
fn post_build_hooks_match_the_reference() {
	same(&[
		"printf '#!/bin/sh\\necho ran hook; echo $DRV_PATH $OUT_PATHS >> $ROOT/ran\\n' > hook; chmod +x hook; nix-build x.nix -A two.first --no-out-link --post-build-hook $PWD/hook; cat ran",
		"nix-build x.nix -A two.first --no-out-link --check --post-build-hook $PWD/hook; cat ran",
		"nix-build x.nix -A two.first --no-out-link --post-build-hook $PWD/hook; cat ran",
		"printf '#!/bin/sh\\nexit 4\\n' > hook; nix-build x.nix -A stable --no-out-link --post-build-hook $PWD/hook; nix-store -q store/*-stable",
		"nix-build x.nix -A stateful --no-out-link --post-build-hook $PWD/missing > /dev/null 2>&1; echo $?",
	]);
}
