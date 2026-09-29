use std::{collections::BTreeSet, fs, path::PathBuf};

use nix_compat::nixhash::{CAHash, HashAlgo, NixHash};

#[test]
fn blake3_matches_cppnix_vectors() {
	for (input, expected_hex) in [
		(
			"abc",
			"6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85",
		),
		(
			"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
			"c19012cc2aaf0dc3d8e5c45a1b79114d2df42abb2a410bf54be09e891af06ff8",
		),
		(
			"IETF",
			"83a2de1ee6f4e6ab686889248f4ec0cf4cc5709446a682ffd1cbb4d6165181e2",
		),
	] {
		let actual = HashAlgo::Blake3.digest_bytes(input);
		assert_eq!(
			actual.as_nix_lowerhex_string_fmt().to_string(),
			format!("blake3:{expected_hex}")
		);
		assert_eq!(
			NixHash::from_str(&format!("blake3:{expected_hex}"), None).unwrap(),
			actual
		);
		assert_eq!(NixHash::from_sri(&actual.to_sri_string()).unwrap(), actual);
		assert_eq!(
			NixHash::from_nix_nixbase32(&actual.to_nix_nixbase32()),
			Some(actual)
		);
	}
}

#[test]
fn blake3_json_matches_cppnix_fixture() {
	let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("../../tests/nix/src/libutil-tests/data/hash/blake3.json");
	let fixture = fs::read_to_string(path).expect("CppNix hash fixture must exist");
	let expected = HashAlgo::Blake3.digest_bytes("asdf");
	let decoded: NixHash = serde_json::from_str(&fixture).expect("parse CppNix hash JSON");
	assert_eq!(decoded, expected);
	assert_eq!(serde_json::to_string(&expected).unwrap(), fixture.trim());
}

#[test]
fn sha256_json_matches_cppnix_fixture() {
	let name = "sha256.json";
	let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("../../tests/nix/src/libutil-tests/data/hash")
		.join(name);
	let fixture = fs::read_to_string(path).expect("CppNix hash fixture must exist");
	let expected = HashAlgo::Sha256.digest_bytes("asdf");
	let decoded: NixHash = serde_json::from_str(&fixture).expect("parse CppNix hash JSON");
	assert_eq!(decoded, expected);
	assert_eq!(serde_json::to_string(&expected).unwrap(), fixture.trim());
}

#[test]
fn mislabeled_sha512_fixture_matches_cppnix_test_value() {
	// CppNix 2.35.2's HashJSON test constructs SHA256 for its "sha512" row.
	// The copied fixture therefore has a SHA256 SRI, despite its filename.
	let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("../../tests/nix/src/libutil-tests/data/hash/sha512.json");
	let fixture = fs::read_to_string(path).expect("CppNix hash fixture must exist");
	let cppnix_value = HashAlgo::Sha256.digest_bytes("asdf");
	let decoded: NixHash = serde_json::from_str(&fixture).expect("parse CppNix hash JSON");
	assert_eq!(decoded, cppnix_value);
	assert_eq!(
		serde_json::to_string(&cppnix_value).unwrap(),
		fixture.trim()
	);
	assert_ne!(decoded, HashAlgo::Sha512.digest_bytes("asdf"));
}

#[test]
fn different_algorithms_with_the_same_digest_stay_distinct() {
	let digest = [0; 32];
	let hashes = BTreeSet::from([NixHash::Sha256(digest), NixHash::Blake3(digest)]);
	assert_eq!(hashes.len(), 2);
}

#[test]
fn blake3_content_addresses_round_trip() {
	let digest = HashAlgo::Blake3.digest_bytes("abc");
	for address in [CAHash::Flat(digest.clone()), CAHash::Nar(digest.clone())] {
		let printed = address.to_string();
		assert_eq!(CAHash::from_nix_hex_str(&printed), Some(address));
	}
}
