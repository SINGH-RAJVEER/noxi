//! CppNix 2.35.2 Git blob and tree object fixtures, plus malformed framing.

use std::path::{Path, PathBuf};

use nix_compat::git::{self, ObjectKind};

fn fixture(name: &str) -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../tests/nix/src/libutil-tests/data/git")
		.join(name)
}

#[test]
fn blob_fixture_matches_cppnix() {
	let source = std::fs::read(fixture("hello-world.bin")).expect("blob source");
	let bytes = std::fs::read(fixture("hello-world-blob.bin")).expect("blob fixture");
	let object = git::parse_object(&bytes).expect("CppNix blob");
	assert_eq!(object.kind, ObjectKind::Blob);
	assert_eq!(object.body, source);
	assert_eq!(git::encode_object(ObjectKind::Blob, &source), bytes);
}

#[test]
fn tree_fixtures_match_cppnix_entries_and_bytes() {
	for (stem, hash_len) in [("tree-sha1", 20), ("tree-sha256", 32)] {
		let bytes = std::fs::read(fixture(&format!("{stem}.bin"))).expect("tree fixture");
		let text =
			std::fs::read_to_string(fixture(&format!("{stem}.txt"))).expect("CppNix tree listing");
		let object = git::parse_object(&bytes).expect("CppNix tree");
		assert_eq!(object.kind, ObjectKind::Tree);
		let entries = git::parse_tree(object.body, hash_len).expect("CppNix tree entries");
		assert_eq!(entries.len(), text.lines().count());
		for (entry, line) in entries.iter().zip(text.lines()) {
			let (description, name) = line.split_once('\t').expect("tree listing row");
			let mut fields = description.split_ascii_whitespace();
			let mode = u32::from_str_radix(fields.next().unwrap(), 8).unwrap();
			let object_type = fields.next().unwrap();
			let hash = data_encoding::HEXLOWER
				.decode(fields.next().unwrap().as_bytes())
				.unwrap();
			assert!(fields.next().is_none());
			assert_eq!(entry.mode, mode, "{stem}/{name}");
			assert_eq!(entry.name, name.as_bytes(), "{stem}/{name}");
			assert_eq!(entry.hash, hash, "{stem}/{name}");
			assert_eq!(object_type, if mode == 0o40000 { "tree" } else { "blob" });
		}
		assert_eq!(git::encode_tree(&entries, hash_len).unwrap(), object.body);
		assert_eq!(git::encode_object(ObjectKind::Tree, object.body), bytes);
	}
}

#[test]
fn malformed_git_objects_are_rejected() {
	for bytes in [
		b"blob 4\0abc".as_slice(),
		b"blob 02\0ab",
		b"blob -1\0x",
		b"commit 1\0x",
		b"tree 1\0x\0",
	] {
		assert!(git::parse_object(bytes).is_err(), "{bytes:?}");
	}
	assert!(git::parse_tree(b"100644 foo\0short", 20).is_err());
	assert!(git::parse_tree(b"100644 a/b\0xxxxxxxxxxxxxxxxxxxx", 20).is_err());
}
