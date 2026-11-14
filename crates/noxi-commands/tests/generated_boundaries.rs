//! Generated pure-evaluation, metadata and hashing counterexamples against CppNix.
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::{
	path::{Path, PathBuf},
	process::{Command, Output},
};

fn evaluate(binary: &Path, root: &Path, expression: &str, config: &str, flags: &[&str]) -> Output {
	Command::new(binary)
		.arg0("nix-instantiate")
		.env_clear()
		.env("HOME", root)
		.env("NIX_CONF_DIR", root)
		.env("NIX_USER_CONF_FILES", "")
		.env("NIX_PATH", "")
		.env("NIX_CONFIG", config)
		.current_dir(root)
		.args([
			"--store", "dummy://", "--eval", "--strict", "--expr", expression,
		])
		.args(flags)
		.output()
		.unwrap()
}

fn compare(root: &Path, expression: &str, config: &str, flags: &[&str]) {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate");
	let expected = evaluate(&reference, root, expression, config, flags);
	let actual = evaluate(
		Path::new(env!("CARGO_BIN_EXE_nix-instantiate")),
		root,
		expression,
		config,
		flags,
	);
	assert!(
		matches!(expected.status.code(), Some(0 | 1)),
		"reference {expected:?}"
	);
	assert_eq!(
		(actual.status.code(), actual.stdout, actual.stderr),
		(expected.status.code(), expected.stdout, expected.stderr),
		"{expression} {flags:?} {config}"
	);
}

#[test]
fn invalid_text_contexts_keep_the_path_realisation_error() {
	let root = tempfile::tempdir().unwrap();
	for expression in [
		"builtins.hashFile \"sha256\" (builtins.toFile \"f\" \"c\")",
		"builtins.pathExists (builtins.toFile \"f\" \"c\")",
		"builtins.import (builtins.toFile \"f\" \"c\")",
		"builtins.readFile (builtins.toFile \"f\" \"c\")",
	] {
		for flags in [vec![], vec!["--show-trace"]] {
			compare(root.path(), expression, "", &flags);
		}
	}
}

#[test]
fn restricted_fetch_uris_follow_reference_prefix_and_error_order() {
	let root = tempfile::tempdir().unwrap();
	for expression in [
		"builtins.fetchurl \"https://example.invalid/repo\"",
		"builtins.fetchTarball \"https://example.invalid/repo\"",
		"builtins.fetchTarball { url = \"https://example.invalid/repo\"; name = \"bad/name\"; }",
		"builtins.fetchGit \"https://example.invalid/repo\"",
		"builtins.fetchTree { type = \"git\"; url = \"https://example.invalid/repo\"; }",
	] {
		compare(
			root.path(),
			expression,
			"experimental-features = flakes\n",
			&["--restrict-eval"],
		);
	}
	for prefix in [
		"https://example.invalid/repo",
		"https://example.invalid",
		"https:",
		"https://example.in",
	] {
		for expression in [
			"builtins.fetchurl \"https://example.invalid/repo\"",
			"builtins.fetchTarball \"https://example.invalid/repo\"",
		] {
			compare(
				root.path(),
				expression,
				&format!("allowed-uris = {prefix}\n"),
				&["--restrict-eval", "--pure-eval"],
			);
		}
	}
}

#[test]
fn pure_evaluation_hides_impure_constants_regardless_of_builder_order() {
	let root = tempfile::tempdir().unwrap();
	for expression in [
		"builtins ? currentSystem",
		"builtins ? currentTime",
		"builtins.currentSystem",
		"builtins.currentTime",
	] {
		compare(root.path(), expression, "", &["--pure-eval"]);
	}
	for flags in [vec![], vec!["--pure-eval", "--impure"]] {
		compare(
			root.path(),
			"[ (builtins ? currentSystem) (builtins ? currentTime) ]",
			"",
			&flags,
		);
	}
}

#[test]
fn restricted_metadata_preserves_the_final_symlink_without_allowing_target_reads() {
	let root = tempfile::tempdir().unwrap();
	std::fs::create_dir(root.path().join("allowed")).unwrap();
	std::fs::create_dir(root.path().join("outside-dir")).unwrap();
	std::fs::write(root.path().join("outside"), "secret").unwrap();
	std::fs::write(root.path().join("outside-dir/data"), "secret").unwrap();
	symlink("../outside", root.path().join("allowed/link")).unwrap();
	symlink("../missing", root.path().join("allowed/dangling")).unwrap();
	symlink("../outside-dir", root.path().join("allowed/dir-link")).unwrap();
	compare(root.path(), "builtins.readDir ./allowed/link", "", &[]);
	for expression in [
		"builtins.filterSource (_: _: true) ./allowed/link",
		"builtins.filterSource (_: _: true) ./allowed/../outside",
		"import ./allowed/link",
	] {
		compare(root.path(), expression, "", &["--restrict-eval"]);
	}
	compare(
		root.path(),
		"builtins.pathExists ./allowed/dir-link/data",
		"",
		&[],
	);
	compare(
		root.path(),
		"builtins.readFileType ./allowed/dir-link/data",
		"",
		&[],
	);
	for expression in [
		"builtins.pathExists ./allowed/link",
		"builtins.readFileType ./allowed/link",
		"builtins.pathExists ./allowed/dangling",
		"builtins.readFileType ./allowed/dangling",
		"builtins.pathExists ./allowed/dir-link",
		"builtins.readFileType ./allowed/dir-link",
		"builtins.readFile ./allowed/link",
		"builtins.readDir ./allowed/dir-link",
		"builtins.pathExists ./allowed/dir-link/data",
		"builtins.readFileType ./allowed/dir-link/data",
	] {
		compare(
			root.path(),
			expression,
			"",
			&["--restrict-eval", "-I", "./allowed"],
		);
	}
}

#[test]
fn hash_builtins_apply_algorithm_features_and_argument_order() {
	let root = tempfile::tempdir().unwrap();
	std::fs::write(root.path().join("data"), b"abc\xff").unwrap();
	for config in ["", "experimental-features = blake3-hashes\n"] {
		for expression in [
			"builtins.hashString \"blake3\" \"abc\"",
			"builtins.hashFile \"blake3\" ./data",
			"builtins.hashString \"sha256\" \"abc\"",
			"builtins.hashFile \"sha256\" ./data",
			"builtins.hashString \"blake3\" (throw \"unused\")",
			"builtins.hashFile \"blake3\" (throw \"unused\")",
			"builtins.hashString \"unknown\" (throw \"unused\")",
			"builtins.hashString {} (throw \"unused\")",
			"builtins.hashString (builtins.appendContext \"sha256\" { \"/nix/store/00000000000000000000000000000000-test\".path = true; }) \"abc\"",
			"builtins.hashFile (builtins.appendContext \"sha256\" { \"/nix/store/00000000000000000000000000000000-test\".path = true; }) ./data",
		] {
			compare(root.path(), expression, config, &["--show-trace"]);
		}
	}
}
