//! Checks `nix_compat::cppnix_json` against CppNix 2.35.2's JSON fixtures for
//! store paths, content addresses, output specifications and deriving paths.
//!
//! Every file in these fixture directories must appear in [`EXPECTATIONS`].
//! Each fixture is decoded, compared with the value CppNix's unit test
//! constructs where that value is not obvious from the file, and encoded again
//! byte for byte.

use std::path::{Path, PathBuf};

use nix_compat::cppnix_json::{self, DYNAMIC_DERIVATIONS, Error};
use nix_compat::derived_path::{DerivedPath, OutputSpec};
use nix_compat::nixhash::{CAHash, NixHash};
use serde_json::Value;
use sha2::{Digest, Sha256};

const FIXTURE_DIR: &str = "../../tests/nix/src/libstore-tests/data";

#[derive(Debug, Clone, Copy)]
enum Kind {
	StorePath,
	ContentAddress,
	OutputsSpec,
	ExtendedOutputsSpec,
	SingleDerivedPath,
	DerivedPath,
}

#[derive(Debug, Clone, Copy)]
enum Expect {
	/// Decodes and encodes back to the same bytes.
	RoundTrip,
	/// CppNix accepts this only with the named experimental feature, so under
	/// default settings both reject it. nix-rs does not implement the feature
	/// yet, which is a gap for matrix rows that enable it.
	Experimental(&'static str),
}

use Expect::*;
use Kind::*;

const EXPECTATIONS: &[(&str, Kind, Expect)] = &[
	("content-address/nar.json", ContentAddress, RoundTrip),
	("content-address/text.json", ContentAddress, RoundTrip),
	(
		"derived-path/multi_built_built.json",
		DerivedPath,
		Experimental(DYNAMIC_DERIVATIONS),
	),
	(
		"derived-path/multi_built_built_wildcard.json",
		DerivedPath,
		Experimental(DYNAMIC_DERIVATIONS),
	),
	("derived-path/multi_opaque.json", DerivedPath, RoundTrip),
	("derived-path/mutli_built.json", DerivedPath, RoundTrip),
	(
		"derived-path/single_built_built.json",
		SingleDerivedPath,
		Experimental(DYNAMIC_DERIVATIONS),
	),
	(
		"derived-path/single_built.json",
		SingleDerivedPath,
		RoundTrip,
	),
	(
		"derived-path/single_opaque.json",
		SingleDerivedPath,
		RoundTrip,
	),
	("outputs-spec/all.json", OutputsSpec, RoundTrip),
	(
		"outputs-spec/extended/all.json",
		ExtendedOutputsSpec,
		RoundTrip,
	),
	(
		"outputs-spec/extended/def.json",
		ExtendedOutputsSpec,
		RoundTrip,
	),
	(
		"outputs-spec/extended/name.json",
		ExtendedOutputsSpec,
		RoundTrip,
	),
	(
		"outputs-spec/extended/names.json",
		ExtendedOutputsSpec,
		RoundTrip,
	),
	("outputs-spec/name.json", OutputsSpec, RoundTrip),
	("outputs-spec/names.json", OutputsSpec, RoundTrip),
	("store-path/simple.json", StorePath, RoundTrip),
];

const DIRECTORIES: &[&str] = &[
	"content-address",
	"derived-path",
	"outputs-spec",
	"store-path",
];

fn fixture_root() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR)
}

fn collect(dir: &Path, root: &Path, out: &mut Vec<String>) {
	for entry in std::fs::read_dir(dir).expect("fixture directory") {
		let path = entry.expect("directory entry").path();
		if path.is_dir() {
			collect(&path, root, out);
		} else {
			out.push(
				path.strip_prefix(root)
					.unwrap()
					.to_string_lossy()
					.into_owned(),
			);
		}
	}
}

/// Decodes a fixture and encodes it again.
fn round_trip(kind: Kind, value: &Value) -> Result<Value, Error> {
	Ok(match kind {
		StorePath => cppnix_json::store_path_to_json(&cppnix_json::store_path_from_json(value)?),
		ContentAddress => {
			cppnix_json::content_address_to_json(&cppnix_json::content_address_from_json(value)?)
		}
		OutputsSpec => {
			cppnix_json::outputs_spec_to_json(&cppnix_json::outputs_spec_from_json(value)?)
		}
		ExtendedOutputsSpec => cppnix_json::extended_outputs_spec_to_json(
			cppnix_json::extended_outputs_spec_from_json(value)?.as_ref(),
		),
		SingleDerivedPath => {
			let (path, output) = cppnix_json::single_derived_path_from_json(value)?;
			cppnix_json::single_derived_path_to_json(&path, output.as_ref())
		}
		DerivedPath => {
			cppnix_json::derived_path_to_json(&cppnix_json::derived_path_from_json(value)?)
		}
	})
}

/// CppNix writes these fixtures with `json::dump(2)` and a final newline.
fn render(value: &Value) -> String {
	serde_json::to_string_pretty(value).unwrap() + "\n"
}

#[test]
fn every_fixture_is_classified() {
	let root = fixture_root();
	let mut found = Vec::new();
	for dir in DIRECTORIES {
		collect(&root.join(dir), &root, &mut found);
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
fn fixtures_match_expectations() {
	let root = fixture_root();
	let mut mismatches = Vec::new();
	for (name, kind, expect) in EXPECTATIONS {
		let text = std::fs::read_to_string(root.join(name)).expect("fixture");
		let value: Value = serde_json::from_str(&text).expect("fixture is JSON");
		match (expect, round_trip(*kind, &value)) {
			(RoundTrip, Ok(encoded)) if render(&encoded) == text => {}
			(RoundTrip, Ok(encoded)) => {
				mismatches.push(format!("{name}: re-encoded as\n{}", render(&encoded)))
			}
			(RoundTrip, Err(error)) => mismatches.push(format!("{name}: {error}")),
			(Experimental(feature), Err(Error::Unsupported(got))) if got == *feature => {}
			(Experimental(feature), other) => {
				mismatches.push(format!("{name}: expected to need {feature}, got {other:?}"))
			}
		}
	}
	assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

fn read(name: &str) -> Value {
	serde_json::from_str(&std::fs::read_to_string(fixture_root().join(name)).unwrap()).unwrap()
}

/// The values CppNix's `content-address.cc` and `derived-path.cc` construct.
#[test]
fn decoded_values_match_cppnix_tests() {
	let sha256 = |s: &str| Sha256::digest(s.as_bytes()).into();
	assert_eq!(
		cppnix_json::content_address_from_json(&read("content-address/text.json")),
		Ok(CAHash::Text(sha256("asdf")))
	);
	assert_eq!(
		cppnix_json::content_address_from_json(&read("content-address/nar.json")),
		Ok(CAHash::Nar(NixHash::Sha256(sha256("qwer"))))
	);

	let drv: nix_compat::store_path::StorePath =
		"g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo.drv".parse().unwrap();
	assert_eq!(
		cppnix_json::derived_path_from_json(&read("derived-path/mutli_built.json")),
		Ok(DerivedPath::Built {
			drv_path: drv.clone(),
			outputs: "bar,baz".parse().unwrap(),
		})
	);
	assert_eq!(
		cppnix_json::single_derived_path_from_json(&read("derived-path/single_built.json")),
		Ok((drv.clone(), Some("bar".parse().unwrap())))
	);
	assert_eq!(
		cppnix_json::extended_outputs_spec_from_json(&read("outputs-spec/extended/def.json")),
		Ok(None)
	);
	assert_eq!(
		cppnix_json::outputs_spec_from_json(&read("outputs-spec/all.json")),
		Ok(OutputSpec::All)
	);
}
