//! CppNix 2.35.2's ATerm derivation parser, `parseDerivation`, for the full
//! derivation model in [`super::derivation`].
//!
//! Unlike [`crate::derivation::UnverifiedDerivation`], this reads every
//! output kind the reference accepts: floating and impure content-addressed
//! outputs and the `DrvWithVersion("xp-dyn-drv", ...)` form with nested
//! dynamic input derivations, each behind the experimental feature CppNix
//! requires. [`Derivation::unparse`] writes the reference's canonical form
//! back.
//!
//! The parser follows the reference's quirks: list separators are optional
//! (`endOfList`), the first of two outputs with the same name wins while the
//! last input derivation and environment entry win, store paths are
//! canonicalised lexically before they are checked, a fixed output's path is
//! only checked to be absolute (it is recomputed when written), and bytes
//! after the closing parenthesis are ignored.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use data_encoding::{BASE64, HEXLOWER_PERMISSIVE};
use serde_json::Value;

use crate::derivation::OutputName;
use crate::nixbase32;
use crate::nixhash::{CAHash, HashAlgo, NixHash};
use crate::store_path::{self, StorePath};

use super::derivation::{
	CA_DERIVATIONS, Derivation, Features, IMPURE_DERIVATIONS, InputNode, Output,
};
use super::{DYNAMIC_DERIVATIONS, GIT_HASHING};

/// Experimental feature that allows BLAKE3 hashes.
pub const BLAKE3_HASHES: &str = "blake3-hashes";

/// A failure to parse an ATerm derivation, with the reference's message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AtermError {
	/// CppNix's `FormatError`. `readDerivation` prefixes these with
	/// `error parsing derivation '<path>': `.
	Format(String),
	/// A required experimental feature is disabled
	/// (`MissingExperimentalFeature`).
	MissingFeature {
		feature: &'static str,
		reason: String,
	},
	/// Any other error the reference raises while parsing, such as
	/// `BadStorePath`, `BadHash` or `UsageError`.
	Other(String),
	/// Input the reference accepts but this model cannot hold. The message
	/// names what is missing; it is not one of the reference's.
	Unsupported(String),
}

impl AtermError {
	/// Whether `readDerivation` wraps this error in
	/// `error parsing derivation '<path>': ...`.
	pub fn is_format_error(&self) -> bool {
		matches!(self, Self::Format(_))
	}
}

impl fmt::Display for AtermError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Format(message) | Self::Other(message) | Self::Unsupported(message) => {
				f.write_str(message)
			}
			Self::MissingFeature { feature, reason } => {
				write!(f, "experimental Nix feature '{feature}' is disabled")?;
				if !reason.is_empty() {
					write!(f, " ({reason})")?;
				}
				write!(
					f,
					"; add '--extra-experimental-features {feature}' to enable it"
				)
			}
		}
	}
}

impl std::error::Error for AtermError {}

fn format_error(message: impl Into<String>) -> AtermError {
	AtermError::Format(message.into())
}

fn require(enabled: bool, feature: &'static str, reason: &str) -> Result<(), AtermError> {
	if enabled {
		Ok(())
	} else {
		Err(AtermError::MissingFeature {
			feature,
			reason: reason.to_owned(),
		})
	}
}

/// The experimental settings the reference's parser consults.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AtermFeatures {
	pub derivation: Features,
	/// `blake3-hashes`, which `parseHashAlgo` checks for `blake3`.
	pub blake3_hashes: bool,
}

impl From<Features> for AtermFeatures {
	fn from(derivation: Features) -> Self {
		Self {
			derivation,
			blake3_hashes: false,
		}
	}
}

/// CppNix's `StringViewStream`.
struct Stream<'a> {
	rest: &'a [u8],
}

impl<'a> Stream<'a> {
	fn peek(&self) -> Option<u8> {
		self.rest.first().copied()
	}

	fn expect(&mut self, s: &str) -> Result<(), AtermError> {
		match self.rest.strip_prefix(s.as_bytes()) {
			Some(rest) => {
				self.rest = rest;
				Ok(())
			}
			None => Err(format_error(format!("expected string '{s}'"))),
		}
	}

	/// `parseString`: a C-style string with `\n`, `\r` and `\t` escapes;
	/// any other escaped byte stands for itself.
	fn string(&mut self) -> Result<Vec<u8>, AtermError> {
		self.expect("\"")?;
		let data = self.rest;
		let mut start = 0;
		let mut end = None;
		while start < data.len() {
			let Some(found) = data[start..].iter().position(|&b| b == b'"') else {
				break;
			};
			let idx = start + found;
			let mut pos = idx;
			while pos > 0 && data[pos - 1] == b'\\' {
				pos -= 1;
			}
			if (idx - pos) % 2 == 0 {
				end = Some(idx);
				break;
			}
			start = idx + 1;
		}
		let end = end.ok_or_else(|| format_error("unterminated string in derivation"))?;
		let content = &data[..end];
		self.rest = &data[end + 1..];

		let mut out = Vec::with_capacity(content.len());
		let mut i = 0;
		while i < content.len() {
			if content[i] == b'\\' {
				if i == content.len() - 1 {
					return Err(format_error("unterminated string in derivation"));
				}
				out.push(match content[i + 1] {
					b'n' => b'\n',
					b'r' => b'\r',
					b't' => b'\t',
					byte => byte,
				});
				i += 2;
			} else {
				out.push(content[i]);
				i += 1;
			}
		}
		Ok(out)
	}

	fn utf8_string(&mut self, what: &str) -> Result<String, AtermError> {
		let bytes = self.string()?;
		String::from_utf8(bytes).map_err(|_| {
			AtermError::Unsupported(format!(
				"non-UTF-8 {what} in derivation; the JSON derivation model holds text only"
			))
		})
	}

	/// `parsePath`.
	fn path(&mut self) -> Result<String, AtermError> {
		let s = self.utf8_string("path")?;
		validate_path(&s)?;
		Ok(s)
	}

	/// `endOfList`: a comma continues the list and `]` ends it; anything
	/// else is read as the next element, so separators are optional.
	fn end_of_list(&mut self) -> bool {
		match self.peek() {
			Some(b',') => {
				self.rest = &self.rest[1..];
				false
			}
			Some(b']') => {
				self.rest = &self.rest[1..];
				true
			}
			_ => false,
		}
	}

	/// `parseStrings`.
	fn strings(&mut self, are_paths: bool, what: &str) -> Result<BTreeSet<String>, AtermError> {
		let mut out = BTreeSet::new();
		self.expect("[")?;
		while !self.end_of_list() {
			out.insert(if are_paths {
				self.path()?
			} else {
				self.utf8_string(what)?
			});
		}
		Ok(out)
	}
}

/// `validatePath`.
fn validate_path(s: &str) -> Result<(), AtermError> {
	if !s.starts_with('/') {
		return Err(format_error(format!("bad path '{s}' in derivation")));
	}
	Ok(())
}

/// `canonPath` without symlink resolution: drop empty and `.` components and
/// apply `..` lexically.
fn canon_path(path: &str) -> String {
	let mut parts: Vec<&str> = Vec::new();
	for part in path.split('/') {
		match part {
			"" | "." => {}
			".." => {
				parts.pop();
			}
			part => parts.push(part),
		}
	}
	let mut out = String::with_capacity(path.len());
	for part in &parts {
		out.push('/');
		out.push_str(part);
	}
	if out.is_empty() {
		out.push('/');
	}
	out
}

/// CppNix's `checkName`, with its messages.
fn check_name(name: &str) -> Result<(), String> {
	if name.is_empty() {
		return Err("name must not be empty".to_owned());
	}
	if name.len() > 211 {
		return Err(format!(
			"name '{name}' must be no longer than 211 characters"
		));
	}
	let bytes = name.as_bytes();
	if bytes[0] == b'.' {
		if bytes.len() == 1 {
			return Err(format!("name '{name}' is not valid"));
		}
		if bytes[1] == b'-' {
			return Err(format!(
				"name '{name}' is not valid: first dash-separated component must not be '.'"
			));
		}
		if bytes[1] == b'.' {
			if bytes.len() == 2 {
				return Err(format!("name '{name}' is not valid"));
			}
			if bytes[2] == b'-' {
				return Err(format!(
					"name '{name}' is not valid: first dash-separated component must not be '..'"
				));
			}
		}
	}
	for c in name.chars() {
		if !(c.is_ascii_alphanumeric() || "+-._?=".contains(c)) {
			return Err(format!("name '{name}' contains illegal character '{c}'"));
		}
	}
	Ok(())
}

/// `StoreDirConfig::parseStorePath`, with the reference's messages.
pub fn parse_store_path(path: &str) -> Result<StorePath, AtermError> {
	if path.is_empty() {
		return Err(AtermError::Other(
			"empty path is not a valid store path".to_owned(),
		));
	}
	let canonical = canon_path(path);
	let (parent, base) = canonical.rsplit_once('/').unwrap_or(("", &canonical));
	let parent = if parent.is_empty() { "/" } else { parent };
	if parent != store_path::store_dir() {
		return Err(AtermError::Other(format!(
			"path \"{canonical}\" is not in the Nix store"
		)));
	}
	const HASH_LEN: usize = 32;
	if base.len() < HASH_LEN + 1 {
		return Err(AtermError::Other(format!(
			"'{base}' is too short to be a valid store path"
		)));
	}
	if base.as_bytes()[HASH_LEN] != b'-' {
		return Err(AtermError::Other(format!(
			"'{base}' can't name a store path because the hash part is not followed by a '-'"
		)));
	}
	if let Some(c) = base[..HASH_LEN].chars().find(|&c| {
		matches!(c, 'e' | 'o' | 'u' | 't') || !(c.is_ascii_digit() || c.is_ascii_lowercase())
	}) {
		return Err(AtermError::Other(format!(
			"store path '{base}' contains illegal base-32 character '{c}'"
		)));
	}
	check_name(&base[HASH_LEN + 1..]).map_err(|message| {
		AtermError::Other(format!(
			"path '{base}' is not a valid store path: {message}"
		))
	})?;
	StorePath::from_bytes(base.as_bytes()).map_err(|e| AtermError::Other(format!("{e}")))
}

/// `parseHashAlgo`.
fn parse_hash_algo(s: &str, features: AtermFeatures) -> Result<HashAlgo, AtermError> {
	match s {
		"blake3" => {
			require(features.blake3_hashes, BLAKE3_HASHES, "")?;
			Ok(HashAlgo::Blake3)
		}
		"md5" => Ok(HashAlgo::Md5),
		"sha1" => Ok(HashAlgo::Sha1),
		"sha256" => Ok(HashAlgo::Sha256),
		"sha512" => Ok(HashAlgo::Sha512),
		_ => Err(AtermError::Other(format!(
			"unknown hash algorithm '{s}', expect 'blake3', 'md5', 'sha1', 'sha256', or 'sha512'"
		))),
	}
}

/// `Hash::parseNonSRIUnprefixed`: the encoding is chosen by length.
fn parse_hash(s: &str, algo: HashAlgo) -> Result<NixHash, AtermError> {
	let size = algo.digest_length();
	let (digest, encoding) = if s.len() == size * 2 {
		(HEXLOWER_PERMISSIVE.decode(s.as_bytes()).ok(), "base-16")
	} else if s.len() == nixbase32::encode_len(size) {
		(nixbase32::decode(s.as_bytes()).ok(), "nix32")
	} else if s.len() == BASE64.encode_len(size) {
		(BASE64.decode(s.as_bytes()).ok(), "base-64")
	} else {
		return Err(AtermError::Other(format!(
			"hash '{s}' has wrong length for hash algorithm '{algo}'"
		)));
	};
	let digest =
		digest.ok_or_else(|| AtermError::Other(format!("invalid {encoding} hash '{s}'")))?;
	NixHash::from_algo_and_digest(algo, &digest).map_err(|e| AtermError::Other(e.to_string()))
}

/// `parseDerivationOutput`.
fn parse_output(
	path: &str,
	algo: &str,
	hash: &str,
	features: AtermFeatures,
) -> Result<Output, AtermError> {
	if algo.is_empty() {
		if path.is_empty() {
			return Ok(Output::Deferred);
		}
		validate_path(path)?;
		return Ok(Output::InputAddressed(parse_store_path(path)?));
	}

	// `ContentAddressMethod::parsePrefix`.
	let (method, algo) = if let Some(rest) = algo.strip_prefix("r:") {
		("nar", rest)
	} else if let Some(rest) = algo.strip_prefix("git:") {
		require(features.derivation.git_hashing, GIT_HASHING, "")?;
		("git", rest)
	} else if let Some(rest) = algo.strip_prefix("text:") {
		("text", rest)
	} else {
		("flat", algo)
	};
	if method == "text" {
		require(
			features.derivation.dynamic_derivations,
			DYNAMIC_DERIVATIONS,
			"text-hashed derivation output",
		)?;
	}
	let hash_algo = parse_hash_algo(algo, features)?;

	if hash == "impure" {
		require(
			features.derivation.impure_derivations,
			IMPURE_DERIVATIONS,
			"",
		)?;
		if !path.is_empty() {
			return Err(format_error(
				"impure derivation output should not specify output path",
			));
		}
		Ok(Output::Impure {
			method: method.to_owned(),
			hash_algo,
		})
	} else if !hash.is_empty() {
		validate_path(path)?;
		let hash = parse_hash(hash, hash_algo)?;
		let ca = match method {
			"flat" => CAHash::Flat(hash),
			"nar" => CAHash::Nar(hash),
			"text" => match hash {
				NixHash::Sha256(digest) => CAHash::Text(digest),
				_ => {
					return Err(AtermError::Unsupported(
						"text-hashed fixed outputs other than SHA-256 are not representable"
							.to_owned(),
					));
				}
			},
			_ => CAHash::Git(hash),
		};
		Ok(Output::Fixed(ca))
	} else {
		require(features.derivation.ca_derivations, CA_DERIVATIONS, "")?;
		if !path.is_empty() {
			return Err(format_error(
				"content-addressing derivation output should not specify output path",
			));
		}
		Ok(Output::Floating {
			method: method.to_owned(),
			hash_algo,
		})
	}
}

fn output_name(name: String) -> OutputName {
	OutputName::from_aterm_unchecked(name.into())
}

/// `parseDerivedPathMapNode`.
fn parse_node(stream: &mut Stream<'_>, dynamic: bool) -> Result<InputNode, AtermError> {
	let names = |set: BTreeSet<String>| set.into_iter().map(output_name).collect();
	if !dynamic {
		return Ok(InputNode {
			outputs: names(stream.strings(false, "output name")?),
			dynamic_outputs: BTreeMap::new(),
		});
	}
	match stream.peek() {
		Some(b'[') => Ok(InputNode {
			outputs: names(stream.strings(false, "output name")?),
			dynamic_outputs: BTreeMap::new(),
		}),
		Some(b'(') => {
			stream.expect("(")?;
			let outputs = names(stream.strings(false, "output name")?);
			stream.expect(",[")?;
			let mut dynamic_outputs = BTreeMap::new();
			while !stream.end_of_list() {
				stream.expect("(")?;
				let name = output_name(stream.utf8_string("output name")?);
				stream.expect(",")?;
				let child = parse_node(stream, dynamic)?;
				dynamic_outputs.insert(name, child);
				stream.expect(")")?;
			}
			stream.expect(")")?;
			Ok(InputNode {
				outputs,
				dynamic_outputs,
			})
		}
		_ => Err(format_error("invalid inputDrvs entry in derivation")),
	}
}

/// Read the node of one input derivation of a versioned derivation from the
/// start of `bytes`, and return it with what follows.
pub fn parse_input_node(bytes: &[u8]) -> Result<(InputNode, &[u8]), AtermError> {
	let mut stream = Stream { rest: bytes };
	let node = parse_node(&mut stream, true)?;
	Ok((node, stream.rest))
}

impl Derivation {
	/// `parseDerivation`: read the ATerm text of a derivation called `name`
	/// (its store path name without `.drv`).
	pub fn from_aterm(
		bytes: &[u8],
		name: &str,
		features: impl Into<AtermFeatures>,
	) -> Result<Self, AtermError> {
		let features = features.into();
		let mut stream = Stream { rest: bytes };
		stream.expect("D")?;
		let dynamic = match stream.peek() {
			Some(b'e') => {
				stream.expect("erive(")?;
				false
			}
			Some(b'r') => {
				stream.expect("rvWithVersion(")?;
				let version = stream.string()?;
				if version != b"xp-dyn-drv" {
					return Err(format_error(format!(
						"Unknown derivation ATerm format version '{}'",
						String::from_utf8_lossy(&version)
					)));
				}
				require(
					features.derivation.dynamic_derivations,
					DYNAMIC_DERIVATIONS,
					&format!("derivation '{name}', ATerm format version 'xp-dyn-drv'"),
				)?;
				stream.expect(",")?;
				true
			}
			_ => {
				return Err(AtermError::Other(
					"derivation does not start with 'Derive' or 'DrvWithVersion'".to_owned(),
				));
			}
		};

		let mut outputs = BTreeMap::new();
		stream.expect("[")?;
		while !stream.end_of_list() {
			stream.expect("(")?;
			let id = stream.utf8_string("output name")?;
			stream.expect(",")?;
			let path = stream.utf8_string("output path")?;
			stream.expect(",")?;
			let algo = stream.utf8_string("hash algorithm")?;
			stream.expect(",")?;
			let hash = stream.utf8_string("hash")?;
			stream.expect(")")?;
			let output = parse_output(&path, &algo, &hash, features)?;
			// `emplace`: the first output with a name wins.
			outputs.entry(output_name(id)).or_insert(output);
		}

		let mut input_derivations = BTreeMap::new();
		stream.expect(",[")?;
		while !stream.end_of_list() {
			stream.expect("(")?;
			let drv_path = stream.path()?;
			stream.expect(",")?;
			let drv_path = parse_store_path(&drv_path)?;
			let node = parse_node(&mut stream, dynamic)?;
			// `insert_or_assign`: the last entry for a path wins.
			input_derivations.insert(drv_path, node);
			stream.expect(")")?;
		}

		stream.expect(",")?;
		let input_sources = stream
			.strings(true, "input source")?
			.iter()
			.map(|path| parse_store_path(path))
			.collect::<Result<BTreeSet<_>, _>>()?;
		stream.expect(",")?;
		let system = stream.utf8_string("platform")?;
		stream.expect(",")?;
		let builder = stream.utf8_string("builder")?;

		let mut args = Vec::new();
		stream.expect(",[")?;
		while !stream.end_of_list() {
			args.push(stream.utf8_string("argument")?);
		}

		let mut env = BTreeMap::new();
		let mut structured_attrs = None;
		stream.expect(",[")?;
		while !stream.end_of_list() {
			stream.expect("(")?;
			let key = stream.utf8_string("environment variable name")?;
			stream.expect(",")?;
			let value = stream.utf8_string("environment variable value")?;
			if key == "__json" {
				let parsed: Value = serde_json::from_str(&value).map_err(|e| {
					AtermError::Other(format!("cannot process __json attribute: {e}"))
				})?;
				structured_attrs = Some(parsed);
			} else {
				env.insert(key, value);
			}
			stream.expect(")")?;
		}

		// Bytes after the closing parenthesis are not read.
		stream.expect(")")?;
		Ok(Self {
			name: name.to_owned(),
			outputs,
			system,
			builder,
			args,
			env,
			input_sources,
			input_derivations,
			structured_attrs,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const ALL: AtermFeatures = AtermFeatures {
		derivation: Features {
			ca_derivations: true,
			dynamic_derivations: true,
			impure_derivations: true,
			git_hashing: true,
		},
		blake3_hashes: true,
	};

	fn parse(s: &str, features: AtermFeatures) -> Result<Derivation, AtermError> {
		Derivation::from_aterm(s.as_bytes(), "x", features)
	}

	#[test]
	fn canon_path_is_lexical() {
		assert_eq!(canon_path("/nix//store/./a/../b/"), "/nix/store/b");
		assert_eq!(canon_path("/.."), "/");
		assert_eq!(canon_path("/"), "/");
	}

	#[test]
	fn store_path_messages() {
		let cases = [
			("", "empty path is not a valid store path"),
			("/tmp/x", "path \"/tmp/x\" is not in the Nix store"),
			("/nix/store", "path \"/nix/store\" is not in the Nix store"),
			(
				"/nix/store/abc",
				"'abc' is too short to be a valid store path",
			),
			(
				"/nix/store/g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q_foo",
				"'g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q_foo' can't name a store path because the hash part is not followed by a '-'",
			),
			(
				"/nix/store/e1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo",
				"store path 'e1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo' contains illegal base-32 character 'e'",
			),
			(
				"/nix/store/g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-..-a",
				"path 'g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-..-a' is not a valid store path: name '..-a' is not valid: first dash-separated component must not be '..'",
			),
			(
				"/nix/store/g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-a!b",
				"path 'g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-a!b' is not a valid store path: name 'a!b' contains illegal character '!'",
			),
		];
		for (path, message) in cases {
			assert_eq!(parse_store_path(path).expect_err(path).to_string(), message);
		}
		assert_eq!(
			parse_store_path("/nix//store/./g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo/")
				.unwrap()
				.to_absolute_path(),
			"/nix/store/g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo"
		);
	}

	#[test]
	fn floating_output_requires_feature() {
		let drv = r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",[],[])"#;
		let err = parse(drv, AtermFeatures::default()).unwrap_err();
		assert_eq!(
			err.to_string(),
			"experimental Nix feature 'ca-derivations' is disabled; add '--extra-experimental-features ca-derivations' to enable it"
		);
		let parsed = parse(drv, ALL).unwrap();
		assert_eq!(
			parsed.outputs[&OutputName::out()],
			Output::Floating {
				method: "nar".to_owned(),
				hash_algo: HashAlgo::Sha256
			}
		);
		assert_eq!(parsed.unparse(false, None).unwrap(), drv);
	}

	#[test]
	fn text_output_and_dynamic_version_messages() {
		let text = r#"Derive([("out","","text:sha256","")],[],[],"s","b",[],[])"#;
		assert_eq!(
			parse(text, AtermFeatures::default())
				.unwrap_err()
				.to_string(),
			"experimental Nix feature 'dynamic-derivations' is disabled (text-hashed derivation output); add '--extra-experimental-features dynamic-derivations' to enable it"
		);
		let versioned = r#"DrvWithVersion("xp-dyn-drv",[],[],[],"s","b",[],[])"#;
		assert_eq!(
			parse(versioned, AtermFeatures::default())
				.unwrap_err()
				.to_string(),
			"experimental Nix feature 'dynamic-derivations' is disabled (derivation 'x', ATerm format version 'xp-dyn-drv'); add '--extra-experimental-features dynamic-derivations' to enable it"
		);
		// Without a dynamic input the reference writes the old form.
		assert_eq!(
			parse(versioned, ALL).unwrap().unparse(false, None).unwrap(),
			r#"Derive([],[],[],"s","b",[],[])"#
		);
		let unknown = r#"DrvWithVersion("v2",[],[],[],"s","b",[],[])"#;
		let err = parse(unknown, ALL).unwrap_err();
		assert!(err.is_format_error());
		assert_eq!(
			err.to_string(),
			"Unknown derivation ATerm format version 'v2'"
		);
	}

	#[test]
	fn output_path_rules() {
		let impure_with_path =
			r#"Derive([("out","/nix/store/x","r:sha256","impure")],[],[],"s","b",[],[])"#;
		assert_eq!(
			parse(impure_with_path, ALL).unwrap_err(),
			AtermError::Format("impure derivation output should not specify output path".into())
		);
		let floating_with_path =
			r#"Derive([("out","/nix/store/x","sha256","")],[],[],"s","b",[],[])"#;
		assert_eq!(
			parse(floating_with_path, ALL).unwrap_err(),
			AtermError::Format(
				"content-addressing derivation output should not specify output path".into()
			)
		);
		let fixed_relative = r#"Derive([("out","x","sha256","0000000000000000000000000000000000000000000000000000000000000000")],[],[],"s","b",[],[])"#;
		assert_eq!(
			parse(fixed_relative, ALL).unwrap_err(),
			AtermError::Format("bad path 'x' in derivation".into())
		);
		let unknown_algo = r#"Derive([("out","","r:sha3","")],[],[],"s","b",[],[])"#;
		assert_eq!(
			parse(unknown_algo, ALL).unwrap_err().to_string(),
			"unknown hash algorithm 'sha3', expect 'blake3', 'md5', 'sha1', 'sha256', or 'sha512'"
		);
	}

	#[test]
	fn list_and_duplicate_quirks() {
		// Separators are optional, the first output and last environment
		// entry win, and trailing bytes are ignored.
		let drv = r#"Derive([("out","","",""),("out","/nix/store/g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-x","","")],[],[],"s","b",[,"a""b"],[("K","1")("K","2")])trailing"#;
		let parsed = parse(drv, AtermFeatures::default()).unwrap();
		assert_eq!(parsed.outputs[&OutputName::out()], Output::Deferred);
		assert_eq!(parsed.args, ["a", "b"]);
		assert_eq!(parsed.env["K"], "2");
	}

	#[test]
	fn not_a_derivation() {
		assert_eq!(
			parse("Dx", ALL).unwrap_err(),
			AtermError::Other("derivation does not start with 'Derive' or 'DrvWithVersion'".into())
		);
		assert_eq!(
			parse("", ALL).unwrap_err(),
			AtermError::Format("expected string 'D'".into())
		);
		assert_eq!(
			parse(r#"Derive([("out","","","),"#, ALL).unwrap_err(),
			AtermError::Format("unterminated string in derivation".into())
		);
	}
}
