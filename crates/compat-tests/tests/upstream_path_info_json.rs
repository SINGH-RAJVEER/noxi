//! Checks `nix_compat::cppnix_json::{PathInfo, NarInfo}` against CppNix
//! 2.35.2's path-info and nar-info JSON fixtures in all three format versions.
//!
//! Every file in both fixture directories must appear in [`EXPECTATIONS`].
//! Each fixture must decode to the value CppNix's `path-info.cc` or
//! `nar-info.cc` constructs, and a written fixture must encode back to the
//! same bytes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use nix_compat::cppnix_json::{NarInfo, PathInfo, PathInfoFormat, Signature};
use nix_compat::nixhash::{CAHash, NixHash};
use nix_compat::store_path::{StorePath, build_ca_path};
use serde_json::Value;
use sha2::{Digest, Sha256};

const FIXTURE_DIR: &str = "../../tests/nix/src/libstore-tests/data";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
	PathInfo,
	NarInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Contents {
	/// `makeEmpty()`: only the store directory and NAR hash.
	Empty,
	/// `makeFull`/`makeNarInfo` without impure information.
	Pure,
	/// `makeFull`/`makeNarInfo` with impure information.
	Impure,
}

/// `(file, kind, format, contents, written)`. CppNix only reads the
/// fixtures with `written` false; it never writes JSON without a version.
const EXPECTATIONS: &[(&str, Kind, PathInfoFormat, Contents, bool)] = &[
	(
		"nar-info/json-1/impure.json",
		Kind::NarInfo,
		PathInfoFormat::V1,
		Contents::Impure,
		true,
	),
	(
		"nar-info/json-1/pure.json",
		Kind::NarInfo,
		PathInfoFormat::V1,
		Contents::Pure,
		true,
	),
	(
		"nar-info/json-1/pure_noversion.json",
		Kind::NarInfo,
		PathInfoFormat::V1,
		Contents::Pure,
		false,
	),
	(
		"nar-info/json-2/impure.json",
		Kind::NarInfo,
		PathInfoFormat::V2,
		Contents::Impure,
		true,
	),
	(
		"nar-info/json-2/pure.json",
		Kind::NarInfo,
		PathInfoFormat::V2,
		Contents::Pure,
		true,
	),
	(
		"nar-info/json-3/impure.json",
		Kind::NarInfo,
		PathInfoFormat::V3,
		Contents::Impure,
		true,
	),
	(
		"nar-info/json-3/pure.json",
		Kind::NarInfo,
		PathInfoFormat::V3,
		Contents::Pure,
		true,
	),
	(
		"path-info/json-1/empty_impure.json",
		Kind::PathInfo,
		PathInfoFormat::V1,
		Contents::Empty,
		true,
	),
	(
		"path-info/json-1/empty_pure.json",
		Kind::PathInfo,
		PathInfoFormat::V1,
		Contents::Empty,
		true,
	),
	(
		"path-info/json-1/impure.json",
		Kind::PathInfo,
		PathInfoFormat::V1,
		Contents::Impure,
		true,
	),
	(
		"path-info/json-1/pure.json",
		Kind::PathInfo,
		PathInfoFormat::V1,
		Contents::Pure,
		true,
	),
	(
		"path-info/json-1/pure_noversion.json",
		Kind::PathInfo,
		PathInfoFormat::V1,
		Contents::Pure,
		false,
	),
	(
		"path-info/json-2/empty_impure.json",
		Kind::PathInfo,
		PathInfoFormat::V2,
		Contents::Empty,
		true,
	),
	(
		"path-info/json-2/empty_pure.json",
		Kind::PathInfo,
		PathInfoFormat::V2,
		Contents::Empty,
		true,
	),
	(
		"path-info/json-2/impure.json",
		Kind::PathInfo,
		PathInfoFormat::V2,
		Contents::Impure,
		true,
	),
	(
		"path-info/json-2/pure.json",
		Kind::PathInfo,
		PathInfoFormat::V2,
		Contents::Pure,
		true,
	),
	(
		"path-info/json-3/empty_impure.json",
		Kind::PathInfo,
		PathInfoFormat::V3,
		Contents::Empty,
		true,
	),
	(
		"path-info/json-3/empty_pure.json",
		Kind::PathInfo,
		PathInfoFormat::V3,
		Contents::Empty,
		true,
	),
	(
		"path-info/json-3/impure.json",
		Kind::PathInfo,
		PathInfoFormat::V3,
		Contents::Impure,
		true,
	),
	(
		"path-info/json-3/pure.json",
		Kind::PathInfo,
		PathInfoFormat::V3,
		Contents::Pure,
		true,
	),
];

const DIRECTORIES: &[&str] = &["nar-info", "path-info"];
const STORE_DIR: &str = "/nix/store";
const NAR_HASH: &str = "sha256-FePFYIlMuycIXPZbWi7LGEiMmZSX9FMbaQenWBzm1Sc=";

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

fn store_path(s: &str) -> StorePath {
	s.parse().unwrap()
}

/// `makeEmpty()` in `path-info.cc`.
fn make_empty() -> PathInfo {
	PathInfo::new(STORE_DIR, NixHash::from_sri(NAR_HASH).unwrap())
}

/// `makeFullKeyed()` in `path-info.cc`: a NAR-hashed content-addressed path
/// named `foo` that refers to `bar` and to itself.
fn make_full(impure: bool) -> PathInfo {
	let ca_hash = NixHash::Sha256(Sha256::digest(b"(...)").into());
	let bar = store_path("g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-bar");
	let foo = build_ca_path("foo", true, &ca_hash, [bar.as_ref()], true)
		.unwrap()
		.to_owned();
	let mut info = make_empty();
	info.ca = Some(CAHash::Nar(ca_hash));
	info.nar_size = 34878;
	info.references = BTreeSet::from([bar, foo]);
	if impure {
		info.deriver = Some(store_path("g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-bar.drv"));
		info.registration_time = 23423;
		info.ultimate = true;
		info.signatures = ["asdf", "qwer"]
			.into_iter()
			.map(|name| Signature {
				key_name: name.to_owned(),
				sig: vec![0; 64],
			})
			.collect();
	}
	info
}

/// `makeNarInfo()` in `nar-info.cc`.
fn make_nar_info(impure: bool) -> NarInfo {
	let mut nar_info = NarInfo {
		info: make_full(impure),
		url: String::new(),
		compression: None,
		file_hash: None,
		file_size: 0,
	};
	if impure {
		nar_info.url = "nar/1w1fff338fvdw53sqgamddn1b2xgds473pv6y13gizdbqjv4i5p3.nar.xz".to_owned();
		nar_info.compression = Some("xz".to_owned());
		nar_info.file_hash = Some(NixHash::from_sri(NAR_HASH).unwrap());
		nar_info.file_size = 4029176;
	}
	nar_info
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
fn fixtures_match_cppnix_values_and_bytes() {
	let root = fixture_root();
	let mut mismatches = Vec::new();
	for &(name, kind, format, contents, written) in EXPECTATIONS {
		let text = std::fs::read_to_string(root.join(name)).expect("fixture");
		let value: Value = serde_json::from_str(&text).expect("fixture is JSON");
		let impure = contents == Contents::Impure;
		let include_impure = impure || name.contains("empty_impure");
		let encoded = match kind {
			Kind::PathInfo => {
				let expected = match contents {
					Contents::Empty => make_empty(),
					_ => make_full(impure),
				};
				match PathInfo::from_json(&value, STORE_DIR) {
					Ok(got) if got == expected => got.to_json(include_impure, format),
					Ok(got) => {
						mismatches.push(format!("{name}: decoded {got:?}"));
						continue;
					}
					Err(error) => {
						mismatches.push(format!("{name}: {error}"));
						continue;
					}
				}
			}
			Kind::NarInfo => {
				let expected = make_nar_info(impure);
				match NarInfo::from_json(&value, STORE_DIR) {
					Ok(got) if got == expected => got.to_json(include_impure, format),
					Ok(got) => {
						mismatches.push(format!("{name}: decoded {got:?}"));
						continue;
					}
					Err(error) => {
						mismatches.push(format!("{name}: {error}"));
						continue;
					}
				}
			}
		};
		// CppNix writes these fixtures with `json::dump(2)` and a final newline.
		let rendered = serde_json::to_string_pretty(&encoded).unwrap() + "\n";
		if written && rendered != text {
			mismatches.push(format!("{name}: re-encoded as\n{rendered}"));
		}
	}
	assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn version_2_requires_store_dir() {
	let value = serde_json::json!({
		"version": 2,
		"narHash": NAR_HASH,
		"narSize": 0,
		"references": [],
		"ca": null,
	});
	assert!(PathInfo::from_json(&value, STORE_DIR).is_err());
	let value = serde_json::json!({"version": 4, "narHash": NAR_HASH});
	assert!(PathInfo::from_json(&value, STORE_DIR).is_err());
}
