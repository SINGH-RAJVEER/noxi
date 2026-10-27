//! Build-time view of a derivation, including floating CA outputs. The shared
//! compatibility parser still handles the body and ordinary output types.

use std::collections::BTreeMap;

use nix_compat::derivation::UnverifiedDerivation;
use nix_compat::nixhash::HashAlgo;

use crate::{Error, FileIngestionMethod, Result};

#[derive(Clone, Debug)]
pub struct FloatingOutput {
	pub method: FileIngestionMethod,
	pub hash_algo: HashAlgo,
}

/// `basic` retains deferred output names; `floating` supplies their hash
/// methods. Use `to_aterm_bytes`, rather than serializing `basic`, to retain
/// the floating output declarations.
#[derive(Clone, Debug)]
pub struct BuildDerivation {
	pub basic: UnverifiedDerivation,
	pub floating: BTreeMap<String, FloatingOutput>,
}

fn invalid() -> Error {
	Error::Other("invalid derivation output declaration".to_owned())
}

fn take(input: &mut &[u8], prefix: &[u8]) -> Result<()> {
	*input = input.strip_prefix(prefix).ok_or_else(invalid)?;
	Ok(())
}

// CppNix parseString: backslash escapes n/r/t, and quotes any other byte.
fn string(input: &mut &[u8]) -> Result<Vec<u8>> {
	take(input, b"\"")?;
	let mut out = Vec::new();
	loop {
		let (&byte, rest) = input.split_first().ok_or_else(invalid)?;
		*input = rest;
		match byte {
			b'"' => return Ok(out),
			b'\\' => {
				let (&escaped, rest) = input.split_first().ok_or_else(invalid)?;
				*input = rest;
				out.push(match escaped {
					b'n' => b'\n',
					b'r' => b'\r',
					b't' => b'\t',
					byte => byte,
				});
			}
			byte => out.push(byte),
		}
	}
}

impl BuildDerivation {
	pub fn from_aterm_bytes(bytes: &[u8], ca_derivations: bool) -> Result<Self> {
		// Read just the output tuples, preserving the remainder for the
		// shared parser. Floating methods are represented separately, never
		// silently interpreted as input-addressed or fixed outputs.
		let mut rest = bytes;
		take(&mut rest, b"Derive([")?;
		let mut normalised = b"Derive([".to_vec();
		let mut floating = BTreeMap::new();
		let mut ordinary = false;
		let mut first = true;
		while !rest.starts_with(b"]") {
			if !first {
				take(&mut rest, b",")?;
				normalised.push(b',');
			}
			first = false;
			let start = rest;
			take(&mut rest, b"(")?;
			let name = string(&mut rest)?;
			take(&mut rest, b",")?;
			let path = string(&mut rest)?;
			take(&mut rest, b",")?;
			let algo_start = rest;
			let algo = string(&mut rest)?;
			take(&mut rest, b",")?;
			let hash = string(&mut rest)?;
			take(&mut rest, b")")?;
			if path.is_empty() && !algo.is_empty() && hash.is_empty() {
				if !ca_derivations {
					return Err(Error::Other("experimental Nix feature 'ca-derivations' is disabled; add '--extra-experimental-features ca-derivations' to enable it".to_owned()));
				}
				let algo = std::str::from_utf8(&algo).map_err(|_| invalid())?;
				let (method, algo) = if let Some(algo) = algo.strip_prefix("r:") {
					(FileIngestionMethod::NixArchive, algo)
				} else {
					(FileIngestionMethod::Flat, algo)
				};
				let hash_algo = algo.parse().map_err(|e| Error::Other(format!("{e}")))?;
				let name = String::from_utf8(name).map_err(|_| invalid())?;
				floating
					.entry(name)
					.or_insert(FloatingOutput { method, hash_algo });
				normalised.extend_from_slice(&start[..start.len() - algo_start.len()]);
				normalised.extend_from_slice(b"\"\",\"\")");
			} else {
				ordinary = true;
				normalised.extend_from_slice(&start[..start.len() - rest.len()]);
			}
		}
		if ordinary && !floating.is_empty() {
			return Err(Error::Other(
				"derivation mixes floating and other output types".to_owned(),
			));
		}
		normalised.extend_from_slice(rest);
		let basic = UnverifiedDerivation::from_aterm_bytes(&normalised)
			.map_err(|e| Error::Other(format!("error parsing derivation: {e:?}")))?;
		Ok(Self { basic, floating })
	}

	pub fn to_aterm_bytes(&self) -> Vec<u8> {
		if self.floating.is_empty() {
			return self.basic.to_aterm_bytes();
		}
		let bytes = self.basic.to_aterm_bytes();
		let mut rest = bytes.as_slice();
		take(&mut rest, b"Derive([").expect("serialized derivation");
		let mut out = b"Derive([".to_vec();
		let mut first = true;
		while !rest.starts_with(b"]") {
			if !first {
				take(&mut rest, b",").unwrap();
				out.push(b',');
			}
			first = false;
			let start = rest;
			take(&mut rest, b"(").unwrap();
			let name = String::from_utf8(string(&mut rest).unwrap()).unwrap();
			let name_end = start.len() - rest.len();
			for _ in 0..3 {
				take(&mut rest, b",").unwrap();
				string(&mut rest).unwrap();
			}
			take(&mut rest, b")").unwrap();
			let spec = &self.floating[&name];
			out.extend_from_slice(&start[..name_end]);
			out.extend_from_slice(
				format!(
					",\"\",\"{}{}\",\"\")",
					if spec.method == FileIngestionMethod::NixArchive {
						"r:"
					} else {
						""
					},
					spec.hash_algo.as_str()
				)
				.as_bytes(),
			);
		}
		out.extend_from_slice(rest);
		out
	}
}
