use std::fmt::Display;
use std::str::FromStr;

#[cfg(feature = "serde")]
use serde_with::{DeserializeFromStr, SerializeDisplay};

use crate::nixhash::Error;

/// This are the hash algorithms supported by cppnix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(DeserializeFromStr, SerializeDisplay))]
pub enum HashAlgo {
	Md5,
	Sha1,
	Sha256,
	Sha512,
	Blake3,
}

impl HashAlgo {
	// return the algo name as a &'static str.
	pub const fn as_str(&self) -> &'static str {
		match self {
			HashAlgo::Md5 => "md5",
			HashAlgo::Sha1 => "sha1",
			HashAlgo::Sha256 => "sha256",
			HashAlgo::Sha512 => "sha512",
			HashAlgo::Blake3 => "blake3",
		}
	}

	// return the prefix this would have in the SRI case.
	pub const fn sri_prefix(&self) -> &'static str {
		match self {
			HashAlgo::Md5 => "md5-",
			HashAlgo::Sha1 => "sha1-",
			HashAlgo::Sha256 => "sha256-",
			HashAlgo::Sha512 => "sha512-",
			HashAlgo::Blake3 => "blake3-",
		}
	}

	// return the number of bytes in the digest of the given hash algo.
	pub const fn digest_length(&self) -> usize {
		match self {
			HashAlgo::Sha1 => 20,
			HashAlgo::Sha256 => 32,
			HashAlgo::Sha512 => 64,
			HashAlgo::Md5 => 16,
			HashAlgo::Blake3 => 32,
		}
	}
}

impl Display for HashAlgo {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.as_str())
	}
}

impl FromStr for HashAlgo {
	type Err = Error;

	fn from_str(algo_str: &str) -> Result<Self, Self::Err> {
		match algo_str {
			"md5" => Ok(Self::Md5),
			"sha1" => Ok(Self::Sha1),
			"sha256" => Ok(Self::Sha256),
			"sha512" => Ok(Self::Sha512),
			"blake3" => Ok(Self::Blake3),
			_ => Err(Error::InvalidAlgo(algo_str.to_string())),
		}
	}
}

impl TryFrom<&str> for HashAlgo {
	type Error = Error;
	fn try_from(algo_str: &str) -> Result<Self, Self::Error> {
		algo_str.parse()
	}
}
