//! Build-time view of a derivation, including floating CA outputs. The shared
//! compatibility parser still handles the body and ordinary output types.

use std::collections::BTreeMap;

use nix_compat::cppnix_json::derivation::InputNode;
use nix_compat::cppnix_json::derivation_aterm::parse_input_node;
use nix_compat::cppnix_json::derivation_hash::unparse_node;
use nix_compat::derivation::{OutputName, UnverifiedDerivation};
use nix_compat::nixhash::HashAlgo;
use nix_compat::store_path::StorePath;

use crate::{Error, FileIngestionMethod, Result};

#[derive(Clone, Debug)]
pub struct FloatingOutput {
	pub method: FileIngestionMethod,
	pub hash_algo: HashAlgo,
}

/// The additional content-address methods used in experimental declarations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExperimentalMethod {
	Flat,
	NixArchive,
	Text,
	Git,
}

impl ExperimentalMethod {
	/// How a store computes a content address of this method.
	pub fn ingestion_method(self) -> FileIngestionMethod {
		match self {
			Self::Flat => FileIngestionMethod::Flat,
			Self::NixArchive => FileIngestionMethod::NixArchive,
			Self::Text => FileIngestionMethod::Text,
			Self::Git => FileIngestionMethod::Git,
		}
	}

	pub fn parse(name: &str) -> Result<Self> {
		match name {
			"flat" => Ok(Self::Flat),
			"nar" | "recursive" => Ok(Self::NixArchive),
			"text" => Ok(Self::Text),
			"git" => Ok(Self::Git),
			_ => Err(invalid()),
		}
	}

	/// CppNix StoreDirConfig::makeFixedOutputPathFromCA, with no references.
	pub fn fixed_path(
		self,
		name: &str,
		hash: &nix_compat::nixhash::NixHash,
	) -> Result<nix_compat::store_path::StorePath> {
		use nix_compat::{nixhash::NixHash, store_path};
		match self {
			Self::Text => {
				let NixHash::Sha256(hash) = hash else {
					return Err(Error::Other("text-hashed outputs require SHA-256".into()));
				};
				store_path::build_text_path_from_content_digest(name, *hash, [])
					.map(|path| path.to_owned())
					.map_err(|e| Error::Other(e.to_string()))
			}
			Self::Git => {
				if !matches!(hash.algo(), HashAlgo::Sha1 | HashAlgo::Sha256) {
					return Err(Error::Other(format!(
						"Git file ingestion must use SHA-1 or SHA-256 hash, but instead using: {}",
						hash.algo().as_str()
					)));
				}
				store_path::build_git_path(name, hash)
					.map(|path| path.to_owned())
					.map_err(|e| Error::Other(e.to_string()))
			}
			Self::Flat | Self::NixArchive => {
				store_path::build_ca_path(name, self == Self::NixArchive, hash, [], false)
					.map(|path| path.to_owned())
					.map_err(|e| Error::Other(e.to_string()))
			}
		}
	}

	fn prefix(self) -> &'static str {
		match self {
			Self::Flat => "",
			Self::NixArchive => "r:",
			Self::Text => "text:",
			Self::Git => "git:",
		}
	}
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OutputFeatures {
	pub ca_derivations: bool,
	pub impure_derivations: bool,
	pub dynamic_derivations: bool,
	pub git_hashing: bool,
}

fn require_feature(enabled: bool, name: &str, reason: &str) -> Result<()> {
	if enabled {
		Ok(())
	} else {
		Err(Error::Other(format!(
			"experimental Nix feature '{name}' is disabled{reason}; add '--extra-experimental-features {name}' to enable it"
		)))
	}
}

/// Output declarations that can be constructed and registered before their
/// build implementation is available. Their method and impurity are retained
/// exactly in ATerm instead of treating them as ordinary deferred outputs.
#[derive(Clone, Debug)]
pub struct ExperimentalOutput {
	pub method: ExperimentalMethod,
	pub hash_algo: HashAlgo,
	pub impure: bool,
	pub fixed: Option<(
		nix_compat::store_path::StorePath,
		nix_compat::nixhash::NixHash,
	)>,
}

/// `basic` retains deferred output names; `floating` supplies their hash
/// methods. Use `to_aterm_bytes`, rather than serializing `basic`, to retain
/// the floating output declarations.
#[derive(Clone, Debug)]
pub struct BuildDerivation {
	pub basic: UnverifiedDerivation,
	pub floating: BTreeMap<String, FloatingOutput>,
	pub experimental: BTreeMap<String, ExperimentalOutput>,
	/// Inputs that are outputs of derivations that input derivations produce,
	/// by the output that is the derivation. Each input derivation here is
	/// also one of `basic`, possibly without outputs of its own.
	pub dynamic: BTreeMap<StorePath, BTreeMap<OutputName, InputNode>>,
}

impl BuildDerivation {
	/// `DerivationType::isImpure`.
	pub fn is_impure(&self) -> bool {
		self.experimental.values().any(|output| output.impure)
	}

	/// `DerivationType::isFixed`, by any content address method.
	pub fn is_fixed(&self) -> bool {
		self.basic.outputs().is_fixed()
			|| !self.experimental.is_empty()
				&& self
					.experimental
					.values()
					.all(|output| output.fixed.is_some())
	}

	/// Outputs without a path until they are realised, other than impure
	/// ones.
	pub fn has_floating_outputs(&self) -> bool {
		!self.floating.is_empty()
			|| self
				.experimental
				.values()
				.any(|output| !output.impure && output.fixed.is_none())
	}

	/// Input-addressed outputs that wait for content-addressed inputs.
	pub fn is_deferred(&self) -> bool {
		self.basic.outputs().is_deferred()
			&& self.floating.is_empty()
			&& self.experimental.is_empty()
	}
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
		Self::from_aterm_bytes_with_features(
			bytes,
			OutputFeatures {
				ca_derivations,
				..OutputFeatures::default()
			},
		)
	}

	pub fn from_aterm_bytes_with_features(bytes: &[u8], features: OutputFeatures) -> Result<Self> {
		// Read just the output tuples, preserving the remainder for the
		// shared parser. Floating methods are represented separately, never
		// silently interpreted as input-addressed or fixed outputs.
		let mut rest = bytes;
		let versioned = rest.starts_with(b"DrvWithVersion(");
		if versioned {
			take(&mut rest, b"DrvWithVersion(\"xp-dyn-drv\",[")?;
			require_feature(features.dynamic_derivations, "dynamic-derivations", "")?;
		} else {
			take(&mut rest, b"Derive([")?;
		}
		let mut normalised = b"Derive([".to_vec();
		let mut floating = BTreeMap::new();
		let mut experimental = BTreeMap::new();
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
			if (!algo.is_empty() && path.is_empty() && (hash.is_empty() || hash == b"impure"))
				|| (algo.starts_with(b"text:") || algo.starts_with(b"git:")) && !hash.is_empty()
			{
				let algo = std::str::from_utf8(&algo).map_err(|_| invalid())?;
				let (method, algo) = if let Some(algo) = algo.strip_prefix("r:") {
					("nar", algo)
				} else if let Some(algo) = algo.strip_prefix("text:") {
					("text", algo)
				} else if let Some(algo) = algo.strip_prefix("git:") {
					("git", algo)
				} else {
					("flat", algo)
				};
				if method == "git" {
					require_feature(features.git_hashing, "git-hashing", "")?;
				}
				if method == "text" {
					require_feature(
						features.dynamic_derivations,
						"dynamic-derivations",
						" (text-hashed derivation output)",
					)?;
				}
				let hash_algo = algo.parse().map_err(|e| Error::Other(format!("{e}")))?;
				if hash == b"impure" {
					require_feature(features.impure_derivations, "impure-derivations", "")?;
				} else if hash.is_empty() {
					require_feature(features.ca_derivations, "ca-derivations", "")?;
				}
				let fixed = if !path.is_empty() && !hash.is_empty() {
					Some((
						nix_compat::store_path::StorePath::from_absolute_path(&path)
							.map_err(|_| invalid())?,
						nix_compat::nixhash::NixHash::from_str(
							std::str::from_utf8(&hash).map_err(|_| invalid())?,
							Some(hash_algo),
						)
						.map_err(|_| invalid())?,
					))
				} else {
					None
				};
				let name = String::from_utf8(name).map_err(|_| invalid())?;
				if hash == b"impure" || matches!(method, "text" | "git") {
					experimental.entry(name).or_insert(ExperimentalOutput {
						method: ExperimentalMethod::parse(method)?,
						hash_algo,
						impure: hash == b"impure",
						fixed,
					});
				} else {
					floating.entry(name).or_insert(FloatingOutput {
						method: if method == "nar" {
							FileIngestionMethod::NixArchive
						} else {
							FileIngestionMethod::Flat
						},
						hash_algo,
					});
				}
				normalised.extend_from_slice(&start[..start.len() - algo_start.len()]);
				normalised.extend_from_slice(b"\"\",\"\")");
			} else {
				ordinary = true;
				normalised.extend_from_slice(&start[..start.len() - rest.len()]);
			}
		}
		if ordinary && (!floating.is_empty() || !experimental.is_empty()) {
			return Err(Error::Other(
				"derivation mixes floating and other output types".to_owned(),
			));
		}
		let mut dynamic = BTreeMap::new();
		if versioned {
			take(&mut rest, b"],[")?;
			normalised.extend_from_slice(b"],[");
			let mut first = true;
			while !rest.starts_with(b"]") {
				if !first {
					take(&mut rest, b",")?;
					normalised.push(b',');
				}
				first = false;
				let start = rest;
				take(&mut rest, b"(")?;
				let path = string(&mut rest)?;
				take(&mut rest, b",")?;
				normalised.extend_from_slice(&start[..start.len() - rest.len()]);
				let mut names = rest;
				let (node, tail) =
					parse_input_node(rest).map_err(|e| Error::Other(e.to_string()))?;
				let _ = names.strip_prefix(b"(").map(|inner| names = inner);
				let names_start = names;
				take(&mut names, b"[")?;
				while !names.starts_with(b"]") {
					let _ = names.strip_prefix(b",").map(|inner| names = inner);
					string(&mut names)?;
				}
				take(&mut names, b"]")?;
				normalised.extend_from_slice(&names_start[..names_start.len() - names.len()]);
				rest = tail;
				take(&mut rest, b")")?;
				normalised.push(b')');
				if !node.dynamic_outputs.is_empty() {
					let path = StorePath::from_absolute_path(&path).map_err(|_| invalid())?;
					dynamic.insert(path, node.dynamic_outputs);
				}
			}
		}
		normalised.extend_from_slice(rest);
		let basic = UnverifiedDerivation::from_aterm_bytes(&normalised).map_err(|e| {
			Error::Other(format!(
				"error parsing derivation: {}",
				parser_error_message(&e)
			))
		})?;
		Ok(Self {
			basic,
			floating,
			experimental,
			dynamic,
		})
	}

	/// `Derivation::unparse`'s versioned form, if there are dynamic inputs.
	fn with_dynamic_inputs(&self, bytes: Vec<u8>) -> Vec<u8> {
		if self.dynamic.is_empty() {
			return bytes;
		}
		let mut rest = bytes.as_slice();
		take(&mut rest, b"Derive([").expect("serialized derivation");
		let mut out = b"DrvWithVersion(\"xp-dyn-drv\",[".to_vec();
		let start = rest;
		while !rest.starts_with(b"]") {
			let _ = rest.strip_prefix(b",").map(|inner| rest = inner);
			take(&mut rest, b"(").unwrap();
			for index in 0..4 {
				if index > 0 {
					take(&mut rest, b",").unwrap();
				}
				string(&mut rest).unwrap();
			}
			take(&mut rest, b")").unwrap();
		}
		out.extend_from_slice(&start[..start.len() - rest.len()]);
		out.extend_from_slice(b"],[");
		for (index, (path, names)) in self.basic.input_derivations().iter().enumerate() {
			if index > 0 {
				out.push(b',');
			}
			let mut node = String::new();
			unparse_node(
				&mut node,
				&InputNode {
					outputs: names.clone(),
					dynamic_outputs: self.dynamic.get(path).cloned().unwrap_or_default(),
				},
			);
			out.extend_from_slice(format!("(\"{}\"{node})", path.to_absolute_path()).as_bytes());
		}
		take(&mut rest, b"],[").unwrap();
		while !rest.starts_with(b"]") {
			let _ = rest.strip_prefix(b",").map(|inner| rest = inner);
			take(&mut rest, b"(").unwrap();
			string(&mut rest).unwrap();
			take(&mut rest, b",[").unwrap();
			while !rest.starts_with(b"]") {
				let _ = rest.strip_prefix(b",").map(|inner| rest = inner);
				string(&mut rest).unwrap();
			}
			take(&mut rest, b"])").unwrap();
		}
		out.extend_from_slice(rest);
		out
	}

	/// Fixed outputs hide their build provenance from input-addressed parents.
	pub fn fixed_hash_derivation_modulo(&self) -> Option<nix_compat::derivation::DrvHashModulo> {
		if self.experimental.is_empty() || !self.floating.is_empty() {
			return None;
		}
		let mut hashes = BTreeMap::new();
		for (name, output) in &self.experimental {
			let (path, hash) = output.fixed.as_ref()?;
			let payload = format!(
				"fixed:out:{}{}:{}",
				output.method.prefix(),
				hash.as_nix_lowerhex_string_fmt(),
				path.to_absolute_path()
			);
			hashes.insert(
				name.parse().ok()?,
				nix_compat::nixhash::Sha256::digest_bytes(payload.as_bytes()),
			);
		}
		Some(nix_compat::derivation::DrvHashModulo::Fixed(hashes))
	}

	pub fn to_aterm_bytes(&self) -> Vec<u8> {
		self.with_dynamic_inputs(self.to_unversioned_bytes())
	}

	fn to_unversioned_bytes(&self) -> Vec<u8> {
		if self.floating.is_empty() && self.experimental.is_empty() {
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
			let (prefix, hash_algo, hash, path) = if let Some(spec) = self.experimental.get(&name) {
				let prefix = spec.method.prefix();
				let (hash, path) = if let Some((path, hash)) = &spec.fixed {
					(
						format!("{}", hash.as_nix_lowerhex_string_fmt())
							.split_once(':')
							.unwrap()
							.1
							.to_owned(),
						path.to_absolute_path(),
					)
				} else {
					(
						if spec.impure {
							"impure".into()
						} else {
							String::new()
						},
						String::new(),
					)
				};
				(prefix, spec.hash_algo, hash, path)
			} else {
				let spec = &self.floating[&name];
				(
					if spec.method == FileIngestionMethod::NixArchive {
						"r:"
					} else {
						""
					},
					spec.hash_algo,
					String::new(),
					String::new(),
				)
			};
			out.extend_from_slice(&start[..name_end]);
			out.extend_from_slice(
				format!(",\"{path}\",\"{prefix}{}\",\"{hash}\")", hash_algo.as_str()).as_bytes(),
			);
		}
		out.extend_from_slice(rest);
		out
	}
}

/// The message of a derivation parser error, without the parser's debug
/// form of the remaining input.
pub(crate) fn parser_error_message(e: &nix_compat::derivation::ParserError<&[u8]>) -> String {
	match e {
		nix_compat::derivation::ParserError::Parser(e) => e.code.to_string(),
		nix_compat::derivation::ParserError::Incomplete => "premature EOF".to_owned(),
		nix_compat::derivation::ParserError::Validation(e) => e.to_string(),
	}
}
