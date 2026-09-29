//! JSON values shared with CppNix fetchers.

use serde::{Deserialize, Serialize, ser::SerializeMap};

fn default_key_type() -> String {
	"ssh-ed25519".to_owned()
}

/// A fetcher's public-key declaration. This describes a remote source key;
/// narinfo trusted keys use [`crate::narinfo::VerifyingKey`] instead.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
pub struct PublicKey {
	#[serde(default = "default_key_type")]
	pub r#type: String,
	pub key: String,
}

impl Serialize for PublicKey {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let mut map = serializer.serialize_map(Some(2))?;
		map.serialize_entry("key", &self.key)?;
		map.serialize_entry("type", &self.r#type)?;
		map.end()
	}
}
