//! The JSON encodings CppNix 2.35.2 uses for store objects.
//!
//! These differ from the `serde` implementations elsewhere in this crate,
//! which follow Snix's conventions. For example, CppNix writes store paths
//! as bare `<digest>-<name>` strings and content addresses as an object with
//! an SRI hash. Each function here mirrors one of CppNix's
//! `adl_serializer<T>::to_json`/`from_json` pairs and names it in its
//! documentation.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use crate::derivation::OutputName;
use crate::derived_path::{DerivedPath, OutputSpec};
use crate::nixhash::{CAHash, NixHash};
use crate::store_path::StorePath;

/// Experimental feature that allows nested (dynamic) derivation outputs.
pub const DYNAMIC_DERIVATIONS: &str = "dynamic-derivations";
/// Experimental feature that allows `git` content addresses.
pub const GIT_HASHING: &str = "git-hashing";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
	#[error("expected {expected}, got {got}")]
	Type {
		expected: &'static str,
		got: &'static str,
	},
	#[error("missing field '{0}'")]
	MissingField(&'static str),
	#[error("invalid store path '{0}'")]
	StorePath(String),
	#[error("invalid output name '{0}'")]
	OutputName(String),
	#[error("an explicit output set must not be empty")]
	EmptyOutputs,
	#[error("invalid hash '{0}'")]
	Hash(String),
	#[error("unknown content-addressing method '{0}', expected `text`, `flat`, `nar`, or `git`")]
	Method(String),
	#[error("text content addresses must use sha256")]
	TextNotSha256,
	/// CppNix only accepts this input with the named experimental feature,
	/// and nix-rs does not implement that feature yet. Under default settings
	/// both reject it.
	#[error("requires the experimental feature '{0}', which nix-rs does not support")]
	Unsupported(&'static str),
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

fn as_str<'a>(value: &'a Value, expected: &'static str) -> Result<&'a str, Error> {
	value.as_str().ok_or(Error::Type {
		expected,
		got: type_name(value),
	})
}

fn as_object(value: &Value) -> Result<&Map<String, Value>, Error> {
	value.as_object().ok_or(Error::Type {
		expected: "object",
		got: type_name(value),
	})
}

fn field<'a>(object: &'a Map<String, Value>, name: &'static str) -> Result<&'a Value, Error> {
	object.get(name).ok_or(Error::MissingField(name))
}

/// `adl_serializer<StorePath>`: the base name, without the store directory.
pub fn store_path_to_json(path: &StorePath) -> Value {
	Value::String(path.to_string())
}

/// `adl_serializer<StorePath>`: parses a `<digest>-<name>` base name.
pub fn store_path_from_json(value: &Value) -> Result<StorePath, Error> {
	let s = as_str(value, "store path string")?;
	StorePath::from_bytes(s.as_bytes()).map_err(|_| Error::StorePath(s.to_owned()))
}

/// `adl_serializer<Hash>`: an SRI string.
pub fn hash_to_json(hash: &NixHash) -> Value {
	Value::String(hash.to_sri_string())
}

/// `adl_serializer<Hash>`: parses an SRI string.
pub fn hash_from_json(value: &Value) -> Result<NixHash, Error> {
	let s = as_str(value, "SRI hash string")?;
	NixHash::from_sri(s).map_err(|_| Error::Hash(s.to_owned()))
}

/// `adl_serializer<ContentAddress>`: `{"method": ..., "hash": <SRI>}`.
pub fn content_address_to_json(ca: &CAHash) -> Value {
	let method = match ca {
		CAHash::Flat(_) => "flat",
		CAHash::Nar(_) => "nar",
		CAHash::Text(_) => "text",
	};
	json!({ "method": method, "hash": hash_to_json(&ca.hash()) })
}

/// `adl_serializer<ContentAddress>`. The `git` method needs `git-hashing`,
/// which [`CAHash`] cannot represent.
pub fn content_address_from_json(value: &Value) -> Result<CAHash, Error> {
	let object = as_object(value)?;
	let method = as_str(field(object, "method")?, "method string")?;
	let hash = hash_from_json(field(object, "hash")?)?;
	match method {
		"flat" => Ok(CAHash::Flat(hash)),
		"nar" => Ok(CAHash::Nar(hash)),
		"text" => match hash {
			NixHash::Sha256(digest) => Ok(CAHash::Text(digest)),
			_ => Err(Error::TextNotSha256),
		},
		"git" => Err(Error::Unsupported(GIT_HASHING)),
		other => Err(Error::Method(other.to_owned())),
	}
}

fn output_name_from_json(value: &Value) -> Result<OutputName, Error> {
	let s = as_str(value, "output name string")?;
	s.parse().map_err(|_| Error::OutputName(s.to_owned()))
}

fn output_name_to_json(name: &OutputName) -> Value {
	Value::String(String::from_utf8_lossy(name.as_bytes()).into_owned())
}

/// `adl_serializer<OutputsSpec>`: `["*"]` for all outputs, otherwise the
/// sorted names.
pub fn outputs_spec_to_json(spec: &OutputSpec) -> Value {
	match spec {
		OutputSpec::All => json!(["*"]),
		OutputSpec::Named(names) => Value::Array(names.iter().map(output_name_to_json).collect()),
	}
}

/// `adl_serializer<OutputsSpec>`. CppNix reads the array as a set, so
/// duplicates collapse, and only the exact set `{"*"}` means all outputs.
pub fn outputs_spec_from_json(value: &Value) -> Result<OutputSpec, Error> {
	let items = value.as_array().ok_or(Error::Type {
		expected: "array of output names",
		got: type_name(value),
	})?;
	let strings = items
		.iter()
		.map(|item| as_str(item, "output name string"))
		.collect::<Result<BTreeSet<_>, _>>()?;
	if strings.len() == 1 && strings.contains("*") {
		return Ok(OutputSpec::All);
	}
	if strings.is_empty() {
		return Err(Error::EmptyOutputs);
	}
	let names = items
		.iter()
		.map(output_name_from_json)
		.collect::<Result<BTreeSet<_>, _>>()?;
	Ok(OutputSpec::Named(names))
}

/// `adl_serializer<ExtendedOutputsSpec>`: `null` selects the default outputs.
pub fn extended_outputs_spec_to_json(spec: Option<&OutputSpec>) -> Value {
	spec.map_or(Value::Null, outputs_spec_to_json)
}

/// `adl_serializer<ExtendedOutputsSpec>`.
pub fn extended_outputs_spec_from_json(value: &Value) -> Result<Option<OutputSpec>, Error> {
	if value.is_null() {
		Ok(None)
	} else {
		outputs_spec_from_json(value).map(Some)
	}
}

/// The `drvPath` of a built deriving path. CppNix accepts a nested built
/// path here only with `dynamic-derivations`.
fn derivation_path_from_json(value: &Value) -> Result<StorePath, Error> {
	match value {
		Value::Object(_) => Err(Error::Unsupported(DYNAMIC_DERIVATIONS)),
		_ => store_path_from_json(value),
	}
}

/// `adl_serializer<SingleDerivedPath>`: an opaque path, or one output of a
/// derivation, `{"drvPath": ..., "output": ...}`.
pub fn single_derived_path_to_json(drv_path: &StorePath, output: Option<&OutputName>) -> Value {
	match output {
		None => store_path_to_json(drv_path),
		Some(output) => json!({
			"drvPath": store_path_to_json(drv_path),
			"output": output_name_to_json(output),
		}),
	}
}

/// `adl_serializer<SingleDerivedPath>`. Returns the path and, for a built
/// path, the output name.
pub fn single_derived_path_from_json(
	value: &Value,
) -> Result<(StorePath, Option<OutputName>), Error> {
	match value {
		Value::Object(object) => Ok((
			derivation_path_from_json(field(object, "drvPath")?)?,
			Some(output_name_from_json(field(object, "output")?)?),
		)),
		_ => Ok((store_path_from_json(value)?, None)),
	}
}

/// `adl_serializer<DerivedPath>`: an opaque path, or
/// `{"drvPath": ..., "outputs": [...]}`.
pub fn derived_path_to_json(path: &DerivedPath) -> Value {
	match path {
		DerivedPath::Opaque(path) => store_path_to_json(path),
		DerivedPath::Built { drv_path, outputs } => json!({
			"drvPath": store_path_to_json(drv_path),
			"outputs": outputs_spec_to_json(outputs),
		}),
	}
}

/// `adl_serializer<DerivedPath>`.
pub fn derived_path_from_json(value: &Value) -> Result<DerivedPath, Error> {
	match value {
		Value::Object(object) => Ok(DerivedPath::Built {
			drv_path: derivation_path_from_json(field(object, "drvPath")?)?,
			outputs: outputs_spec_from_json(field(object, "outputs")?)?,
		}),
		_ => Ok(DerivedPath::Opaque(store_path_from_json(value)?)),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn outputs_spec_star_only_alone() {
		assert_eq!(
			outputs_spec_from_json(&json!(["*", "*"])),
			Ok(OutputSpec::All)
		);
		assert!(matches!(
			outputs_spec_from_json(&json!(["*", "out"])),
			Err(Error::OutputName(_))
		));
		assert_eq!(outputs_spec_from_json(&json!([])), Err(Error::EmptyOutputs));
	}

	#[test]
	fn content_address_rejects_unknown_method() {
		let hash = "sha256-9vLqj0XYoFfJVmoz+ZR02i5camYE1zYSFlDicwxvsKM=";
		assert_eq!(
			content_address_from_json(&json!({"method": "recursive", "hash": hash})),
			Err(Error::Method("recursive".into()))
		);
		assert_eq!(
			content_address_from_json(&json!({"method": "git", "hash": hash})),
			Err(Error::Unsupported(GIT_HASHING))
		);
	}
}
