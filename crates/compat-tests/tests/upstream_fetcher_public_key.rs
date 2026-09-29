//! CppNix 2.35.2's fetcher public-key JSON characterization cases.

use std::path::{Path, PathBuf};

use nix_compat::fetchers::PublicKey;

fn fixture(name: &str) -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../tests/nix/src/libfetchers-tests/data/public-key")
		.join(format!("{name}.json"))
}

#[test]
fn public_key_json_matches_cppnix() {
	for (stem, key_type) in [("simple", "ssh-rsa"), ("defaultType", "ssh-ed25519")] {
		let text = std::fs::read_to_string(fixture(stem)).expect("fixture JSON");
		let key: PublicKey = serde_json::from_str(&text).expect("CppNix public key");
		assert_eq!(key.r#type, key_type);
		assert_eq!(key.key, "ABCDE");
		assert_eq!(
			serde_json::to_string_pretty(&key).unwrap(),
			text.trim_end(),
			"{stem}"
		);
	}
}

#[test]
fn missing_type_is_read_with_cppnix_default_and_written_explicitly() {
	let text = std::fs::read_to_string(fixture("noRoundTrip")).expect("fixture JSON");
	let key: PublicKey = serde_json::from_str(&text).expect("CppNix public key");
	assert_eq!(key.r#type, "ssh-ed25519");
	assert_eq!(key.key, "ABCDE");
	assert_ne!(serde_json::to_string_pretty(&key).unwrap(), text.trim_end());
	assert_eq!(
		serde_json::to_value(&key).unwrap(),
		serde_json::json!({"key": "ABCDE", "type": "ssh-ed25519"})
	);
}
