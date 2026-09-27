//! Compare a small pure-evaluation corpus with a selected CppNix binary.
//! This is development evidence, not the complete upstream language suite.

use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode, Output};

use serde_json::{Value, json};

const CASES: &[(&str, &str)] = &[
	("integer", "1 + 2 * 3"),
	("boolean", "true && !false"),
	("string", "\"hello\" + \" world\""),
	("list", "[ 1 2 3 ]"),
	("attributes", "{ a = 1; b = [ 2 3 ]; }"),
	("inherit", "let x = 42; in { inherit x; }"),
	("function", "(x: x + 1) 41"),
	("lazy-branch", "if true then 1 else (1 / 0)"),
	("recursive-attributes", "rec { a = 1; b = a + 1; }"),
	("builtin", "builtins.length [ 1 2 3 ]"),
];

fn run(program: &Path, args: &[&str], home: &Path) -> Result<Output, String> {
	Command::new(program)
		.args(args)
		.env("HOME", home)
		.env("NIX_PATH", "")
		.env("NIX_CONFIG", "")
		.output()
		.map_err(|error| format!("cannot run {}: {error}", program.display()))
}

fn captured(output: &Output) -> Value {
	json!({
		"exit_code": output.status.code(),
		"stdout": String::from_utf8_lossy(&output.stdout),
		"stderr": String::from_utf8_lossy(&output.stderr),
	})
}

fn execute() -> Result<bool, String> {
	let mut args = env::args().skip(1);
	let (Some(reference), Some(candidate), Some(report), None) =
		(args.next(), args.next(), args.next(), args.next())
	else {
		return Err("usage: differential-eval REFERENCE_NIX_INSTANTIATE NIXRS REPORT.json".into());
	};
	let reference = Path::new(&reference);
	let candidate = Path::new(&candidate);
	let home = tempfile::tempdir().map_err(|error| format!("temporary HOME: {error}"))?;
	let reference_version = run(reference, &["--version"], home.path())?;
	let candidate_version = run(candidate, &["--version"], home.path())?;
	if !reference_version.status.success() || !candidate_version.status.success() {
		return Err("cannot read reference or candidate version".into());
	}

	let mut all_match = true;
	let mut cases = Vec::with_capacity(CASES.len());
	for (name, expression) in CASES {
		let reference_output = run(
			reference,
			&["--eval", "--strict", "--expr", expression],
			home.path(),
		)?;
		let candidate_output = run(candidate, &["eval", "--expr", expression], home.path())?;
		let matches = reference_output.status.code() == candidate_output.status.code()
			&& reference_output.stdout == candidate_output.stdout
			&& reference_output.stderr == candidate_output.stderr;
		all_match &= matches;
		cases.push(json!({
			"name": name,
			"expression": expression,
			"matches": matches,
			"reference": captured(&reference_output),
			"candidate": captured(&candidate_output),
		}));
	}
	let report_json = json!({
		"platform": env::consts::ARCH.to_owned() + "-" + env::consts::OS,
		"reference": {"path": reference, "version": String::from_utf8_lossy(&reference_version.stdout).trim()},
		"candidate": {"path": candidate, "version": String::from_utf8_lossy(&candidate_version.stdout).trim()},
		"reference_argv": ["--eval", "--strict", "--expr", "EXPRESSION"],
		"candidate_argv": ["eval", "--expr", "EXPRESSION"],
		"environment": {"HOME": "isolated temporary directory", "NIX_PATH": "", "NIX_CONFIG": ""},
		"all_match": all_match,
		"cases": cases,
	});
	let serialized = serde_json::to_vec_pretty(&report_json)
		.map_err(|error| format!("serialize report: {error}"))?;
	fs::write(&report, serialized).map_err(|error| format!("write {report}: {error}"))?;
	println!(
		"{}: {}/{} cases match",
		report,
		cases.iter().filter(|case| case["matches"] == true).count(),
		cases.len()
	);
	Ok(all_match)
}

fn main() -> ExitCode {
	match execute() {
		Ok(true) => ExitCode::SUCCESS,
		Ok(false) => ExitCode::FAILURE,
		Err(error) => {
			eprintln!("error: {error}");
			ExitCode::from(2)
		}
	}
}
