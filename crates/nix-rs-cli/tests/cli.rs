use std::process::Command;

use tempfile::tempdir;

#[test]
fn evaluates_a_pure_expression() {
	let output = Command::new(env!("CARGO_BIN_EXE_nixrs"))
		.args(["eval", "--expr", "1 + 2"])
		.output()
		.expect("run nixrs");
	assert!(output.status.success());
	assert_eq!(output.stdout, b"3\n");
}

#[test]
fn evaluates_nested_values_before_printing() {
	let output = Command::new(env!("CARGO_BIN_EXE_nixrs"))
		.args(["eval", "--expr", "rec { a = 1; b = [ (a + 1) ]; }"])
		.output()
		.expect("run nixrs");
	assert!(output.status.success());
	assert_eq!(output.stdout, b"{ a = 1; b = [ 2 ]; }\n");
}

#[test]
fn evaluates_a_pure_file() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("answer.nix");
	std::fs::write(&path, "6 * 7").expect("write expression");
	let output = Command::new(env!("CARGO_BIN_EXE_nixrs"))
		.args(["eval", "--file", path.to_str().expect("UTF-8 path")])
		.output()
		.expect("run nixrs");
	assert!(output.status.success());
	assert_eq!(output.stdout, b"42\n");
}

#[test]
fn reports_its_preview_version() {
	let output = Command::new(env!("CARGO_BIN_EXE_nixrs"))
		.arg("--version")
		.output()
		.expect("run nixrs");
	assert_eq!(output.status.code(), Some(0));
	assert_eq!(
		output.stdout,
		format!("nixrs {} (preview)\n", env!("CARGO_PKG_VERSION")).as_bytes()
	);
	assert!(output.stderr.is_empty());
}

#[test]
fn rejects_extra_arguments_without_evaluating() {
	let output = Command::new(env!("CARGO_BIN_EXE_nixrs"))
		.args(["eval", "--expr", "1 + 2", "unexpected"])
		.output()
		.expect("run nixrs");
	assert_eq!(output.status.code(), Some(2));
	assert!(output.stdout.is_empty());
	assert!(output.stderr.starts_with(b"usage:"));
}

#[test]
fn strict_evaluation_reports_errors_in_nested_values() {
	let output = Command::new(env!("CARGO_BIN_EXE_nixrs"))
		.args(["eval", "--expr", "{ answer = missingName; }"])
		.output()
		.expect("run nixrs");
	assert_eq!(output.status.code(), Some(1));
	assert!(output.stdout.is_empty());
	assert!(
		output
			.stderr
			.windows(b"missingName".len())
			.any(|part| part == b"missingName")
	);
}

#[test]
fn rejects_non_utf8_input_file() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("invalid.nix");
	std::fs::write(&path, [0xff]).expect("write invalid UTF-8");
	let output = Command::new(env!("CARGO_BIN_EXE_nixrs"))
		.args(["eval", "--file", path.to_str().expect("UTF-8 path")])
		.output()
		.expect("run nixrs");
	assert_eq!(output.status.code(), Some(1));
	assert!(output.stdout.is_empty());
	assert!(output.stderr.starts_with(b"error: cannot read"));
}

#[test]
fn reports_a_missing_file() {
	let directory = tempdir().expect("temporary directory");
	let path = directory.path().join("missing.nix");
	let output = Command::new(env!("CARGO_BIN_EXE_nixrs"))
		.args(["eval", "--file", path.to_str().expect("UTF-8 path")])
		.output()
		.expect("run nixrs");
	assert_eq!(output.status.code(), Some(1));
	assert!(output.stderr.starts_with(b"error: cannot read"));
}

#[test]
fn reports_evaluation_errors() {
	let output = Command::new(env!("CARGO_BIN_EXE_nixrs"))
		.args(["eval", "--expr", "missingName"])
		.output()
		.expect("run nixrs");
	assert!(!output.status.success());
	assert!(
		output
			.stderr
			.windows(b"missingName".len())
			.any(|part| part == b"missingName")
	);
}

#[test]
fn rejects_unimplemented_commands() {
	let output = Command::new(env!("CARGO_BIN_EXE_nixrs"))
		.args(["build", ".#hello"])
		.output()
		.expect("run nixrs");
	assert_eq!(output.status.code(), Some(2));
	assert!(output.stderr.starts_with(b"usage:"));
}
