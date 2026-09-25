//! Parses every ATerm derivation fixture shipped with CppNix and checks
//! `nix-compat` against it.
//!
//! Every `.drv` file under the fixture directory must appear in [`EXPECTATIONS`].
//! A fixture whose behaviour changes (a gap gets fixed, or a round trip breaks)
//! fails this test, so the table always describes the current state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nix_compat::derivation::UnverifiedDerivation;

const FIXTURE_DIR: &str = "../../tests/upstream/nix/src/libstore-tests/data/derivation";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
	/// Parses, and serialising it again reproduces the file byte for byte.
	RoundTrip,
	/// Parses, and serialises to an equivalent derivation after canonicalising its input sources.
	Canonicalizes,
	/// CppNix accepts this file, but `nix-compat` does not yet.
	Gap(&'static str),
	/// Invalid input that CppNix also rejects.
	Reject,
	/// Invalid according to CppNix, but only detectable once output paths are
	/// verified, so the unverified parser accepts it.
	AcceptedUnverified,
}

use Expect::*;

const CA_FLOATING: &str = "floating content-addressed outputs (ca-derivations) are not supported";
const DEFERRED: &str = "deferred outputs with empty path and hash (masked form) are not supported";
const DYN_DRV: &str = "DrvWithVersion / dynamic derivations are not supported";

const EXPECTATIONS: &[(&str, Expect)] = &[
	("bad-ca-fixed-path.drv", AcceptedUnverified),
	("bad-old-version-dyn-deps.drv", Reject),
	("bad-version.drv", Reject),
	("ca/advanced-attributes-defaults.drv", Gap(CA_FLOATING)),
	("ca/advanced-attributes.drv", Gap(CA_FLOATING)),
	(
		"ca/advanced-attributes-structured-attrs-defaults.drv",
		Gap(CA_FLOATING),
	),
	(
		"ca/advanced-attributes-structured-attrs.drv",
		Gap(CA_FLOATING),
	),
	("dyn-dep-derivation.drv", Gap(DYN_DRV)),
	("firefox.drv", Canonicalizes),
	("hello.drv", Canonicalizes),
	("ia/advanced-attributes-defaults.drv", RoundTrip),
	("ia/advanced-attributes.drv", RoundTrip),
	(
		"ia/advanced-attributes-structured-attrs-defaults.drv",
		RoundTrip,
	),
	("ia/advanced-attributes-structured-attrs.drv", RoundTrip),
	("masked/intermediate-first.drv", RoundTrip),
	("masked/intermediate-first-fully-masked.drv", Gap(DEFERRED)),
	("masked/intermediate-second.drv", RoundTrip),
	("masked/intermediate-second-fully-masked.drv", Gap(DEFERRED)),
	("masked/parent-joined.drv", Gap(DEFERRED)),
	("masked/parent-joined-fully-masked.drv", Gap(DEFERRED)),
	("masked/parent-split.drv", Gap(DEFERRED)),
	("masked/parent-split-fully-masked.drv", Gap(DEFERRED)),
	("masked/source-first.drv", RoundTrip),
	("masked/source-second.drv", RoundTrip),
	(
		"simple-derivation.drv",
		Gap("derivations with zero outputs are rejected"),
	),
	(
		"windows-store-dir.drv",
		Gap("store directories other than /nix/store are not supported"),
	),
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
			Canonicalizes => parsed.as_ref().is_ok_and(|drv| {
				let canonical = drv.to_aterm_bytes();
				canonical != bytes
					&& UnverifiedDerivation::from_aterm_bytes(&canonical)
						.is_ok_and(|reparsed| reparsed == *drv)
			}),
			AcceptedUnverified => parsed.is_ok(),
			Gap(_) | Reject => parsed.is_err(),
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
