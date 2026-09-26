//! Checks the hashes that decide input-addressed output paths against CppNix's
//! masked-hash fixtures.
//!
//! Two different hashes are involved:
//!
//! - Hash derivation modulo: the derivation with its output paths filled in and
//!   each input derivation replaced by its own hash derivation modulo. Dependents
//!   refer to a derivation by this hash, so it appears as an input key in the
//!   parent's fully masked form.
//! - The fully masked form (`<stem>-fully-masked.drv`): the same, but with the
//!   output paths and output environment variables blanked. Its SHA-256 is what
//!   the derivation's own output paths are computed from.
//!
//! The fixtures form the graph from CppNix issue #16307: two fixed-output
//! sources with equal content but different builders, and two intermediates that
//! each depend on one of them. Everything downstream of the sources must hash
//! the same.
//!
//! The parents (`parent-split`, `parent-joined`) have deferred outputs. One
//! takes `out` and `dev` from different intermediates, the other takes both from
//! one; because the intermediates mask to the same hash, both must end up with
//! the same output path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nix_compat::derivation::{Derivation, UnverifiedDerivation, lookup_fn};
use nix_compat::nixhash::Sha256;
use nix_compat::store_path::{StorePath, build_output_path};

const FIXTURE_DIR: &str = "../../tests/upstream/nix/src/libstore-tests/data/derivation/masked";

fn fixture(name: &str) -> Vec<u8> {
	let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
		.join(FIXTURE_DIR)
		.join(name);
	std::fs::read(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn parse(name: &str) -> UnverifiedDerivation {
	UnverifiedDerivation::from_aterm_bytes(&fixture(name))
		.unwrap_or_else(|e| panic!("parsing {name}: {e:?}"))
		.clone()
}

/// The drv path and hash derivation modulo of both fixed-output sources.
fn sources() -> HashMap<StorePath, Sha256> {
	["source-first.drv", "source-second.drv"]
		.into_iter()
		.map(|name| {
			let drv = parse(name);
			let drv_path = drv.calculate_derivation_path("source").expect("drv path");
			let hdm = drv
				.hash_derivation_modulo(lookup_fn(|_| None))
				.expect("fixed-output derivations need no lookup");
			(drv_path, hdm)
		})
		.collect()
}

#[test]
fn fixed_output_sources_verify_and_share_a_hash() {
	let sources = sources();
	assert_eq!(sources.len(), 2, "the sources are different derivations");

	let hashes: Vec<_> = sources.values().collect();
	assert_eq!(
		hashes[0], hashes[1],
		"fixed-output derivations with the same output hash must mask to the same hash"
	);

	for name in ["source-first.drv", "source-second.drv"] {
		Derivation::from_aterm_bytes("source", lookup_fn(|_| None), &fixture(name))
			.unwrap_or_else(|e| panic!("{name} must verify: {e:?}"));
	}
}

/// Input derivation keys (hex hashes) of a fully masked fixture.
fn masked_input_keys(name: &str) -> Vec<String> {
	let text = String::from_utf8(fixture(name)).expect("fixtures are UTF-8");
	let input_drvs = text
		.split("],[")
		.nth(1)
		.expect("fully masked form has an input derivations list");
	input_drvs
		.split("(\"")
		.skip(1)
		.map(|entry| entry.split('"').next().unwrap().to_string())
		.collect()
}

fn output_paths_from(masked: &[u8], drv: &UnverifiedDerivation) -> Vec<(String, StorePath)> {
	let hash = Sha256::digest_bytes(masked);
	drv.outputs()
		.names()
		.map(|output_name| {
			let path_name = if output_name.as_str() == "out" {
				"intermediate".to_string()
			} else {
				format!("intermediate-{output_name}")
			};
			let path = build_output_path(&path_name, &hash, output_name)
				.expect("valid output path")
				.to_owned();
			(output_name.to_string(), path)
		})
		.collect()
}

fn stated_paths(drv: &UnverifiedDerivation) -> Vec<(String, StorePath)> {
	drv.outputs()
		.iter()
		.map(|(name, path)| (name.to_string(), path.clone()))
		.collect()
}

#[test]
fn intermediates_match_cppnix_hashes() {
	let sources = sources();
	let lookup = || lookup_fn(|path| sources.get(&path.to_owned()).copied());

	let mut modulo_hashes = Vec::new();
	for stem in ["intermediate-first", "intermediate-second"] {
		let drv = parse(&format!("{stem}.drv"));
		let fully_masked = fixture(&format!("{stem}-fully-masked.drv"));

		// nix-compat derives output paths from the same masked form CppNix writes.
		let recalculated: Vec<_> = drv
			.recalculate_outputs("intermediate", lookup())
			.unwrap_or_else(|e| panic!("{stem}: {e:?}"))
			.iter()
			.map(|(name, path)| (name.to_string(), path.clone()))
			.collect();
		assert_eq!(
			recalculated,
			output_paths_from(&fully_masked, &drv),
			"{stem}: output paths must come from the SHA-256 of CppNix's fully masked form"
		);

		// Upstream fixture quirk: CppNix's test fills in the output paths
		// before the `dev`/`out` environment variables exist, so the stated
		// paths come from a masked form with an empty environment. The golden
		// fully masked form is written afterwards, with those variables blanked.
		// CppNix's own validation masks the variables that exist, so it computes
		// the same paths as nix-compat and would reject the stated ones too.
		let without_output_env = String::from_utf8(fully_masked)
			.expect("fixtures are UTF-8")
			.replace(r#"[("dev",""),("out","")])"#, "[])");
		assert_eq!(
			stated_paths(&drv),
			output_paths_from(without_output_env.as_bytes(), &drv),
			"{stem}: stated paths come from the masked form without output environment variables"
		);
		assert!(
			Derivation::from_aterm_bytes(
				"intermediate",
				lookup(),
				&fixture(&format!("{stem}.drv"))
			)
			.is_err(),
			"{stem}: verification must reject the stated paths, as CppNix's validation does"
		);

		modulo_hashes.push(
			drv.hash_derivation_modulo(lookup())
				.unwrap_or_else(|e| panic!("{stem}: {e:?}")),
		);
	}

	assert_eq!(
		modulo_hashes[0], modulo_hashes[1],
		"intermediates that differ only in the provenance of a fixed-output input must hash the same"
	);

	// The parents' fully masked forms name that input by this hash.
	let expected_key = format!("{:x}", modulo_hashes[0]);
	for parent in ["parent-joined", "parent-split"] {
		assert_eq!(
			masked_input_keys(&format!("{parent}-fully-masked.drv")),
			vec![expected_key.clone()],
			"{parent}: input key must be the intermediates' hash derivation modulo"
		);
	}
}

/// The drv path and hash derivation modulo of both intermediates.
fn intermediates() -> HashMap<StorePath, Sha256> {
	let sources = sources();
	["intermediate-first.drv", "intermediate-second.drv"]
		.into_iter()
		.map(|name| {
			let drv = parse(name);
			let drv_path = drv
				.calculate_derivation_path("intermediate")
				.expect("drv path");
			let hdm = drv
				.hash_derivation_modulo(lookup_fn(|path| sources.get(&path.to_owned()).copied()))
				.expect("hash derivation modulo");
			(drv_path, hdm)
		})
		.collect()
}

#[test]
fn parents_merge_masked_inputs() {
	let intermediates = intermediates();

	let mut paths = Vec::new();
	for stem in ["parent-split", "parent-joined"] {
		let bytes = fixture(&format!("{stem}.drv"));
		let drv = parse(&format!("{stem}.drv"));
		assert!(drv.outputs().is_deferred(), "{stem}: outputs are deferred");
		assert_eq!(drv.to_aterm_bytes(), bytes, "{stem}: round-trips");

		// The drv paths CppNix wrote as inputs are the ones nix-compat computes
		// for the intermediates.
		for input in drv.input_derivations().keys() {
			assert!(
				intermediates.contains_key(input),
				"{stem}: input {input} must be a drv path computed for an intermediate"
			);
		}

		// Filling in the deferred outputs uses the fully masked form, whose
		// input key merges the output names of both intermediates.
		let lookup = lookup_fn(|path| intermediates.get(&path.to_owned()).copied());
		let filled: Vec<_> = drv
			.recalculate_outputs("parent", lookup)
			.unwrap_or_else(|e| panic!("{stem}: {e:?}"))
			.iter()
			.map(|(name, path)| (name.to_string(), path.clone()))
			.collect();
		let masked = Sha256::digest_bytes(fixture(&format!("{stem}-fully-masked.drv")));
		let out = "out".parse().unwrap();
		let expected = build_output_path("parent", &masked, &out)
			.unwrap()
			.to_owned();
		assert_eq!(
			filled,
			vec![("out".to_string(), expected)],
			"{stem}: output path must come from CppNix's fully masked form"
		);

		// CppNix's validation accepts deferred outputs (with a warning when
		// they could be filled in).
		let lookup = lookup_fn(|path| intermediates.get(&path.to_owned()).copied());
		Derivation::from_aterm_bytes("parent", lookup, &bytes)
			.unwrap_or_else(|e| panic!("{stem} must verify: {e:?}"));

		paths.push(filled);
	}

	assert_eq!(
		paths[0], paths[1],
		"parents taking the same outputs from indistinguishable intermediates must get the same path"
	);
}
