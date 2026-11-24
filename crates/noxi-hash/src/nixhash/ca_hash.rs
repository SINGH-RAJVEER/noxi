use crate::nixbase32;
use crate::nixhash::NixHash;
use std::borrow::Cow;

/// A Nix CAHash describes a content-addressed hash of a path.
///
/// The way Nix prints it as a string is a bit confusing, but there's essentially
/// four modes, `Flat`, `Nar`, `Text` and `Git`.
/// `Flat` and `Nar` support all algorithms that [NixHash] supports;
/// `Text` only supports sha256.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CAHash {
	Flat(NixHash),  // "fixed flat"
	Nar(NixHash),   // "fixed recursive"
	Text([u8; 32]), // "text", only supports sha256
	Git(NixHash),   // "fixed git", only supports sha1 and sha256
}

/// Representation for the supported hash modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashMode {
	Flat,
	Nar,
	Text,
	Git,
}

impl CAHash {
	pub fn hash(&self) -> Cow<'_, NixHash> {
		match *self {
			CAHash::Flat(ref digest) => Cow::Borrowed(digest),
			CAHash::Nar(ref digest) => Cow::Borrowed(digest),
			CAHash::Git(ref digest) => Cow::Borrowed(digest),
			CAHash::Text(digest) => Cow::Owned(NixHash::Sha256(digest)),
		}
	}

	pub fn mode(&self) -> HashMode {
		match self {
			CAHash::Flat(_) => HashMode::Flat,
			CAHash::Nar(_) => HashMode::Nar,
			CAHash::Text(_) => HashMode::Text,
			CAHash::Git(_) => HashMode::Git,
		}
	}

	/// Constructs a [CAHash] from the textual representation,
	/// which is one of these:
	/// - `text:sha256:$nixbase32sha256digest`
	/// - `fixed:r:$algo:$nixbase32digest`
	/// - `fixed:git:$algo:$nixbase32digest`
	/// - `fixed:$algo:$nixbase32digest`
	///
	/// These formats are used in NARInfo, for example. As in CppNix's
	/// `ContentAddress::parse`, the digest may also be base-16 or base-64,
	/// chosen by its length (`Hash::parseNonSRIUnprefixed`).
	pub fn from_nix_hex_str(s: &str) -> Option<Self> {
		let (tag, s) = s.split_once(':')?;

		let (mode, rest) = match tag {
			"text" => (HashMode::Text, s),
			"fixed" => match s.strip_prefix("r:") {
				Some(rest) => (HashMode::Nar, rest),
				None => match s.strip_prefix("git:") {
					Some(rest) => (HashMode::Git, rest),
					None => (HashMode::Flat, s),
				},
			},
			_ => return None,
		};
		let (algo, digest) = rest.split_once(':')?;
		let hash = super::decode_digest(digest.as_bytes(), algo.parse().ok()?).ok()?;
		match (mode, hash) {
			(HashMode::Text, NixHash::Sha256(digest)) => Some(CAHash::Text(digest)),
			(HashMode::Text, _) => None,
			(HashMode::Flat, hash) => Some(CAHash::Flat(hash)),
			(HashMode::Nar, hash) => Some(CAHash::Nar(hash)),
			(HashMode::Git, hash @ (NixHash::Sha1(_) | NixHash::Sha256(_))) => {
				Some(CAHash::Git(hash))
			}
			(HashMode::Git, _) => None,
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
				NixHash::Blake3(h) => ("fixed:blake3", &h[..]),
			},
			CAHash::Nar(h) => match h {
				NixHash::Md5(h) => ("fixed:r:md5", &h[..]),
				NixHash::Sha1(h) => ("fixed:r:sha1", &h[..]),
				NixHash::Sha256(h) => ("fixed:r:sha256", &h[..]),
				NixHash::Sha512(h) => ("fixed:r:sha512", &h[..]),
				NixHash::Blake3(h) => ("fixed:r:blake3", &h[..]),
			},
			CAHash::Git(h) => match h {
				NixHash::Md5(h) => ("fixed:git:md5", &h[..]),
				NixHash::Sha1(h) => ("fixed:git:sha1", &h[..]),
				NixHash::Sha256(h) => ("fixed:git:sha256", &h[..]),
				NixHash::Sha512(h) => ("fixed:git:sha512", &h[..]),
				NixHash::Blake3(h) => ("fixed:git:blake3", &h[..]),
			},
			CAHash::Text(h) => ("text:sha256", &h[..]),
		};

		write!(f, "{}:{}", algo, nixbase32::encode(hash))
	}
}

#[cfg(test)]
mod tests {
	use super::CAHash;
	use crate::nixhash::NixHash;
	use hex_literal::hex;

	const ABC: [u8; 32] = hex!("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");

	/// CppNix 2.35.2 `ContentAddress::parse` reads the digest in any encoding
	/// whose length fits the algorithm, and `render` writes nix32.
	#[test]
	fn parses_every_digest_encoding_and_renders_nix32() {
		let nar = CAHash::Nar(NixHash::Sha256(ABC));
		for digest in [
			"1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s",
			"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
			"BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD",
			"ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=",
		] {
			assert_eq!(
				CAHash::from_nix_hex_str(&format!("fixed:r:sha256:{digest}")),
				Some(nar.clone()),
				"{digest}"
			);
		}
		assert_eq!(
			nar.to_string(),
			"fixed:r:sha256:1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s"
		);
		assert_eq!(
			CAHash::from_nix_hex_str(
				"text:sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
			),
			Some(CAHash::Text(ABC))
		);
		assert_eq!(
			CAHash::from_nix_hex_str("fixed:git:sha1:a9993e364706816aba3e25717850c26c9cd0d89d")
				.map(|ca| ca.to_string())
				.as_deref(),
			Some("fixed:git:sha1:kpcd173cq987hw957sx6m0868wv3x6d9")
		);
		for invalid in [
			"fixed:sha256:abc",
			"fixed:sha3:abc",
			"narwhal:sha256:1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s",
			"fixed:git:md5:900150983cd24fb0d6963f7d28e17f72",
			"text:sha1:a9993e364706816aba3e25717850c26c9cd0d89d",
			"fixed:r:sha256",
		] {
			assert_eq!(CAHash::from_nix_hex_str(invalid), None, "{invalid}");
		}
	}
}
