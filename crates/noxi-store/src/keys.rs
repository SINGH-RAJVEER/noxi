//! Ed25519 signing keys in CppNix's `NAME:BASE64` format, following
//! `src/libutil/signature/local-keys.cc`. A secret key holds libsodium's
//! 64 bytes, the seed followed by the public key.

use data_encoding::BASE64;
use ed25519::signature::Signer;

use crate::{Error, Result, ValidPathInfo};

const SECRET_KEY_BYTES: usize = 64;

/// CppNix's `base64::decode`: newlines are skipped and decoding stops at
/// the first `=`. The reference's message prints no character.
fn decode_base64(s: &str) -> std::result::Result<Vec<u8>, String> {
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

/// `Key::Key`: `NAME:BASE64` with a non-empty name and data. Errors carry
/// the reference's trace; the name is still empty when it is added.
fn parse_key(s: &str) -> Result<(String, Vec<u8>)> {
	let traced = |message: &str| {
		Error::Other(format!(
			"\n\u{2026} while decoding key named ''\n\nerror: {message}"
		))
	};
	let colon = match s.find(':') {
		Some(colon) if colon > 0 => colon,
		_ => return Err(traced("key is corrupt")),
	};
	let data = decode_base64(&s[colon + 1..]).map_err(|e| traced(&e))?;
	if data.is_empty() {
		return Err(traced("key is corrupt"));
	}
	Ok((s[..colon].to_owned(), data))
}

/// A secret signing key, `SecretKey`.
pub struct SecretKey {
	name: String,
	key: [u8; SECRET_KEY_BYTES],
}

impl SecretKey {
	/// `SecretKey::SecretKey(std::string_view)`.
	pub fn parse(s: &str) -> Result<Self> {
		let (name, data) = parse_key(s)?;
		let key = data
			.try_into()
			.map_err(|_| Error::Other("secret key is not valid".into()))?;
		Ok(Self { name, key })
	}

	/// `SecretKey::generate`: a fresh key pair from the system's random
	/// source.
	pub fn generate(name: &str) -> Result<Self> {
		let mut seed = [0u8; 32];
		let mut filled = 0;
		while filled < seed.len() {
			// SAFETY: the buffer is valid for the remaining length.
			let n = unsafe {
				libc::getrandom(seed[filled..].as_mut_ptr().cast(), seed.len() - filled, 0)
			};
			if n < 0 {
				let error = std::io::Error::last_os_error();
				if error.kind() == std::io::ErrorKind::Interrupted {
					continue;
				}
				return Err(Error::Other("key generation failed".into()));
			}
			filled += n as usize;
		}
		let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
		let mut key = [0u8; SECRET_KEY_BYTES];
		key[..32].copy_from_slice(&seed);
		key[32..].copy_from_slice(signing.verifying_key().as_bytes());
		Ok(Self {
			name: name.to_owned(),
			key,
		})
	}

	pub fn name(&self) -> &str {
		&self.name
	}

	/// `Key::to_string`.
	pub fn to_text(&self) -> String {
		format!("{}:{}", self.name, BASE64.encode(&self.key))
	}

	/// `SecretKey::toPublicKey`: libsodium copies the public half of the
	/// secret key rather than deriving it.
	pub fn to_public_text(&self) -> String {
		format!("{}:{}", self.name, BASE64.encode(&self.key[32..]))
	}

	/// `SecretKey::signDetached`, as a `NAME:BASE64` signature.
	pub fn sign_detached(&self, data: &[u8]) -> String {
		let seed: [u8; 32] = self.key[..32].try_into().expect("32-byte seed");
		let signature = ed25519_dalek::SigningKey::from_bytes(&seed).sign(data);
		format!("{}:{}", self.name, BASE64.encode(&signature.to_bytes()))
	}
}

impl ValidPathInfo {
	/// `ValidPathInfo::sign`: the signature of the path's fingerprint.
	pub fn signature(&self, key: &SecretKey) -> Result<String> {
		let fingerprint = self.fingerprint().ok_or_else(|| {
			Error::Other(format!(
				"cannot calculate fingerprint of path '{}' because its size is not known",
				self.path.to_absolute_path()
			))
		})?;
		Ok(key.sign_detached(fingerprint.as_bytes()))
	}
}

/// Order signatures as CppNix's `std::set<Signature>` does: by key name,
/// then by the raw signature bytes.
pub fn sort_signatures<'a>(sigs: impl IntoIterator<Item = &'a String>) -> Vec<&'a String> {
	let mut sorted: Vec<_> = sigs.into_iter().collect();
	sorted.sort_by_cached_key(|sig| match sig.split_once(':') {
		Some((name, data)) => (
			name.to_owned(),
			decode_base64(data).unwrap_or_default(),
			sig.to_string(),
		),
		None => (sig.to_string(), Vec::new(), sig.to_string()),
	});
	sorted
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn generated_keys_round_trip_and_verify() {
		let key = SecretKey::generate("cache.example.org-1").unwrap();
		let parsed = SecretKey::parse(&format!("{}\n", key.to_text())).unwrap();
		assert_eq!(parsed.to_public_text(), key.to_public_text());
		let public = nix_compat::narinfo::VerifyingKey::parse(&key.to_public_text()).unwrap();
		let signature = key.sign_detached(b"fingerprint");
		let signature = nix_compat::narinfo::Signature::<&str>::parse(&signature).unwrap();
		assert!(public.verify("fingerprint", &signature));
	}

	#[test]
	fn corrupt_keys_match_reference_messages() {
		for (input, message) in [
			("", "key is corrupt"),
			(":abc", "key is corrupt"),
			("foo:!!!", "invalid character in Base64 string: ''"),
		] {
			let Err(Error::Other(error)) = SecretKey::parse(input) else {
				panic!("{input} parsed");
			};
			assert!(error.ends_with(message), "{error}");
		}
		let Err(Error::Other(error)) = SecretKey::parse("foo:YWJj") else {
			panic!("short key parsed");
		};
		assert_eq!(error, "secret key is not valid");
	}

	#[test]
	fn signatures_sort_by_name_before_text() {
		let a = "k-1:AAAA".to_owned();
		let b = "k:AAAA".to_owned();
		assert_eq!(sort_signatures([&a, &b]), [&b, &a]);
	}
}
