//! Extra-corpus builtin forcing and context regressions against CppNix 2.35.2.
use std::os::unix::process::CommandExt;
use std::{
	path::{Path, PathBuf},
	process::Command,
};

fn compare(expressions: &[&str]) {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate");
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	let home = tempfile::tempdir().unwrap();
	for expression in expressions {
		for trace in [false, true] {
			let run = |binary: &Path| {
				let mut command = Command::new(binary);
				command.arg0("nix-instantiate");
				command
					.env_clear()
					.env("HOME", home.path())
					.env("NIX_CONF_DIR", home.path())
					.env("NIX_USER_CONF_FILES", "")
					.env("NIX_CONFIG", "experimental-features = nix-command flakes\n")
					.env("PATH", std::env::var_os("PATH").unwrap())
					.env("NIX_PATH", "")
					.current_dir(home.path())
					.args([
						"--store", "dummy://", "--eval", "--strict", "--expr", expression,
					]);
				if trace {
					command.arg("--show-trace");
				}
				command.output().unwrap()
			};
			let expected = run(&reference);
			let actual = run(candidate);
			assert!(
				matches!(expected.status.code(), Some(0 | 1)),
				"reference crashed: {expected:?}"
			);
			assert_eq!(
				(actual.status.code(), &actual.stdout, &actual.stderr),
				(expected.status.code(), &expected.stdout, &expected.stderr),
				"{expression}, trace={trace}\ncandidate: {}\nreference: {}",
				String::from_utf8_lossy(&actual.stderr),
				String::from_utf8_lossy(&expected.stderr)
			);
		}
	}
}

#[test]
fn fetcher_arguments_and_flake_references_match_reference() {
	compare(&[
		"builtins.parseFlakeRef null",
		"builtins.parseFlakeRef (throw \"argument\")",
		"builtins.parseFlakeRef \"\"",
		"builtins.parseFlakeRef \"/abs\"",
		"builtins.parseFlakeRef \"./relative\"",
		"builtins.parseFlakeRef \"github:NixOS/nixpkgs/23.05?dir=lib\"",
		"builtins.parseFlakeRef (builtins.toFile \"f\" \"c\")",
		"builtins.flakeRefToString []",
		"builtins.flakeRefToString (throw \"argument\")",
		"builtins.getFlake []",
		"builtins.getFlake (throw \"argument\")",
		"builtins.getFlake { outPath = \"/out\"; }",
		"builtins.fetchGit []",
		"builtins.fetchGit { url = \"/nonexistent\"; }",
		"builtins.fetchMercurial { url = \"/nonexistent\"; }",
		"builtins.fetchurl 1",
		"builtins.fetchTarball null",
		"builtins.fetchurl (throw \"url\")",
		"builtins.fetchTarball (throw \"url\")",
		"builtins.fetchurl { url = 1; }",
		"builtins.fetchTarball { url = throw \"url\"; }",
		"builtins.fetchurl { url = \"/nonexistent\"; name = 1; }",
		"builtins.fetchTarball { url = \"/nonexistent\"; sha256 = 1; }",
		"builtins.fetchurl { url = \"/nonexistent\"; }",
	]);
}
