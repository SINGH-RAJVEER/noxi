//! Hashing, output paths and resolution for the JSON derivation model,
//! following CppNix 2.35.2's `derivations.cc`: `unparse`, `type`,
//! `hashDerivationModulo`, `fillInOutputPaths`, `checkInvariants` and
//! `tryResolve`.
//!
//! Input derivations are read through a callback, the role CppNix's store
//! plays when it recurses into them.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use data_encoding::HEXLOWER;
use serde_json::{Value, json};

use crate::derivation::OutputName;
use crate::nixbase32;
use crate::nixhash::{CAHash, Sha256};
use crate::store_path::{
	self, StorePath, build_ca_path, build_output_path, build_text_path_from_content_digest,
	compress_hash,
};

use super::derivation::{Derivation, InputNode, Output};
use super::store_path_to_json;

/// A derivation error, with CppNix's message.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct Error(pub String);

/// CppNix's `DerivationType`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DerivationType {
	InputAddressed { deferred: bool },
	ContentAddressed { sandboxed: bool, fixed: bool },
	Impure,
}

/// CppNix's `DrvHashModulo`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HashModulo {
	/// The hash of an input-addressed derivation, shared by its outputs.
	DrvHash(Sha256),
	/// The hash of each output of a fixed-output derivation.
	CaOutputHashes(BTreeMap<OutputName, Sha256>),
	/// A derivation whose output paths are not known until it is built.
	Deferred,
}

/// A path to a derivation, or to a derivation that is itself an output of
/// another one, CppNix's `SingleDerivedPath`.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum SingleDerivedPath {
	Opaque(StorePath),
	Built {
		drv_path: Box<SingleDerivedPath>,
		output: OutputName,
	},
}

/// Reads an input derivation, as CppNix's `readInvalidDerivation` does.
pub type ReadDerivation<'a> = &'a dyn Fn(&StorePath) -> Option<Derivation>;

fn unquoted(out: &mut String, s: &str) {
	out.push('"');
	out.push_str(s);
	out.push('"');
}

fn escaped(out: &mut String, s: &str) {
	out.push('"');
	for c in s.chars() {
		match c {
			'"' | '\\' => {
				out.push('\\');
				out.push(c);
			}
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			c => out.push(c),
		}
	}
	out.push('"');
}

fn list<'a>(
	out: &mut String,
	items: impl IntoIterator<Item = &'a str>,
	quote: fn(&mut String, &str),
) {
	out.push('[');
	for (i, item) in items.into_iter().enumerate() {
		if i > 0 {
			out.push(',');
		}
		quote(out, item);
	}
	out.push(']');
}

fn unparse_node(out: &mut String, node: &InputNode) {
	out.push(',');
	let outputs = node.outputs.iter().map(|o| o.as_str().unwrap_or_default());
	if node.dynamic_outputs.is_empty() {
		list(out, outputs, unquoted);
	} else {
		out.push('(');
		list(out, outputs, unquoted);
		out.push_str(",[");
		for (i, (name, child)) in node.dynamic_outputs.iter().enumerate() {
			if i > 0 {
				out.push(',');
			}
			out.push('(');
			unquoted(out, name.as_str().unwrap_or_default());
			unparse_node(out, child);
			out.push(')');
		}
		out.push_str("])");
	}
}

/// The `method:` prefix CppNix writes before a floating output's hash
/// algorithm.
fn method_prefix(method: &str) -> &'static str {
	match method {
		"nar" => "r:",
		"text" => "text:",
		"git" => "git:",
		_ => "",
	}
}

/// `ContentAddress::printMethodAlgo`.
fn method_algo(ca: &CAHash) -> String {
	match ca {
		CAHash::Flat(hash) => hash.algo().to_string(),
		CAHash::Nar(hash) => format!("r:{}", hash.algo()),
		CAHash::Text(_) => "text:sha256".to_owned(),
	}
}

/// `outputPathName`: the name of an output's store path.
pub fn output_path_name(drv_name: &str, output: &OutputName) -> String {
	if output == &OutputName::out() {
		drv_name.to_owned()
	} else {
		format!("{drv_name}-{output}")
	}
}

fn path_error(e: store_path::ParseStorePathError) -> Error {
	Error(e.to_string())
}

/// The store path of a fixed output, `DerivationOutput::CAFixed::path`.
pub fn fixed_output_path(
	drv_name: &str,
	output: &OutputName,
	ca: &CAHash,
) -> Result<StorePath, Error> {
	let name = output_path_name(drv_name, output);
	let path = match ca {
		CAHash::Flat(hash) => build_ca_path(&name, false, hash, [], false),
		CAHash::Nar(hash) => build_ca_path(&name, true, hash, [], false),
		CAHash::Text(digest) => build_text_path_from_content_digest(&name, *digest, []),
	};
	path.map(|p| p.to_owned()).map_err(path_error)
}

/// `Store::makeOutputPath`.
fn make_output_path(
	drv_name: &str,
	output: &OutputName,
	hash: &Sha256,
) -> Result<StorePath, Error> {
	let name = output_path_name(drv_name, output);
	build_output_path(&name, hash, output)
		.map(|p| p.to_owned())
		.map_err(path_error)
}

/// `rewriteStrings`: each rewrite in order, repeated until no match is left.
pub fn rewrite_strings(s: &str, rewrites: &BTreeMap<String, String>) -> String {
	let mut s = s.to_owned();
	for (from, to) in rewrites {
		if from == to || from.is_empty() {
			continue;
		}
		let mut start = 0;
		while let Some(found) = s[start..].find(from.as_str()) {
			let at = start + found;
			s.replace_range(at..at + from.len(), to);
			start = at;
		}
	}
	s
}

impl Derivation {
	/// `BasicDerivation::type`.
	pub fn derivation_type(&self) -> Result<DerivationType, Error> {
		let mut ty: Option<DerivationType> = None;
		let mut floating_algo = None;
		for (name, output) in &self.outputs {
			let new = match output {
				Output::InputAddressed(_) => DerivationType::InputAddressed { deferred: false },
				Output::Fixed(_) => DerivationType::ContentAddressed {
					sandboxed: false,
					fixed: true,
				},
				Output::Floating { .. } => DerivationType::ContentAddressed {
					sandboxed: true,
					fixed: false,
				},
				Output::Deferred => DerivationType::InputAddressed { deferred: true },
				Output::Impure { .. } => DerivationType::Impure,
			};
			match ty {
				None => ty = Some(new),
				Some(old) if old != new => {
					return Err(Error("can't mix derivation output types".into()));
				}
				Some(DerivationType::ContentAddressed { fixed: true, .. }) => {
					return Err(Error("only one fixed output is allowed for now".into()));
				}
				Some(_) => {}
			}
			match output {
				Output::Fixed(_) if name != &OutputName::out() => {
					return Err(Error("single fixed output must be named \"out\"".into()));
				}
				Output::Floating { hash_algo, .. } => match floating_algo {
					None => floating_algo = Some(*hash_algo),
					Some(algo) if algo != *hash_algo => {
						return Err(Error(
							"all floating outputs must use the same hash algorithm".into(),
						));
					}
					Some(_) => {}
				},
				_ => {}
			}
		}
		ty.ok_or_else(|| Error("must have at least one output".into()))
	}

	/// The structured attributes as CppNix stores them in `__json`.
	fn structured_attrs_json(&self) -> Option<String> {
		self.structured_attrs
			.as_ref()
			.map(|attrs| attrs.to_string())
	}

	/// The ATerm text of the derivation, `Derivation::unparse`. With
	/// `mask_outputs`, output paths and their environment variables are
	/// blank; `actual_inputs` replaces the input derivations, keyed by
	/// their hash modulo.
	pub fn unparse(
		&self,
		mask_outputs: bool,
		actual_inputs: Option<&BTreeMap<String, InputNode>>,
	) -> Result<String, Error> {
		let mut s = String::new();
		let dynamic = self
			.input_derivations
			.values()
			.any(|node| !node.dynamic_outputs.is_empty());
		if dynamic {
			s.push_str("DrvWithVersion(");
			unquoted(&mut s, "xp-dyn-drv");
			s.push(',');
		} else {
			s.push_str("Derive(");
		}

		s.push('[');
		for (i, (name, output)) in self.outputs.iter().enumerate() {
			if i > 0 {
				s.push(',');
			}
			s.push('(');
			unquoted(&mut s, name.as_str().unwrap_or_default());
			let (path, algo, hash) = match output {
				Output::InputAddressed(path) => (
					if mask_outputs {
						String::new()
					} else {
						path.to_absolute_path()
					},
					String::new(),
					String::new(),
				),
				Output::Fixed(ca) => (
					if mask_outputs {
						String::new()
					} else {
						fixed_output_path(&self.name, name, ca)?.to_absolute_path()
					},
					method_algo(ca),
					HEXLOWER.encode(ca.hash().digest_as_bytes()),
				),
				Output::Floating { method, hash_algo } => (
					String::new(),
					format!("{}{hash_algo}", method_prefix(method)),
					String::new(),
				),
				Output::Deferred => (String::new(), String::new(), String::new()),
				Output::Impure { method, hash_algo } => (
					String::new(),
					format!("{}{hash_algo}", method_prefix(method)),
					"impure".to_owned(),
				),
			};
			for field in [path, algo, hash] {
				s.push(',');
				unquoted(&mut s, &field);
			}
			s.push(')');
		}

		s.push_str("],[");
		match actual_inputs {
			Some(inputs) => {
				for (i, (hash, node)) in inputs.iter().enumerate() {
					if i > 0 {
						s.push(',');
					}
					s.push('(');
					unquoted(&mut s, hash);
					unparse_node(&mut s, node);
					s.push(')');
				}
			}
			None => {
				// CppNix orders store paths by their text.
				let mut inputs: Vec<_> = self
					.input_derivations
					.iter()
					.map(|(path, node)| (path.to_absolute_path(), node))
					.collect();
				inputs.sort_by(|a, b| a.0.cmp(&b.0));
				for (i, (path, node)) in inputs.iter().enumerate() {
					if i > 0 {
						s.push(',');
					}
					s.push('(');
					unquoted(&mut s, path);
					unparse_node(&mut s, node);
					s.push(')');
				}
			}
		}
		s.push_str("],");

		let mut srcs: Vec<String> = self
			.input_sources
			.iter()
			.map(StorePath::to_absolute_path)
			.collect();
		srcs.sort();
		list(&mut s, srcs.iter().map(String::as_str), unquoted);
		s.push(',');
		unquoted(&mut s, &self.system);
		s.push(',');
		escaped(&mut s, &self.builder);
		s.push(',');
		list(&mut s, self.args.iter().map(String::as_str), escaped);

		s.push_str(",[");
		let mut env: BTreeMap<&str, &str> = BTreeMap::new();
		if let Some(json) = &self.structured_attrs_json() {
			if self.env.contains_key("__json") {
				return Err(Error(
					"Cannot have an environment variable named '__json'. This key is reserved for encoding structured attrs".into(),
				));
			}
			env.insert("__json", json);
			for (key, value) in &self.env {
				env.insert(key, value);
			}
			self.unparse_env(&mut s, &env, mask_outputs);
		} else {
			for (key, value) in &self.env {
				env.insert(key, value);
			}
			self.unparse_env(&mut s, &env, mask_outputs);
		}
		s.push_str("])");
		Ok(s)
	}

	fn unparse_env(&self, s: &mut String, env: &BTreeMap<&str, &str>, mask_outputs: bool) {
		for (i, (key, value)) in env.iter().enumerate() {
			if i > 0 {
				s.push(',');
			}
			s.push('(');
			escaped(s, key);
			s.push(',');
			let is_output = self.outputs.keys().any(|name| name.as_str() == Some(key));
			escaped(s, if mask_outputs && is_output { "" } else { value });
			s.push(')');
		}
	}

	/// The store path `writeDerivation` gives the derivation: a text path
	/// referring to its inputs.
	pub fn path(&self) -> Result<StorePath, Error> {
		let aterm = self.unparse(false, None)?;
		let references: BTreeSet<&StorePath> = self
			.input_sources
			.iter()
			.chain(self.input_derivations.keys())
			.collect();
		build_text_path_from_content_digest(
			&format!("{}.drv", self.name),
			Sha256::digest_bytes(aterm.as_bytes()),
			references.into_iter().map(StorePath::as_ref),
		)
		.map(|p| p.to_owned())
		.map_err(path_error)
	}

	/// `hashDerivationModulo`. Input derivations are read with `read`.
	pub fn hash_modulo(
		&self,
		mask_outputs: bool,
		read: ReadDerivation<'_>,
	) -> Result<HashModulo, Error> {
		let ty = self.derivation_type()?;
		if let DerivationType::ContentAddressed { fixed: true, .. } = ty {
			let mut hashes = BTreeMap::new();
			for (name, output) in &self.outputs {
				let Output::Fixed(ca) = output else {
					unreachable!("a fixed derivation has only fixed outputs");
				};
				let path = fixed_output_path(&self.name, name, ca)?;
				let hash = Sha256::digest_display(format_args!(
					"fixed:out:{}:{}:{}",
					method_algo(ca),
					HEXLOWER.encode(ca.hash().digest_as_bytes()),
					path.to_absolute_path()
				));
				hashes.insert(name.clone(), hash);
			}
			return Ok(HashModulo::CaOutputHashes(hashes));
		}
		if matches!(
			ty,
			DerivationType::ContentAddressed { .. } | DerivationType::Impure
		) {
			return Ok(HashModulo::Deferred);
		}

		let mut inputs = BTreeMap::new();
		for (drv_path, node) in &self.input_derivations {
			if !node.dynamic_outputs.is_empty() {
				return Ok(HashModulo::Deferred);
			}
			let input = read(drv_path).ok_or_else(|| {
				Error(format!(
					"path '{}' is not valid",
					drv_path.to_absolute_path()
				))
			})?;
			match input.hash_modulo(false, read)? {
				HashModulo::Deferred => return Ok(HashModulo::Deferred),
				HashModulo::DrvHash(hash) => {
					inputs.insert(format!("{hash:x}"), node.clone());
				}
				HashModulo::CaOutputHashes(hashes) => {
					for output in &node.outputs {
						let hash = hashes.get(output).ok_or_else(|| {
							Error(format!(
								"no hash for output '{output}' of derivation '{}'",
								self.name
							))
						})?;
						inputs.insert(
							format!("{hash:x}"),
							InputNode {
								outputs: BTreeSet::from([OutputName::out()]),
								dynamic_outputs: BTreeMap::new(),
							},
						);
					}
				}
			}
		}
		Ok(HashModulo::DrvHash(Sha256::digest_bytes(
			self.unparse(mask_outputs, Some(&inputs))?.as_bytes(),
		)))
	}

	/// `processDerivationOutputPaths`.
	fn process_output_paths(
		&mut self,
		fill_in: bool,
		read: ReadDerivation<'_>,
		warnings: &mut Vec<String>,
	) -> Result<(), Error> {
		let mut hash_modulo: Option<HashModulo> = None;
		let names: Vec<OutputName> = self.outputs.keys().cloned().collect();
		for name in names {
			let key = name.to_string();
			let output = self.outputs[&name].clone();
			let expected = match &output {
				Output::InputAddressed(_) | Output::Deferred => {
					if hash_modulo.is_none() {
						hash_modulo = Some(self.hash_modulo(true, read)?);
					}
					match hash_modulo.as_ref().unwrap() {
						HashModulo::DrvHash(hash) => {
							let out_path = make_output_path(&self.name, &name, hash)?;
							match &output {
								Output::InputAddressed(path) if *path != out_path => {
									return Err(Error(format!(
										"derivation has incorrect output '{}', should be '{}'",
										path.to_absolute_path(),
										out_path.to_absolute_path()
									)));
								}
								Output::Deferred if !fill_in => {
									warnings.push(format!(
										"derivation has incorrect deferred output, should be '{}'.\nThis will be an error in future versions of Nix; compatibility of CA derivations will be broken.",
										out_path.to_absolute_path()
									));
									continue;
								}
								Output::Deferred => {
									self.outputs.insert(
										name.clone(),
										Output::InputAddressed(out_path.clone()),
									);
								}
								_ => {}
							}
							out_path
						}
						HashModulo::CaOutputHashes(_) => {
							return Err(Error(
								"an input-addressed output has fixed output hashes".into(),
							));
						}
						HashModulo::Deferred => match &output {
							Output::InputAddressed(path) => {
								return Err(Error(format!(
									"derivation has incorrect output '{}', should be deferred",
									path.to_absolute_path()
								)));
							}
							_ => continue,
						},
					}
				}
				Output::Fixed(ca) => fixed_output_path(&self.name, &name, ca)?,
				Output::Floating { .. } | Output::Impure { .. } => continue,
			};

			let expected = expected.to_absolute_path();
			if fill_in {
				let value = self.env.entry(key.clone()).or_default();
				if value.is_empty() {
					*value = expected.clone();
				}
			}
			match self.env.get(&key) {
				None => {
					return Err(Error(format!(
						"derivation has missing environment variable '{key}', should be '{expected}' but is not present"
					)));
				}
				Some(actual) if *actual != expected => {
					return Err(Error(format!(
						"derivation has incorrect environment variable '{key}', should be '{expected}' but is actually '{actual}'"
					)));
				}
				Some(_) => {}
			}
		}
		self.derivation_type()?;
		Ok(())
	}

	/// `Derivation::fillInOutputPaths`: gives deferred outputs their
	/// input-addressed paths where they are known, fills in missing or empty
	/// output environment variables, and checks the remaining paths.
	pub fn fill_in_output_paths(
		&mut self,
		read: ReadDerivation<'_>,
		warnings: &mut Vec<String>,
	) -> Result<(), Error> {
		self.process_output_paths(true, read, warnings)
	}

	/// `Derivation::checkInvariants`.
	pub fn check_invariants(
		&self,
		read: ReadDerivation<'_>,
		warnings: &mut Vec<String>,
	) -> Result<(), Error> {
		self.clone().process_output_paths(false, read, warnings)
	}

	/// `BasicDerivation::applyRewrites`.
	pub fn apply_rewrites(&mut self, rewrites: &BTreeMap<String, String>) -> Result<(), Error> {
		if rewrites.is_empty() {
			return Ok(());
		}
		self.builder = rewrite_strings(&self.builder, rewrites);
		for arg in &mut self.args {
			*arg = rewrite_strings(arg, rewrites);
		}
		let mut env = BTreeMap::new();
		for (key, value) in &self.env {
			env.entry(rewrite_strings(key, rewrites))
				.or_insert_with(|| rewrite_strings(value, rewrites));
		}
		self.env = env;
		if let Some(json) = self.structured_attrs_json() {
			let rewritten = rewrite_strings(&json, rewrites);
			self.structured_attrs = Some(
				serde_json::from_str(&rewritten)
					.map_err(|e| Error(format!("rewritten structured attributes: {e}")))?,
			);
		}
		Ok(())
	}

	/// `Derivation::tryResolve`: replaces the input derivations with the
	/// outputs `query` reports, rewriting their placeholders when
	/// `ca_derivations` is enabled, and fills in the output paths of the
	/// result. Returns `None` if an input output is unknown.
	pub fn try_resolve(
		&self,
		query: &dyn Fn(&SingleDerivedPath, &OutputName) -> Option<StorePath>,
		ca_derivations: bool,
		read: ReadDerivation<'_>,
		warnings: &mut Vec<String>,
	) -> Result<Option<Derivation>, Error> {
		let mut resolved = self.clone();
		resolved.input_derivations.clear();
		let mut rewrites = BTreeMap::new();
		for (drv_path, node) in &self.input_derivations {
			let resolved_input = try_resolve_input(
				&mut resolved.input_sources,
				&mut rewrites,
				ca_derivations,
				None,
				&SingleDerivedPath::Opaque(drv_path.clone()),
				node,
				query,
			);
			if !resolved_input {
				return Ok(None);
			}
		}
		resolved.apply_rewrites(&rewrites)?;
		resolved.fill_in_output_paths(read, warnings)?;
		Ok(Some(resolved))
	}

	/// `adl_serializer<BasicDerivation>::to_json`: the derivation with its
	/// input sources as `inputs`. Input derivations are not written.
	pub fn to_basic_json(&self) -> Value {
		let mut value = self.to_json();
		value.as_object_mut().unwrap().insert(
			"inputs".to_owned(),
			json!(
				self.input_sources
					.iter()
					.map(store_path_to_json)
					.collect::<Vec<_>>()
			),
		);
		value
	}
}

/// `DownstreamPlaceholder::unknownCaOutput`, as a digest.
fn unknown_ca_output(drv_path: &StorePath, output: &OutputName) -> Sha256 {
	let drv_name = drv_path
		.name()
		.strip_suffix(".drv")
		.unwrap_or(drv_path.name());
	Sha256::digest_display(format_args!(
		"nix-upstream-output:{}:{}",
		nixbase32::encode(drv_path.digest()),
		output_path_name(drv_name, output)
	))
}

/// `DownstreamPlaceholder::unknownDerivation`, as a digest.
fn unknown_derivation(placeholder: &Sha256, output: &OutputName) -> Sha256 {
	let compressed: [u8; 20] = compress_hash(placeholder.as_bytes());
	Sha256::digest_display(format_args!(
		"nix-computed-output:{}:{output}",
		nixbase32::encode(&compressed)
	))
}

/// `DownstreamPlaceholder::render`.
fn render_placeholder(digest: &Sha256) -> String {
	let mut out = String::from("/");
	write!(out, "{}", nixbase32::encode(digest.as_bytes())).unwrap();
	out
}

fn try_resolve_input(
	input_sources: &mut BTreeSet<StorePath>,
	rewrites: &mut BTreeMap<String, String>,
	ca_derivations: bool,
	placeholder: Option<&Sha256>,
	drv_path: &SingleDerivedPath,
	node: &InputNode,
	query: &dyn Fn(&SingleDerivedPath, &OutputName) -> Option<StorePath>,
) -> bool {
	let placeholder_for = |output: &OutputName| match (placeholder, drv_path) {
		(Some(placeholder), _) => unknown_derivation(placeholder, output),
		(None, SingleDerivedPath::Opaque(path)) => unknown_ca_output(path, output),
		(None, SingleDerivedPath::Built { .. }) => {
			unreachable!("a built input has a placeholder")
		}
	};

	for output in &node.outputs {
		let Some(actual) = query(drv_path, output) else {
			return false;
		};
		if ca_derivations {
			rewrites
				.entry(render_placeholder(&placeholder_for(output)))
				.or_insert_with(|| actual.to_absolute_path());
		}
		input_sources.insert(actual);
	}
	for (output, child) in &node.dynamic_outputs {
		let next = placeholder_for(output);
		let built = SingleDerivedPath::Built {
			drv_path: Box::new(drv_path.clone()),
			output: output.clone(),
		};
		if !try_resolve_input(
			input_sources,
			rewrites,
			ca_derivations,
			Some(&next),
			&built,
			child,
			query,
		) {
			return false;
		}
	}
	true
}
