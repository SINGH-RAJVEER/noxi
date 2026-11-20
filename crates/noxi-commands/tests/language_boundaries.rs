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

#[test]
fn generic_closure_prints_elements_when_an_error_passes() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate");
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	let home = tempfile::tempdir().unwrap();
	// Comparing the keys and calling the operator force parts of the
	// elements before the error, and a duplicate key leaves no comparison
	// frame behind for the next element.
	for expression in [
		r#"let x = "a" + ""; y = 1 + 1; in builtins.genericClosure { startSet = [ { key = [ 1 y ]; other = y; } { key = [ 1 x ]; other = x; } ]; operator = e: [ ]; }"#,
		r#"let x = "a" + ""; in builtins.genericClosure { startSet = [ { key = 1; v = x; } ]; operator = e: builtins.seq e.v "not a list"; }"#,
		r#"let x = "a" + ""; in builtins.genericClosure { startSet = [ { key = 1; v = x; } { key = 1; v = x; } (builtins.seq x "bad") ]; operator = e: [ ]; }"#,
		r#"let x = "a" + ""; in builtins.genericClosure { startSet = [ { key = 1; v = x; } { key = x; v = x; } ]; operator = e: [ ]; }"#,
	] {
		for trace in [false, true] {
			let expected = evaluate(&reference, home.path(), expression, trace);
			let actual = evaluate(candidate, home.path(), expression, trace);
			assert_eq!(
				(actual.status.code(), actual.stdout, actual.stderr),
				(expected.status.code(), expected.stdout, expected.stderr),
				"{expression}, trace={trace}"
			);
		}
	}
}

#[test]
fn structured_attribute_floats_give_the_reference_derivation_path() {
	// nlohmann writes `1.5e-07`, `1e+100`, `-0.0` and `1.0` into `__json`;
	// the paths are those the reference printed for these expressions.
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate");
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	let home = tempfile::tempdir().unwrap();
	for (attrs, path) in [
		(
			"w = 1.5e-7;",
			"/nix/store/c091826sbvlczj5q6alikyk4bg9w0592-f.drv",
		),
		(
			"v = 1.0e100; w = 1.5e-7; x = -0.0; y = 1.0; z = { b = 0.1; a = [ 1.5e-5 123456789012345678.0 ]; };",
			"/nix/store/f49mqj0yv6nrgi2gbz0ybzcayhj8xvmq-f.drv",
		),
	] {
		let expression = format!(
			"(derivation {{ name = \"f\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; __structuredAttrs = true; {attrs} }}).drvPath"
		);
		let actual = evaluate(candidate, home.path(), &expression, false);
		assert_eq!(
			String::from_utf8_lossy(&actual.stdout),
			format!("\"{path}\"\n"),
			"{expression}: {actual:?}"
		);
		let expected = evaluate(&reference, home.path(), &expression, false);
		assert_eq!(
			(actual.status.code(), actual.stdout, actual.stderr),
			(expected.status.code(), expected.stdout, expected.stderr),
			"{expression}"
		);
	}
}
