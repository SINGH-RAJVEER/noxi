//! Parses every ATerm derivation fixture shipped with CppNix and checks
//! `nix-compat` against it.
//!
//! Every `.drv` file under the fixture directory must appear in [`EXPECTATIONS`].
//! A fixture whose behaviour changes (a gap gets fixed, or a round trip breaks)
//! fails this test, so the table always describes the current state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nix_compat::derivation::{Derivation, UnverifiedDerivation, lookup_fn};

const FIXTURE_DIR: &str = "../../tests/upstream/nix/src/libstore-tests/data/derivation";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
	/// Parses, and serialising it again reproduces the file byte for byte.
	RoundTrip,
	/// CppNix only parses this with the named experimental feature enabled, so
	/// under default settings both reject it. `nix-compat` has no support for
	/// the feature yet, which is a gap for matrix rows that enable it.
	Experimental(&'static str),
	/// Invalid input that CppNix also rejects while parsing.
	Reject,
	/// A known gap: CppNix accepts this, but `nix-compat` rejects it under the
	/// named stricter rule, which it took from CppNix's development branch.
	Divergent(&'static str),
}

use Expect::*;

const CA_FLOATING: &str = "ca-derivations";
const DYN_DRV: &str = "dynamic-derivations";
/// CppNix 2.35.2 inserts input derivations into a map in any order, letting a
/// later duplicate replace an earlier one. The anonymised fixtures list inputs
/// that share a digest out of path order.
const UNSORTED_INPUT_DRVS: &str = "input derivations must be strictly sorted";

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
	("firefox.drv", Divergent(UNSORTED_INPUT_DRVS)),
	("hello.drv", Divergent(UNSORTED_INPUT_DRVS)),
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
			Ok(_) => "parses but re-serialises differently",
			Err(_) => "fails to parse",
		};

		let ok = match expect {
			RoundTrip => actual == "round-trips",
			Experimental(_) | Reject | Divergent(_) => parsed.is_err(),
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
