//! Checks the first-party comparison runner independently of the copied Nix fixtures.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::tempdir;

fn executable(path: &Path, script: &str) {
	fs::write(path, format!("#!/bin/sh\nset -eu\n{script}\n")).expect("write mock executable");
	fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("make mock executable");
}

const REFERENCE: &str = r#"
if [ "$1" = '--version' ]; then
	printf 'reference 2.35.2\n'
	exit 0
fi
[ "$#" -eq 4 ] && [ "$1" = '--eval' ] && [ "$2" = '--strict' ] && [ "$3" = '--expr' ]
printf '%s|%s|%s|%s\n' "$4" "$HOME" "$NIX_PATH" "$NIX_CONFIG"
"#;

const CANDIDATE: &str = r#"
if [ "$1" = '--version' ]; then
	printf 'candidate preview\n'
	exit 0
fi
[ "$#" -eq 3 ] && [ "$1" = 'eval' ] && [ "$2" = '--expr' ]
printf '%s|%s|%s|%s\n' "$3" "$HOME" "$NIX_PATH" "$NIX_CONFIG"
"#;

fn run(reference: &Path, candidate: &Path, report: &Path) -> Output {
	Command::new(env!("CARGO_BIN_EXE_differential-eval"))
		.args([reference, candidate, report])
		.env("HOME", "/unisolated-home")
		.env("NIX_PATH", "unisolated-path")
		.env("NIX_CONFIG", "unisolated-config")
		.env("NIX_REMOTE", "ssh://unisolated-store")
		.output()
		.expect("run differential evaluator")
}

fn read_report(path: &Path) -> Value {
	serde_json::from_slice(&fs::read(path).expect("read report")).expect("parse report")
}

#[test]
fn matching_engines_produce_a_complete_isolated_report() {
	let dir = tempdir().expect("temporary directory");
	let reference = dir.path().join("reference");
	let candidate = dir.path().join("candidate");
	let report = dir.path().join("report.json");
	executable(&reference, REFERENCE);
	executable(&candidate, CANDIDATE);

	let output = run(&reference, &candidate, &report);
	assert_eq!(output.status.code(), Some(0), "{output:?}");
	let json = read_report(&report);
	assert_eq!(json["all_match"], true);
	assert_eq!(json["timeout_ms"], 20000);
	assert_eq!(json["cases"].as_array().unwrap().len(), 10);
	assert_eq!(json["reference"]["version"], "reference 2.35.2");
	assert_eq!(json["candidate"]["version"], "candidate preview");
	assert_eq!(
		json["reference_argv"],
		serde_json::json!(["--eval", "--strict", "--expr", "EXPRESSION"])
	);
	assert_eq!(
		json["candidate_argv"],
		serde_json::json!(["eval", "--expr", "EXPRESSION"])
	);
	for case in json["cases"].as_array().unwrap() {
		assert_eq!(case["matches"], true, "{case}");
		let stdout = case["reference"]["stdout"].as_str().unwrap();
		let fields: Vec<_> = stdout.trim_end().split('|').collect();
		assert_eq!(fields[0], case["expression"].as_str().unwrap());
		assert_ne!(fields[1], "/unisolated-home");
		assert!(Path::new(fields[1]).is_absolute());
		assert_eq!(&fields[2..], &["", ""]);
	}
}

#[test]
fn reference_symlink_keeps_its_legacy_command_name() {
	let dir = tempdir().expect("temporary directory");
	let target = dir.path().join("nix");
	let reference = dir.path().join("nix-instantiate");
	let candidate = dir.path().join("candidate");
	let report = dir.path().join("report.json");
	executable(
		&target,
		"if [ \"$1\" = '--version' ]; then printf 'version\\n'; else printf '%s\\n' \"${0##*/}\"; fi",
	);
	symlink(&target, &reference).expect("create legacy command symlink");
	executable(
		&candidate,
		"if [ \"$1\" = '--version' ]; then printf 'version\\n'; else printf 'nix-instantiate\\n'; fi",
	);
	let output = run(&reference, &candidate, &report);
	let json = read_report(&report);
	assert_eq!(output.status.code(), Some(0), "{output:?}\n{json}");
	assert_eq!(json["cases"][0]["reference"]["stdout"], "nix-instantiate\n");
}

#[test]
fn inherited_nix_store_configuration_is_replaced_for_both_engines() {
	let dir = tempdir().expect("temporary directory");
	let reference = dir.path().join("reference");
	let candidate = dir.path().join("candidate");
	let report = dir.path().join("report.json");
	let script = "if [ \"$1\" = '--version' ]; then printf 'version\\n'; else printf '%s\\n' \"${NIX_REMOTE-unset}\"; fi";
	executable(&reference, script);
	executable(&candidate, script);
	let output = run(&reference, &candidate, &report);
	assert_eq!(output.status.code(), Some(0), "{output:?}");
	let json = read_report(&report);
	assert_eq!(json["cases"][0]["reference"]["stdout"], "dummy://\n");
	assert_eq!(json["cases"][0]["candidate"]["stdout"], "dummy://\n");
}

#[test]
fn timed_out_case_is_recorded_as_a_mismatch() {
	let dir = tempdir().expect("temporary directory");
	let reference = dir.path().join("reference");
	let candidate = dir.path().join("candidate");
	let report = dir.path().join("report.json");
	executable(&reference, REFERENCE);
	executable(
		&candidate,
		&format!("case \"${{3-}}\" in\n\t'1 + 2 * 3') while :; do :; done ;;\nesac\n{CANDIDATE}"),
	);
	let output = Command::new(env!("CARGO_BIN_EXE_differential-eval"))
		.args([&reference, &candidate, &report])
		.env("NOXI_DIFF_TIMEOUT_MS", "100")
		.output()
		.expect("run differential evaluator");
	assert_eq!(output.status.code(), Some(1), "{output:?}");
	let json = read_report(&report);
	assert_eq!(json["timeout_ms"], 100);
	assert_eq!(json["cases"][0]["candidate"]["timed_out"], true);
	assert_eq!(json["cases"][0]["matches"], false);
	assert_eq!(json["cases"][1]["matches"], true);
}

#[test]
fn a_single_difference_fails_and_is_recorded() {
	let dir = tempdir().expect("temporary directory");
	let reference = dir.path().join("reference");
	let candidate = dir.path().join("candidate");
	let report = dir.path().join("report.json");
	executable(&reference, REFERENCE);
	executable(
		&candidate,
		&format!(
			"case \"${{3-}}\" in\n\t'1 + 2 * 3') printf 'different\\n'; exit 1 ;;\nesac\n{CANDIDATE}"
		),
	);

	let output = run(&reference, &candidate, &report);
	assert_eq!(output.status.code(), Some(1), "{output:?}");
	assert!(String::from_utf8_lossy(&output.stdout).contains("9/10 cases match"));
	let json = read_report(&report);
	assert_eq!(json["all_match"], false);
	let mismatches: Vec<_> = json["cases"]
		.as_array()
		.unwrap()
		.iter()
		.filter(|case| case["matches"] == false)
		.collect();
	assert_eq!(mismatches.len(), 1);
	assert_eq!(mismatches[0]["name"], "integer");
	assert_eq!(mismatches[0]["reference"]["exit_code"], 0);
	assert_eq!(mismatches[0]["candidate"]["exit_code"], 1);
	assert_eq!(mismatches[0]["candidate"]["stdout"], "different\n");
}

#[test]
fn stderr_alone_is_a_mismatch() {
	let dir = tempdir().expect("temporary directory");
	let reference = dir.path().join("reference");
	let candidate = dir.path().join("candidate");
	let report = dir.path().join("report.json");
	executable(&reference, REFERENCE);
	executable(
		&candidate,
		&format!(
			"case \"${{3-}}\" in\n\t'true && !false') printf 'warning\\n' >&2 ;;\nesac\n{CANDIDATE}"
		),
	);

	let output = run(&reference, &candidate, &report);
	assert_eq!(output.status.code(), Some(1), "{output:?}");
	let json = read_report(&report);
	let case = json["cases"]
		.as_array()
		.unwrap()
		.iter()
		.find(|case| case["name"] == "boolean")
		.unwrap();
	assert_eq!(case["matches"], false);
	assert_eq!(case["reference"]["stdout"], case["candidate"]["stdout"]);
	assert_eq!(
		case["reference"]["exit_code"],
		case["candidate"]["exit_code"]
	);
	assert_eq!(case["candidate"]["stderr"], "warning\n");
}

#[test]
fn crashes_with_equal_output_are_a_mismatch() {
	let dir = tempdir().expect("temporary directory");
	let reference = dir.path().join("reference");
	let candidate = dir.path().join("candidate");
	let report = dir.path().join("report.json");
	// Both print the same invalid UTF-8, then die by different signals.
	let crash = |expression_arg, signal| {
		format!(
			"case \"${{{expression_arg}-}}\" in\n\t'1 + 2 * 3') printf '\\377\\n'; kill -{signal} $$ ;;\nesac\n"
		)
	};
	executable(&reference, &format!("{}{REFERENCE}", crash(4, "ABRT")));
	executable(&candidate, &format!("{}{CANDIDATE}", crash(3, "TERM")));

	let output = run(&reference, &candidate, &report);
	assert_eq!(output.status.code(), Some(1), "{output:?}");
	let json = read_report(&report);
	let case = &json["cases"][0];
	assert_eq!(case["matches"], false);
	assert_eq!(case["reference"]["stdout"], case["candidate"]["stdout"]);
	assert_eq!(case["reference"]["exit_code"], Value::Null);
	assert_eq!(case["reference"]["signal"], 6);
	assert_eq!(case["candidate"]["signal"], 15);
	assert_eq!(case["candidate"]["stdout_hex"], "ff0a");
	assert_eq!(case["candidate"]["exit_code"], Value::Null);
	assert_eq!(json["cases"][1]["reference"]["signal"], Value::Null);
}

#[test]
fn report_identifies_the_compared_binaries() {
	let dir = tempdir().expect("temporary directory");
	let reference = dir.path().join("reference");
	let candidate = dir.path().join("candidate");
	let report = dir.path().join("report.json");
	executable(&reference, REFERENCE);
	executable(&candidate, CANDIDATE);

	let output = run(&reference, &candidate, &report);
	assert_eq!(output.status.code(), Some(0), "{output:?}");
	let json = read_report(&report);
	let digest = |path: &Path| {
		let digest = Sha256::digest(fs::read(path).unwrap());
		digest
			.iter()
			.map(|byte| format!("{byte:02x}"))
			.collect::<String>()
	};
	assert_eq!(json["reference"]["sha256"], digest(&reference));
	assert_eq!(json["candidate"]["sha256"], digest(&candidate));
	assert_ne!(json["reference"]["sha256"], json["candidate"]["sha256"]);
}

#[test]
fn version_failure_does_not_write_a_partial_report() {
	let dir = tempdir().expect("temporary directory");
	let reference = dir.path().join("reference");
	let candidate = dir.path().join("candidate");
	let report = dir.path().join("report.json");
	executable(&reference, "exit 1");
	executable(&candidate, CANDIDATE);

	let output = run(&reference, &candidate, &report);
	assert_eq!(output.status.code(), Some(2));
	assert!(
		String::from_utf8_lossy(&output.stderr)
			.contains("cannot read reference or candidate version")
	);
	assert!(!report.exists());
}
