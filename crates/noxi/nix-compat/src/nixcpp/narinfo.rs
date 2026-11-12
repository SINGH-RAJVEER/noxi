//! `.narinfo` files as CppNix 2.35.2 reads and writes them
//! (`src/libstore/nar-info.cc`).
//!
//! [`crate::narinfo::NarInfo`] is strict and round-trips the quirks of
//! existing caches. This reader instead accepts and rejects what the
//! reference does, with its error messages: unknown fields are ignored,
//! later fields replace earlier ones, an empty `Compression` means `bzip2`,
//! and hashes may use any encoding the reference accepts.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use data_encoding::BASE64;

use crate::cppnix_json::Signature;
use crate::nixbase32;
use crate::nixhash::{CAHash, HashAlgo, NixHash};
use crate::store_path::{StorePath, store_dir};

/// A parsed `.narinfo` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NarInfo {
	pub path: StorePath,
	/// The NAR's location, relative to the cache.
	pub url: String,
	/// A name `parseCompressionAlgo` accepts; a missing or empty field reads
	/// as `bzip2`.
	pub compression: String,
	pub file_hash: Option<NixHash>,
	/// Zero when absent.
	pub file_size: u64,
	pub nar_hash: NixHash,
	pub nar_size: u64,
	pub references: BTreeSet<StorePath>,
	/// The deriver; the `.drv` suffix is part of the name.
	pub deriver: Option<StorePath>,
	pub sigs: BTreeSet<Signature>,
	pub ca: Option<CAHash>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
	/// The file's own structure is wrong.
	#[error(
		"NAR info file '{whence}' is corrupt: {reason}{}",
		line.map(|line| format!(" at line {line}")).unwrap_or_default()
	)]
	Corrupt {
		whence: String,
		reason: &'static str,
		/// The entry, counted from one, or `None` for a missing field.
		line: Option<usize>,
	},
	/// An error the reference passes through from its store path, hash,
	/// signature or content-address parsers.
	#[error("{0}")]
	Field(String),
}

/// The compression methods `parseCompressionAlgo` knows.
pub const COMPRESSION_ALGOS: &[&str] = &[
	"none", "br", "bzip2", "compress", "grzip", "gzip", "lrzip", "lz4", "lzip", "lzma", "lzop",
	"xz", "zstd",
];

impl NarInfo {
	/// Parses `text`, which the reference reads from the file `whence`.
	pub fn parse(text: &[u8], whence: &str) -> Result<Self, Error> {
		let mut line = 1;
		let corrupt = |reason, line| Error::Corrupt {
			whence: whence.to_owned(),
			reason,
			line: Some(line),
		};

		let mut path = None;
		let mut url = String::new();
		let mut compression = None;
		let mut file_hash = None;
		let mut file_size = 0;
		let mut nar_hash = None;
		let mut nar_size = 0;
		let mut references = BTreeSet::new();
		let mut deriver = None;
		let mut sigs = BTreeSet::new();
		let mut ca = None;

		let mut pos = 0;
		while pos < text.len() {
			let colon = find(text, b':', pos).ok_or_else(|| corrupt("expecting ':'", line))?;
			let name = &text[pos..colon];
			// The value starts two bytes after the colon, whatever they are.
			let eol =
				find(text, b'\n', colon + 2).ok_or_else(|| corrupt("expecting '\\n'", line))?;
			let value = String::from_utf8_lossy(&text[colon + 2..eol]);
			let value = value.as_ref();

			let hash_field = |value: &str| match parse_any_prefixed(value) {
				Ok(hash) => Ok(hash),
				Err(HashError::Bad(_)) => Err(corrupt("bad hash", line)),
				Err(HashError::Other(message)) => Err(Error::Field(message)),
			};

			match name {
				b"StorePath" => path = Some(parse_store_path(value).map_err(Error::Field)?),
				b"URL" => url = value.to_owned(),
				b"Compression" => {
					compression = Some(if value.is_empty() {
						"bzip2".to_owned()
					} else if COMPRESSION_ALGOS.contains(&value) {
						value.to_owned()
					} else {
						return Err(corrupt("invalid Compression", line));
					})
				}
				b"FileHash" => file_hash = Some(hash_field(value)?),
				b"FileSize" => {
					file_size = parse_u64(value).ok_or_else(|| corrupt("invalid FileSize", line))?
				}
				b"NarHash" => nar_hash = Some(hash_field(value)?),
				b"NarSize" => {
					nar_size = parse_u64(value).ok_or_else(|| corrupt("invalid NarSize", line))?
				}
				b"References" => {
					let refs: Vec<&str> = value.split(' ').filter(|r| !r.is_empty()).collect();
					if !references.is_empty() {
						return Err(corrupt("extra References", line));
					}
					for r in refs {
						references.insert(parse_store_path_base(r).map_err(Error::Field)?);
					}
				}
				b"Deriver" => {
					if value != "unknown-deriver" {
						deriver = Some(parse_store_path_base(value).map_err(Error::Field)?);
					}
				}
				b"Sig" => {
					sigs.insert(parse_signature(value).map_err(Error::Field)?);
				}
				b"CA" => {
					if ca.is_some() {
						return Err(corrupt("extra CA", line));
					}
					if !value.is_empty() {
						ca = Some(parse_content_address(value).map_err(Error::Field)?);
					}
				}
				_ => {}
			}

			pos = eol + 1;
			line += 1;
		}

		let missing = |reason| Error::Corrupt {
			whence: whence.to_owned(),
			reason,
			line: None,
		};
		let path = path.ok_or_else(|| missing("StorePath missing"))?;
		let nar_hash = nar_hash.ok_or_else(|| missing("NarHash missing"))?;
		if url.is_empty() {
			return Err(missing("URL missing"));
		}
		if nar_size == 0 {
			return Err(missing("NarSize missing or zero"));
		}

		Ok(NarInfo {
			path,
			url,
			compression: compression.unwrap_or_else(|| "bzip2".to_owned()),
			file_hash,
			file_size,
			nar_hash,
			nar_size,
			references,
			deriver,
			sigs,
			ca,
		})
	}
}

/// `NarInfo::to_string`. The reference asserts that both hashes are
/// SHA-256; other algorithms are written the same way.
impl std::fmt::Display for NarInfo {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let nix32 = |hash: &NixHash| {
			format!(
				"{}:{}",
				hash.algo(),
				nixbase32::encode(hash.digest_as_bytes())
			)
		};
		writeln!(f, "StorePath: {}", self.path.to_absolute_path())?;
		writeln!(f, "URL: {}", self.url)?;
		writeln!(f, "Compression: {}", self.compression)?;
		if let Some(hash) = &self.file_hash {
			writeln!(f, "FileHash: {}", nix32(hash))?;
		}
		if self.file_size != 0 {
			writeln!(f, "FileSize: {}", self.file_size)?;
		}
		writeln!(f, "NarHash: {}", nix32(&self.nar_hash))?;
		writeln!(f, "NarSize: {}", self.nar_size)?;
		let mut refs = String::new();
		for (i, r) in self.references.iter().enumerate() {
			if i > 0 {
				refs.push(' ');
			}
			write!(refs, "{r}")?;
		}
		writeln!(f, "References: {refs}")?;
		if let Some(deriver) = &self.deriver {
			writeln!(f, "Deriver: {deriver}")?;
		}
		for sig in &self.sigs {
			writeln!(f, "Sig: {}:{}", sig.key_name, BASE64.encode(&sig.sig))?;
		}
		if let Some(ca) = &self.ca {
			writeln!(f, "CA: {ca}")?;
		}
		Ok(())
	}
}

fn find(text: &[u8], byte: u8, from: usize) -> Option<usize> {
	text.get(from..)?
		.iter()
		.position(|b| *b == byte)
		.map(|i| from + i)
}

/// `string2Int<uint64_t>`: decimal digits only, without a sign.
fn parse_u64(s: &str) -> Option<u64> {
	if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
		return None;
	}
	s.parse().ok()
}

/// The `StorePath` constructor and `checkName`.
pub fn parse_store_path_base(base: &str) -> Result<StorePath, String> {
	const HASH_LEN: usize = 32;
	let bytes = base.as_bytes();
	if bytes.len() < HASH_LEN + 1 {
		return Err(format!("'{base}' is too short to be a valid store path"));
	}
	if bytes[HASH_LEN] != b'-' {
		return Err(format!(
			"'{base}' can't name a store path because the hash part is not followed by a '-'"
		));
	}
	if let Some(c) = bytes[..HASH_LEN].iter().find(|c| {
		matches!(c, b'e' | b'o' | b'u' | b't') || !(c.is_ascii_digit() || c.is_ascii_lowercase())
	}) {
		return Err(format!(
			"store path '{base}' contains illegal base-32 character '{}'",
			*c as char
		));
	}
	let name = &base[HASH_LEN + 1..];
	check_name(name).map_err(|e| format!("path '{base}' is not a valid store path: {e}"))?;
	StorePath::from_bytes(bytes)
		.map_err(|e| format!("path '{base}' is not a valid store path: {e}"))
}

fn check_name(name: &str) -> Result<(), String> {
	if name.is_empty() {
		return Err("name must not be empty".into());
	}
	if name.len() > 211 {
		return Err(format!(
			"name '{name}' must be no longer than 211 characters"
		));
	}
	if name == "." || name == ".." {
		return Err(format!("name '{name}' is not valid"));
	}
	for dots in ["..", "."] {
		if name.starts_with(&format!("{dots}-")) {
			return Err(format!(
				"name '{name}' is not valid: first dash-separated component must not be '{dots}'"
			));
		}
	}
	if let Some(c) = name
		.chars()
		.find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.' | '_' | '?' | '=')))
	{
		return Err(format!("name '{name}' contains illegal character '{c}'"));
	}
	Ok(())
}

/// `StoreDirConfig::parseStorePath`: the path is made canonical lexically
/// and must be directly inside the store.
pub fn parse_store_path(path: &str) -> Result<StorePath, String> {
	if path.is_empty() {
		return Err("empty path is not a valid store path".into());
	}
	if !path.starts_with('/') {
		return Err(format!("not an absolute path: \"{path}\""));
	}
	let mut components = Vec::new();
	for component in path.split('/') {
		match component {
			"" | "." => {}
			".." => {
				components.pop();
			}
			_ => components.push(component),
		}
	}
	let canonical = format!("/{}", components.join("/"));
	match canonical.rsplit_once('/') {
		Some((parent, base)) if parent == store_dir() => parse_store_path_base(base),
		_ => Err(format!("path \"{canonical}\" is not in the Nix store")),
	}
}

/// `Signature::parse`, with `base64::decode`, which stops at `=` and skips
/// newlines.
pub fn parse_signature(s: &str) -> Result<Signature, String> {
	let corrupt = || "signature is corrupt".to_owned();
	let (key_name, data) = s
		.split_once(':')
		.filter(|(key_name, _)| !key_name.is_empty())
		.ok_or_else(corrupt)?;
	let sig = decode_base64(data)?;
	if sig.is_empty() {
		return Err(corrupt());
	}
	Ok(Signature {
		key_name: key_name.to_owned(),
		sig,
	})
}

/// `base64::decode`. The reference's message shows no character: its
/// format string does not print the coloured character it is given.
fn decode_base64(s: &str) -> Result<Vec<u8>, String> {
	let mut res = Vec::new();
	let mut data = 0u32;
	let mut bits = 0;
	for byte in s.bytes() {
		let digit = match byte {
			b'=' => break,
			b'\n' => continue,
			b'A'..=b'Z' => byte - b'A',
			b'a'..=b'z' => byte - b'a' + 26,
			b'0'..=b'9' => byte - b'0' + 52,
			b'+' => 62,
			b'/' => 63,
			_ => return Err("invalid character in Base64 string: ''".into()),
		};
		bits += 6;
		data = data.wrapping_shl(6) | u32::from(digit);
		if bits >= 8 {
			res.push((data >> (bits - 8)) as u8);
			bits -= 8;
		}
	}
	Ok(res)
}

fn decode_base16(s: &str) -> Option<Vec<u8>> {
	data_encoding::HEXLOWER_PERMISSIVE.decode(s.as_bytes()).ok()
}

/// `BaseNix32::decode`, which can return more bytes than the digest has
/// when the leading character carries extra bits.
fn decode_nix32(s: &str) -> Option<Vec<u8>> {
	const CHARS: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
	let mut res: Vec<u8> = Vec::new();
	for (n, c) in s.bytes().rev().enumerate() {
		let digit = CHARS.iter().position(|x| *x == c)? as u16;
		let b = n * 5;
		let (i, j) = (b / 8, b % 8);
		res.resize(i + 1, 0);
		res[i] |= (digit << j) as u8;
		if digit >> (8 - j) != 0 {
			res.resize(i + 2, 0);
			res[i + 1] |= (digit >> (8 - j)) as u8;
		}
	}
	Some(res)
}

enum HashError {
	/// `BadHash`, which narinfo hash fields report as a bad hash.
	Bad(String),
	/// Errors from `parseHashAlgo`, which pass through.
	Other(String),
}

impl HashError {
	fn message(self) -> String {
		match self {
			HashError::Bad(message) | HashError::Other(message) => message,
		}
	}
}

/// `parseHashAlgo`, without the `blake3-hashes` experimental feature.
fn parse_hash_algo(s: &str) -> Result<HashAlgo, HashError> {
	match s {
		"md5" => Ok(HashAlgo::Md5),
		"sha1" => Ok(HashAlgo::Sha1),
		"sha256" => Ok(HashAlgo::Sha256),
		"sha512" => Ok(HashAlgo::Sha512),
		"blake3" => Err(HashError::Other(
			"experimental Nix feature 'blake3-hashes' is disabled; add '--extra-experimental-features blake3-hashes' to enable it".into(),
		)),
		_ => Err(HashError::Other(format!(
			"unknown hash algorithm '{s}', expect 'blake3', 'md5', 'sha1', 'sha256', or 'sha512'"
		))),
	}
}

/// `parseLowLevel`: a decoding error leaves the digest empty, so it is
/// reported as a length mismatch.
fn hash_from_digest(
	rest: &str,
	algo: HashAlgo,
	encoding: &str,
	digest: Option<Vec<u8>>,
) -> Result<NixHash, HashError> {
	let digest = digest.unwrap_or_default();
	if digest.len() != algo.digest_length() {
		return Err(HashError::Bad(format!(
			"invalid {encoding} hash '{rest}', length {} != expected length {}",
			digest.len(),
			algo.digest_length()
		)));
	}
	Ok(NixHash::from_algo_and_digest(algo, &digest).expect("digest length was checked"))
}

/// `Hash::parseNonSRIUnprefixed`: the encoding follows from the length.
fn parse_unprefixed(rest: &str, algo: HashAlgo) -> Result<NixHash, HashError> {
	let size = algo.digest_length();
	if rest.len() == size * 2 {
		hash_from_digest(rest, algo, "base16", decode_base16(rest))
	} else if rest.len() == nixbase32::encode_len(size) {
		hash_from_digest(rest, algo, "nix32", decode_nix32(rest))
	} else if rest.len() == BASE64.encode_len(size) {
		hash_from_digest(rest, algo, "Base64", decode_base64(rest).ok())
	} else {
		Err(HashError::Bad(format!(
			"hash '{rest}' has wrong length for hash algorithm '{algo}'"
		)))
	}
}

/// `Hash::parseAnyPrefixed`: `<algo>:<hash>` in any encoding, or SRI.
fn parse_any_prefixed(s: &str) -> Result<NixHash, HashError> {
	if let Some((algo, rest)) = s.split_once(':') {
		parse_unprefixed(rest, parse_hash_algo(algo)?)
	} else if let Some((algo, rest)) = s.split_once('-') {
		let algo = parse_hash_algo(algo)?;
		hash_from_digest(rest, algo, "SRI", decode_base64(rest).ok())
	} else {
		Err(HashError::Bad(format!(
			"hash '{s}' does not include a type"
		)))
	}
}

/// `ContentAddress::parse`, without the `git-hashing` experimental feature.
pub fn parse_content_address(s: &str) -> Result<CAHash, String> {
	let (prefix, rest) = s.split_once(':').ok_or_else(|| {
		format!("not a content address because it is not in the form '<prefix>:<rest>': {s}")
	})?;
	let (method, rest) = match prefix {
		"text" => ("text", rest),
		"fixed" => {
			if let Some(rest) = rest.strip_prefix("r:") {
				("nar", rest)
			} else if rest.starts_with("git:") {
				return Err("experimental Nix feature 'git-hashing' is disabled; add '--extra-experimental-features git-hashing' to enable it".into());
			} else {
				("flat", rest)
			}
		}
		_ => {
			return Err(format!(
				"content address prefix '{prefix}' is unrecognized. Recogonized prefixes are 'text' or 'fixed'"
			));
		}
	};
	let (algo, rest) = rest.split_once(':').ok_or_else(|| {
		format!("content address hash must be in form '<algo>:<hash>', but found: {s}")
	})?;
	let hash = parse_unprefixed(rest, parse_hash_algo(algo).map_err(HashError::message)?)
		.map_err(HashError::message)?;
	Ok(match (method, hash) {
		("text", NixHash::Sha256(digest)) => CAHash::Text(digest),
		// The reference accepts this and later fails an assertion.
		("text", hash) => {
			return Err(format!(
				"text content address with a {} hash is not supported",
				hash.algo()
			));
		}
		("nar", hash) => CAHash::Nar(hash),
		(_, hash) => CAHash::Flat(hash),
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	const GOOD: &str = "StorePath: /nix/store/xp0dpr9yz5h0fk5c5lpj7zzqjbdh3w8c-f
URL: nar/04zwf782yjwnh3q6hz5izfd6jyip8kgw6g6yj43fiqhbyhdd0dqw.nar
Compression: none
FileHash: sha256:04zwf782yjwnh3q6hz5izfd6jyip8kgw6g6yj43fiqhbyhdd0dqw
FileSize: 120
NarHash: sha256:04zwf782yjwnh3q6hz5izfd6jyip8kgw6g6yj43fiqhbyhdd0dqw
NarSize: 120
References: \n\
CA: fixed:r:sha256:04zwf782yjwnh3q6hz5izfd6jyip8kgw6g6yj43fiqhbyhdd0dqw
";

	#[test]
	fn round_trip() {
		let info = NarInfo::parse(GOOD.as_bytes(), "x.narinfo").unwrap();
		assert_eq!(info.to_string(), GOOD);
	}

	#[test]
	fn hashes_in_every_encoding() {
		let nix32 =
			parse_any_prefixed("sha256:04zwf782yjwnh3q6hz5izfd6jyip8kgw6g6yj43fiqhbyhdd0dqw")
				.ok()
				.unwrap();
		let hex = format!(
			"sha256:{}",
			data_encoding::HEXLOWER.encode(nix32.digest_as_bytes())
		);
		let sri = nix32.to_sri_string();
		let b64 = format!("sha256:{}", BASE64.encode(nix32.digest_as_bytes()));
		for s in [hex, sri, b64] {
			assert_eq!(parse_any_prefixed(&s).ok(), Some(nix32.clone()), "{s}");
		}
	}

	#[test]
	fn nix32_overflow_is_a_length_error() {
		let s = format!("z{}", "0".repeat(51));
		match parse_unprefixed(&s, HashAlgo::Sha256) {
			Err(HashError::Bad(message)) => {
				assert_eq!(
					message,
					format!("invalid nix32 hash '{s}', length 33 != expected length 32")
				)
			}
			_ => panic!(),
		}
	}
}
