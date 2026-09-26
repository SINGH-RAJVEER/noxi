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
