use crate::nixbase32;
use crate::nixhash::NixHash;
use std::borrow::Cow;

/// A Nix CAHash describes a content-addressed hash of a path.
///
/// The way Nix prints it as a string is a bit confusing, but there's essentially
/// three modes, `Flat`, `Nar` and `Text`.
/// `Flat` and `Nar` support all 4 algos that [NixHash] supports
/// (sha1, md5, sha256, sha512), `Text` only supports sha256.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CAHash {
	Flat(NixHash),  // "fixed flat"
	Nar(NixHash),   // "fixed recursive"
	Text([u8; 32]), // "text", only supports sha256
}

/// Representation for the supported hash modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashMode {
	Flat,
	Nar,
	Text,
}

impl CAHash {
	pub fn hash(&self) -> Cow<'_, NixHash> {
		match *self {
			CAHash::Flat(ref digest) => Cow::Borrowed(digest),
			CAHash::Nar(ref digest) => Cow::Borrowed(digest),
			CAHash::Text(digest) => Cow::Owned(NixHash::Sha256(digest)),
		}
	}

	pub fn mode(&self) -> HashMode {
		match self {
			CAHash::Flat(_) => HashMode::Flat,
			CAHash::Nar(_) => HashMode::Nar,
			CAHash::Text(_) => HashMode::Text,
		}
	}

	/// Constructs a [CAHash] from the textual representation,
	/// which is one of the three:
	/// - `text:sha256:$nixbase32sha256digest`
	/// - `fixed:r:$algo:$nixbase32digest`
	/// - `fixed:$algo:$nixbase32digest`
	///
	/// These formats are used in NARInfo, for example.
	pub fn from_nix_hex_str(s: &str) -> Option<Self> {
		let (tag, s) = s.split_once(':')?;

		match tag {
			"text" => {
				let digest = s.strip_prefix("sha256:")?;
				let digest = nixbase32::decode_fixed(digest).ok()?;
				Some(CAHash::Text(digest))
			}
			"fixed" => {
				if let Some(s) = s.strip_prefix("r:") {
					NixHash::from_nix_nixbase32(s).map(CAHash::Nar)
				} else {
					NixHash::from_nix_nixbase32(s).map(CAHash::Flat)
				}
			}
			_ => None,
		}
	}
}

/// Formats a [CAHash] in the Nix default hash format, which is the format
/// that's used in NARInfos for example.
impl std::fmt::Display for CAHash {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let (algo, hash) = match self {
			CAHash::Flat(h) => match h {
				NixHash::Md5(h) => ("fixed:md5", &h[..]),
				NixHash::Sha1(h) => ("fixed:sha1", &h[..]),
				NixHash::Sha256(h) => ("fixed:sha256", &h[..]),
				NixHash::Sha512(h) => ("fixed:sha512", &h[..]),
			},
			CAHash::Nar(h) => match h {
				NixHash::Md5(h) => ("fixed:r:md5", &h[..]),
				NixHash::Sha1(h) => ("fixed:r:sha1", &h[..]),
				NixHash::Sha256(h) => ("fixed:r:sha256", &h[..]),
				NixHash::Sha512(h) => ("fixed:r:sha512", &h[..]),
			},
			CAHash::Text(h) => ("text:sha256", &h[..]),
		};

		write!(f, "{}:{}", algo, nixbase32::encode(hash))
	}
}
