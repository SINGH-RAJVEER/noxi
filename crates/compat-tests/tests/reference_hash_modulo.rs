//! Checks `nix-compat`'s hash derivation modulo against derivations written by
//! the CppNix 2.35.2 reference.
//!
//! `fixtures/hash-modulo-collision/graph.nix` builds the graph from CppNix issue
//! #16307: two fixed-output sources with the same output but different builders,
//! and two intermediates that each use one of them. The intermediates mask to the
//! same hash, and the parent takes a different output from each, so the parent's
//! masked form has two inputs with one hash. 2.35.2 keeps the output names of the
//! input that comes later in path order; merging them would give the parent a
//! different output path.
//!
//! The `.drv` files are exactly what 2.35.2 wrote. To regenerate them, run
//! `just reference`, then
//! `target/nix-reference/bin/nix-instantiate --store "$(mktemp -d)" crates/compat-tests/fixtures/hash-modulo-collision/graph.nix`
//! and copy every `.drv` file from that store's `nix/store/` here.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use nix_compat::derivation::{Derivation, UnverifiedDerivation, lookup_fn};
use nix_compat::nixhash::Sha256;
use nix_compat::store_path::StorePath;

const FIXTURE_DIR: &str = concat!(
	env!("CARGO_MANIFEST_DIR"),
	"/fixtures/hash-modulo-collision"
);

/// Every fixture derivation, keyed by the store path CppNix gave it, with its
/// derivation name and bytes.
fn fixtures() -> BTreeMap<StorePath, (String, Vec<u8>)> {
	std::fs::read_dir(FIXTURE_DIR)
		.expect("fixture directory must be readable")
		.map(|entry| entry.expect("directory entry").path())
		.filter(|path| path.extension().is_some_and(|e| e == "drv"))
		.map(|path| {
			let file_name = path.file_name().unwrap().to_str().unwrap();
			let drv_path = StorePath::from_bytes(file_name.as_bytes()).expect("store path name");
			let name = drv_path
				.name()
				.strip_suffix(".drv")
				.expect("derivation name")
				.to_string();
			(drv_path, (name, read(&path)))
		})
		.collect()
}

fn read(path: &Path) -> Vec<u8> {
	std::fs::read(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// Verify every derivation in dependency order, which recomputes its output
/// paths from its inputs' hashes, and return each one's hash modulo.
fn verify_all() -> (
	BTreeMap<StorePath, (String, Vec<u8>)>,
	HashMap<StorePath, Sha256>,
) {
	let drvs = fixtures();
	let mut hashes = HashMap::<StorePath, Sha256>::new();

	while hashes.len() < drvs.len() {
		let ready: Vec<_> = drvs
			.iter()
			.filter(|(path, (_, bytes))| {
				!hashes.contains_key(*path)
					&& UnverifiedDerivation::from_aterm_bytes(bytes)
						.expect("2.35.2 derivations must parse")
						.input_derivations()
						.keys()
						.all(|input| hashes.contains_key(input))
			})
			.collect();
		assert!(
			!ready.is_empty(),
			"the fixture graph must be acyclic and complete"
		);

		for (path, (name, bytes)) in ready {
			let lookup = || lookup_fn(|input| hashes.get(&input.to_owned()).copied());
			let drv = UnverifiedDerivation::from_aterm_bytes(bytes).unwrap();
			assert_eq!(
				&drv.calculate_derivation_path(name).unwrap(),
				path,
				"derivation path"
			);
			Derivation::from_aterm_bytes(name, lookup(), bytes).unwrap_or_else(|e| {
				panic!("{path}: output paths differ from CppNix 2.35.2's: {e:?}")
			});
			let hash = drv.hash_derivation_modulo(lookup()).unwrap();
			hashes.insert(path.clone(), hash);
		}
	}

	(drvs, hashes)
}

#[test]
fn output_paths_match_the_reference() {
	verify_all();
}

/// Without equal intermediate hashes, the test above would not exercise a
/// collision.
#[test]
fn intermediates_share_a_hash() {
	let (drvs, hashes) = verify_all();
	let intermediate_hashes: Vec<_> = drvs
		.iter()
		.filter(|(_, (name, _))| name == "intermediate")
		.map(|(path, _)| hashes[path])
		.collect();
	assert_eq!(intermediate_hashes.len(), 2);
	assert_eq!(intermediate_hashes[0], intermediate_hashes[1]);
}
