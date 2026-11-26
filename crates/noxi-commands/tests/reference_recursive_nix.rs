//! Recursive Nix compared with CppNix 2.35.2: what a build that requires
//! `recursive-nix` can reach through its socket.
//!
//! A probe derivation runs Nix commands against `NIX_REMOTE` and writes
//! each command's exit status and errors to its output. Both engines build
//! it in the same diverted store, one after the other, and the reports must
//! be equal. The reference comes from `NOXI_REFERENCE_BIN`, else
//! `target/nix-reference/bin`; without it the tests print a notice and
//! return.
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

/// The probe. `r` runs a command and records whether it succeeded and the
/// error lines it printed, with the store directory and build directory
/// replaced, so that only what the socket allows is compared.
const EXPR: &str = r#"
let
	bin = builtins.getEnv "NIX_BIN_DIR";
	mk = attrs: derivation ({
		system = "x86_64-linux";
		builder = "/bin/sh";
		PATH = "${bin}:/run/current-system/sw/bin:/usr/bin:/bin";
	} // attrs);
	# The same derivation under both engines.
	leaf = derivation { name = "leaf"; system = "x86_64-linux"; builder = "/bin/sh"; args = [ "-c" "echo leaf > $out" ]; };
in {
	inherit leaf;
	probe = mk {
		name = "probe";
		requiredSystemFeatures = [ "recursive-nix" ];
		dummy = builtins.toFile "dummy" "dummy";
		# A string without context: valid, but not an input of the build.
		unreachable = builtins.getEnv "UNREACHABLE";
		leafDrv = builtins.unsafeDiscardStringContext leaf.drvPath;
		args = [ "-c" ''
			r() {
				if "$@" > out.txt 2> err.txt; then echo "ok: $*"; else echo "fails: $*"; fi
				grep -h 'error' err.txt | sed "s|$NIX_STORE|STORE|g; s|$NIX_BUILD_TOP|TOP|g" || true
			}
			o="--extra-experimental-features nix-command"
			{
				echo "NIX_REMOTE=$(echo "$NIX_REMOTE" | sed "s|$NIX_BUILD_TOP|TOP|")"
				echo "modes: $(stat -c %a "$NIX_BUILD_TOP" "$NIX_BUILD_TOP/.nix-socket" | tr '\n' ' ')"
				r nix $o path-info $dummy
				r nix $o path-info $unreachable
				r nix $o build --no-link $unreachable
				r nix-store -r $unreachable
				r nix-store -q --references $unreachable
				r nix-store -q --referrers $dummy
				r nix-store -q --roots $dummy
				r nix-store -q --deriver $dummy
				r nix-store -r $leafDrv
				r nix $o store gc
				echo "all: $(nix $o path-info --all | wc -l)"
				nix $o path-info --json $dummy | sed "s|$NIX_STORE|STORE|g"; echo
				echo added > added
				added=$(nix $o store add-path ./added)
				r nix $o path-info $added
				echo "all: $(nix $o path-info --all | wc -l)"
			} > $out
		'' ];
	};
}
"#;

fn report(bin: &Path, root: &Path) -> String {
	noxi_test_support::remove_tree(root).unwrap();
	fs::create_dir_all(root.join("etc")).unwrap();
	fs::write(
		root.join("etc/nix.conf"),
		"experimental-features = nix-command recursive-nix\nsandbox = false\nbuild-users-group =\nsubstituters =\n",
	)
	.unwrap();
	fs::write(root.join("x.nix"), EXPR).unwrap();
	fs::write(root.join("secret"), "not an input\n").unwrap();
	let run = |script: &str| {
		let output = Command::new("bash")
			.args(["-c", script])
			.current_dir(root)
			.env_clear()
			.env("HOME", root.join("home"))
			.env("NIX_BIN_DIR", bin)
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
		assert!(
			output.status.success(),
			"{script}: {}",
			String::from_utf8_lossy(&output.stderr)
		);
		String::from_utf8(output.stdout).unwrap()
	};
	let secret = run("nix-store --add secret");
	run(&format!(
		"export UNREACHABLE={}; nix-instantiate x.nix -A leaf > /dev/null 2>&1; cat \"$(nix-build x.nix -A probe --no-out-link 2> build.log)\"",
		secret.trim()
	))
}

#[test]
fn recursive_builds_reach_only_their_inputs_like_the_reference() {
	let Some(reference) = reference_dir() else {
		return;
	};
	let dir = noxi_test_support::tempdir().unwrap();
	let root = dir.path().join("engine");
	let expected = report(&reference, &root);
	let actual = report(candidate_dir(), &root);
	assert_eq!(actual, expected);
	// The negative cases the comparison covers: the valid path that is not
	// an input and the derivation that is not one cannot be queried or
	// built, and the socket is the builder's alone.
	assert!(expected.contains("modes: 700 600"), "{expected}");
	assert!(expected.contains("fails: nix-store -r /"), "{expected}");
	assert!(expected.contains("-secret"), "{expected}");
	assert!(
		expected.contains("in recursive Nix because path is unknown"),
		"{expected}"
	);
	assert!(
		expected
			.lines()
			.filter(|line| line.starts_with("fails:"))
			.count()
			>= 4,
		"{expected}"
	);
}
