//! CppNix 2.35.2's derivation options: the `derivation-options` JSON
//! fixtures, and the options it derives from the advanced-attributes
//! derivations (`derivation-advanced-attrs.cc`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bstr::BString;
use nix_compat::cppnix_json::derivation::{Derivation as JsonDerivation, Features};
use nix_compat::derivation::{OutputName, UnverifiedDerivation};
use nix_compat::store_path::{StorePath, downstream_placeholder};
use nix_rs_build::options::{
	self, DerivationOptions, DerivedInput, DrvRef, Input, OutputChecksSpec,
};
use serde_json::Value;

const ROOT: &str = "../../tests/nix/src/libstore-tests/data/derivation";

/// Every fixture under `{ia,ca}/derivation-options`, and whether CppNix
/// also reads it as `DerivationOptions<StorePath>` (the IA ones).
const JSON_FIXTURES: &[(&str, bool)] = &[
	("ia/derivation-options/defaults.json", true),
	("ia/derivation-options/all_set.json", true),
	("ia/derivation-options/structuredAttrs_defaults.json", true),
	("ia/derivation-options/structuredAttrs_all_set.json", true),
	("ca/derivation-options/all_set.json", false),
	("ca/derivation-options/structuredAttrs_all_set.json", false),
];

/// Each advanced-attributes derivation, the options fixture CppNix
/// compares its options with, and the features it requires.
const DERIVATIONS: &[(&str, &str, &[&str])] = &[
	(
		"advanced-attributes-defaults",
		"ia/derivation-options/defaults.json",
		&[],
	),
	(
		"advanced-attributes",
		"derivation-options/all_set.json",
		&["rainbow", "uid-range"],
	),
	(
		"advanced-attributes-structured-attrs-defaults",
		"ia/derivation-options/structuredAttrs_defaults.json",
		&[],
	),
	(
		"advanced-attributes-structured-attrs",
		"derivation-options/structuredAttrs_all_set.json",
		&["rainbow", "uid-range"],
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

fn pretty(value: &Value) -> String {
	serde_json::to_string_pretty(value).unwrap() + "\n"
}

fn round_trip<I: Input>(text: &str, value: &Value, name: &str) -> DerivationOptions<I> {
	let options =
		DerivationOptions::<I>::from_json(value).unwrap_or_else(|e| panic!("{name}: {e}"));
	assert_eq!(pretty(&options.to_json()), text, "{name}");
	options
}

#[test]
fn every_derivation_options_fixture_is_listed() {
	let mut found = BTreeSet::new();
	for dir in ["ia/derivation-options", "ca/derivation-options"] {
		for entry in std::fs::read_dir(root().join(dir)).unwrap() {
			found.insert(format!(
				"{dir}/{}",
				entry.unwrap().file_name().to_string_lossy()
			));
		}
	}
	let listed: BTreeSet<String> = JSON_FIXTURES
		.iter()
		.map(|(name, _)| name.to_string())
		.collect();
	assert_eq!(found, listed);
}

#[test]
fn json_fixtures_read_and_write_byte_for_byte() {
	for (name, store_paths) in JSON_FIXTURES {
		let (text, value) = fixture(name);
		let derived: DerivationOptions<DerivedInput> = round_trip(&text, &value, name);
		if *store_paths {
			let resolved: DerivationOptions<StorePath> = round_trip(&text, &value, name);
			// Without built inputs, resolving changes nothing.
			assert_eq!(derived.try_resolve(|_, _| None), Some(resolved), "{name}");
		}
	}
}

fn without_json(env: &BTreeMap<BString, BString>) -> BTreeMap<BString, BString> {
	env.iter()
		.filter(|(k, _)| k.as_slice() != b"__json")
		.map(|(k, v)| (k.clone(), v.clone()))
		.collect()
}

fn check(
	name: &str,
	options: &DerivationOptions<DerivedInput>,
	expected: &str,
	features: BTreeSet<String>,
	known_output_paths: bool,
) {
	let (text, _) = fixture(expected);
	assert_eq!(pretty(&options.to_json()), text, "{name}");
	assert_eq!(
		options.get_required_system_features(known_output_paths),
		features,
		"{name}"
	);
	let all_set = !name.contains("defaults");
	assert_eq!(options.substitutes_allowed(false), !all_set, "{name}");
	assert!(options.substitutes_allowed(true), "{name}");
	assert_eq!(options.use_uid_range(known_output_paths), all_set, "{name}");
}

#[test]
fn input_addressed_derivations_give_the_reference_options() {
	for (stem, expected, features) in DERIVATIONS {
		let name = format!("ia/{stem}.drv");
		let bytes = std::fs::read(root().join(&name)).unwrap();
		let drv = UnverifiedDerivation::from_aterm_bytes(&bytes).unwrap();
		let parsed = options::structured_attrs(drv.environment()).unwrap();
		assert_eq!(
			parsed.is_some(),
			stem.contains("structured-attrs"),
			"{name}"
		);
		let options = DerivationOptions::new(
			&without_json(drv.environment()),
			parsed.as_ref(),
			drv.input_derivations(),
			false,
			&mut Vec::new(),
		)
		.unwrap();
		let expected = if expected.starts_with("ia/") {
			expected.to_string()
		} else {
			format!("ia/{expected}")
		};
		let features = features.iter().map(|s| s.to_string()).collect();
		check(&name, &options, &expected, features, true);
	}
}

/// The CA fixtures have floating outputs, which the ATerm parser does not
/// accept yet, so their inputs come from the equivalent derivation JSON.
#[test]
fn content_addressed_derivations_resolve_placeholders_to_their_outputs() {
	let features = Features {
		ca_derivations: true,
		..Features::default()
	};
	for (stem, expected, required) in DERIVATIONS {
		let name = format!("ca/{stem}.json");
		let (_, value) = fixture(&name);
		let drv = JsonDerivation::from_json(&value, features).unwrap();
		let env = drv
			.env
			.iter()
			.map(|(k, v)| (BString::from(k.as_str()), BString::from(v.as_str())))
			.collect();
		let input_drvs: BTreeMap<StorePath, BTreeSet<OutputName>> = drv
			.input_derivations
			.iter()
			.map(|(path, node)| (path.clone(), node.outputs.clone()))
			.collect();
		let options = DerivationOptions::new(
			&env,
			drv.structured_attrs.as_ref().and_then(Value::as_object),
			&input_drvs,
			true,
			&mut Vec::new(),
		)
		.unwrap();
		let expected = if expected.starts_with("ia/") {
			expected.to_string()
		} else {
			format!("ca/{expected}")
		};
		let mut required: BTreeSet<String> = required.iter().map(|s| s.to_string()).collect();
		required.insert("ca-derivations".to_owned());
		check(&name, &options, &expected, required, false);
	}
}

#[test]
fn placeholders_are_plain_paths_without_ca_derivations() {
	let (_, value) = fixture("ca/advanced-attributes.json");
	let features = Features {
		ca_derivations: true,
		..Features::default()
	};
	let drv = JsonDerivation::from_json(&value, features).unwrap();
	let env: BTreeMap<BString, BString> = drv
		.env
		.iter()
		.map(|(k, v)| (BString::from(k.as_str()), BString::from(v.as_str())))
		.collect();
	let input_drvs = drv
		.input_derivations
		.iter()
		.map(|(path, node)| (path.clone(), node.outputs.clone()))
		.collect();
	let (export, rest): (BTreeMap<_, _>, BTreeMap<_, _>) = env
		.into_iter()
		.partition(|(k, _)| k.as_slice() == b"exportReferencesGraph");
	let options = DerivationOptions::new(&rest, None, &input_drvs, false, &mut Vec::new()).unwrap();
	let OutputChecksSpec::ForAllOutputs(checks) = &options.output_checks else {
		panic!("legacy derivations check all outputs together")
	};
	// A placeholder is not a store path, so CppNix reads it as an output name.
	let foo: StorePath = "j56sf12rxpcv5swr14vsjn5cwm6bj03h-foo.drv".parse().unwrap();
	let placeholder = downstream_placeholder(&foo, b"out");
	assert_eq!(
		checks.allowed_references,
		Some(BTreeSet::from([DrvRef::Output(placeholder.clone())]))
	);
	// exportReferencesGraph needs a store path, so it is an error there.
	let error =
		DerivationOptions::new(&export, None, &input_drvs, false, &mut Vec::new()).unwrap_err();
	assert_eq!(
		error.to_string(),
		format!("path '{placeholder}' is not in the Nix store")
	);
}

#[test]
fn try_resolve_replaces_built_inputs() {
	let (text, value) = fixture("ca/derivation-options/all_set.json");
	let options: DerivationOptions<DerivedInput> = round_trip(&text, &value, "ca all_set");
	assert_eq!(options.try_resolve(|_, _| None), None);
	let built: StorePath = "00000000000000000000000000000000-built".parse().unwrap();
	let resolved = options.try_resolve(|_, _| Some(built.clone())).unwrap();
	let OutputChecksSpec::ForAllOutputs(checks) = &resolved.output_checks else {
		panic!()
	};
	assert_eq!(
		checks.allowed_references,
		Some(BTreeSet::from([DrvRef::Input(built.clone())]))
	);
	assert_eq!(
		resolved.export_references_graph["refs1"],
		BTreeSet::from([built])
	);
}
