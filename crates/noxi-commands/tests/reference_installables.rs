//! Output selection, content-addressed outputs and chroot store evaluation in
//! the build commands compared with CppNix 2.35.2, as the upstream functional
//! tests `build.sh`, `ca/*` and `local-overlay-store/*` use them.
//!
//! Store paths hash the store directory, so both engines use the same
//! directory one after the other, removed in between. The reference comes
//! from `NOXI_REFERENCE_BIN`, else `target/nix-reference/bin`; without it the
//! tests print a notice and return.
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

const EXPR: &str = r#"
let
	mk = attrs: derivation ({
		system = "x86_64-linux";
		builder = "/bin/sh";
		args = [ "-c" "for o in $outputs; do eval \"echo $name $dep > \\$$o\"; done; if [ -z \"$outputs\" ]; then echo $name $dep > $out; fi" ];
	} // attrs);
	ca = { __contentAddressed = true; outputHashMode = "recursive"; outputHashAlgo = "sha256"; };
	rootCA = mk ({ name = "root-ca"; } // ca);
in rec {
	a = mk { name = "a"; outputs = [ "first" "second" ]; };
	b = mk { name = "b"; };
	e = mk { name = "e"; outputs = [ "c" "a_a" "b" ]; } // { meta.outputsToInstall = [ "a_a" "b" ]; };
	none = mk { name = "none"; } // { meta.outputsToInstall = [ ]; };
	many = { inherit a b; recurseForDerivations = true; };
	string = "${a.second}";
	root = rootCA;
	dependent = mk ({ name = "dependent-ca"; dep = rootCA; } // ca);
	two = mk ({ name = "two-ca"; outputs = [ "out" "dev" ]; dep = rootCA; } // ca);
	legacy = mk { name = "legacy"; dep = rootCA; };
	ifd = builtins.readFile dependent;
	tool = mk ({ name = "tool"; args = [ "-c" "PATH=/run/current-system/sw/bin:/usr/bin:/bin; mkdir -p $out/bin; echo '#!/bin/sh' > $out/bin/tool; echo 'echo ran $0' >> $out/bin/tool; chmod +x $out/bin/tool" ]; } // ca);
	link = mk { name = "link"; inherit tool; args = [ "-c" "PATH=/run/current-system/sw/bin:/usr/bin:/bin; ln -s $tool $out" ]; };
	outside = mk { name = "outside"; args = [ "-c" "PATH=/run/current-system/sw/bin:/usr/bin:/bin; ln -s /tmp/foo $out" ]; };
	seeded = mk ({ name = "root-ca"; seed = "2"; } // ca);
}
"#;

/// Run the shell commands `steps` with one engine in a fresh diverted store,
/// returning each step's exit status, standard output and error lines.
fn session(bin: &Path, root: &Path, steps: &[&str]) -> Vec<String> {
	noxi_test_support::remove_tree(root).unwrap();
	fs::create_dir_all(root.join("etc")).unwrap();
	fs::write(
		root.join("etc/nix.conf"),
		"experimental-features = nix-command ca-derivations\nsandbox = false\nbuild-users-group =\nsubstituters =\n",
	)
	.unwrap();
	fs::write(root.join("x.nix"), EXPR).unwrap();
	steps
		.iter()
		.map(|step| {
			let output = Command::new("bash")
				.args(["-c", step])
				.current_dir(root)
				.env_clear()
				.env("HOME", root.join("home"))
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
				.filter(|line| line.trim_start().starts_with("error: "))
				.map(|line| format!("{}\n", line.trim_start()))
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
	let Some(reference) = reference_dir() else {
		return;
	};
	let dir = noxi_test_support::tempdir().unwrap();
	let root = dir.path().join("engine");
	let expected = session(&reference, &root, steps);
	let actual = session(candidate_dir(), &root, steps);
	for (expected, actual) in expected.iter().zip(&actual) {
		assert_eq!(actual, expected);
	}
}

#[test]
fn installables_select_the_reference_outputs() {
	same(&[
		"nix build -f x.nix --json --no-link a b",
		"nix build -f x.nix --json --no-link a^first",
		"nix build -f x.nix --json --no-link a^second,first 'b^*'",
		"nix build -f x.nix --json --no-link 'e'",
		"nix build -f x.nix --json --no-link 'e^*'",
		"nix build -f x.nix --json --no-link e^c e.c",
		"nix build -f x.nix --json --no-link none many",
		"nix build -f x.nix --json --no-link string e.a_a.outPath",
		"nix build -f x.nix --print-out-paths --no-link many",
		"nix build -f x.nix a e; ls | sort; readlink result*",
		"nix build -f x.nix --no-link a^",
		"nix build -f x.nix --no-link 'a^*nope'",
		"nix eval -f x.nix b^out",
		"d=$(nix eval -f x.nix --raw a.drvPath); nix build --json --no-link \"$d^first\"; nix build --json --no-link \"$d^*\"",
		"d=$(nix eval -f x.nix --raw a.drvPath); nix build --json --no-link \"$d^\"",
	]);
}

#[test]
fn content_addressed_outputs_match_the_reference() {
	same(&[
		"nix-build x.nix -A root --no-out-link",
		"nix-build x.nix -A dependent -o out; readlink out",
		"cat $(nix-build x.nix -A two.dev --no-out-link)",
		"nix-build x.nix -A two -A legacy --no-out-link",
		"nix build -f x.nix --json --no-link root dependent two",
		"nix build -f x.nix two; readlink result result-dev",
		"nix derivation show -f x.nix two legacy",
		"nix derivation show $(nix-instantiate x.nix -A dependent)^out",
		"nix eval -f x.nix --raw ifd",
		"nix-store -q --binding name $(nix-instantiate x.nix -A dependent)",
		"nix-store -q --outputs $(nix-instantiate x.nix -A dependent)",
		"nix-store -r $(nix-instantiate x.nix -A dependent)",
		"nix-shell x.nix -A dependent --run 'echo $name $dep'",
		"nix run -f x.nix tool",
		"nix shell -f x.nix link -c bash -c 'echo ${PATH%%:*}; tool'",
		"nix shell -f x.nix outside -c true",
		"nix why-depends -f x.nix dependent seeded",
		"rm -f out result*; o=$(nix-build x.nix -A root --no-out-link); nix-store --delete $(nix-store -q --referrers-closure $o) >/dev/null; touch $o; nix-build x.nix -A root --no-out-link; stat -c %F $o",
		"nix copy -f x.nix --to file://$PWD/cache dependent; cat cache/*.narinfo | grep -E '^(StorePath|References)' | sort",
		"nix derivation show $(nix-instantiate x.nix -A root) --extra-experimental-features nix-command > all.json; sed -E 's/.*\"derivations\":\\{\"[^\"]*\":(.*)\\},\"version\":4\\}$/\\1/' all.json > one.json; nix derivation add < one.json; NIX_CONFIG='experimental-features = nix-command' nix derivation add < one.json",
	]);
}

/// Impure evaluation reads the host's store objects next to those of a chroot
/// store; pure evaluation reads only the latter.
#[test]
fn chroot_store_evaluation_reads_host_store_objects() {
	let Some(reference) = reference_dir() else {
		return;
	};
	// Any object of the host store that a chroot store lacks.
	let object = fs::canonicalize(reference.join("nix")).unwrap();
	let object = object.display();
	let run = |expr: &str| -> Vec<String> {
		["--impure", "--pure-eval"]
			.iter()
			.map(|mode| {
				format!(
					"unset NIX_STORE_DIR NIX_STATE_DIR NIX_LOG_DIR; nix eval {mode} --store \"local?root=$PWD/chroot\" --expr '{expr}'"
				)
			})
			.collect()
	};
	let steps: Vec<String> = [
		format!("builtins.pathExists {object}"),
		format!("builtins.readFileType {object}"),
		format!("\"${{{object}}}\""),
		"builtins.readFile (builtins.toFile \"x\" \"y\")".to_owned(),
		"\"${builtins.toFile \"x\" \"y\"}\"".to_owned(),
	]
	.iter()
	.flat_map(|expr| run(expr))
	.collect();
	same(&steps.iter().map(String::as_str).collect::<Vec<_>>());
}
