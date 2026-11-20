//! Parses every ATerm derivation fixture shipped with CppNix and checks
//! `nix-compat` against it.
//!
//! Every `.drv` file under the fixture directory must appear in [`EXPECTATIONS`].
//! A fixture whose behaviour changes (a gap gets fixed, or a round trip breaks)
//! fails this test, so the table always describes the current state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nix_compat::cppnix_json::derivation::{Derivation as FullDerivation, Features};
use nix_compat::derivation::{Derivation, UnverifiedDerivation, lookup_fn};

const FIXTURE_DIR: &str = "../../tests/nix/src/libstore-tests/data/derivation";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
	/// Parses, and serialising it again reproduces the file byte for byte.
	RoundTrip,
	/// Parses, but the file is not in canonical order. Serialising it again
	/// sorts it, as CppNix does, and that canonical form round-trips.
	Canonicalises,
	/// CppNix only parses this with the named experimental feature enabled, so
	/// under default settings both reject it. `UnverifiedDerivation` cannot hold
	/// it; `cppnix_json::derivation::Derivation::from_aterm` reads it when the
	/// feature is enabled and round-trips it byte for byte.
	Experimental(&'static str),
	/// Invalid input that CppNix also rejects while parsing.
	Reject,
}

use Expect::*;

const CA_FLOATING: &str = "ca-derivations";
const DYN_DRV: &str = "dynamic-derivations";

const EXPECTATIONS: &[(&str, Expect)] = &[
	("bad-old-version-dyn-deps.drv", Reject),
	("bad-version.drv", Reject),
	(
		"ca/advanced-attributes-defaults.drv",
		Experimental(CA_FLOATING),
	),
	("ca/advanced-attributes.drv", Experimental(CA_FLOATING)),
	(
		"ca/advanced-attributes-structured-attrs-defaults.drv",
		Experimental(CA_FLOATING),
	),
	(
		"ca/advanced-attributes-structured-attrs.drv",
		Experimental(CA_FLOATING),
	),
	("dyn-dep-derivation.drv", Experimental(DYN_DRV)),
	// The anonymised fixtures list input derivations that share a digest out
	// of path order.
	("firefox.drv", Canonicalises),
	("hello.drv", Canonicalises),
	("ia/advanced-attributes-defaults.drv", RoundTrip),
	("ia/advanced-attributes.drv", RoundTrip),
	(
		"ia/advanced-attributes-structured-attrs-defaults.drv",
		RoundTrip,
	),
	("ia/advanced-attributes-structured-attrs.drv", RoundTrip),
	("simple-derivation.drv", RoundTrip),
];

fn fixture_root() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR)
}

fn collect_drvs(dir: &Path, root: &Path, out: &mut Vec<String>) {
	for entry in std::fs::read_dir(dir).expect("fixture directory must be readable") {
		let path = entry.expect("directory entry").path();
		if path.is_dir() {
			collect_drvs(&path, root, out);
		} else if path.extension().is_some_and(|e| e == "drv") {
			out.push(
				path.strip_prefix(root)
					.unwrap()
					.to_string_lossy()
					.into_owned(),
			);
		}
	}
}

#[test]
fn every_fixture_is_classified() {
	let root = fixture_root();
	let mut found = Vec::new();
	collect_drvs(&root, &root, &mut found);
	found.sort();

	let mut listed: Vec<String> = EXPECTATIONS.iter().map(|(n, _)| n.to_string()).collect();
	listed.sort();

	assert_eq!(
		found, listed,
		"EXPECTATIONS must list exactly the .drv fixtures on disk"
	);
}

#[test]
fn derivations_match_expectations() {
	let root = fixture_root();
	let mut mismatches = BTreeMap::new();

	for (name, expect) in EXPECTATIONS {
		let bytes = std::fs::read(root.join(name)).expect("fixture must be readable");
		let parsed = UnverifiedDerivation::from_aterm_bytes(&bytes);

		let actual = match &parsed {
			Ok(drv) if drv.to_aterm_bytes() == bytes => "round-trips",
			Ok(drv) => {
				let canonical = drv.to_aterm_bytes();
				match UnverifiedDerivation::from_aterm_bytes(&canonical) {
					Ok(again) if again == *drv && again.to_aterm_bytes() == canonical => {
						"canonicalises"
					}
					_ => "parses but its canonical form does not round-trip",
				}
			}
			Err(_) => "fails to parse",
		};

		let ok = match expect {
			RoundTrip => actual == "round-trips",
			Canonicalises => actual == "canonicalises",
			Experimental(_) | Reject => parsed.is_err(),
		};

		if !ok {
			mismatches.insert(*name, format!("expected {expect:?}, but it {actual}"));
		}
	}

	assert!(
		mismatches.is_empty(),
		"fixtures changed behaviour: {mismatches:#?}"
	);
}

/// CppNix parses a derivation without outputs, but rejects it as soon as its
/// type is determined ("must have at least one output"), which verification does.
#[test]
fn zero_output_derivation_fails_verification() {
	let bytes = std::fs::read(fixture_root().join("simple-derivation.drv")).expect("fixture");
	let err = Derivation::from_aterm_bytes("simple-derivation", lookup_fn(|_| None), &bytes)
		.expect_err("a derivation without outputs must not verify");
	assert!(
		format!("{err:?}").contains("NoOutputs"),
		"unexpected error: {err:?}"
	);
}

const ALL_FEATURES: Features = Features {
	ca_derivations: true,
	dynamic_derivations: true,
	impure_derivations: true,
	git_hashing: true,
};

/// The full derivation model reads every fixture CppNix accepts, including
/// the experimental ones once their feature is enabled, and writes the
/// same bytes `UnverifiedDerivation` does wherever both can read the file.
#[test]
fn full_model_matches_fixtures_and_basic_parser() {
	let root = fixture_root();
	let mut mismatches = BTreeMap::new();
	for (name, expect) in EXPECTATIONS {
		let bytes = std::fs::read(root.join(name)).expect("fixture must be readable");
		let drv_name = Path::new(name)
			.file_stem()
			.unwrap()
			.to_string_lossy()
			.into_owned();
		let full = FullDerivation::from_aterm(&bytes, &drv_name, ALL_FEATURES);
		let problem = match (expect, &full) {
			(Reject, Ok(_)) => Some("accepted input CppNix rejects".to_owned()),
			(Reject, Err(_)) => None,
			(_, Err(e)) => Some(format!("failed to parse: {e}")),
			(Experimental(feature), Ok(drv)) => {
				let default = FullDerivation::from_aterm(&bytes, &drv_name, Features::default());
				if drv.unparse(false, None).unwrap().as_bytes() != bytes {
					Some("does not round-trip".to_owned())
				} else if !default
					.as_ref()
					.is_err_and(|e| e.to_string().contains(&format!("'{feature}' is disabled")))
				{
					Some(format!("default features: {default:?}"))
				} else {
					None
				}
			}
			(RoundTrip | Canonicalises, Ok(drv)) => {
				let basic = UnverifiedDerivation::from_aterm_bytes(&bytes)
					.expect("basic parser")
					.to_aterm_bytes();
				let ours = drv.unparse(false, None).unwrap();
				if ours.as_bytes() != basic {
					Some("unparse differs from UnverifiedDerivation".to_owned())
				} else if *expect == RoundTrip && ours.as_bytes() != bytes {
					Some("does not round-trip".to_owned())
				} else {
					None
				}
			}
		};
		if let Some(problem) = problem {
			mismatches.insert(*name, problem);
		}
	}
	assert!(mismatches.is_empty(), "{mismatches:#?}");
}
