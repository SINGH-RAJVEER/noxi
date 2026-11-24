//! Generated counterexamples compared with the pinned CppNix evaluator.
use std::os::unix::process::CommandExt;
use std::{
	path::{Path, PathBuf},
	process::{Command, Output},
};

fn reference() -> PathBuf {
	std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate")
}

fn evaluate(binary: &Path, cwd: &Path, expression: &str, trace: bool) -> Output {
	let mut command = Command::new(binary);
	command.arg0("nix-instantiate");
	command
		.env_clear()
		.env("HOME", cwd)
		.env("NIX_CONF_DIR", cwd)
		.env("NIX_USER_CONF_FILES", "")
		.env("NIX_CONFIG", "")
		.env("NIX_PATH", "")
		.env("PATH", std::env::var_os("PATH").unwrap())
		.current_dir(cwd)
		.args([
			"--store", "dummy://", "--eval", "--strict", "--expr", expression,
		]);
	if trace {
		command.arg("--show-trace");
	}
	command.output().unwrap()
}

fn same(cwd: &Path, expression: &str, trace: bool) {
	let expected = evaluate(&reference(), cwd, expression, trace);
	let actual = evaluate(
		Path::new(env!("CARGO_BIN_EXE_nix-instantiate")),
		cwd,
		expression,
		trace,
	);
	assert!(
		matches!(expected.status.code(), Some(0 | 1)),
		"reference {expression}: {expected:?}"
	);
	assert_eq!(
		(actual.status.code(), actual.stdout, actual.stderr),
		(expected.status.code(), expected.stdout, expected.stderr),
		"{expression}, trace={trace}"
	);
}

#[test]
fn nullable_repetition_captures_match_the_reference() {
	let dir = noxi_test_support::tempdir().unwrap();
	for pattern in [
		"(a*)*",
		"(a*)+",
		"(a?)*",
		"((a)*)*",
		"((a*)b?)*",
		"(a*){0,2}",
		"(a*)*a",
		"(a*|b)*",
	] {
		for text in ["", "a", "aa", "ab", "aba", "baab"] {
			for builtin in ["match", "split"] {
				same(
					dir.path(),
					&format!(
						"builtins.{builtin} {} {}",
						serde_json::json!(pattern),
						serde_json::json!(text)
					),
					false,
				);
			}
		}
	}
}

#[test]
fn json_numbers_and_data_parser_errors_match_the_reference() {
	let dir = noxi_test_support::tempdir().unwrap();
	for input in [
		"-0",
		"-0.0",
		"[-0,-0.0]",
		"{\"a\":-0}",
		"1e309",
		"-1e309",
		"\"\\ud800\"",
		"\"\\udc00\"",
		"01",
		"+1",
		"NaN",
		"Infinity",
		"true false",
		"[1,]",
		"{\"a\":}",
		"{",
		"[",
		"\"",
		"",
		" ",
	] {
		for trace in [false, true] {
			same(
				dir.path(),
				&format!("builtins.fromJSON {}", serde_json::json!(input)),
				trace,
			);
		}
	}
	for expression in [
		"builtins.isInt (builtins.fromJSON \"-0\")",
		"builtins.toJSON (builtins.fromJSON \"-0\")",
		"builtins.isFloat (builtins.fromJSON \"-0.0\")",
	] {
		same(dir.path(), expression, false);
	}
	for input in [
		"a = nan",
		"a = -nan",
		"a = \"\\ud800\"",
		"a = 1\na = 2",
		"[a]\n[a]",
		"a.b = 1\na = 2",
		"a = [",
		"a =",
		"a = true false",
	] {
		for trace in [false, true] {
			same(
				dir.path(),
				&format!("builtins.fromTOML {}", serde_json::json!(input)),
				trace,
			);
		}
	}
}

#[test]
fn invalid_byte_regex_diagnostics_preserve_pattern_bytes() {
	let dir = noxi_test_support::tempdir().unwrap();
	for (index, pattern) in [b"\xff[".as_slice(), b"[\xff", b"(\xff", b"\xff)"]
		.into_iter()
		.enumerate()
	{
		let file = dir.path().join(format!("pattern-{index}"));
		std::fs::write(&file, pattern).unwrap();
		for builtin in ["match", "split"] {
			same(
				dir.path(),
				&format!(
					"builtins.{builtin} (builtins.readFile {}) \"x\"",
					serde_json::json!(file)
				),
				true,
			);
		}
	}
}

#[test]
fn regex_compilation_precedes_subject_forcing() {
	let dir = noxi_test_support::tempdir().unwrap();
	let subject = dir.path().join("subject");
	std::fs::write(&subject, b"\0\xff").unwrap();
	for builtin in ["match", "split"] {
		for pattern in ["[", "(a)"] {
			for argument in [
				"throw \"unused subject\"".to_owned(),
				"{}".to_owned(),
				format!("builtins.readFile {}", serde_json::json!(subject)),
			] {
				let expression = format!(
					"builtins.{builtin} {} ({argument})",
					serde_json::json!(pattern)
				);
				for trace in [false, true] {
					same(dir.path(), &expression, trace);
				}
			}
		}
	}
}
