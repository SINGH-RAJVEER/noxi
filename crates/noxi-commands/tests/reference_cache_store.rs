//! A `file://` binary cache as a store compared with CppNix 2.35.2, as the
//! upstream functional tests `binary-cache.sh`, `ca/nix-copy.sh`,
//! `ca/signatures.sh` and `read-only-store.sh` use it.
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

const EXPR: &str = r#"
let
	mk = attrs: derivation ({
		system = "x86_64-linux";
		builder = "/bin/sh";
		PATH = "/run/current-system/sw/bin:/usr/bin:/bin";
	} // attrs);
	ca = { __contentAddressed = true; outputHashMode = "recursive"; outputHashAlgo = "sha256"; };
	script = text: [ "-c" text ];
in rec {
	leaf = mk { name = "leaf"; args = script "echo leaf > $out"; };
	tree = mk { name = "tree"; args = script "echo built tree; mkdir -p $out/bin; echo run > $out/bin/run; chmod +x $out/bin/run; ln -s ${leaf} $out/leaf; echo data > $out/data"; };
	unbuilt = mk { name = "unbuilt"; args = script "echo unbuilt > $out"; };
	leafCa = mk ({ name = "leaf-ca"; args = script "echo leaf > $out"; } // ca);
	treeCa = mk ({ name = "tree-ca"; outputs = [ "out" "dev" ]; args = script "mkdir $out; ln -s ${leafCa} $out/leaf; ln -s $out $dev"; } // ca);
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
fn default_sandbox_rejects_derivations_that_request_no_chroot() {
	let Some(reference) = reference_dir() else {
		return;
	};
	let dir = noxi_test_support::tempdir().unwrap();
	let steps = [
		"sed -i '/^sandbox =/d' etc/nix.conf",
		r#"drv='derivation { name = "no-chroot"; system = builtins.currentSystem; builder = "/nonexistent"; __noChroot = true; }'
		for flags in '' '--relaxed-sandbox --sandbox'; do
			if nix build --impure --no-link --expr "$drv" $flags >stdout 2>stderr; then
				exit 1
			fi
			grep -Fq "__noChroot' set, but that's not allowed when 'sandbox' is 'true'" stderr || exit 1
		done"#,
	];
	for engine in [&reference, candidate_dir()] {
		let results = session(engine, &dir.path().join("engine"), "nix-command", &steps);
		for (result, step) in results.iter().zip(steps) {
			assert_eq!(
				result,
				&format!("$ {step}\nSome(0)\n"),
				"{}",
				engine.display()
			);
		}
	}
}

#[test]
fn cache_queries_match_the_reference() {
	same(&[
		"nix-build x.nix -A tree --no-out-link; nix copy --to file://$PWD/cache -f x.nix tree 2> /dev/null; find cache | sort",
		"nix path-info --store file://$PWD/cache --all",
		"nix path-info --store file://$PWD/cache --all --json --json-format 2",
		"nix path-info --store file://$PWD/cache -r -S $(nix-store -q $(nix-instantiate x.nix -A tree 2> /dev/null))",
		"nix path-info --store file://$PWD/cache --json --json-format 1 $(nix-store -q $(nix-instantiate x.nix -A leaf 2> /dev/null))",
		"nix path-info --store file://$PWD/cache $PWD/store/00000000000000000000000000000000-missing",
		"t=$(nix-store -q $(nix-instantiate x.nix -A tree 2> /dev/null)); nix store ls --store file://$PWD/cache -l $t; nix store ls --store file://$PWD/cache -R --json $t",
		"t=$(nix-store -q $(nix-instantiate x.nix -A tree 2> /dev/null)); nix store cat --store file://$PWD/cache $t/data; nix store cat --store file://$PWD/cache $t/bin",
		"t=$(nix-store -q $(nix-instantiate x.nix -A tree 2> /dev/null)); nix store dump-path --store file://$PWD/cache $t | sha256sum",
		"nix store verify --store file://$PWD/cache --all --no-trust 2>&1 | sort",
		"nix store verify --store file://$PWD/cache --all 2>&1 | sort",
		"nix store info --store file://$PWD/cache; nix store info --store file://$PWD/cache --json",
		"nix store info --store file://$PWD/fresh; find fresh | sort; cat fresh/nix-cache-info",
	]);
}

#[test]
fn cache_writes_match_the_reference() {
	same(&[
		"nix-instantiate --store file://$PWD/cache x.nix -A tree 2> /dev/null; find cache -name '*.narinfo' | sort",
		"nix-store --store file://$PWD/cache --add x.nix; nix store add --store file://$PWD/cache x.nix; nix store add-file --store file://$PWD/cache x.nix",
		"nix path-info --store file://$PWD/cache --all",
		"nix build --store file://$PWD/cache -f x.nix unbuilt 2>&1 | grep -v '^  /\\|will be built'",
		"nix-build --store file://$PWD/cache x.nix -A unbuilt 2>&1 | grep -v '^  /\\|will be built'; ls result",
		"nix-build x.nix -A tree --no-out-link > /dev/null 2>&1; nix copy --to file://$PWD/cache -f x.nix tree 2> /dev/null; nix build --store file://$PWD/cache -f x.nix tree --json; ls result",
	]);
}

#[test]
fn cache_logs_match_the_reference() {
	same(&[
		"nix-build x.nix -A tree --no-out-link 2> /dev/null; nix copy --to file://$PWD/cache -f x.nix tree 2> /dev/null",
		"nix log --store file://$PWD/cache $(ls -d $PWD/store/*-tree)",
		"nix store copy-log --to file://$PWD/cache $(ls -d $PWD/store/*-tree); cat cache/log/*",
		"nix log --store file://$PWD/cache $(ls -d $PWD/store/*-tree)",
		"rm -rf log; nix log $(ls -d $PWD/store/*-tree)",
		"nix log --substituters file://$PWD/cache $(ls -d $PWD/store/*-tree)",
		"nix store copy-log --from file://$PWD/cache $(ls $PWD/store/*-tree.drv)^'*'; nix log $(ls -d $PWD/store/*-tree)",
		"printf '%s\\n' \"$(ls -d \"$PWD\"/store/*-tree)\" > tree-path; chmod -R u+w store; rm -rf store var log; nix-store -r \"$(cat tree-path)\" --substituters file://$PWD/cache --no-require-sigs >/dev/null 2>&1 && nix log --substituters file://$PWD/cache \"$(cat tree-path)\"",
	]);
}

#[test]
fn copy_all_links_and_profiles_match_the_reference() {
	same(&[
		"nix-build x.nix -A tree --no-out-link >tree-path 2>/dev/null && nix copy --to file://$PWD/cache --all 2>/dev/null",
		"nix path-info --store file://$PWD/cache --all | sort",
		"chmod -R u+w store; rm -rf store var; nix copy --from file://$PWD/cache --no-check-sigs --out-link result --profile profile \"$(cat tree-path)\" 2>/dev/null && readlink result profile profile-1-link",
		"nix copy --from file://$PWD/cache --no-check-sigs --out-link result --profile profile \"$(cat tree-path)\" && readlink result profile profile-1-link",
	]);
}

#[test]
fn build_trace_file_outputs_match_the_reference() {
	same(&[
		"nix-build x.nix -A leafCa --no-out-link >/dev/null 2>&1",
		"nix store build-trace info --file x.nix leafCa",
		"nix store build-trace info --json --file x.nix leafCa",
	]);
}

#[test]
fn copies_from_a_cache_match_the_reference() {
	same(&[
		"nix-build x.nix -A tree -A treeCa --no-out-link > /dev/null 2>&1; nix copy --to file://$PWD/cache -f x.nix tree treeCa 2> /dev/null; find cache -path '*build-trace*' | sort",
		"chmod -R u+w store; rm -rf store var; nix copy --no-check-sigs --from file://$PWD/cache -f x.nix tree 2> /dev/null; ls store",
		"chmod -R u+w store; rm -rf store var; nix copy --no-check-sigs --from file://$PWD/cache -f x.nix treeCa 2> /dev/null; ls store; nix-build x.nix -A treeCa --no-out-link -j0 2> /dev/null",
		"nix path-info --json --json-format 2 $(ls -d $PWD/store/*-tree-ca) | sed 's/\"registrationTime\":[0-9]*//'",
		"nix copy --no-check-sigs --from file://$PWD/cache -f x.nix unbuilt 2>&1 | grep -v '^  /\\|will be built'",
	]);
}

/// An unsigned path or realisation is refused, whichever way it is named.
#[test]
fn unsigned_copies_are_refused_like_the_reference() {
	same(&[
		"nix-build x.nix -A leaf -A leafCa --no-out-link > /dev/null 2>&1; nix copy --to file://$PWD/cache -f x.nix leaf leafCa 2> /dev/null",
		"chmod -R u+w store; rm -rf store var; nix copy --from file://$PWD/cache -f x.nix leaf 2>&1 | grep error; ls store | grep -v drv",
		"nix copy --from file://$PWD/cache -f x.nix leafCa 2>&1 | grep error; nix-build x.nix -A leafCa --no-out-link -j0 2>&1 | grep -c 'local builds are disabled'",
		"nix copy --from file://$PWD/cache?trusted=1 -f x.nix leaf 2>&1 | grep error",
	]);
}

/// Built paths and realisations are signed with `secret-key-files`; a copy
/// trusts those of the matching key only.
#[test]
fn secret_key_files_sign_builds_like_the_reference() {
	same(&[
		"nix key generate-secret --key-name a-1 > a; nix key generate-secret --key-name b-1 > b; nix-build x.nix -A leaf -A leafCa --no-out-link --secret-key-files \"$PWD/a $PWD/b\" 2> /dev/null; nix path-info --sigs $(ls -d $PWD/store/*-leaf $PWD/store/*-leaf-ca) | sed 's/:[A-Za-z0-9+/=]*//g'",
		"nix copy --to file://$PWD/cache -f x.nix leaf leafCa 2> /dev/null; grep -ho '\"keyName\":\"[^\"]*\"' cache/build-trace*/*/*; grep -h ^Sig cache/*.narinfo | sed 's/\\(Sig: [^:]*\\):.*/\\1/'",
		"nix key generate-secret --key-name d-1 > d; l=$(ls -d $PWD/store/*-leaf); nix store sign --store file://$PWD/cache --key-file d $l; nix store sign --store file://$PWD/cache --key-file d $l; nix path-info --store file://$PWD/cache --sigs $l | grep -o ' [abd]-1'; nix store verify --store file://$PWD/cache --trusted-public-keys $(nix key convert-secret-to-public < d) $l",
		"chmod -R u+w store; rm -rf store var; nix copy --from file://$PWD/cache -f x.nix leaf leafCa --trusted-public-keys c-1:oMjUyRqiELXsP0bHrTJVcHXGcnnTHfWFnTBJeH3vMBM= 2>&1 | grep error; ls store | grep -v drv",
		"nix copy --from file://$PWD/cache -f x.nix leaf leafCa --trusted-public-keys $(nix key convert-secret-to-public < b) 2>&1 | grep error; ls store | grep -v drv",
		"chmod -R u+w store; rm -rf store var; nix copy --from file://$PWD/cache -f x.nix leaf leafCa --secret-key-files $PWD/a 2>&1 | grep error; ls store | grep -v drv",
		"nix-build x.nix -A tree --no-out-link --secret-key-files $PWD/missing 2>&1 | grep -v building | tail -1",
	]);
}

/// A path copied to another store is not ultimately trusted there.
#[test]
fn copies_between_stores_are_not_ultimately_trusted() {
	same(&[
		"nix-build x.nix -A leaf --no-out-link > /dev/null 2>&1; l=$(ls -d $PWD/store/*-leaf); nix path-info --sigs $l; nix copy --to $PWD/other $l 2> /dev/null; echo $?; nix copy --no-check-sigs --to $PWD/other $l 2> /dev/null; nix path-info --store $PWD/other --sigs $l",
		"nix store verify --store $PWD/other $(ls -d $PWD/store/*-leaf) 2>&1 | sort",
		"nix store verify $(ls -d $PWD/store/*-leaf) 2>&1 | sort",
	]);
}

#[test]
fn read_only_stores_match_the_reference() {
	same(&[
		"nix-store --add x.nix > path; mkdir other; echo other > other/file",
		"nix-store --store local?read-only=true --extra-experimental-features read-only-local-store --add x.nix",
		"chmod -R u-w store var; nix-store --add other; nix-store --store local?read-only=true --extra-experimental-features read-only-local-store --add other; nix-store --store local?read-only=true --extra-experimental-features read-only-local-store --add x.nix; chmod -R u+w store var; ls store",
	]);
}

#[test]
fn substitute_status_matches_the_reference() {
	same(&[
		"echo '{ inherit (import ./x.nix) leaf tree; }' > y.nix; nix-build x.nix -A leaf --no-out-link > /dev/null 2>&1; nix copy --to file://$PWD/cache -f x.nix leaf 2> /dev/null; chmod -R u+w store; rm -rf store var; nix-env --substituters file://$PWD/cache -f y.nix -qas '*'",
		"echo 'WantMassQuery: 1' >> cache/nix-cache-info; rm -rf home; nix-env --substituters file://$PWD/cache -f y.nix -qas '*'; nix-env --substituters file://$PWD/cache -f y.nix -qas '*' --prebuilt-only",
	]);
}
