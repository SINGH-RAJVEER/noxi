//! CppNix 2.35.2 derivation-output and version-4 derivation JSON fixtures.

use std::path::{Path, PathBuf};

use nix_compat::cppnix_json::derivation::{Derivation, Features, Output};
use nix_compat::cppnix_json::{self, Error};
use serde_json::Value;

const ROOT: &str = "../../tests/nix/src/libstore-tests/data/derivation";
const OUTPUTS: &[(&str, &str)] = &[
	("output-inputAddressed.json", "input-addressed"),
	("output-caFixedFlat.json", "fixed-flat"),
	("output-caFixedNAR.json", "fixed-nar"),
	("output-caFixedText.json", "fixed-text"),
	("output-caFloating.json", "floating"),
	("output-deferred.json", "deferred"),
	("output-impure.json", "impure"),
];
const DERIVATIONS: &[(&str, &str, usize, bool)] = &[
	("simple-derivation.json", "simple-derivation", 0, false),
	("dyn-dep-derivation.json", "dyn-dep-derivation", 0, false),
	("ca/self-contained.json", "myname", 1, false),
	(
		"ia/advanced-attributes-defaults.json",
		"advanced-attributes-defaults",
		1,
		false,
	),
	(
		"ia/advanced-attributes.json",
		"advanced-attributes",
		1,
		false,
	),
	(
		"ia/advanced-attributes-structured-attrs-defaults.json",
		"advanced-attributes-structured-attrs-defaults",
		2,
		true,
	),
	(
		"ia/advanced-attributes-structured-attrs.json",
		"advanced-attributes-structured-attrs",
		3,
		true,
	),
	(
		"ca/advanced-attributes-defaults.json",
		"advanced-attributes-defaults",
		1,
		false,
	),
	(
		"ca/advanced-attributes.json",
		"advanced-attributes",
		1,
		false,
	),
	(
		"ca/advanced-attributes-structured-attrs-defaults.json",
		"advanced-attributes-structured-attrs-defaults",
		2,
		true,
	),
	(
		"ca/advanced-attributes-structured-attrs.json",
		"advanced-attributes-structured-attrs",
		3,
		true,
	),
];

fn root() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join(ROOT)
}

fn fixture(name: &str) -> (String, Value) {
	let text = std::fs::read_to_string(root().join(name)).expect("CppNix fixture");
	let value = serde_json::from_str(&text).expect("fixture JSON");
	(text, value)
}

fn all_features() -> Features {
	Features {
		ca_derivations: true,
		dynamic_derivations: true,
		impure_derivations: true,
		git_hashing: true,
	}
}

fn assert_bytes(text: &str, value: &Value, name: &str) {
	assert_eq!(
		serde_json::to_string_pretty(value).unwrap() + "\n",
		text,
		"{name}"
	);
}

#[test]
fn every_derivation_json_fixture_is_listed() {
	let mut found = Vec::new();
	for directory in ["", "ia", "ca"] {
		for entry in std::fs::read_dir(root().join(directory)).expect("fixture directory") {
			let path = entry.expect("entry").path();
			if path
				.extension()
				.is_some_and(|extension| extension == "json")
			{
				found.push(
					path.strip_prefix(root())
						.unwrap()
						.to_string_lossy()
						.into_owned(),
				);
			}
		}
	}
	let mut listed: Vec<_> = OUTPUTS
		.iter()
		.map(|(name, _)| (*name).to_owned())
		.chain(DERIVATIONS.iter().map(|(name, _, _, _)| (*name).to_owned()))
		.collect();
	found.sort();
	listed.sort();
	assert_eq!(found, listed);
}

#[test]
fn output_json_matches_cppnix() {
	for (name, expected) in OUTPUTS {
		let (text, value) = fixture(name);
		let output = Output::from_json(&value, all_features()).expect(name);
		let kind = match &output {
			Output::InputAddressed(_) => "input-addressed",
			Output::Fixed(nix_compat::nixhash::CAHash::Flat(_)) => "fixed-flat",
			Output::Fixed(nix_compat::nixhash::CAHash::Nar(_)) => "fixed-nar",
			Output::Fixed(nix_compat::nixhash::CAHash::Text(_)) => "fixed-text",
			Output::Floating { .. } => "floating",
			Output::Deferred => "deferred",
			Output::Impure { .. } => "impure",
		};
		assert_eq!(kind, *expected, "{name}");
		assert_bytes(&text, &output.to_json(), name);
	}
}

#[test]
fn derivation_json_matches_cppnix() {
	for (name, expected_name, output_count, structured) in DERIVATIONS {
		let (text, value) = fixture(name);
		let derivation = Derivation::from_json(&value, all_features()).expect(name);
		assert_eq!(&derivation.name, expected_name, "{name}");
		assert_eq!(derivation.outputs.len(), *output_count, "{name}");
		assert_eq!(derivation.structured_attrs.is_some(), *structured, "{name}");
		assert_bytes(&text, &derivation.to_json(), name);
	}

	let (_, simple) = fixture("simple-derivation.json");
	let simple = Derivation::from_json(&simple, Features::default()).unwrap();
	assert_eq!(simple.args, ["bar", "baz"]);
	assert_eq!(simple.env.get("BIG_BAD").map(String::as_str), Some("WOLF"));
	assert_eq!(simple.input_sources.len(), 1);
	let dep = simple.input_derivations.values().next().unwrap();
	assert_eq!(dep.outputs.len(), 2);
	assert!(dep.dynamic_outputs.is_empty());

	let (_, dynamic) = fixture("dyn-dep-derivation.json");
	let dynamic = Derivation::from_json(&dynamic, all_features()).unwrap();
	let dep = dynamic.input_derivations.values().next().unwrap();
	assert_eq!(dep.dynamic_outputs.len(), 2);
	assert_eq!(
		dep.dynamic_outputs
			.values()
			.map(|node| node.outputs.len())
			.sum::<usize>(),
		2
	);
}

#[test]
fn feature_gates_and_malformed_json_match_reference_boundary() {
	for (name, feature) in [
		("output-caFixedText.json", cppnix_json::DYNAMIC_DERIVATIONS),
		("output-caFloating.json", "ca-derivations"),
		("output-impure.json", "impure-derivations"),
	] {
		let (_, value) = fixture(name);
		assert_eq!(
			Output::from_json(&value, Features::default()),
			Err(Error::FeatureRequired(feature)),
			"{name}"
		);
	}
	for (name, feature) in [
		("dyn-dep-derivation.json", cppnix_json::DYNAMIC_DERIVATIONS),
		("ca/self-contained.json", "ca-derivations"),
	] {
		let (_, value) = fixture(name);
		assert_eq!(
			Derivation::from_json(&value, Features::default()).unwrap_err(),
			Error::FeatureRequired(feature),
			"{name}"
		);
	}
	assert_eq!(
		Output::from_json(
			&serde_json::json!({"path": "bad", "hash": "bad"}),
			all_features()
		),
		Err(Error::DerivationOutput)
	);
	let (_, mut value) = fixture("simple-derivation.json");
	value["version"] = serde_json::json!(3);
	assert_eq!(
		Derivation::from_json(&value, all_features()).unwrap_err(),
		Error::DerivationVersion(3)
	);
}
