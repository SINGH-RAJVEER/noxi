//! CppNix 2.35.2's `Hash` parsing and printing from `src/libutil/hash.cc`,
//! with its error messages, for the hash commands.
//!
//! The parser follows `parseAnyHelper`: the algorithm is split off at the
//! first `:`, otherwise at the first `-` (SRI); the encoding of an
//! unprefixed digest follows from its length; and a digest that fails to
//! decode is reported as a length mismatch, because `parseLowLevel` swallows
//! the decoder's error. BLAKE3 needs the `blake3-hashes` experimental
//! feature wherever its name is parsed.

use std::fmt;

use crate::nixbase32;
use crate::nixhash::HashAlgo;

/// `HashFormat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashFormat {
	Base16,
	Nix32,
	Base64,
	Sri,
}

impl HashFormat {
	/// `printHashFormat`.
	pub fn name(self) -> &'static str {
		match self {
			HashFormat::Base16 => "base16",
			HashFormat::Nix32 => "nix32",
			HashFormat::Base64 => "base64",
			HashFormat::Sri => "sri",
		}
	}

	/// `parseHashFormat`. The deprecated name `base32` also returns the
	/// warning CppNix prints for it.
	pub fn parse(name: &str) -> Result<(HashFormat, Option<&'static str>), Error> {
		Ok(match name {
			"base16" => (HashFormat::Base16, None),
			"nix32" => (HashFormat::Nix32, None),
			"base32" => (
				HashFormat::Nix32,
				Some(r#""base32" is a deprecated alias for hash format "nix32"."#),
			),
			"base64" => (HashFormat::Base64, None),
			"sri" => (HashFormat::Sri, None),
			_ => {
				return Err(Error::Usage(format!(
					"unknown hash format '{name}', expect 'base16', 'base32', 'base64', or 'sri'"
				)));
			}
		})
	}
}

/// An error from parsing a hash or its algorithm or format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
	/// A `UsageError`, which commands follow with the `--help` hint.
	Usage(String),
	/// `BadHash` and the missing experimental feature error.
	Bad(String),
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Error::Usage(message) | Error::Bad(message) => f.write_str(message),
		}
	}
}

impl std::error::Error for Error {}

/// The error CppNix's `ExperimentalFeatureSettings::require` raises.
pub fn missing_feature(feature: &str) -> String {
	format!(
		"experimental Nix feature '{feature}' is disabled; add '--extra-experimental-features {feature}' to enable it"
	)
}

/// `parseHashAlgo`. `blake3` is accepted only when `blake3_enabled`.
pub fn parse_algo(name: &str, blake3_enabled: bool) -> Result<HashAlgo, Error> {
	match name {
		"blake3" if !blake3_enabled => Err(Error::Bad(missing_feature("blake3-hashes"))),
		"blake3" => Ok(HashAlgo::Blake3),
		"md5" => Ok(HashAlgo::Md5),
		"sha1" => Ok(HashAlgo::Sha1),
		"sha256" => Ok(HashAlgo::Sha256),
		"sha512" => Ok(HashAlgo::Sha512),
		_ => Err(Error::Usage(format!(
			"unknown hash algorithm '{name}', expect 'blake3', 'md5', 'sha1', 'sha256', or 'sha512'"
		))),
	}
}

/// A hash as CppNix's `Hash` holds it: an algorithm and its digest, which
/// [`Hash::compress`] can shorten while keeping the algorithm's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hash {
	pub algo: HashAlgo,
	pub digest: Vec<u8>,
}

impl Hash {
	/// `Hash::to_string`.
	pub fn to_string(&self, format: HashFormat, include_algo: bool) -> String {
		let mut s = String::new();
		if format == HashFormat::Sri || include_algo {
			s.push_str(self.algo.as_str());
			s.push(if format == HashFormat::Sri { '-' } else { ':' });
		}
		match format {
			HashFormat::Base16 => s.push_str(&data_encoding::HEXLOWER.encode(&self.digest)),
			HashFormat::Nix32 => s.push_str(&nixbase32::encode(&self.digest)),
			HashFormat::Base64 | HashFormat::Sri => {
				s.push_str(&data_encoding::BASE64.encode(&self.digest))
			}
		}
		s
	}

	/// `compressHash`: the digest folded by XOR into `size` bytes.
	pub fn compress(&self, size: usize) -> Hash {
		let mut digest = vec![0; size];
		for (i, byte) in self.digest.iter().enumerate() {
			digest[i % size] ^= byte;
		}
		Hash {
			algo: self.algo,
			digest,
		}
	}
}

/// `Hash::parseAnyReturningFormat`: a hash with or without an algorithm
/// prefix, in any encoding, with the format it was written in. `want` is
/// the algorithm known from context.
pub fn parse_any_returning_format(
	original: &str,
	want: Option<HashAlgo>,
	blake3_enabled: bool,
) -> Result<(Hash, HashFormat), Error> {
	let (prefix, rest, sri) = match original.split_once(':') {
		Some((algo, rest)) => (Some(algo), rest, false),
		None => match original.split_once('-') {
			Some((algo, rest)) => (Some(algo), rest, true),
			None => (None, original, false),
		},
	};
	let parsed = prefix
		.map(|algo| parse_algo(algo, blake3_enabled))
		.transpose()?;
	let algo = match (parsed, want) {
		(None, None) => {
			return Err(Error::Bad(format!(
				"hash '{original}' does not include a type, nor is the type otherwise known from context"
			)));
		}
		(Some(parsed), Some(want)) if parsed != want => {
			return Err(Error::Bad(format!(
				"hash '{original}' should have type '{want}'"
			)));
		}
		(Some(algo), _) | (None, Some(algo)) => algo,
	};
	let format = if sri {
		HashFormat::Sri
	} else {
		format_from_size(rest, algo)?
	};
	Ok((parse_low_level(rest, algo, format, blake3_enabled)?, format))
}

/// `Hash::parseAny`.
pub fn parse_any(
	original: &str,
	want: Option<HashAlgo>,
	blake3_enabled: bool,
) -> Result<Hash, Error> {
	parse_any_returning_format(original, want, blake3_enabled).map(|(hash, _)| hash)
}

/// `baseFromSize`.
fn format_from_size(rest: &str, algo: HashAlgo) -> Result<HashFormat, Error> {
	let size = algo.digest_length();
	if rest.len() == size * 2 {
		Ok(HashFormat::Base16)
	} else if rest.len() == nixbase32::encode_len(size) {
		Ok(HashFormat::Nix32)
	} else if rest.len() == size.div_ceil(3) * 4 {
		Ok(HashFormat::Base64)
	} else {
		Err(Error::Bad(format!(
			"hash '{rest}' has wrong length for hash algorithm '{algo}'"
		)))
	}
}

/// `parseLowLevel`. The `Hash` constructor checks the BLAKE3 feature once
/// more, which matters only when the algorithm came from context.
fn parse_low_level(
	rest: &str,
	algo: HashAlgo,
	format: HashFormat,
	blake3_enabled: bool,
) -> Result<Hash, Error> {
	if algo == HashAlgo::Blake3 && !blake3_enabled {
		return Err(Error::Bad(missing_feature("blake3-hashes")));
	}
	let (decoded, name) = match format {
		HashFormat::Base16 => (decode_base16(rest), "base16"),
		HashFormat::Nix32 => (decode_nix32(rest), "nix32"),
		HashFormat::Base64 => (decode_base64(rest), "Base64"),
		HashFormat::Sri => (decode_base64(rest), "SRI"),
	};
	let digest = decoded.unwrap_or_default();
	if digest.len() != algo.digest_length() {
		return Err(Error::Bad(format!(
			"invalid {name} hash '{rest}', length {} != expected length {}",
			digest.len(),
			algo.digest_length()
		)));
	}
	Ok(Hash { algo, digest })
}

/// `base16::decode`, which accepts both cases. Callers pass an even length.
fn decode_base16(s: &str) -> Option<Vec<u8>> {
	let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
	s.as_bytes()
		.chunks(2)
		.map(|pair| Some(digit(pair[0])? << 4 | digit(*pair.get(1)?)?))
		.collect()
}

/// `BaseNix32::decode`, which returns an extra byte when the leading
/// character carries more bits than the digest has.
fn decode_nix32(s: &str) -> Option<Vec<u8>> {
	const CHARS: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
	let mut res: Vec<u8> = Vec::new();
	for (n, c) in s.bytes().rev().enumerate() {
		let digit = CHARS.iter().position(|x| *x == c)? as u16;
		let (i, j) = (n * 5 / 8, n * 5 % 8);
		res.resize(i + 1, 0);
		res[i] |= (digit << j) as u8;
		if digit >> (8 - j) != 0 {
			res.resize(i + 2, 0);
			res[i + 1] |= (digit >> (8 - j)) as u8;
		}
	}
	Some(res)
}

/// `base64::decode`: stops at the first `=`, skips newlines and keeps
/// leftover bits out of the result.
fn decode_base64(s: &str) -> Option<Vec<u8>> {
	const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
	let mut res = Vec::new();
	let (mut data, mut bits) = (0u32, 0u32);
	for c in s.bytes() {
		match c {
			b'=' => break,
			b'\n' => continue,
			_ => {}
		}
		let digit = CHARS.iter().position(|x| *x == c)? as u32;
		bits += 6;
		data = data.wrapping_shl(6) | digit;
		if bits >= 8 {
			res.push((data >> (bits - 8)) as u8);
			bits -= 8;
		}
	}
	Some(res)
}

#[cfg(test)]
mod tests {
	use super::*;

	const HEX: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

	#[test]
	fn reports_the_parsed_format() {
		let (hash, format) =
			parse_any_returning_format(&format!("sha256:{HEX}"), None, false).unwrap();
		assert_eq!(format, HashFormat::Base16);
		assert_eq!(
			hash.to_string(HashFormat::Sri, true),
			"sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0="
		);
		let (_, format) = parse_any_returning_format(
			"sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0",
			None,
			false,
		)
		.unwrap();
		assert_eq!(format, HashFormat::Sri);
	}

	#[test]
	fn decoding_errors_are_length_mismatches() {
		assert_eq!(
			parse_any(&format!("g{}", &HEX[1..]), Some(HashAlgo::Sha256), false),
			Err(Error::Bad(format!(
				"invalid base16 hash 'g{}', length 0 != expected length 32",
				&HEX[1..]
			)))
		);
		assert_eq!(
			parse_any(
				"zb8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s",
				Some(HashAlgo::Sha256),
				false
			),
			Err(Error::Bad(
				"invalid nix32 hash 'zb8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s', length 33 != expected length 32".into()
			))
		);
	}

	#[test]
	fn blake3_needs_its_feature() {
		let blake3 = "blake3:6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";
		assert_eq!(
			parse_any(blake3, None, false),
			Err(Error::Bad(missing_feature("blake3-hashes")))
		);
		assert!(parse_any(blake3, None, true).is_ok());
		assert_eq!(
			parse_algo("foo", true),
			Err(Error::Usage(
				"unknown hash algorithm 'foo', expect 'blake3', 'md5', 'sha1', 'sha256', or 'sha512'"
					.into()
			))
		);
	}

	#[test]
	fn compresses_by_folding() {
		let hash = Hash {
			algo: HashAlgo::Sha256,
			digest: (0..32).collect(),
		};
		let short = hash.compress(20);
		assert_eq!(short.digest.len(), 20);
		assert_eq!(short.digest[0], 20);
		assert_eq!(short.digest[19], 19);
	}
}
