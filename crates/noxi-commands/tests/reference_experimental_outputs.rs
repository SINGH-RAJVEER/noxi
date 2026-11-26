//! Building text-hashed, Git-hashed and impure outputs compared with CppNix
//! 2.35.2, with the failures their checks must report.
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

const FEATURES: &str =
	"nix-command ca-derivations impure-derivations dynamic-derivations git-hashing";

const EXPR: &str = r#"
let
	mk = attrs: derivation ({
		system = "x86_64-linux";
		builder = "/bin/sh";
		PATH = "/run/current-system/sw/bin:/usr/bin:/bin";
	} // attrs);
	script = text: [ "-c" text ];
	ca = mode: { __contentAddressed = true; outputHashMode = mode; outputHashAlgo = "sha256"; };
	git = algo: ca "git" // { outputHashAlgo = algo; };
	counter = "n=$(cat $ROOT/counter); echo $((n + 1)) > $ROOT/counter; ";
	tree = "mkdir -p $out/dir $out/a; echo hi > $out/dir/file; echo x > $out/a.b; echo y > $out/a/b; chmod +x $out/a/b; ln -s dir/file $out/link";
	fixed = mode: hash: { outputHashMode = mode; outputHashAlgo = "sha256"; outputHash = hash; };
in rec {
	leaf = mk { name = "leaf"; args = script "echo leaf > $out"; };
	text = mk ({ name = "text"; args = script "echo text > $out"; } // ca "text");
	textRef = mk ({ name = "text-ref"; args = script "echo ${leaf} ${text} > $out"; } // ca "text");
	textExec = mk ({ name = "text-exec"; args = script "echo text > $out; chmod +x $out"; } // ca "text");
	textDir = mk ({ name = "text-dir"; args = script "mkdir $out"; } // ca "text");
	textLink = mk ({ name = "text-link"; args = script "ln -s /x $out"; } // ca "text");
	textFixed = mk ({ name = "text-fixed"; args = script "echo text > $out"; } // fixed "text" (builtins.hashString "sha256" "text\n"));
	textBad = mk ({ name = "text-bad"; args = script "echo other > $out"; } // fixed "text" (builtins.hashString "sha256" "text\n"));
	useText = mk { name = "use-text"; args = script "cat ${text} ${textFixed} > $out"; };
	flatRef = mk ({ name = "flat-ref"; args = script "echo ${leaf} > $out"; } // ca "flat");
	gitFile = mk ({ name = "git-file"; args = script "echo hi > $out"; } // git "sha1");
	gitTree = mk ({ name = "git-tree"; args = script tree; } // git "sha1");
	gitTree256 = mk ({ name = "git-tree-256"; args = script tree; } // git "sha256");
	gitRef = mk ({ name = "git-ref"; args = script "echo ${leaf} > $out"; } // git "sha1");
	gitMd5 = mk ({ name = "git-md5"; args = script "echo hi > $out"; } // git "md5");
	gitFixed = mk { name = "git-fixed"; args = script "echo hi > $out"; outputHashMode = "git"; outputHashAlgo = "sha1"; outputHash = "45b983be36b73c0788dc9cbcb76cbb80fc7bb057"; };
	gitBad = mk { name = "git-bad"; args = script "echo other > $out"; outputHashMode = "git"; outputHashAlgo = "sha1"; outputHash = "45b983be36b73c0788dc9cbcb76cbb80fc7bb057"; };
	useGit = mk { name = "use-git"; args = script "cat ${gitTree}/link ${gitFixed} > $out"; };
	impure = mk { name = "impure"; outputs = [ "out" "stuff" ]; args = script "${counter}mkdir $out $stuff; echo $n > $out/n; ln -s $out/n $stuff/n; ln -s $out $out/self"; __impure = true; impureEnvVars = [ "ROOT" ]; };
	impureOnImpure = mk { name = "impure-on-impure"; args = script "cat ${impure}/n ${impure.stuff}/n > $out"; __impure = true; };
	impureFlat = mk { name = "impure-flat"; args = script "mkdir $out"; __impure = true; outputHashMode = "flat"; };
	impureFails = mk { name = "impure-fails"; args = script "exit 7"; __impure = true; };
	pureOnImpure = mk { name = "pure-on-impure"; args = script "cat ${impure}/n > $out"; };
	caOnImpure = mk ({ name = "ca-on-impure"; args = script "cat ${impure}/n > $out"; } // ca "recursive");
	fixedOnImpure = mk ({ name = "fixed-on-impure"; args = script "cat ${impureOnImpure} > /dev/null; echo fixed > $out"; } // fixed "flat" (builtins.hashString "sha256" "fixed\n"));
	pureOnFixed = mk { name = "pure-on-fixed"; args = script "cat ${fixedOnImpure} > $out"; };
	deepPure = mk { name = "deep-pure"; args = script "cat ${pureOnImpure} > $out"; };
	environment = kind: mk ({ name = "environment"; args = script "echo $ROOT checked=$NIX_OUTPUT_CHECKED > $out"; impureEnvVars = [ "ROOT" ]; } // kind);
	inner = mk { name = "inner"; outputs = [ "out" "dev" ]; args = script "echo inner > $out; echo dev > $dev"; };
	produce = drv: name: mk ({ name = name; args = script "cp ${builtins.unsafeDiscardOutputDependency drv.drvPath} $out"; } // ca "text");
	producer = produce inner "inner.drv";
	outer = produce producer "inner.drv.drv";
	dyn = builtins.outputOf producer.outPath;
	wrapper = mk { name = "wrapper"; args = script "cat ${dyn "out"} ${dyn "dev"} ${text} > $out"; };
	both = mk { name = "both"; args = script "cat ${builtins.outputOf (builtins.outputOf outer.outPath "out") "out"} ${wrapper} > $out; echo ${producer} >> $out"; };
	dynFixed = mk ({ name = "dyn-fixed"; args = script "cat ${dyn "out"} > $out"; } // fixed "flat" (builtins.hashString "sha256" "inner\n"));
	dynBad = mk ({ name = "dyn-bad"; args = script "cat ${dyn "dev"} > $out"; } // fixed "flat" (builtins.hashString "sha256" "inner\n"));
	dynCa = mk ({ name = "dyn-ca"; args = script "cat ${dyn "out"} > $out"; } // ca "flat");
	dynMissing = mk { name = "dyn-missing"; args = script "cat ${dyn "nope"} > $out"; };
	dynFails = mk { name = "dyn-fails"; args = script "cat ${dyn "out"}; exit 3"; };
	badProducer = mk ({ name = "bad.drv"; args = script "echo not a derivation > $out"; } // ca "text");
	failingProducer = mk ({ name = "failing.drv"; args = script "exit 4"; } // ca "text");
	dynOfBad = mk { name = "dyn-of-bad"; args = script "cat ${builtins.outputOf badProducer.outPath "out"} > $out"; };
	dynOfFailing = mk { name = "dyn-of-failing"; args = script "cat ${builtins.outputOf failingProducer.outPath "out"} > $out"; };
	fetch = attrs: derivation ({ name = "fetched"; system = "builtin"; builder = "builtin:fetchurl"; outputHashMode = "flat"; outputHashAlgo = "sha256"; outputHash = builtins.hashString "sha256" "one\n"; } // attrs);
	fetched = fetch { url = "file://${builtins.getEnv "ROOT"}/data"; };
	fetchedExec = fetch { name = "fetched-exec"; url = "file://${builtins.getEnv "ROOT"}/data"; executable = "1"; outputHashMode = "recursive"; outputHash = "sha256-pzqGE5pKoUaYGBhCNTBYnBD+NvE0w0zyXqBZuUvUFoI="; };
	fetchedBad = fetch { name = "fetched-bad"; url = "file://${builtins.getEnv "ROOT"}/x.nix"; };
	fetchedPure = derivation { name = "fetched-pure"; system = "builtin"; builder = "builtin:fetchurl"; url = "file://${builtins.getEnv "ROOT"}/data"; };
	fetchedImpure = derivation { name = "fetched-impure"; system = "builtin"; builder = "builtin:fetchurl"; url = "file://${builtins.getEnv "ROOT"}/data"; __impure = true; outputHashMode = "flat"; };
	fetchedMissing = fetch { name = "fetched-missing"; url = "file:///does/not/exist"; };
	fetchedScheme = fetch { name = "fetched-scheme"; url = "gopher://localhost/x"; };
	caFails = mk ({ name = "ca-fails"; args = script "cat ${text}; exit 5"; } // ca "flat");
	impureEnv = environment { __impure = true; };
	pureEnv = environment {};
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
	fs::write(root.join("data"), "one\n").unwrap();
	fs::write(root.join("counter"), "0\n").unwrap();
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

fn same(features: &str, steps: &[&str]) -> Vec<String> {
	let Some(reference) = reference_dir() else {
		return Vec::new();
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
	actual
}

#[test]
fn text_outputs_match_the_reference() {
	let steps = same(
		FEATURES,
		&[
			"nix-build x.nix -A text --no-out-link",
			"nix build -f x.nix textRef --no-link --json; nix-store -q --references $(nix-build x.nix -A textRef --no-out-link)",
			"nix path-info --json --json-format 2 $(nix-build x.nix -A textRef --no-out-link 2> /dev/null) | grep -o '\"ca\":{[^}]*}'",
			"nix-build x.nix -A useText --no-out-link; cat store/*-use-text",
			"nix-build x.nix -A textFixed --no-out-link",
			"nix-build x.nix -A textBad --no-out-link",
			"nix-build x.nix -A textExec --no-out-link",
			"nix-build x.nix -A textDir --no-out-link",
			"nix-build x.nix -A textLink --no-out-link",
			"nix-build x.nix -A flatRef --no-out-link",
			"nix-build x.nix -A text -A textFixed --no-out-link --check",
			"nix-store --verify --check-contents; ls store | grep -v lock",
		],
	);
	// The checks, whatever the reference does.
	for (step, message) in [
		(5, "hash mismatch in fixed-output derivation"),
		(6, "should be a non-executable regular file"),
		(7, "should be a non-executable regular file"),
		(8, "should be a non-executable regular file"),
		(9, "is not allowed to refer to other store paths"),
	] {
		if let Some(output) = steps.get(step) {
			assert!(output.contains(message), "{output}");
		}
	}
}

#[test]
fn git_outputs_match_the_reference() {
	let steps = same(
		FEATURES,
		&[
			"for a in gitFile gitTree gitTree256; do nix-build x.nix -A $a --no-out-link; done",
			"nix path-info --json --json-format 2 store/*-git-tree store/*-git-tree-256 | grep -o '\"ca\":{[^}]*}'",
			"nix-build x.nix -A gitFixed --no-out-link",
			"nix-build x.nix -A gitBad --no-out-link",
			"nix-build x.nix -A gitRef --no-out-link",
			"nix-build x.nix -A gitMd5 --no-out-link",
			"nix-build x.nix -A useGit --no-out-link; cat store/*-use-git",
			"nix-build x.nix -A gitTree --no-out-link --check",
			"nix store verify --no-trust store/*-git-tree",
			"nix store add --mode git --hash-algo sha1 --dry-run store/*-git-tree -n git-tree; nix store add --mode git --hash-algo sha1 store/*-git-tree -n git-tree",
			"nix store add --mode git --hash-algo md5 x.nix",
			"nix store add --mode git --hash-algo sha1 x.nix --option experimental-features nix-command; nix hash path --mode git x.nix --option experimental-features nix-command",
			"nix store add --mode text x.nix; nix store add --mode text --dry-run x.nix; nix store add --mode other x.nix",
			"nix-store --verify --check-contents; ls store | grep -v lock",
		],
	);
	for (step, message) in [
		(3, "hash mismatch in fixed-output derivation"),
		(4, "is not allowed to refer to other store paths"),
		(5, "Git file ingestion must use SHA-1 or SHA-256 hash"),
		(11, "experimental Nix feature 'git-hashing' is disabled"),
	] {
		if let Some(output) = steps.get(step) {
			assert!(output.contains(message), "{output}");
		}
	}
}

#[test]
fn impure_outputs_match_the_reference() {
	let steps = same(
		FEATURES,
		&[
			"nix build -f x.nix impure --no-link --json --dry-run",
			"nix build -f x.nix impure --no-link --json; cat counter",
			"nix build -f x.nix impure.all --no-link --print-out-paths; cat counter",
			"nix build -f x.nix impure impureOnImpure --no-link --json; cat counter",
			"nix build -f x.nix impureOnImpure -o link; cat link counter",
			"nix build -f x.nix impure impureOnImpure --no-link --json --max-jobs 3 2> /dev/null; cat counter",
			"nix build -f x.nix impure --no-link --rebuild; cat counter",
			"nix build -f x.nix impureFlat --no-link 2>&1 | sed s/tmp-[0-9-]*/tmp/",
			"nix build -f x.nix impureFails --no-link",
			"nix build -f x.nix pureOnImpure --no-link; cat counter",
			"nix build -f x.nix caOnImpure --no-link; cat counter",
			"nix build -f x.nix deepPure --no-link; cat counter",
			"nix build -f x.nix deepPure --no-link --max-jobs 3 2>&1 | grep 'depends on'; cat counter",
			"nix build -f x.nix fixedOnImpure --no-link --print-out-paths 2> /dev/null; cat counter",
			"nix build -f x.nix fixedOnImpure pureOnFixed --no-link --print-out-paths; cat counter",
			"nix build -f x.nix impureEnv pureEnv --no-link --print-out-paths 2> /dev/null | xargs cat",
			"nix build -f x.nix impure --no-link --option experimental-features 'nix-command ca-derivations'",
			"nix build -f x.nix impure --no-link --option experimental-features 'nix-command impure-derivations'; cat counter",
			"nix build $(nix-instantiate x.nix -A impure 2> /dev/null)^out --no-link --option experimental-features 'nix-command ca-derivations'; cat counter",
			"nix-store --verify --check-contents",
		],
	);
	for (step, message) in [
		(7, "should be a non-executable regular file"),
		(9, "depends on impure derivation"),
		(10, "depends on impure derivation"),
		(11, "depends on impure derivation"),
		(12, "depends on impure derivation"),
		(
			16,
			"experimental Nix feature 'impure-derivations' is disabled",
		),
		(17, "experimental Nix feature 'ca-derivations' is disabled"),
		(
			18,
			"experimental Nix feature 'impure-derivations' is disabled",
		),
	] {
		if let Some(output) = steps.get(step) {
			assert!(output.contains(message), "{output}");
		}
	}
}

#[test]
fn dynamic_derivations_match_the_reference() {
	let steps = same(
		FEATURES,
		&[
			"for a in wrapper both dynFixed dynCa; do cat $(nix-instantiate x.nix -A $a 2> /dev/null); echo; done",
			"for a in wrapper both dynFixed dynCa; do nix derivation show $(nix-instantiate x.nix -A $a 2> /dev/null); done 2> /dev/null",
			"nix-build x.nix -A wrapper --no-out-link",
			"cat store/*-wrapper; nix-store -q --references store/*-inner",
			"nix-build x.nix -A wrapper --no-out-link",
			"nix-build x.nix -A both --no-out-link 2> /dev/null; cat store/*-both",
			"nix-build x.nix -A dynFixed --no-out-link",
			"nix-build x.nix -A dynBad --no-out-link",
			"nix-build x.nix -A dynCa --no-out-link --max-jobs 2 2> /dev/null",
			"nix-build x.nix -A dynMissing --no-out-link 2>&1 | grep -v 'dynamic derivation'",
			"nix-build x.nix -A dynFails --no-out-link 2> /dev/null; echo $?",
			"nix-build x.nix -A dynOfBad --no-out-link 2>&1 | grep -c 'error parsing derivation'",
			"nix-build x.nix -A dynOfFailing --no-out-link 2>&1",
			"nix build $(nix-instantiate x.nix -A producer 2> /dev/null)^out^out --no-link --print-out-paths 2> /dev/null",
			"nix build $(nix-instantiate x.nix -A outer 2> /dev/null)^out^out^dev,out --no-link --print-out-paths 2> /dev/null",
			"nix build $(nix-instantiate x.nix -A failingProducer 2> /dev/null)^out^out --no-link 2>&1 | tail -1",
			"nix build --impure --expr '(import ./x.nix).dyn \"dev\"' --no-link --print-out-paths 2> /dev/null",
			"nix build $(nix-instantiate x.nix -A producer 2> /dev/null)^out^out --no-link --option experimental-features 'nix-command ca-derivations'",
			"nix-build $(nix-instantiate x.nix -A wrapper 2> /dev/null) --no-out-link --option experimental-features 'nix-command ca-derivations'",
			"nix-build x.nix -A caFails --no-out-link",
			"nix-store --verify --check-contents; ls store | grep -v lock",
		],
	);
	for (step, message) in [
		(7, "hash mismatch in fixed-output derivation"),
		(10, "100"),
		(
			18,
			"experimental Nix feature 'dynamic-derivations' is disabled",
		),
		(19, "build of resolved derivation"),
	] {
		if let Some(output) = steps.get(step) {
			assert!(output.contains(message), "{output}");
		}
	}
}

/// `builtin:fetchurl` from files.
#[test]
fn fetchurl_matches_the_reference() {
	let steps = same(
		FEATURES,
		&[
			"echo one > data; nix-build x.nix -A fetched --no-out-link; cat store/*-fetched; stat -c %a store/*-fetched",
			"nix-build x.nix -A fetchedExec --no-out-link 2> /dev/null; ls store | grep fetched-exec | grep -v drv; stat -c %a store/*-fetched-exec",
			"nix-build x.nix -A fetchedBad --no-out-link",
			"nix-build x.nix -A fetchedPure --no-out-link 2>&1 | grep -o \"'builtin:fetchurl' must.*\"",
			"nix-build x.nix -A fetchedMissing --no-out-link > /dev/null 2>&1; echo $?",
			"nix-build x.nix -A fetchedScheme --no-out-link --option filetransfer-retry-attempts 1 > /dev/null 2>&1; echo $?",
			"nix build -f x.nix fetchedImpure --no-link --print-out-paths; echo two > data; nix build -f x.nix fetchedImpure --no-link --print-out-paths",
			"echo one > data; nix-build x.nix -A fetched --no-out-link --check; echo two > data; nix-build x.nix -A fetched --no-out-link --check",
			"mkdir -p mirror/sha256; echo one > mirror/sha256/2c8b08da5ce60398e1f19af0e5dccc744df274b826abe585eaba68c525434806; nix-store --delete store/*-fetched > /dev/null 2>&1; nix-build x.nix -A fetched --no-out-link --option hashed-mirrors file://$PWD/mirror; cat store/*-fetched",
		],
	);
	for (step, message) in [
		(2, "hash mismatch in fixed-output derivation"),
		(3, "must be a fixed-output or impure derivation"),
		(4, "\n1\n"),
		(7, "hash mismatch in fixed-output derivation"),
	] {
		if let Some(output) = steps.get(step) {
			assert!(output.contains(message), "{output}");
		}
	}
}

/// `builtin:fetchurl` runs in the sandbox, so it cannot read host files.
#[test]
fn fetchurl_cannot_read_host_files_in_the_sandbox() {
	let dir = noxi_test_support::tempdir().unwrap();
	let secret = dir.path().join("secret");
	fs::write(&secret, "one\n").unwrap();
	let expr = format!(
		r#"derivation {{ name = "stolen"; system = "builtin"; builder = "builtin:fetchurl"; url = "file://{}"; outputHashMode = "flat"; outputHashAlgo = "sha256"; outputHash = builtins.hashString "sha256" "one\n"; }}"#,
		secret.display()
	);
	let mut engines = vec![candidate_dir().to_owned()];
	engines.extend(reference_dir());
	for (index, bin) in engines.iter().enumerate() {
		let root = dir.path().join(format!("root{index}"));
		let output = Command::new(bin.join("nix-build"))
			.env_clear()
			.env("HOME", dir.path().join("home"))
			.env("NIX_CONF_DIR", dir.path().join("none"))
			.env(
				"NIX_CONFIG",
				"sandbox = true\nbuild-users-group =\nsubstituters =\nfiletransfer-retry-attempts = 1\n",
			)
			.args(["--store", &format!("local?root={}", root.display())])
			.args(["--no-out-link", "-E", &expr])
			.output()
			.unwrap();
		let errors = String::from_utf8_lossy(&output.stderr);
		assert_eq!(output.status.code(), Some(1), "{errors}");
		assert!(errors.contains("unable to download 'file://"), "{errors}");
	}
}

/// In the sandbox, impure derivations and fixed outputs of any method share
/// the host's network and `impureEnvVars`; every other output has neither.
#[test]
fn sandboxes_of_experimental_outputs_match_the_reference() {
	let Some(reference) = reference_dir() else {
		return;
	};
	let busybox = fs::canonicalize(
		std::env::var_os("NOXI_TEST_BUSYBOX")
			.map(PathBuf::from)
			.unwrap_or_else(|| {
				Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-busybox")
			}),
	)
	.unwrap();
	let dir = noxi_test_support::tempdir().unwrap();
	let run = |bin: &Path, root: &Path, program: &str, args: &[&str]| {
		Command::new(bin.join(program))
			.env_clear()
			.env("HOME", dir.path().join("home"))
			.env("LEAK", "leaked")
			.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin")
			.env("NIX_CONF_DIR", dir.path().join("none"))
			.env(
				"NIX_CONFIG",
				format!(
					"experimental-features = {FEATURES}\nsandbox = true\nsandbox-paths =\nbuild-users-group =\nsubstituters =\n"
				),
			)
			.args(["--store", &format!("local?root={}", root.display())])
			.args(args)
			.output()
			.unwrap()
	};
	let expr = format!(
		r#"let
			bb = builtins.storePath {};
			mk = name: attrs: derivation ({{
				inherit name;
				system = "x86_64-linux";
				builder = "${{bb}}/bin/sh";
				impureEnvVars = [ "LEAK" ];
				args = [ "-c" ''
					if [ "$(${{bb}}/bin/busybox grep -c : /proc/net/dev)" = 1 ]; then net=private; else net=shared; fi
					[ -e /etc/resolv.conf ] && resolv=yes || resolv=no
					echo "$net $resolv '$LEAK' '$NIX_OUTPUT_CHECKED'" >&2
					echo fixed > $out
				'' ];
			}} // attrs);
			ca = mode: {{ __contentAddressed = true; outputHashMode = mode; outputHashAlgo = if mode == "git" then "sha1" else "sha256"; }};
		in {{
			pure = mk "pure" {{}};
			text = mk "text" (ca "text");
			git = mk "git" (ca "git");
			impure = mk "impure" {{ __impure = true; }};
			textFixed = mk "text-fixed" {{ outputHashMode = "text"; outputHashAlgo = "sha256"; outputHash = builtins.hashString "sha256" "fixed\n"; }};
			gitFixed = mk "git-fixed" {{ outputHashMode = "git"; outputHashAlgo = "sha1"; outputHash = "eaaa2f850255dbabab16074c24aaa935b9804f3d"; }};
		}}"#,
		busybox.display()
	);
	let mut results = Vec::new();
	for (name, bin) in [
		("reference", reference.as_path()),
		("noxi", candidate_dir()),
	] {
		let root = dir.path().join(name);
		let root_uri = format!("local?root={}", root.display());
		let copied = Command::new(reference.join("nix"))
			.env_clear()
			.env("HOME", dir.path().join("home"))
			.args(["--extra-experimental-features", "nix-command", "copy"])
			.args(["--no-check-sigs", "--to", &root_uri])
			.arg(&busybox)
			.output()
			.unwrap();
		assert!(copied.status.success(), "{copied:?}");
		let mut outputs = Vec::new();
		for attr in ["pure", "text", "git", "impure", "textFixed", "gitFixed"] {
			let output = run(
				bin,
				&root,
				"nix",
				&[
					"build",
					"-L",
					"--no-link",
					"--impure",
					"--expr",
					&format!("({expr}).{attr}"),
				],
			);
			let text = String::from_utf8_lossy(&output.stderr).into_owned();
			let line = text
				.lines()
				.find(|line| line.contains("> private") || line.contains("> shared"))
				.unwrap_or_else(|| panic!("{name} {attr}: {text}"))
				.to_owned();
			outputs.push(format!("{attr}: {line}"));
		}
		results.push(outputs);
	}
	assert_eq!(results[1], results[0]);
	for (index, line) in results[1].iter().enumerate() {
		let isolated = index < 3;
		assert_eq!(line.contains("> private no '' ''"), isolated, "{line}");
		assert_eq!(line.contains("'leaked'"), !isolated, "{line}");
	}
}
