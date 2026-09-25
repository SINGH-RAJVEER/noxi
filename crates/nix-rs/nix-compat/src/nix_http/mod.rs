use bstr::ByteSlice;
use tracing::trace;

use crate::nixbase32;

/// The mime type used for NAR files, both compressed and uncompressed
pub const MIME_TYPE_NAR: &str = "application/x-nix-nar";
/// The mime type used for NARInfo files
pub const MIME_TYPE_NARINFO: &str = "text/x-nix-narinfo";
/// The mime type used for the `nix-cache-info` file
pub const MIME_TYPE_CACHE_INFO: &str = "text/x-nix-cache-info";
/// The mime type used for `$outhash.ls` files
pub const MIME_TYPE_NAR_LISTING: &str = "application/json";

/// Parses a `14cx20k6z4hq508kqi2lm79qfld5f9mf7kiafpqsjs3zlmycza0k.nar.xz`
/// string and returns the nixbase32-decoded digest, as well as the compression
/// suffix (which might be empty).
pub fn parse_nar_str<S>(s: &S) -> Option<([u8; 32], &[u8])>
where
	S: AsRef<[u8]> + ?Sized,
{
	if s.as_ref().len() < 52 + 4 {
		trace!("nar_str too short");
		return None;
	}
	let (hash_str, suffix) = s.as_ref().split_at(52);
	// we know this is 52 bytes, so it's ok to unwrap here.
	let hash_str_fixed: [u8; 52] = hash_str.try_into().unwrap();

	// suffix needs to start with ".nar".
	let compression_suffix = suffix
		.as_bstr()
		.strip_prefix(b".nar")
		.ok_or_else(|| {
			trace!("suffix does not start with .nar");
		})
		.ok()?;

	let digest = nixbase32::decode_fixed(hash_str_fixed)
		.inspect_err(|err| {
			trace!(%err, "invalid nixbase32 encoding");
		})
		.ok()?;

	Some((digest, compression_suffix))
}

#[derive(Debug, PartialEq, Eq)]
pub enum RequestType {
	Narinfo,
	Listing,
}

/// Parses a `3mzh8lvgbynm9daj7c82k2sfsfhrsfsy.narinfo` or `3mzh8lvgbynm9daj7c82k2sfsfhrsfsy.ls`
/// string and returns the nixbase32-decoded digest, and what was requested.
pub fn parse_outhash_str(s: impl AsRef<[u8]>) -> Option<([u8; 20], RequestType)> {
	if s.as_ref().len() < 32 + 3 {
		trace!("outhash_str too short");
		return None;
	}

	let (hash_str, suffix) = s.as_ref().split_at(32);
	// we know this is 32 bytes, so it's ok to unwrap here.
	let hash_str_fixed: [u8; 32] = hash_str.try_into().unwrap();

	let request_type = match suffix {
		b".narinfo" => RequestType::Narinfo,
		b".ls" => RequestType::Listing,
		_ => {
			trace!("invalid string, no .narinfo or .ls suffix");
			return None;
		}
	};

	let digest = nixbase32::decode_fixed(hash_str_fixed)
		.inspect_err(|err| {
			trace!(%err, "invalid nixbase32 encoding");
		})
		.ok()?;

	Some((digest, request_type))
}

#[cfg(test)]
mod test {
	use crate::nix_http::RequestType;

	use super::{parse_nar_str, parse_outhash_str};
	use hex_literal::hex;

	#[test]
	fn parse_nar_str_success() {
		assert_eq!(
			(
				hex!("13a8cf7ca57f68a9f1752acee36a72a55187d3a954443c112818926f26109d91"),
				"".as_bytes()
			),
			parse_nar_str("14cx20k6z4hq508kqi2lm79qfld5f9mf7kiafpqsjs3zlmycza0k.nar").unwrap()
		);

		assert_eq!(
			(
				hex!("13a8cf7ca57f68a9f1752acee36a72a55187d3a954443c112818926f26109d91"),
				".xz".as_bytes()
			),
			parse_nar_str("14cx20k6z4hq508kqi2lm79qfld5f9mf7kiafpqsjs3zlmycza0k.nar.xz").unwrap()
		)
	}

	#[test]
	fn parse_nar_str_failure() {
		assert!(parse_nar_str("14cx20k6z4hq508kqi2lm79qfld5f9mf7kiafpqsjs3zlmycza0").is_none());
		assert!(
			parse_nar_str("14cx20k6z4hq508kqi2lm79qfld5f9mf7kiafpqsjs3zlmycza0🦊.nar").is_none()
		)
	}
	#[test]
	fn parse_outhash_str_success() {
		assert_eq!(
			(
				hex!("8a12321522fd91efbd60ebb2481af88580f61600"),
				RequestType::Narinfo
			),
			parse_outhash_str("00bgd045z0d4icpbc2yyz4gx48ak44la.narinfo").unwrap()
		);
		assert_eq!(
			(
				hex!("8a12321522fd91efbd60ebb2481af88580f61600"),
				RequestType::Listing
			),
			parse_outhash_str("00bgd045z0d4icpbc2yyz4gx48ak44la.ls").unwrap()
		);
	}

	#[test]
	fn parse_outhash_str_failure() {
		assert!(parse_outhash_str("00bgd045z0d4icpbc2yyz4gx48ak44la").is_none());
		assert!(parse_outhash_str("/00bgd045z0d4icpbc2yyz4gx48ak44la").is_none());
		assert!(parse_outhash_str("000000").is_none());
		assert!(parse_outhash_str("00bgd045z0d4icpbc2yyz4gx48ak44l🦊.narinfo").is_none());
		assert!(parse_outhash_str("00bgd045z0d4icpbc2yyz4gx48ak44la.nah").is_none());
	}
}
