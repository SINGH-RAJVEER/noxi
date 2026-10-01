//! Options a derivation sets through its environment or structured
//! attributes, following CppNix's `derivation-options.cc`.

use std::collections::{BTreeMap, BTreeSet};

use bstr::{BString, ByteSlice};
use nix_compat::store_path::{STORE_DIR_WITH_SLASH, StorePath};
use serde_json::Value;

use crate::error::{BuildError, Result};

/// An entry of a reference check: a store path or one of this derivation's
/// output names.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RefSpec {
	Path(StorePath),
	Output(String),
}

/// Checks on an output's references and size.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputChecks {
	/// Whether the requisite checks ignore the output's reference to itself.
	pub ignore_self_refs: bool,
	pub max_size: Option<u64>,
	pub max_closure_size: Option<u64>,
	pub allowed_references: Option<BTreeSet<RefSpec>>,
	pub disallowed_references: BTreeSet<RefSpec>,
	pub allowed_requisites: Option<BTreeSet<RefSpec>>,
	pub disallowed_requisites: BTreeSet<RefSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputChecksSpec {
	/// The top-level attributes of a derivation without structured attributes.
	All(OutputChecks),
	/// `outputChecks` of a derivation with structured attributes.
	PerOutput(BTreeMap<String, OutputChecks>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DerivationOptions {
	pub output_checks: OutputChecksSpec,
	pub unsafe_discard_references: BTreeMap<String, bool>,
	pub pass_as_file: BTreeSet<String>,
	pub export_references_graph: BTreeMap<String, BTreeSet<StorePath>>,
	pub no_chroot: bool,
	pub impure_host_deps: BTreeSet<String>,
	pub impure_env_vars: BTreeSet<String>,
	pub required_system_features: BTreeSet<String>,
	pub prefer_local_build: bool,
	pub allow_substitutes: bool,
}

/// Split on CppNix's default whitespace, dropping empty tokens.
pub(crate) fn tokenize(s: &str) -> impl Iterator<Item = &str> {
	s.split([' ', '\t', '\n', '\r']).filter(|t| !t.is_empty())
}

/// A store path with an optional subpath, as `toStorePath` accepts.
fn to_store_path(s: &str) -> Result<StorePath> {
	let rest = s
		.strip_prefix(STORE_DIR_WITH_SLASH)
		.ok_or_else(|| BuildError::Other(format!("path '{s}' is not in the Nix store")))?;
	let base = rest.split('/').next().unwrap_or(rest);
	StorePath::from_bytes(base.as_bytes()).map_err(|e| BuildError::Other(format!("path '{s}' is not a valid store path: {e}")))
}

fn parse_ref(s: &str) -> RefSpec {
	match s.strip_prefix(STORE_DIR_WITH_SLASH) {
		Some(rest) if !rest.contains('/') => match StorePath::from_bytes(rest.as_bytes()) {
			Ok(path) => RefSpec::Path(path),
			Err(_) => RefSpec::Output(s.to_owned()),
		},
		_ => RefSpec::Output(s.to_owned()),
	}
}

/// The structured attributes of a derivation, if it has `__json`.
pub fn structured_attrs(env: &BTreeMap<BString, BString>) -> Result<Option<serde_json::Map<String, Value>>> {
	let Some(json) = env.get(&BString::from("__json")) else {
		return Ok(None);
	};
	match serde_json::from_slice(json) {
		Ok(Value::Object(map)) => Ok(Some(map)),
		Ok(_) => Err(BuildError::Other("structured attributes are not a JSON object".to_owned())),
		Err(e) => Err(BuildError::Other(format!("cannot process __json attribute: {e}"))),
	}
}

fn json_string(value: &Value, name: &str) -> Result<String> {
	value
		.as_str()
		.map(str::to_owned)
		.ok_or_else(|| BuildError::Other(format!("while parsing attribute \"{name}\": expected a string")))
}

fn json_string_list(value: &Value, name: &str) -> Result<Vec<String>> {
	value
		.as_array()
		.ok_or_else(|| BuildError::Other(format!("while parsing attribute \"{name}\": expected a list")))?
		.iter()
		.map(|v| json_string(v, name))
		.collect()
}

fn json_bool(value: &Value, name: &str) -> Result<bool> {
	value
		.as_bool()
		.ok_or_else(|| BuildError::Other(format!("while parsing attribute \"{name}\": expected a Boolean")))
}

fn flatten(value: &Value, out: &mut BTreeSet<String>) -> Result<()> {
	match value {
		Value::Array(items) => {
			for item in items {
				flatten(item, out)?;
			}
			Ok(())
		}
		Value::String(s) => {
			out.insert(s.clone());
			Ok(())
		}
		_ => Err(BuildError::Other("'exportReferencesGraph' value is not an array or a string".to_owned())),
	}
}

fn valid_file_name(name: &str) -> bool {
	let mut chars = name.chars();
	matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
		&& chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

impl DerivationOptions {
	pub fn new(
		env: &BTreeMap<BString, BString>,
		parsed: Option<&serde_json::Map<String, Value>>,
	) -> Result<Self> {
		let env_str = |name: &str| env.get(&BString::from(name)).map(|v| v.to_str_lossy().into_owned());

		let bool_attr = |name: &str, default: bool| -> Result<bool> {
			match parsed {
				Some(attrs) => attrs.get(name).map(|v| json_bool(v, name)).transpose().map(|v| v.unwrap_or(default)),
				None => Ok(env_str(name).map(|v| v == "1").unwrap_or(default)),
			}
		};
		let string_set_attr = |name: &str| -> Result<Option<BTreeSet<String>>> {
			match parsed {
				Some(attrs) => attrs
					.get(name)
					.map(|v| json_string_list(v, name).map(|l| l.into_iter().collect()))
					.transpose(),
				None => Ok(env_str(name).map(|v| tokenize(&v).map(str::to_owned).collect())),
			}
		};

		let output_checks = match parsed {
			Some(attrs) => {
				let mut per_output = BTreeMap::new();
				if let Some(checks) = attrs.get("outputChecks") {
					let checks = checks
						.as_object()
						.ok_or_else(|| BuildError::Other("'outputChecks' is not an object".to_owned()))?;
					for (output_name, output) in checks {
						let output = output.as_object().ok_or_else(|| {
							BuildError::Other(format!("'outputChecks.\"{output_name}\"' is not an object"))
						})?;
						let refs = |name: &str| -> Result<Option<BTreeSet<RefSpec>>> {
							output
								.get(name)
								.map(|v| {
									json_string_list(v, &format!("outputChecks.\"{output_name}\".{name}"))
										.map(|l| l.iter().map(|s| parse_ref(s)).collect())
								})
								.transpose()
						};
						per_output.insert(
							output_name.clone(),
							OutputChecks {
								ignore_self_refs: false,
								max_size: output.get("maxSize").and_then(Value::as_u64),
								max_closure_size: output.get("maxClosureSize").and_then(Value::as_u64),
								allowed_references: refs("allowedReferences")?,
								disallowed_references: refs("disallowedReferences")?.unwrap_or_default(),
								allowed_requisites: refs("allowedRequisites")?,
								disallowed_requisites: refs("disallowedRequisites")?.unwrap_or_default(),
							},
						);
					}
				}
				OutputChecksSpec::PerOutput(per_output)
			}
			None => {
				let refs = |name: &str| -> Result<Option<BTreeSet<RefSpec>>> {
					Ok(string_set_attr(name)?.map(|set| set.iter().map(|s| parse_ref(s)).collect()))
				};
				OutputChecksSpec::All(OutputChecks {
					ignore_self_refs: true,
					max_size: None,
					max_closure_size: None,
					allowed_references: refs("allowedReferences")?,
					disallowed_references: refs("disallowedReferences")?.unwrap_or_default(),
					allowed_requisites: refs("allowedRequisites")?,
					disallowed_requisites: refs("disallowedRequisites")?.unwrap_or_default(),
				})
			}
		};

		let mut unsafe_discard_references = BTreeMap::new();
		if let Some(attrs) = parsed
			&& let Some(udr) = attrs.get("unsafeDiscardReferences")
		{
			let udr = udr.as_object().ok_or_else(|| {
				BuildError::Other("while parsing attribute 'unsafeDiscardReferences': expected an object".to_owned())
			})?;
			for (name, value) in udr {
				unsafe_discard_references.insert(name.clone(), json_bool(value, "unsafeDiscardReferences")?);
			}
		}

		let pass_as_file = match (parsed, env_str("passAsFile")) {
			(None, Some(s)) => tokenize(&s).map(str::to_owned).collect(),
			_ => BTreeSet::new(),
		};

		let mut export_references_graph = BTreeMap::new();
		match parsed {
			Some(attrs) => {
				if let Some(Value::Object(graph)) = attrs.get("exportReferencesGraph") {
					for (key, paths) in graph {
						let mut strings = BTreeSet::new();
						flatten(paths, &mut strings)?;
						let paths = strings.iter().map(|s| to_store_path(s)).collect::<Result<_>>()?;
						export_references_graph.insert(key.clone(), paths);
					}
				}
			}
			None => {
				let s = env_str("exportReferencesGraph").unwrap_or_default();
				let tokens: Vec<&str> = tokenize(&s).collect();
				if tokens.len() % 2 != 0 {
					return Err(BuildError::Other(format!(
						"odd number of tokens in 'exportReferencesGraph': '{s}'"
					)));
				}
				for pair in tokens.chunks(2) {
					if !valid_file_name(pair[0]) {
						return Err(BuildError::Other(format!(
							"invalid file name '{}' in 'exportReferencesGraph'",
							pair[0]
						)));
					}
					export_references_graph.insert(pair[0].to_owned(), BTreeSet::from([to_store_path(pair[1])?]));
				}
			}
		}

		Ok(Self {
			output_checks,
			unsafe_discard_references,
			pass_as_file,
			export_references_graph,
			no_chroot: bool_attr("__noChroot", false)?,
			impure_host_deps: string_set_attr("__impureHostDeps")?.unwrap_or_default(),
			impure_env_vars: string_set_attr("impureEnvVars")?.unwrap_or_default(),
			required_system_features: string_set_attr("requiredSystemFeatures")?.unwrap_or_default(),
			prefer_local_build: bool_attr("preferLocalBuild", false)?,
			allow_substitutes: bool_attr("allowSubstitutes", true)?,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn env(pairs: &[(&str, &str)]) -> BTreeMap<BString, BString> {
		pairs.iter().map(|(k, v)| (BString::from(*k), BString::from(*v))).collect()
	}

	#[test]
	fn legacy_attributes() {
		let e = env(&[
			("passAsFile", "a  b\tc"),
			("allowedReferences", "out /nix/store/00000000000000000000000000000000-x"),
			("exportReferencesGraph", "graph /nix/store/00000000000000000000000000000000-x/sub"),
			("preferLocalBuild", "1"),
			("allowSubstitutes", ""),
			("requiredSystemFeatures", "kvm big-parallel"),
		]);
		let o = DerivationOptions::new(&e, None).unwrap();
		assert_eq!(o.pass_as_file, ["a", "b", "c"].map(String::from).into());
		assert!(o.prefer_local_build);
		assert!(!o.allow_substitutes);
		let OutputChecksSpec::All(checks) = o.output_checks else { panic!() };
		assert!(checks.ignore_self_refs);
		assert_eq!(checks.allowed_references.unwrap().len(), 2);
		assert_eq!(o.export_references_graph["graph"].len(), 1);
		assert_eq!(o.required_system_features.len(), 2);
	}

	#[test]
	fn rejects_odd_export_graph() {
		let e = env(&[("exportReferencesGraph", "a")]);
		assert_eq!(
			DerivationOptions::new(&e, None).unwrap_err().to_string(),
			"odd number of tokens in 'exportReferencesGraph': 'a'"
		);
	}
}
