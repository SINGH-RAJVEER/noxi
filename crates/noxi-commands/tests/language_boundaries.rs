//! Evaluation boundaries compared byte for byte with pinned CppNix.
use std::{
	path::{Path, PathBuf},
	process::{Command, Output},
};

fn evaluate(binary: &Path, home: &Path, expression: &str, trace: bool) -> Output {
	let mut command = Command::new(binary);
	command
		.env_clear()
		.env("PATH", std::env::var_os("PATH").unwrap())
		.env("HOME", home)
		.env("NIX_CONF_DIR", home)
		.env("NIX_USER_CONF_FILES", "/dev/null")
		.env("NIX_CONFIG", "")
		.env("NIX_REMOTE", "dummy://")
		.args(["--eval", "--strict", "--expr", expression]);
	if trace {
		command.arg("--show-trace");
	}
	command.output().unwrap()
}

#[test]
fn disabled_derivation_features_match_reference_errors_and_forcing() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate");
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	let home = tempfile::tempdir().unwrap();
	for control in ["__contentAddressed", "__impure"] {
		for value in ["true", "false", "null", "1", "throw \"control\""] {
			let expression = format!(
				"(derivation {{ name = \"probe\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; {control} = {value}; }}).drvPath"
			);
			for trace in [false, true] {
				let expected = evaluate(&reference, home.path(), &expression, trace);
				let actual = evaluate(candidate, home.path(), &expression, trace);
				assert_eq!(
					(actual.status.code(), actual.stdout, actual.stderr),
					(expected.status.code(), expected.stdout, expected.stderr),
					"{expression}, trace={trace}"
				);
			}
		}
	}
}

#[test]
fn nested_byte_strings_and_attribute_names_print_without_replacement() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate");
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	let home = tempfile::tempdir().unwrap();
	let path = home.path().join("bytes");
	std::fs::write(&path, b"\xff\"\n\t\\${").unwrap();
	for body in [
		"[ s { value = s; } ]",
		"builtins.listToAttrs [{ name = s; value = s; }]",
	] {
		let expression = format!(
			"let s = builtins.readFile {}; in {body}",
			serde_json::json!(path)
		);
		let expected = evaluate(&reference, home.path(), &expression, false);
		let actual = evaluate(candidate, home.path(), &expression, false);
		assert!(expected.status.success(), "{expected:?}");
		assert_eq!(
			(actual.status.code(), actual.stdout, actual.stderr),
			(expected.status.code(), expected.stdout, expected.stderr),
			"{expression}"
		);
	}
}
