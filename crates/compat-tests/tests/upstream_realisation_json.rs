//! Checks `nix_compat::cppnix_json`'s realisation and build-result types
//! against CppNix 2.35.2's fixtures: JSON values and bytes, realisation
//! fingerprints, and ed25519 signatures made with the test key from
//! CppNix's `realisation.cc`.
//!
//! Every file in both fixture directories must appear in [`EXPECTATIONS`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use nix_compat::cppnix_json::{
	BuildOutcome, BuildResult, DrvOutput, Realisation, Signature, UnkeyedRealisation,
};
use nix_compat::narinfo::parse_keypair;
use nix_compat::store_path::StorePath;
use serde_json::Value;

const FIXTURE_DIR: &str = "../../tests/nix/src/libstore-tests/data";
const DIRECTORIES: &[&str] = &["build-result", "realisation"];

/// `testSecretKey` in CppNix's `realisation.cc`.
const TEST_SECRET_KEY: &str = "test-key:tU7tTvLcScf8pmz/eTV0BEtLmRsPpZfKaRcd0nCN+pysBZPHSeg61/u2oc7mIOewfuAY1V1BiX32homTaDJ2Jw==";

#[derive(Debug, Clone, Copy)]
enum Check {
	/// A keyed realisation; `true` if CppNix writes it, not only reads it.
	Realisation(&'static str, bool),
	UnkeyedRealisation(&'static str),
	/// The fingerprint of the named realisation, compared byte for byte.
	Fingerprint(&'static str),
	/// The test key's signature over the named realisation's fingerprint.
	Signature(&'static str),
	BuildResult(&'static str),
}

const EXPECTATIONS: &[(&str, Check)] = &[
	(
		"build-result/not-deterministic.json",
		Check::BuildResult("not-deterministic"),
	),
	(
		"build-result/output-rejected.json",
		Check::BuildResult("output-rejected"),
	),
	("build-result/success.json", Check::BuildResult("success")),
	(
		"realisation/simple-fingerprint.txt",
		Check::Fingerprint("simple"),
	),
	("realisation/simple-sig.json", Check::Signature("simple")),
	(
		"realisation/simple.json",
		Check::Realisation("simple", true),
	),
	(
		"realisation/unkeyed-simple.json",
		Check::UnkeyedRealisation("simple"),
	),
	(
		"realisation/unkeyed-with-signature.json",
		Check::UnkeyedRealisation("with-signature"),
	),
	(
		"realisation/with-signature-fingerprint.txt",
		Check::Fingerprint("with-signature"),
	),
	(
		"realisation/with-signature-sig.json",
		Check::Signature("with-signature"),
	),
	(
		"realisation/with-signature.json",
		Check::Realisation("with-signature", true),
	),
	(
		"realisation/with-signature-unstructured.json",
		Check::Realisation("with-signature", false),
	),
];

fn fixture_root() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR)
}

fn store_path(s: &str) -> StorePath {
	s.parse().unwrap()
}

/// `unkeyedSimple` and `unkeyedWithSignature` in `realisation.cc`.
fn unkeyed(name: &str) -> UnkeyedRealisation {
	match name {
		"simple" => UnkeyedRealisation {
			out_path: store_path("g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo"),
			signatures: BTreeSet::new(),
		},
		"with-signature" => UnkeyedRealisation {
			out_path: store_path("g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo.drv"),
			signatures: BTreeSet::from([Signature {
				key_name: "asdf".into(),
				sig: vec![0; 64],
			}]),
		},
		_ => unreachable!(),
	}
}

/// `testDrvOutput` in `realisation.cc`.
fn test_drv_output() -> DrvOutput {
	DrvOutput {
		drv_path: store_path("g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-bar.drv"),
		output_name: "foo".into(),
	}
}

/// The values `build-result.cc` constructs.
fn build_result(name: &str) -> BuildResult {
	let failure = |status: &str, times_built, start_time, stop_time| BuildResult {
		outcome: BuildOutcome::Failure {
			status: status.into(),
			error_msg: "no idea why".into(),
			is_non_deterministic: false,
		},
		times_built,
		start_time,
		stop_time,
		cpu_user: None,
		cpu_system: None,
	};
	match name {
		"not-deterministic" => failure("NotDeterministic", 1, 0, 0),
		"output-rejected" => failure("OutputRejected", 3, 30, 50),
		"success" => BuildResult {
			outcome: BuildOutcome::Success {
				status: "Built".into(),
				built_outputs: BTreeMap::from(["foo", "bar"].map(|output| {
					(
						output.to_owned(),
						UnkeyedRealisation {
							out_path: store_path(&format!(
								"g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-{output}"
							)),
							signatures: BTreeSet::new(),
						},
					)
				})),
			},
			times_built: 3,
			start_time: 30,
			stop_time: 50,
			cpu_user: Some(500_000_000),
			cpu_system: Some(604_000_000),
		},
		_ => unreachable!(),
	}
}

/// CppNix writes JSON fixtures with `json::dump(2)` and a final newline.
fn render(value: &Value) -> String {
	serde_json::to_string_pretty(value).unwrap() + "\n"
}

#[test]
fn every_fixture_is_classified() {
	let root = fixture_root();
	let mut found = Vec::new();
	for dir in DIRECTORIES {
		for entry in std::fs::read_dir(root.join(dir)).expect("fixture directory") {
			let path = entry.unwrap().path();
			found.push(
				path.strip_prefix(&root)
					.unwrap()
					.to_string_lossy()
					.into_owned(),
			);
		}
	}
	found.sort();
	let mut listed: Vec<String> = EXPECTATIONS.iter().map(|e| e.0.to_owned()).collect();
	listed.sort();
	assert_eq!(
		found, listed,
		"EXPECTATIONS must list exactly the fixtures on disk"
	);
}

#[test]
fn fixtures_match_cppnix() {
	let root = fixture_root();
	let (signing_key, verifying_key) = parse_keypair(TEST_SECRET_KEY).expect("test key");
	let mut mismatches = Vec::new();
	for &(name, check) in EXPECTATIONS {
		let text = std::fs::read_to_string(root.join(name)).expect("fixture");
		let mut expect = |ok: bool, what: &str| {
			if !ok {
				mismatches.push(format!("{name}: {what}"));
			}
		};
		match check {
			Check::Fingerprint(realisation) => {
				expect(
					unkeyed(realisation).fingerprint(&test_drv_output()) == text,
					"fingerprint differs",
				);
			}
			Check::Signature(realisation) => {
				let value: Value = serde_json::from_str(&text).unwrap();
				let fixture = Signature::from_json(&value).expect("signature JSON");
				let realisation = unkeyed(realisation);
				let signed = realisation.sign(&test_drv_output(), &signing_key);
				expect(signed == fixture, "signature differs");
				expect(render(&signed.to_json()) == text, "signature JSON differs");
				expect(
					realisation.check_signature(&test_drv_output(), &verifying_key, &fixture),
					"signature does not verify",
				);
			}
			Check::Realisation(realisation, written) => {
				let value: Value = serde_json::from_str(&text).unwrap();
				let expected = Realisation {
					id: test_drv_output(),
					value: unkeyed(realisation),
				};
				match Realisation::from_json(&value) {
					Ok(got) => {
						expect(got == expected, "decoded value differs");
						expect(
							!written || render(&got.to_json()) == text,
							"re-encoding differs",
						);
					}
					Err(error) => expect(false, &error.to_string()),
				}
			}
			Check::UnkeyedRealisation(realisation) => {
				let value: Value = serde_json::from_str(&text).unwrap();
				match UnkeyedRealisation::from_json(&value) {
					Ok(got) => {
						expect(got == unkeyed(realisation), "decoded value differs");
						expect(render(&got.to_json()) == text, "re-encoding differs");
					}
					Err(error) => expect(false, &error.to_string()),
				}
			}
			Check::BuildResult(result) => {
				let value: Value = serde_json::from_str(&text).unwrap();
				match BuildResult::from_json(&value) {
					Ok(got) => {
						expect(got == build_result(result), "decoded value differs");
						expect(render(&got.to_json()) == text, "re-encoding differs");
					}
					Err(error) => expect(false, &error.to_string()),
				}
			}
		}
	}
	assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn wrong_key_does_not_verify() {
	let (_, verifying_key) = parse_keypair(TEST_SECRET_KEY).unwrap();
	let realisation = unkeyed("simple");
	let forged = Signature {
		key_name: "test-key".into(),
		sig: vec![0; 64],
	};
	assert!(!realisation.check_signature(&test_drv_output(), &verifying_key, &forged));
	let unknown_status = serde_json::json!({
		"timesBuilt": 0, "startTime": 0, "stopTime": 0,
		"success": true, "status": "Built!", "builtOutputs": {},
	});
	assert!(BuildResult::from_json(&unknown_status).is_err());
}
