//! ATerm derivation bytes, paths and parse errors recorded from CppNix
//! 2.35.2 in `fixtures/derivation-reference/cases.json` (see its
//! `generate.py`), checked against `nix_compat::cppnix_json`.
//!
//! JSON cases went through the reference's `nix derivation add`: the ATerm it
//! wrote must equal `unparse` byte for byte, the store path must match, and
//! parsing the ATerm must give the `nix derivation show` JSON. ATerm cases
//! were added with `nix-store --add`, which parses them: the reference's
//! acceptance, its error message and its experimental-feature gating must
//! match `Derivation::from_aterm`.

use std::collections::BTreeMap;
use std::path::Path;

use nix_compat::cppnix_json::derivation::{Derivation, Features, Output};
use nix_compat::cppnix_json::derivation_aterm::{AtermError, AtermFeatures};
use nix_compat::cppnix_json::derivation_hash::fixed_output_path;
use nix_compat::derivation::OutputName;
use nix_compat::store_path::StorePath;
use serde_json::Value;

const ALL: AtermFeatures = AtermFeatures {
	derivation: Features {
		ca_derivations: true,
		dynamic_derivations: true,
		impure_derivations: true,
		git_hashing: true,
	},
	// The recorded runs leave `blake3-hashes` disabled.
	blake3_hashes: false,
};

/// Cases where the reference's message comes from a library noxi does not
/// reproduce; only the prefix is compared.
const MESSAGE_PREFIX_ONLY: &[(&str, &str)] =
	&[("structured-invalid", "cannot process __json attribute: ")];

fn cases() -> Value {
	let path =
		Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/derivation-reference/cases.json");
	serde_json::from_str(&std::fs::read_to_string(path).expect("cases.json")).expect("JSON")
}

fn str_field<'a>(value: &'a Value, name: &str) -> &'a str {
	value[name]
		.as_str()
		.unwrap_or_else(|| panic!("{name} in {value}"))
}

/// The derivation name of a store path, `Derivation::nameFromPath`.
fn drv_name(path: &str) -> String {
	let path = StorePath::from_absolute_path(path.trim().as_bytes()).expect("store path");
	path.name().strip_suffix(".drv").expect(".drv").to_owned()
}

/// The single derivation in `nix derivation show` output.
fn shown(show: &Value) -> Value {
	assert_eq!(show["exit_code"], 0, "{show}");
	let value: Value = serde_json::from_str(str_field(show, "stdout")).expect("show JSON");
	let derivations = value["derivations"].as_object().expect("derivations");
	assert_eq!(derivations.len(), 1);
	derivations.values().next().unwrap().clone()
}

/// The message of an `error: ...` diagnostic, without the trailing usage
/// hint a `UsageError` adds.
fn message(stderr: &str) -> &str {
	let message = stderr.strip_prefix("error: ").unwrap_or(stderr);
	match message.find("\nTry '") {
		Some(end) => &message[..end],
		None => message.trim_end_matches('\n'),
	}
}

#[test]
fn json_cases_match_reference_bytes_paths_and_json() {
	let cases = cases();
	let mut written: BTreeMap<StorePath, Derivation> = BTreeMap::new();
	let mut checked = 0;
	for case in cases["json_cases"].as_array().unwrap() {
		let name = str_field(case, "name");
		assert_eq!(case["add"]["exit_code"], 0, "{name}: {}", case["add"]);
		let path_text = str_field(&case["add"], "stdout").trim();
		let path = StorePath::from_absolute_path(path_text.as_bytes()).expect("path");
		let aterm = str_field(case, "aterm");

		// JSON to ATerm, as `nix derivation add` does.
		let mut from_json = Derivation::from_json(&case["input"], ALL.derivation).expect(name);
		let read = |p: &StorePath| written.get(p).cloned();
		let mut warnings = Vec::new();
		from_json
			.fill_in_output_paths(&read, &mut warnings)
			.unwrap_or_else(|e| panic!("{name}: {e}"));
		assert_eq!(
			from_json.unparse(false, None).unwrap(),
			aterm,
			"{name}: unparse"
		);
		assert_eq!(from_json.path().unwrap(), path, "{name}: path");

		// ATerm to JSON, as `nix derivation show` does.
		let parsed = Derivation::from_aterm(aterm.as_bytes(), &drv_name(path_text), ALL)
			.unwrap_or_else(|e| panic!("{name}: {e}"));
		assert_eq!(
			parsed.unparse(false, None).unwrap(),
			aterm,
			"{name}: round trip"
		);
		assert_eq!(parsed.to_json(), shown(&case["show"]), "{name}: show JSON");
		assert_eq!(parsed.path().unwrap(), path, "{name}: path from ATerm");

		written.insert(path, parsed);
		checked += 1;
	}
	assert_eq!(checked, 12);
}

#[test]
fn aterm_cases_match_reference_parser() {
	let cases = cases();
	let mut mismatches = Vec::new();
	for case in cases["aterm_cases"].as_array().unwrap() {
		let name = str_field(case, "name");
		let add = &case["add"];
		let stderr = str_field(add, "stderr");
		let ours = Derivation::from_aterm(str_field(case, "aterm").as_bytes(), name, ALL);
		// `nix-store --add` parses the file, then checks its invariants;
		// failing those means the parser accepted it.
		let accepted = add["exit_code"] == 0 || stderr.contains("… while checking derivation");
		let outcome = match (&ours, accepted) {
			(Ok(drv), true) => {
				if case.get("show_all").is_some() && drv.to_json() != shown(&case["show_all"]) {
					Err(format!("show JSON differs: {}", drv.to_json()))
				} else {
					Ok(())
				}
			}
			(Err(e), true) => Err(format!("the reference accepts it, noxi fails: {e}")),
			(Ok(_), false) => Err(format!("the reference fails ({stderr}), noxi accepts it")),
			(Err(e), false) => {
				let expected = message(stderr);
				let actual = if e.is_format_error() {
					let path = stderr
						.strip_prefix("error: error parsing derivation '")
						.and_then(|rest| rest.split_once('\''))
						.map(|(path, _)| path)
						.unwrap_or("<unwrapped>");
					format!("error parsing derivation '{path}': {e}")
				} else {
					e.to_string()
				};
				let prefix = MESSAGE_PREFIX_ONLY
					.iter()
					.find(|(case, _)| *case == name)
					.map(|(_, prefix)| *prefix);
				match prefix {
					Some(prefix) if actual.starts_with(prefix) && expected.starts_with(prefix) => {
						Ok(())
					}
					_ if actual == expected => Ok(()),
					_ => Err(format!(
						"message differs:\n  reference: {expected}\n  noxi:      {actual}"
					)),
				}
			}
		};
		if let Err(e) = outcome {
			mismatches.push(format!("{name}: {e}"));
		}

		// Feature gating: `nix derivation show` with only `nix-command`.
		if let Some(show) = case.get("show_default") {
			let default = Derivation::from_aterm(
				str_field(case, "aterm").as_bytes(),
				name,
				AtermFeatures::default(),
			);
			let expected = (show["exit_code"] != 0).then(|| message(str_field(show, "stderr")));
			let actual = default.as_ref().err().map(AtermError::to_string);
			if expected != actual.as_deref() {
				mismatches.push(format!(
					"{name}: default features: reference {expected:?}, noxi {actual:?}"
				));
			}
		}
	}
	assert!(mismatches.is_empty(), "{mismatches:#?}");
}

/// For fixed outputs the reference only checks that the recorded path is
/// absolute and then reports the path it expects; that path must be the one
/// noxi computes from the parsed content address, whatever encoding the
/// hash used.
#[test]
fn fixed_output_paths_match_reference_invariant_messages() {
	let cases = cases();
	let mut checked = 0;
	for case in cases["aterm_cases"].as_array().unwrap() {
		let name = str_field(case, "name");
		let stderr = str_field(&case["add"], "stderr");
		let Some(expected) = stderr
			.split_once("environment variable 'out', should be '")
			.and_then(|(_, rest)| rest.split_once('\''))
			.map(|(path, _)| path)
		else {
			continue;
		};
		let drv = Derivation::from_aterm(str_field(case, "aterm").as_bytes(), name, ALL)
			.unwrap_or_else(|e| panic!("{name}: {e}"));
		let Output::Fixed(ca) = &drv.outputs[&OutputName::out()] else {
			panic!("{name}: not a fixed output");
		};
		assert_eq!(
			fixed_output_path(name, &OutputName::out(), ca)
				.unwrap()
				.to_absolute_path(),
			expected,
			"{name}"
		);
		checked += 1;
	}
	assert_eq!(checked, 3, "nix32, base-64 and upper-case hex hashes");
}
