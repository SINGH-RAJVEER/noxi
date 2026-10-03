//! CppNix 2.35.2's `memory-source-accessor` and `nar-listing` JSON
//! characterization cases, built from the same example trees as
//! `memory-source-accessor.cc`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use nix_compat::fso::File;
use nix_compat::nar::listing::{ListingEntry, ShallowListingEntry};

fn fixture(dir: &str, name: &str) -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../tests/nix/src/libutil-tests/data")
		.join(dir)
		.join(format!("{name}.json"))
}

fn read(dir: &str, name: &str) -> String {
	std::fs::read_to_string(fixture(dir, name)).expect("fixture JSON")
}

fn regular(executable: bool, contents: &[u8]) -> File {
	File::Regular {
		executable,
		contents: contents.to_vec(),
	}
}

fn example_simple() -> File {
	regular(false, b"asdf")
}

fn example_complex() -> File {
	File::Directory {
		entries: BTreeMap::from([
			("foo".into(), regular(false, b"hello\n\0\n\tworld!")),
			(
				"bar".into(),
				File::Directory {
					entries: BTreeMap::from([
						("baz".into(), regular(true, b"good day,\n\0\n\tworld!")),
						(
							"quux".into(),
							File::Symlink {
								target: "/over/there".into(),
							},
						),
					]),
				},
			),
		]),
	}
}

/// CppNix writes these files with `dump(2)` and a trailing newline.
fn written<T: serde::Serialize>(value: &T) -> String {
	serde_json::to_string_pretty(value).unwrap() + "\n"
}

#[test]
fn memory_source_accessor_json_matches_cppnix() {
	for (stem, expected) in [("simple", example_simple()), ("complex", example_complex())] {
		let text = read("memory-source-accessor", stem);
		let decoded: File = serde_json::from_str(&text).expect("CppNix accessor JSON");
		assert_eq!(decoded, expected, "{stem}");
		assert_eq!(written(&expected), text, "{stem}");
	}
}

#[test]
fn nar_listings_match_cppnix() {
	let text = read("nar-listing", "deep");
	let expected = example_complex().list_deep();
	let decoded: ListingEntry = serde_json::from_str(&text).expect("CppNix deep listing");
	assert_eq!(decoded, expected);
	assert_eq!(written(&expected), text);

	let text = read("nar-listing", "shallow");
	let expected = example_complex().list_shallow();
	let decoded: ShallowListingEntry = serde_json::from_str(&text).expect("CppNix shallow listing");
	assert_eq!(decoded, expected);
	assert_eq!(written(&expected), text);
}

#[test]
fn accessor_json_rejects_what_cppnix_rejects() {
	for (json, why) in [
		(r#"{"type":"regular","contents":"x"}"#, "missing executable"),
		(
			r#"{"type":"regular","executable":false}"#,
			"missing contents",
		),
		(r#"{"type":"fifo"}"#, "unknown type"),
		(r#"{"entries":{}}"#, "missing type"),
	] {
		assert!(serde_json::from_str::<File>(json).is_err(), "{why}");
	}
	// Contents that are not UTF-8 have no JSON string form.
	assert!(serde_json::to_string(&regular(false, b"\xff")).is_err());
}

#[test]
fn listing_reads_absent_and_zero_offsets_as_unknown() {
	let entry: ListingEntry = serde_json::from_str(r#"{"type":"regular","narOffset":0}"#).unwrap();
	assert_eq!(
		entry,
		ListingEntry::Regular {
			size: None,
			executable: false,
			nar_offset: None,
		}
	);
	assert_eq!(
		serde_json::to_string(&entry).unwrap(),
		r#"{"executable":false,"type":"regular"}"#
	);

	let entry: ListingEntry =
		serde_json::from_str(r#"{"type":"regular","size":3,"narOffset":96}"#).unwrap();
	assert_eq!(
		serde_json::to_string(&entry).unwrap(),
		r#"{"executable":false,"narOffset":96,"size":3,"type":"regular"}"#
	);
}
