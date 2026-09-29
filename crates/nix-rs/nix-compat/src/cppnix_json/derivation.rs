//! CppNix 2.35.2's version-4 derivation and derivation-output JSON.
//!
//! This model preserves output kinds that the ATerm-oriented
//! `UnverifiedDerivation` cannot represent, including floating and impure
//! content-addressed outputs and nested dynamic input dependencies.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use crate::derivation::OutputName;
use crate::nixhash::{CAHash, HashAlgo};
use crate::store_path::StorePath;

use super::{
	DYNAMIC_DERIVATIONS, Error, GIT_HASHING, as_object, as_str, content_address_from_json,
	content_address_to_json, field, output_name_from_json, output_name_to_json,
	store_path_from_json, store_path_to_json, type_name,
};

pub const CA_DERIVATIONS: &str = "ca-derivations";
pub const IMPURE_DERIVATIONS: &str = "impure-derivations";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Features {
	pub ca_derivations: bool,
	pub dynamic_derivations: bool,
	pub impure_derivations: bool,
	pub git_hashing: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Output {
	InputAddressed(StorePath),
	Fixed(CAHash),
	Floating { method: String, hash_algo: HashAlgo },
	Deferred,
	Impure { method: String, hash_algo: HashAlgo },
}

fn method_algo(
	object: &Map<String, Value>,
	features: Features,
) -> Result<(String, HashAlgo), Error> {
	let method = as_str(field(object, "method")?, "method string")?;
	if !matches!(method, "text" | "flat" | "nar" | "git") {
		return Err(Error::Method(method.to_owned()));
	}
	if method == "text" && !features.dynamic_derivations {
		return Err(Error::FeatureRequired(DYNAMIC_DERIVATIONS));
	}
	if method == "git" && !features.git_hashing {
		return Err(Error::FeatureRequired(GIT_HASHING));
	}
	let algo = as_str(field(object, "hashAlgo")?, "hash algorithm string")?;
	let hash_algo = algo.parse().map_err(|_| Error::Hash(algo.to_owned()))?;
	Ok((method.to_owned(), hash_algo))
}

impl Output {
	/// `adl_serializer<DerivationOutput>::from_json`.
	pub fn from_json(value: &Value, features: Features) -> Result<Self, Error> {
		let object = as_object(value)?;
		let keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
		match keys.iter().copied().collect::<Vec<_>>().as_slice() {
			["path"] => Ok(Self::InputAddressed(store_path_from_json(field(
				object, "path",
			)?)?)),
			["hash", "method"] => {
				let ca = content_address_from_json(value)?;
				if matches!(ca, CAHash::Text(_)) && !features.dynamic_derivations {
					return Err(Error::FeatureRequired(DYNAMIC_DERIVATIONS));
				}
				Ok(Self::Fixed(ca))
			}
			["hashAlgo", "method"] => {
				if !features.ca_derivations {
					return Err(Error::FeatureRequired(CA_DERIVATIONS));
				}
				let (method, hash_algo) = method_algo(object, features)?;
				Ok(Self::Floating { method, hash_algo })
			}
			[] => Ok(Self::Deferred),
			["hashAlgo", "impure", "method"] => {
				if !features.impure_derivations {
					return Err(Error::FeatureRequired(IMPURE_DERIVATIONS));
				}
				let (method, hash_algo) = method_algo(object, features)?;
				Ok(Self::Impure { method, hash_algo })
			}
			_ => Err(Error::DerivationOutput),
		}
	}

	/// `adl_serializer<DerivationOutput>::to_json`.
	pub fn to_json(&self) -> Value {
		match self {
			Self::InputAddressed(path) => json!({ "path": store_path_to_json(path) }),
			Self::Fixed(ca) => content_address_to_json(ca),
			Self::Floating { method, hash_algo } => {
				json!({ "method": method, "hashAlgo": hash_algo.as_str() })
			}
			Self::Deferred => json!({}),
			Self::Impure { method, hash_algo } => {
				json!({ "method": method, "hashAlgo": hash_algo.as_str(), "impure": true })
			}
		}
	}
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputNode {
	pub outputs: BTreeSet<OutputName>,
	pub dynamic_outputs: BTreeMap<OutputName, InputNode>,
}

impl InputNode {
	fn from_json(value: &Value, features: Features) -> Result<Self, Error> {
		let object = as_object(value)?;
		let outputs = field(object, "outputs")?.as_array().ok_or(Error::Type {
			expected: "array of output names",
			got: type_name(field(object, "outputs")?),
		})?;
		let outputs = outputs
			.iter()
			.map(output_name_from_json)
			.collect::<Result<BTreeSet<_>, _>>()?;
		let children = as_object(field(object, "dynamicOutputs")?)?;
		let mut dynamic_outputs = BTreeMap::new();
		for (name, child) in children {
			if !features.dynamic_derivations {
				return Err(Error::FeatureRequired(DYNAMIC_DERIVATIONS));
			}
			let name = output_name_from_json(&Value::String(name.clone()))?;
			dynamic_outputs.insert(name, Self::from_json(child, features)?);
		}
		Ok(Self {
			outputs,
			dynamic_outputs,
		})
	}

	fn to_json(&self) -> Value {
		let children: Map<String, Value> = self
			.dynamic_outputs
			.iter()
			.map(|(name, node)| (name.to_string(), node.to_json()))
			.collect();
		json!({
			"outputs": self.outputs.iter().map(output_name_to_json).collect::<Vec<_>>(),
			"dynamicOutputs": children,
		})
	}
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Derivation {
	pub name: String,
	pub outputs: BTreeMap<OutputName, Output>,
	pub system: String,
	pub builder: String,
	pub args: Vec<String>,
	pub env: BTreeMap<String, String>,
	pub input_sources: BTreeSet<StorePath>,
	pub input_derivations: BTreeMap<StorePath, InputNode>,
	pub structured_attrs: Option<Value>,
}

impl Derivation {
	/// `adl_serializer<Derivation>::from_json` for version 4.
	pub fn from_json(value: &Value, features: Features) -> Result<Self, Error> {
		let object = as_object(value)?;
		let version = field(object, "version")?
			.as_u64()
			.ok_or(Error::Unsigned("version"))?;
		if version != 4 {
			return Err(Error::DerivationVersion(version));
		}
		let outputs = as_object(field(object, "outputs")?)?;
		let mut parsed_outputs = BTreeMap::new();
		for (name, value) in outputs {
			let name = output_name_from_json(&Value::String(name.clone()))?;
			parsed_outputs.insert(name, Output::from_json(value, features)?);
		}
		let args = field(object, "args")?.as_array().ok_or(Error::Type {
			expected: "array of strings",
			got: type_name(field(object, "args")?),
		})?;
		let args = args
			.iter()
			.map(|value| as_str(value, "string").map(str::to_owned))
			.collect::<Result<Vec<_>, _>>()?;
		let env = as_object(field(object, "env")?)?
			.iter()
			.map(|(name, value)| Ok((name.clone(), as_str(value, "string")?.to_owned())))
			.collect::<Result<BTreeMap<_, _>, Error>>()?;
		let inputs = as_object(field(object, "inputs")?)?;
		let srcs = field(inputs, "srcs")?.as_array().ok_or(Error::Type {
			expected: "array of store paths",
			got: type_name(field(inputs, "srcs")?),
		})?;
		let input_sources = srcs
			.iter()
			.map(store_path_from_json)
			.collect::<Result<BTreeSet<_>, _>>()?;
		let mut input_derivations = BTreeMap::new();
		for (path, node) in as_object(field(inputs, "drvs")?)? {
			let path = store_path_from_json(&Value::String(path.clone()))?;
			input_derivations.insert(path, InputNode::from_json(node, features)?);
		}
		Ok(Self {
			name: as_str(field(object, "name")?, "string")?.to_owned(),
			outputs: parsed_outputs,
			system: as_str(field(object, "system")?, "string")?.to_owned(),
			builder: as_str(field(object, "builder")?, "string")?.to_owned(),
			args,
			env,
			input_sources,
			input_derivations,
			structured_attrs: object.get("structuredAttrs").cloned(),
		})
	}

	/// `adl_serializer<Derivation>::to_json` for version 4.
	pub fn to_json(&self) -> Value {
		let outputs: Map<String, Value> = self
			.outputs
			.iter()
			.map(|(name, output)| (name.to_string(), output.to_json()))
			.collect();
		let input_derivations: Map<String, Value> = self
			.input_derivations
			.iter()
			.map(|(path, node)| (path.to_string(), node.to_json()))
			.collect();
		let mut value = json!({
			"name": self.name,
			"version": 4,
			"outputs": outputs,
			"system": self.system,
			"builder": self.builder,
			"args": self.args,
			"env": self.env,
			"inputs": {
				"srcs": self.input_sources.iter().map(store_path_to_json).collect::<Vec<_>>(),
				"drvs": input_derivations,
			},
		});
		if let Some(attrs) = &self.structured_attrs {
			value
				.as_object_mut()
				.unwrap()
				.insert("structuredAttrs".to_owned(), attrs.clone());
		}
		value
	}
}
