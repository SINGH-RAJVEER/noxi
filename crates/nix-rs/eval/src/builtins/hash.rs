use bstr::ByteSlice;
use data_encoding::HEXLOWER;
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512, digest::Output};

use nix_rs_hash::nixhash::{HashAlgo, NixHash};

use crate::ErrorKind;

/// Reads through all data from the passed reader, and returns the resulting [Digest].
/// The exact hash function used is left generic over all [Digest].
fn hash<D: Digest + std::io::Write>(mut r: impl std::io::Read) -> Result<Output<D>, ErrorKind> {
	let mut hasher = D::new();
	std::io::copy(&mut r, &mut hasher)?;
	Ok(hasher.finalize())
}

/// For a given algo "string" and reader for data, calculate the digest
/// and return it as a hexlower encoded [String].
pub fn hash_nix_string(algo: impl AsRef<[u8]>, s: impl std::io::Read) -> Result<String, ErrorKind> {
	match algo.as_ref() {
		b"md5" => Ok(HEXLOWER.encode(hash::<Md5>(s)?.as_bstr())),
		b"sha1" => Ok(HEXLOWER.encode(hash::<Sha1>(s)?.as_bstr())),
		b"sha256" => Ok(HEXLOWER.encode(hash::<Sha256>(s)?.as_bstr())),
		b"sha512" => Ok(HEXLOWER.encode(hash::<Sha512>(s)?.as_bstr())),
		_ => Err(ErrorKind::UnknownHashType(
			algo.as_ref().as_bstr().to_string(),
		)),
	}
}

/// CppNix's `HashFormat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashFormat {
	Base16,
	Nix32,
	Base64,
	Sri,
}

/// CppNix's `parseHashFormat`, including its warning for `base32`.
pub fn parse_hash_format(name: &[u8]) -> Result<HashFormat, ErrorKind> {
	match name {
		b"base16" => Ok(HashFormat::Base16),
		b"nix32" => Ok(HashFormat::Nix32),
		b"base32" => {
			eprintln!(r#"warning: "base32" is a deprecated alias for hash format "nix32"."#);
			Ok(HashFormat::Nix32)
		}
		b"base64" => Ok(HashFormat::Base64),
		b"sri" => Ok(HashFormat::Sri),
		_ => Err(ErrorKind::EvalError(format!(
			"unknown hash format '{}', expect 'base16', 'base32', 'base64', or 'sri'",
			name.as_bstr()
		))),
	}
}

/// CppNix's `parseHashAlgo`. BLAKE3 needs the `blake3-hashes` experimental
/// feature.
pub fn parse_hash_algo(name: &[u8], blake3: bool) -> Result<HashAlgo, ErrorKind> {
	match name {
		b"md5" => Ok(HashAlgo::Md5),
		b"sha1" => Ok(HashAlgo::Sha1),
		b"sha256" => Ok(HashAlgo::Sha256),
		b"sha512" => Ok(HashAlgo::Sha512),
		b"blake3" if blake3 => Ok(HashAlgo::Blake3),
		b"blake3" => Err(blake3_disabled()),
		_ => Err(ErrorKind::EvalError(format!(
			"unknown hash algorithm '{}', expect 'blake3', 'md5', 'sha1', 'sha256', or 'sha512'",
			name.as_bstr()
		))),
	}
}

fn blake3_disabled() -> ErrorKind {
	ErrorKind::EvalError(
		"experimental Nix feature 'blake3-hashes' is disabled; add '--extra-experimental-features blake3-hashes' to enable it"
			.into(),
	)
}

/// CppNix's `Hash::parseAny`: an SRI hash, an `algo:digest` hash, or a bare
/// digest of `algo`.
pub fn parse_any(text: &[u8], algo: Option<HashAlgo>, blake3: bool) -> Result<NixHash, ErrorKind> {
	let text = text
		.to_str()
		.map_err(|_| ErrorKind::InvalidHash(text.as_bstr().to_string()))?;
	if !blake3 && (text.starts_with("blake3-") || text.starts_with("blake3:")) {
		return Err(blake3_disabled());
	}
	NixHash::from_str(text, algo).map_err(|error| match error {
		nix_rs_hash::nixhash::Error::MissingInlineHashAlgo(_) => ErrorKind::EvalError(format!(
			"hash '{text}' does not include a type, nor is the type otherwise known from context"
		)),
		error => ErrorKind::InvalidHash(format!("{text}: {error}")),
	})
}

/// CppNix's `Hash::to_string`, with the algorithm only in SRI form.
pub fn to_string(hash: &NixHash, format: HashFormat) -> String {
	let digest = hash.digest_as_bytes();
	match format {
		HashFormat::Base16 => HEXLOWER.encode(digest),
		HashFormat::Nix32 => nix_rs_hash::nixbase32::encode(digest),
		HashFormat::Base64 => data_encoding::BASE64.encode(digest),
		HashFormat::Sri => hash.to_sri_string(),
	}
}
