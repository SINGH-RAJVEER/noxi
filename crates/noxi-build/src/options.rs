//! Options a derivation sets through its environment or structured
//! attributes, following CppNix 2.35.2's `derivation-options.cc`.
//!
//! [`DerivationOptions`] is generic over how inputs are named, as CppNix's
//! `DerivationOptions<Inputs>` is: [`DerivedInput`] while a
//! content-addressed input may still be a placeholder, and [`StorePath`]
//! once every input is known ([`DerivationOptions::try_resolve`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

use bstr::{BString, ByteSlice};
use nix_compat::cppnix_json;
use nix_compat::derivation::OutputName;
use nix_compat::store_path::{StorePath, downstream_placeholder, store_dir_with_slash};
use serde_json::{Map, Value, json};

use crate::error::{BuildError, Result};

/// How a derivation option names an input path: CppNix's `StorePath` or
/// `SingleDerivedPath` JSON serializers.
pub trait Input: Clone + Debug + Ord {
	fn to_json(&self) -> Value;
	fn from_json(value: &Value) -> Result<Self>;
}

impl Input for StorePath {
	fn to_json(&self) -> Value {
		cppnix_json::store_path_to_json(self)
	}

	fn from_json(value: &Value) -> Result<Self> {
		cppnix_json::store_path_from_json(value).map_err(json_error)
	}
}

/// CppNix's `SingleDerivedPath`: a store path, or an output of a
/// derivation that may not have been built yet. Outputs of outputs, which
/// need `dynamic-derivations`, are not supported.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DerivedInput {
	Opaque(StorePath),
	Built {
		drv_path: StorePath,
		output: OutputName,
	},
}

impl Input for DerivedInput {
	fn to_json(&self) -> Value {
		match self {
			DerivedInput::Opaque(path) => cppnix_json::single_derived_path_to_json(path, None),
			DerivedInput::Built { drv_path, output } => {
				cppnix_json::single_derived_path_to_json(drv_path, Some(output))
			}
		}
	}

	fn from_json(value: &Value) -> Result<Self> {
		match cppnix_json::single_derived_path_from_json(value).map_err(json_error)? {
			(path, None) => Ok(DerivedInput::Opaque(path)),
			(drv_path, Some(output)) => Ok(DerivedInput::Built { drv_path, output }),
		}
	}
}

/// CppNix's `DrvRef<Input>`: an entry of a reference check, either one of
/// the derivation's own outputs or an input. Own outputs sort first, as
/// they do in CppNix's `std::variant<OutputName, Input>`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DrvRef<I> {
	Output(String),
	Input(I),
}

impl<I: Input> DrvRef<I> {
	fn to_json(&self) -> Value {
		match self {
			DrvRef::Output(name) => json!({ "drvPath": "self", "output": name }),
			DrvRef::Input(input) => input.to_json(),
		}
	}

	fn from_json(value: &Value) -> Result<Self> {
		if let Value::Object(object) = value
			&& object.get("drvPath").and_then(Value::as_str) == Some("self")
		{
			return Ok(DrvRef::Output(
				get_string(value_at(object, "output")?)?.to_owned(),
			));
		}
		I::from_json(value).map(DrvRef::Input)
	}
}

/// Checks on an output's references and size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputChecks<I> {
	/// Whether the requisite checks ignore the output's reference to itself.
	pub ignore_self_refs: bool,
	pub max_size: Option<u64>,
	pub max_closure_size: Option<u64>,
	pub allowed_references: Option<BTreeSet<DrvRef<I>>>,
	pub disallowed_references: BTreeSet<DrvRef<I>>,
	pub allowed_requisites: Option<BTreeSet<DrvRef<I>>>,
	pub disallowed_requisites: BTreeSet<DrvRef<I>>,
}

impl<I> Default for OutputChecks<I> {
	fn default() -> Self {
		Self {
			ignore_self_refs: false,
			max_size: None,
			max_closure_size: None,
			allowed_references: None,
			disallowed_references: BTreeSet::new(),
			allowed_requisites: None,
			disallowed_requisites: BTreeSet::new(),
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputChecksSpec<I> {
	/// The top-level attributes of a derivation without structured attributes.
	ForAllOutputs(OutputChecks<I>),
	/// `outputChecks` of a derivation with structured attributes.
	PerOutput(BTreeMap<String, OutputChecks<I>>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DerivationOptions<I> {
	pub output_checks: OutputChecksSpec<I>,
	pub unsafe_discard_references: BTreeMap<String, bool>,
	pub pass_as_file: BTreeSet<String>,
	pub export_references_graph: BTreeMap<String, BTreeSet<I>>,
	pub additional_sandbox_profile: String,
	pub no_chroot: bool,
	pub impure_host_deps: BTreeSet<String>,
	pub impure_env_vars: BTreeSet<String>,
	pub allow_local_networking: bool,
	pub required_system_features: BTreeSet<String>,
	pub prefer_local_build: bool,
	pub allow_substitutes: bool,
}

impl<I> Default for DerivationOptions<I> {
	fn default() -> Self {
		Self {
			output_checks: OutputChecksSpec::ForAllOutputs(OutputChecks {
				ignore_self_refs: true,
				..OutputChecks::default()
			}),
			unsafe_discard_references: BTreeMap::new(),
			pass_as_file: BTreeSet::new(),
			export_references_graph: BTreeMap::new(),
			additional_sandbox_profile: String::new(),
			no_chroot: false,
			impure_host_deps: BTreeSet::new(),
			impure_env_vars: BTreeSet::new(),
			allow_local_networking: false,
			required_system_features: BTreeSet::new(),
			prefer_local_build: false,
			allow_substitutes: true,
		}
	}
}

/// Split on CppNix's default whitespace, dropping empty tokens.
pub(crate) fn tokenize(s: &str) -> impl Iterator<Item = &str> {
	s.split([' ', '\t', '\n', '\r']).filter(|t| !t.is_empty())
}

fn json_error(error: cppnix_json::Error) -> BuildError {
	BuildError::Other(error.to_string())
}

fn type_name(value: &Value) -> &'static str {
	match value {
		Value::Null => "null",
		Value::Bool(_) => "boolean",
		Value::Number(_) => "number",
		Value::String(_) => "string",
		Value::Array(_) => "array",
		Value::Object(_) => "object",
	}
}

/// json-utils' `ensureType` error.
fn wrong_type(value: &Value, expected: &str) -> BuildError {
	BuildError::Other(format!(
		"Expected JSON value to be of type '{expected}' but it is of type '{}': {value}",
		type_name(value)
	))
}

fn value_at<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a Value> {
	object.get(key).ok_or_else(|| {
		BuildError::Other(format!(
			"Expected JSON object to contain key '{key}' but it doesn't: {}",
			Value::Object(object.clone())
		))
	})
}

fn get_object(value: &Value) -> Result<&Map<String, Value>> {
	value.as_object().ok_or_else(|| wrong_type(value, "object"))
}

fn get_string(value: &Value) -> Result<&str> {
	value.as_str().ok_or_else(|| wrong_type(value, "string"))
}

fn get_boolean(value: &Value) -> Result<bool> {
	value.as_bool().ok_or_else(|| wrong_type(value, "boolean"))
}

fn get_unsigned(value: &Value) -> Result<u64> {
	value.as_u64().ok_or_else(|| {
		BuildError::Other(format!(
			"Expected JSON value to be an unsigned integral number but it is of type '{}': {value}",
			type_name(value)
		))
	})
}

fn get_string_list(value: &Value) -> Result<Vec<String>> {
	value
		.as_array()
		.ok_or_else(|| wrong_type(value, "array"))?
		.iter()
		.map(|v| get_string(v).map(str::to_owned))
		.collect()
}

fn get_string_set(value: &Value) -> Result<BTreeSet<String>> {
	Ok(get_string_list(value)?.into_iter().collect())
}

/// Prefix an error with one of CppNix's `addTrace` messages.
fn trace<T>(result: Result<T>, context: impl FnOnce() -> String) -> Result<T> {
	result.map_err(|e| BuildError::Other(format!("{e}\n… {}", context())))
}

/// CppNix's `StoreDirConfig::maybeParseStorePath`: exactly a store path,
/// with no subpath.
fn parse_store_path(s: &str) -> Option<StorePath> {
	let rest = s.strip_prefix(store_dir_with_slash())?;
	if rest.contains('/') {
		return None;
	}
	StorePath::from_bytes(rest.as_bytes()).ok()
}

/// CppNix's `StoreDirConfig::toStorePath`: a store path with an optional
/// subpath.
fn to_store_path(s: &str) -> Result<StorePath> {
	let rest = s
		.strip_prefix(store_dir_with_slash())
		.ok_or_else(|| BuildError::Other(format!("path '{s}' is not in the Nix store")))?;
	let base = rest.split('/').next().unwrap_or(rest);
	StorePath::from_bytes(base.as_bytes())
		.map_err(|e| BuildError::Other(format!("path '{s}' is not a valid store path: {e}")))
}

/// The structured attributes of a derivation environment, if it has
/// `__json` (`StructuredAttrs::tryExtract`).
pub fn structured_attrs(env: &BTreeMap<BString, BString>) -> Result<Option<Map<String, Value>>> {
	let Some(json) = env.get(&BString::from("__json")) else {
		return Ok(None);
	};
	match serde_json::from_slice(json) {
		Ok(Value::Object(map)) => Ok(Some(map)),
		Ok(other) => Err(wrong_type(&other, "object")),
		Err(e) => Err(BuildError::Other(format!(
			"cannot process __json attribute: {e}"
		))),
	}
}

fn flatten(value: &Value, out: &mut BTreeSet<String>) -> Result<()> {
	match value {
		Value::Array(items) => items.iter().try_for_each(|item| flatten(item, out)),
		Value::String(s) => {
			out.insert(s.clone());
			Ok(())
		}
		_ => Err(BuildError::Other(
			"'exportReferencesGraph' value is not an array or a string".to_owned(),
		)),
	}
}

/// `[A-Za-z_][A-Za-z0-9_.-]*`, the file names `exportReferencesGraph` accepts.
fn valid_file_name(name: &str) -> bool {
	let mut chars = name.chars();
	matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
		&& chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

const STRUCTURED_ONLY: [&str; 6] = [
	"allowedReferences",
	"allowedRequisites",
	"disallowedRequisites",
	"disallowedReferences",
	"maxSize",
	"maxClosureSize",
];

impl DerivationOptions<DerivedInput> {
	/// CppNix's `derivationOptionsFromStructuredAttrs` for
	/// `SingleDerivedPath` inputs.
	///
	/// `env` is the environment without `__json`; `parsed` holds the
	/// structured attributes, if any. With `ca_derivations`, references to
	/// the placeholder of an output of one of `input_drvs` become that
	/// output. Warnings CppNix prints are appended to `warnings`.
	pub fn new(
		env: &BTreeMap<BString, BString>,
		parsed: Option<&Map<String, Value>>,
		input_drvs: &BTreeMap<StorePath, BTreeSet<OutputName>>,
		ca_derivations: bool,
		warnings: &mut Vec<String>,
	) -> Result<Self> {
		let mut placeholders = BTreeMap::new();
		if ca_derivations {
			for (drv_path, outputs) in input_drvs {
				for output in outputs {
					placeholders.insert(
						downstream_placeholder(drv_path, output.as_ref()),
						DerivedInput::Built {
							drv_path: drv_path.clone(),
							output: output.clone(),
						},
					);
				}
			}
		}
		// A placeholder followed by a subpath names the placeholder, as a
		// store path followed by a subpath names the store path.
		let find_placeholder = |s: &str| -> Option<DerivedInput> {
			let key = match s.get(1..).and_then(|rest| rest.find('/')) {
				Some(slash) => &s[..slash + 1],
				None => s,
			};
			placeholders.get(key).cloned()
		};
		let parse_input = |s: &str| -> Result<DerivedInput> {
			match find_placeholder(s) {
				Some(built) => Ok(built),
				None => to_store_path(s).map(DerivedInput::Opaque),
			}
		};
		let parse_ref = |s: &str| -> DrvRef<DerivedInput> {
			if let Some(built) = find_placeholder(s) {
				return DrvRef::Input(built);
			}
			match parse_store_path(s) {
				Some(path) => DrvRef::Input(DerivedInput::Opaque(path)),
				None => DrvRef::Output(s.to_owned()),
			}
		};

		let env_str = |name: &str| {
			env.get(&BString::from(name))
				.map(|v| v.to_str_lossy().into_owned())
		};
		let string_attr = |name: &str| -> Result<Option<String>> {
			match parsed {
				Some(attrs) => attrs
					.get(name)
					.map(|v| {
						trace(get_string(v).map(str::to_owned), || {
							format!("while parsing attribute \"{name}\"")
						})
					})
					.transpose(),
				None => Ok(env_str(name)),
			}
		};
		let bool_attr = |name: &str, default: bool| -> Result<bool> {
			match parsed {
				Some(attrs) => attrs
					.get(name)
					.map(|v| {
						trace(get_boolean(v), || {
							format!("while parsing attribute \"{name}\"")
						})
					})
					.transpose()
					.map(|v| v.unwrap_or(default)),
				None => Ok(env_str(name).map_or(default, |v| v == "1")),
			}
		};
		let string_set_attr = |name: &str| -> Result<Option<BTreeSet<String>>> {
			match parsed {
				Some(attrs) => attrs
					.get(name)
					.map(|v| {
						trace(get_string_set(v), || {
							format!("while parsing attribute \"{name}\"")
						})
					})
					.transpose(),
				None => Ok(env_str(name).map(|v| tokenize(&v).map(str::to_owned).collect())),
			}
		};

		if let Some(attrs) = parsed {
			for name in STRUCTURED_ONLY {
				if attrs.contains_key(name) {
					warnings.push(format!(
						"'structuredAttrs' disables the effect of the top-level attribute '{name}'; use 'outputChecks' instead"
					));
				}
			}
		}

		let output_checks = match parsed {
			Some(attrs) => {
				let mut per_output = BTreeMap::new();
				if let Some(checks) = attrs.get("outputChecks") {
					for (output_name, output) in get_object(checks)? {
						let output = get_object(output)?;
						let refs = |name: &str| -> Result<Option<BTreeSet<DrvRef<DerivedInput>>>> {
							output
								.get(name)
								.map(|v| {
									trace(
										get_string_list(v)
											.map(|l| l.iter().map(|s| parse_ref(s)).collect()),
										|| {
											format!(
												"while parsing attribute 'outputChecks.\"{output_name}\".{name}'"
											)
										},
									)
								})
								.transpose()
						};
						per_output.insert(
							output_name.clone(),
							OutputChecks {
								ignore_self_refs: false,
								max_size: output.get("maxSize").map(get_unsigned).transpose()?,
								max_closure_size: output
									.get("maxClosureSize")
									.map(get_unsigned)
									.transpose()?,
								allowed_references: refs("allowedReferences")?,
								disallowed_references: refs("disallowedReferences")?
									.unwrap_or_default(),
								allowed_requisites: refs("allowedRequisites")?,
								disallowed_requisites: refs("disallowedRequisites")?
									.unwrap_or_default(),
							},
						);
					}
				}
				OutputChecksSpec::PerOutput(per_output)
			}
			None => {
				let refs =
					|name: &str| -> Result<Option<BTreeSet<DrvRef<DerivedInput>>>> {
						Ok(string_set_attr(name)?
							.map(|set| set.iter().map(|s| parse_ref(s)).collect()))
					};
				OutputChecksSpec::ForAllOutputs(OutputChecks {
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
		if let Some(udr) = parsed.and_then(|attrs| attrs.get("unsafeDiscardReferences")) {
			let result = get_object(udr).and_then(|udr| {
				udr.iter().try_for_each(|(name, value)| {
					unsafe_discard_references.insert(name.clone(), get_boolean(value)?);
					Ok(())
				})
			});
			trace(result, || {
				"while parsing attribute 'unsafeDiscardReferences'".to_owned()
			})?;
		}

		let mut pass_as_file = BTreeSet::new();
		if let Some(s) = env_str("passAsFile") {
			if parsed.is_some() {
				warnings.push(
					"'structuredAttrs' disables the effect of the top-level attribute 'passAsFile'; because all JSON is always passed via file"
						.to_owned(),
				);
			} else {
				pass_as_file = tokenize(&s).map(str::to_owned).collect();
			}
		}

		let mut export_references_graph = BTreeMap::new();
		match parsed {
			Some(attrs) => match attrs.get("exportReferencesGraph") {
				None => {}
				Some(Value::Object(graph)) => {
					for (key, paths) in graph {
						let mut strings = BTreeSet::new();
						flatten(paths, &mut strings)?;
						let paths = strings
							.iter()
							.map(|s| parse_input(s))
							.collect::<Result<_>>()?;
						export_references_graph.insert(key.clone(), paths);
					}
				}
				Some(_) => warnings.push(
					"'exportReferencesGraph' in structured attrs is not a JSON object, ignoring"
						.to_owned(),
				),
			},
			None => {
				let s = env_str("exportReferencesGraph").unwrap_or_default();
				let tokens: Vec<&str> = tokenize(&s).collect();
				if !tokens.len().is_multiple_of(2) {
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
					export_references_graph
						.insert(pair[0].to_owned(), BTreeSet::from([parse_input(pair[1])?]));
				}
			}
		}

		Ok(Self {
			output_checks,
			unsafe_discard_references,
			pass_as_file,
			export_references_graph,
			additional_sandbox_profile: string_attr("__sandboxProfile")?.unwrap_or_default(),
			no_chroot: bool_attr("__noChroot", false)?,
			impure_host_deps: string_set_attr("__impureHostDeps")?.unwrap_or_default(),
			impure_env_vars: string_set_attr("impureEnvVars")?.unwrap_or_default(),
			allow_local_networking: bool_attr("__darwinAllowLocalNetworking", false)?,
			required_system_features: string_set_attr("requiredSystemFeatures")?
				.unwrap_or_default(),
			prefer_local_build: bool_attr("preferLocalBuild", false)?,
			allow_substitutes: bool_attr("allowSubstitutes", true)?,
		})
	}

	/// CppNix's `tryResolve`: replace each built input with the path
	/// `resolve` gives for it, or return `None` if one is unknown.
	pub fn try_resolve(
		&self,
		mut resolve: impl FnMut(&StorePath, &OutputName) -> Option<StorePath>,
	) -> Option<DerivationOptions<StorePath>> {
		let mut resolve_input = |input: &DerivedInput| match input {
			DerivedInput::Opaque(path) => Some(path.clone()),
			DerivedInput::Built { drv_path, output } => resolve(drv_path, output),
		};
		let mut resolve_set =
			|set: &BTreeSet<DrvRef<DerivedInput>>| -> Option<BTreeSet<DrvRef<StorePath>>> {
				set.iter()
					.map(|r| match r {
						DrvRef::Output(name) => Some(DrvRef::Output(name.clone())),
						DrvRef::Input(input) => resolve_input(input).map(DrvRef::Input),
					})
					.collect()
			};
		let mut resolve_checks =
			|c: &OutputChecks<DerivedInput>| -> Option<OutputChecks<StorePath>> {
				Some(OutputChecks {
					ignore_self_refs: c.ignore_self_refs,
					max_size: c.max_size,
					max_closure_size: c.max_closure_size,
					allowed_references: match &c.allowed_references {
						Some(set) => Some(resolve_set(set)?),
						None => None,
					},
					disallowed_references: resolve_set(&c.disallowed_references)?,
					allowed_requisites: match &c.allowed_requisites {
						Some(set) => Some(resolve_set(set)?),
						None => None,
					},
					disallowed_requisites: resolve_set(&c.disallowed_requisites)?,
				})
			};
		let output_checks = match &self.output_checks {
			OutputChecksSpec::ForAllOutputs(checks) => {
				OutputChecksSpec::ForAllOutputs(resolve_checks(checks)?)
			}
			OutputChecksSpec::PerOutput(map) => OutputChecksSpec::PerOutput(
				map.iter()
					.map(|(name, checks)| Some((name.clone(), resolve_checks(checks)?)))
					.collect::<Option<_>>()?,
			),
		};
		let mut export_references_graph = BTreeMap::new();
		for (name, inputs) in &self.export_references_graph {
			let paths = inputs
				.iter()
				.map(|input| match input {
					DerivedInput::Opaque(path) => Some(path.clone()),
					DerivedInput::Built { drv_path, output } => resolve(drv_path, output),
				})
				.collect::<Option<_>>()?;
			export_references_graph.insert(name.clone(), paths);
		}
		Some(DerivationOptions {
			output_checks,
			unsafe_discard_references: self.unsafe_discard_references.clone(),
			pass_as_file: self.pass_as_file.clone(),
			export_references_graph,
			additional_sandbox_profile: self.additional_sandbox_profile.clone(),
			no_chroot: self.no_chroot,
			impure_host_deps: self.impure_host_deps.clone(),
			impure_env_vars: self.impure_env_vars.clone(),
			allow_local_networking: self.allow_local_networking,
			required_system_features: self.required_system_features.clone(),
			prefer_local_build: self.prefer_local_build,
			allow_substitutes: self.allow_substitutes,
		})
	}
}

impl<I: Input> DerivationOptions<I> {
	/// `getRequiredSystemFeatures`. A derivation whose output paths are not
	/// known in advance also requires `ca-derivations`.
	pub fn get_required_system_features(&self, has_known_output_paths: bool) -> BTreeSet<String> {
		let mut features = self.required_system_features.clone();
		if !has_known_output_paths {
			features.insert("ca-derivations".to_owned());
		}
		features
	}

	/// `substitutesAllowed`.
	pub fn substitutes_allowed(&self, always_allow_substitutes: bool) -> bool {
		always_allow_substitutes || self.allow_substitutes
	}

	/// `useUidRange`.
	pub fn use_uid_range(&self, has_known_output_paths: bool) -> bool {
		self.get_required_system_features(has_known_output_paths)
			.contains("uid-range")
	}

	/// `adl_serializer<DerivationOptions<Inputs>>::to_json`.
	pub fn to_json(&self) -> Value {
		let output_checks = match &self.output_checks {
			OutputChecksSpec::ForAllOutputs(checks) => json!({ "forAllOutputs": checks.to_json() }),
			OutputChecksSpec::PerOutput(map) => {
				let map: Map<String, Value> =
					map.iter().map(|(k, v)| (k.clone(), v.to_json())).collect();
				json!({ "perOutput": map })
			}
		};
		let export_references_graph: Map<String, Value> = self
			.export_references_graph
			.iter()
			.map(|(k, v)| (k.clone(), Value::Array(v.iter().map(I::to_json).collect())))
			.collect();
		json!({
			"outputChecks": output_checks,
			"unsafeDiscardReferences": self.unsafe_discard_references,
			"passAsFile": self.pass_as_file,
			"exportReferencesGraph": export_references_graph,
			"additionalSandboxProfile": self.additional_sandbox_profile,
			"noChroot": self.no_chroot,
			"impureHostDeps": self.impure_host_deps,
			"impureEnvVars": self.impure_env_vars,
			"allowLocalNetworking": self.allow_local_networking,
			"requiredSystemFeatures": self.required_system_features,
			"preferLocalBuild": self.prefer_local_build,
			"allowSubstitutes": self.allow_substitutes,
		})
	}

	/// `adl_serializer<DerivationOptions<Inputs>>::from_json`.
	pub fn from_json(value: &Value) -> Result<Self> {
		let object = get_object(value)?;
		let output_checks = get_object(value_at(object, "outputChecks")?)?;
		let output_checks = match (
			output_checks.get("forAllOutputs"),
			output_checks.get("perOutput"),
		) {
			(Some(all), None) => OutputChecksSpec::ForAllOutputs(OutputChecks::from_json(all)?),
			(None, Some(per_output)) => OutputChecksSpec::PerOutput(
				get_object(per_output)?
					.iter()
					.map(|(k, v)| Ok((k.clone(), OutputChecks::from_json(v)?)))
					.collect::<Result<_>>()?,
			),
			_ => {
				return Err(BuildError::Other(
					"Exactly one of 'perOutput' or 'forAllOutputs' is required".to_owned(),
				));
			}
		};
		let unsafe_discard_references = get_object(value_at(object, "unsafeDiscardReferences")?)?
			.iter()
			.map(|(k, v)| Ok((k.clone(), get_boolean(v)?)))
			.collect::<Result<_>>()?;
		let export_references_graph = get_object(value_at(object, "exportReferencesGraph")?)?
			.iter()
			.map(|(k, v)| {
				let inputs = v
					.as_array()
					.ok_or_else(|| wrong_type(v, "array"))?
					.iter()
					.map(I::from_json)
					.collect::<Result<_>>()?;
				Ok((k.clone(), inputs))
			})
			.collect::<Result<_>>()?;
		Ok(Self {
			output_checks,
			unsafe_discard_references,
			pass_as_file: get_string_set(value_at(object, "passAsFile")?)?,
			export_references_graph,
			additional_sandbox_profile: get_string(value_at(object, "additionalSandboxProfile")?)?
				.to_owned(),
			no_chroot: get_boolean(value_at(object, "noChroot")?)?,
			impure_host_deps: get_string_set(value_at(object, "impureHostDeps")?)?,
			impure_env_vars: get_string_set(value_at(object, "impureEnvVars")?)?,
			allow_local_networking: get_boolean(value_at(object, "allowLocalNetworking")?)?,
			required_system_features: get_string_set(value_at(object, "requiredSystemFeatures")?)?,
			prefer_local_build: get_boolean(value_at(object, "preferLocalBuild")?)?,
			allow_substitutes: get_boolean(value_at(object, "allowSubstitutes")?)?,
		})
	}
}

fn ref_set_to_json<I: Input>(set: &BTreeSet<DrvRef<I>>) -> Value {
	Value::Array(set.iter().map(DrvRef::to_json).collect())
}

fn ref_set_from_json<I: Input>(value: &Value) -> Result<BTreeSet<DrvRef<I>>> {
	value
		.as_array()
		.ok_or_else(|| wrong_type(value, "array"))?
		.iter()
		.map(DrvRef::from_json)
		.collect()
}

fn nullable(value: &Value) -> Option<&Value> {
	(!value.is_null()).then_some(value)
}

impl<I: Input> OutputChecks<I> {
	fn to_json(&self) -> Value {
		json!({
			"ignoreSelfRefs": self.ignore_self_refs,
			"maxSize": self.max_size,
			"maxClosureSize": self.max_closure_size,
			"allowedReferences": self.allowed_references.as_ref().map(ref_set_to_json),
			"disallowedReferences": ref_set_to_json(&self.disallowed_references),
			"allowedRequisites": self.allowed_requisites.as_ref().map(ref_set_to_json),
			"disallowedRequisites": ref_set_to_json(&self.disallowed_requisites),
		})
	}

	fn from_json(value: &Value) -> Result<Self> {
		let object = get_object(value)?;
		Ok(Self {
			ignore_self_refs: get_boolean(value_at(object, "ignoreSelfRefs")?)?,
			max_size: nullable(value_at(object, "maxSize")?)
				.map(get_unsigned)
				.transpose()?,
			max_closure_size: nullable(value_at(object, "maxClosureSize")?)
				.map(get_unsigned)
				.transpose()?,
			allowed_references: nullable(value_at(object, "allowedReferences")?)
				.map(ref_set_from_json)
				.transpose()?,
			disallowed_references: ref_set_from_json(value_at(object, "disallowedReferences")?)?,
			allowed_requisites: nullable(value_at(object, "allowedRequisites")?)
				.map(ref_set_from_json)
				.transpose()?,
			disallowed_requisites: ref_set_from_json(value_at(object, "disallowedRequisites")?)?,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn env(pairs: &[(&str, &str)]) -> BTreeMap<BString, BString> {
		pairs
			.iter()
			.map(|(k, v)| (BString::from(*k), BString::from(*v)))
			.collect()
	}

	fn options(e: &BTreeMap<BString, BString>) -> Result<DerivationOptions<DerivedInput>> {
		DerivationOptions::new(e, None, &BTreeMap::new(), false, &mut Vec::new())
	}

	#[test]
	fn legacy_attributes() {
		let e = env(&[
			("passAsFile", "a  b\tc"),
			(
				"allowedReferences",
				"out /nix/store/00000000000000000000000000000000-x",
			),
			(
				"exportReferencesGraph",
				"graph /nix/store/00000000000000000000000000000000-x/sub",
			),
			("preferLocalBuild", "1"),
			("allowSubstitutes", ""),
			("requiredSystemFeatures", "kvm big-parallel"),
		]);
		let o = options(&e).unwrap();
		assert_eq!(o.pass_as_file, ["a", "b", "c"].map(String::from).into());
		assert!(o.prefer_local_build);
		assert!(!o.allow_substitutes);
		let OutputChecksSpec::ForAllOutputs(checks) = o.output_checks else {
			panic!()
		};
		assert!(checks.ignore_self_refs);
		assert_eq!(checks.allowed_references.unwrap().len(), 2);
		assert_eq!(o.export_references_graph["graph"].len(), 1);
		assert_eq!(o.required_system_features.len(), 2);
	}

	#[test]
	fn rejects_odd_export_graph() {
		let e = env(&[("exportReferencesGraph", "a")]);
		assert_eq!(
			options(&e).unwrap_err().to_string(),
			"odd number of tokens in 'exportReferencesGraph': 'a'"
		);
	}

	#[test]
	fn structured_attrs_warn_about_ignored_top_level_attributes() {
		let e = env(&[("passAsFile", "a")]);
		let parsed =
			serde_json::from_str::<Value>(r#"{"maxSize": 1, "outputChecks": {}}"#).unwrap();
		let mut warnings = Vec::new();
		let o = DerivationOptions::new(
			&e,
			parsed.as_object(),
			&BTreeMap::new(),
			false,
			&mut warnings,
		)
		.unwrap();
		assert!(o.pass_as_file.is_empty());
		assert_eq!(
			warnings,
			[
				"'structuredAttrs' disables the effect of the top-level attribute 'maxSize'; use 'outputChecks' instead",
				"'structuredAttrs' disables the effect of the top-level attribute 'passAsFile'; because all JSON is always passed via file",
			]
		);
	}
}
