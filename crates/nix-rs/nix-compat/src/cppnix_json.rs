//! The JSON encodings CppNix 2.35.2 uses for store objects.
//!
//! These differ from the `serde` implementations elsewhere in this crate,
//! which follow Snix's conventions. For example, CppNix writes store paths
//! as bare `<digest>-<name>` strings and content addresses as an object with
//! an SRI hash. Each function here mirrors one of CppNix's
//! `adl_serializer<T>::to_json`/`from_json` pairs and names it in its
//! documentation.

use std::collections::{BTreeMap, BTreeSet};

use data_encoding::BASE64;

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
	#[error("unsupported path info JSON format version {0}; supported versions are 1, 2 and 3")]
	Version(u64),
	#[error("'storeDir' field is required in path info JSON format version 2")]
	MissingStoreDir,
	#[error("path '{path}' is not in the store directory '{store_dir}'")]
	NotInStore { path: String, store_dir: String },
	#[error("invalid content address '{0}'")]
	ContentAddress(String),
	#[error("invalid signature '{0}'")]
	Signature(String),
	#[error("unknown compression method '{0}'")]
	Compression(String),
	#[error("expected a non-negative integer for '{0}'")]
	Unsigned(&'static str),
	#[error("expected an integer for '{0}'")]
	Integer(&'static str),
	#[error("unknown build result status '{0}'")]
	BuildStatus(String),
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

/// The versions of CppNix's path info JSON (`PathInfoJsonFormat`).
///
/// Version 1 writes store paths with the store directory, the content
/// address in its `fixed:r:sha256:...` text form, and signatures as
/// `name:base64` strings. Version 2 uses bare store path names and a content
/// address object. Version 3 also writes signatures as objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PathInfoFormat {
	V1 = 1,
	V2 = 2,
	V3 = 3,
}

impl PathInfoFormat {
	fn from_json(value: &Value) -> Result<Self, Error> {
		match value.as_u64() {
			Some(1) => Ok(Self::V1),
			Some(2) => Ok(Self::V2),
			Some(3) => Ok(Self::V3),
			Some(other) => Err(Error::Version(other)),
			None => Err(Error::Unsigned("version")),
		}
	}
}

/// A signature on a store path, as CppNix's `Signature`: a key name and raw
/// signature bytes of any length. Ordered by name, then bytes, like the
/// `std::set<Signature>` CppNix keeps.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Signature {
	pub key_name: String,
	pub sig: Vec<u8>,
}

impl Signature {
	/// Parses `<key-name>:<base64>`, splitting at the first colon.
	pub fn parse(s: &str) -> Result<Self, Error> {
		let (key_name, sig) = s
			.split_once(':')
			.ok_or_else(|| Error::Signature(s.to_owned()))?;
		if key_name.is_empty() {
			return Err(Error::Signature(s.to_owned()));
		}
		let sig = BASE64
			.decode(sig.as_bytes())
			.map_err(|_| Error::Signature(s.to_owned()))?;
		Ok(Self {
			key_name: key_name.to_owned(),
			sig,
		})
	}

	fn to_colon_string(&self) -> String {
		format!("{}:{}", self.key_name, BASE64.encode(&self.sig))
	}

	/// `adl_serializer<Signature>::to_json`.
	pub fn to_json(&self) -> Value {
		json!({ "keyName": self.key_name, "sig": BASE64.encode(&self.sig) })
	}

	/// `adl_serializer<Signature>::from_json`, which accepts either form in
	/// every format version.
	pub fn from_json(value: &Value) -> Result<Self, Error> {
		if let Value::String(s) = value {
			return Self::parse(s);
		}
		let object = as_object(value)?;
		let key_name = as_str(field(object, "keyName")?, "key name string")?;
		let sig = as_str(field(object, "sig")?, "base64 signature string")?;
		Ok(Self {
			key_name: key_name.to_owned(),
			sig: BASE64
				.decode(sig.as_bytes())
				.map_err(|_| Error::Signature(sig.to_owned()))?,
		})
	}
}

/// CppNix's `UnkeyedValidPathInfo`: what the store records about a path,
/// without the path itself.
///
/// As in CppNix, a registration time of 0 means unknown and is written as
/// `null`, and `ultimate` and the signatures are impure information that
/// only appears when requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathInfo {
	pub store_dir: String,
	pub nar_hash: NixHash,
	pub nar_size: u64,
	pub references: BTreeSet<StorePath>,
	pub ca: Option<CAHash>,
	pub deriver: Option<StorePath>,
	pub registration_time: i64,
	pub ultimate: bool,
	pub signatures: BTreeSet<Signature>,
}

impl PathInfo {
	/// A path info with only the required fields, as CppNix's constructor
	/// from a store directory and NAR hash.
	pub fn new(store_dir: impl Into<String>, nar_hash: NixHash) -> Self {
		Self {
			store_dir: store_dir.into(),
			nar_hash,
			nar_size: 0,
			references: BTreeSet::new(),
			ca: None,
			deriver: None,
			registration_time: 0,
			ultimate: false,
			signatures: BTreeSet::new(),
		}
	}

	fn print_store_path(&self, path: &StorePath) -> String {
		format!("{}/{}", self.store_dir, path)
	}

	fn parse_store_path(
		store_dir: &str,
		value: &Value,
		format: PathInfoFormat,
	) -> Result<StorePath, Error> {
		if format != PathInfoFormat::V1 {
			return store_path_from_json(value);
		}
		let s = as_str(value, "store path string")?;
		let base = s
			.strip_prefix(store_dir)
			.and_then(|rest| rest.strip_prefix('/'))
			.ok_or_else(|| Error::NotInStore {
				path: s.to_owned(),
				store_dir: store_dir.to_owned(),
			})?;
		StorePath::from_bytes(base.as_bytes()).map_err(|_| Error::StorePath(s.to_owned()))
	}

	/// `UnkeyedValidPathInfo::toJSON`. `include_impure` adds the deriver,
	/// registration time, `ultimate` flag and signatures.
	pub fn to_json(&self, include_impure: bool, format: PathInfoFormat) -> Value {
		let v1 = format == PathInfoFormat::V1;
		let path = |p: &StorePath| {
			if v1 {
				Value::String(self.print_store_path(p))
			} else {
				store_path_to_json(p)
			}
		};
		let mut object = Map::new();
		object.insert("version".into(), json!(format as u8));
		object.insert("storeDir".into(), json!(self.store_dir));
		object.insert("narHash".into(), hash_to_json(&self.nar_hash));
		object.insert("narSize".into(), json!(self.nar_size));
		object.insert(
			"references".into(),
			Value::Array(self.references.iter().map(path).collect()),
		);
		object.insert(
			"ca".into(),
			match &self.ca {
				None => Value::Null,
				Some(ca) if v1 => Value::String(ca.to_string()),
				Some(ca) => content_address_to_json(ca),
			},
		);
		if include_impure {
			object.insert(
				"deriver".into(),
				self.deriver.as_ref().map_or(Value::Null, path),
			);
			object.insert(
				"registrationTime".into(),
				if self.registration_time == 0 {
					Value::Null
				} else {
					json!(self.registration_time)
				},
			);
			object.insert("ultimate".into(), json!(self.ultimate));
			object.insert(
				"signatures".into(),
				Value::Array(
					self.signatures
						.iter()
						.map(|sig| {
							if format == PathInfoFormat::V3 {
								sig.to_json()
							} else {
								Value::String(sig.to_colon_string())
							}
						})
						.collect(),
				),
			);
		}
		Value::Object(object)
	}

	/// `UnkeyedValidPathInfo::fromJSON`. A missing `version` means version 1.
	/// Version 1 falls back to `default_store_dir` when `storeDir` is absent,
	/// as CppNix falls back to its store's directory.
	pub fn from_json(value: &Value, default_store_dir: &str) -> Result<Self, Error> {
		let object = as_object(value)?;
		let format = object
			.get("version")
			.map_or(Ok(PathInfoFormat::V1), PathInfoFormat::from_json)?;
		let store_dir = match object.get("storeDir") {
			Some(dir) => as_str(dir, "store directory string")?.to_owned(),
			None if format == PathInfoFormat::V1 => default_store_dir.to_owned(),
			None => return Err(Error::MissingStoreDir),
		};
		let mut info = Self::new(store_dir, hash_from_json(field(object, "narHash")?)?);
		info.nar_size = field(object, "narSize")?
			.as_u64()
			.ok_or(Error::Unsigned("narSize"))?;
		let references = field(object, "references")?.as_array().ok_or(Error::Type {
			expected: "array of store paths",
			got: type_name(field(object, "references")?),
		})?;
		for reference in references {
			info.references
				.insert(Self::parse_store_path(&info.store_dir, reference, format)?);
		}
		info.ca = match field(object, "ca")? {
			Value::Null => None,
			ca if format == PathInfoFormat::V1 => {
				let s = as_str(ca, "content address string")?;
				Some(
					CAHash::from_nix_hex_str(s)
						.ok_or_else(|| Error::ContentAddress(s.to_owned()))?,
				)
			}
			ca => Some(content_address_from_json(ca)?),
		};
		if let Some(deriver) = object.get("deriver").filter(|d| !d.is_null()) {
			info.deriver = Some(Self::parse_store_path(&info.store_dir, deriver, format)?);
		}
		if let Some(time) = object.get("registrationTime").filter(|t| !t.is_null()) {
			info.registration_time = time.as_i64().ok_or(Error::Integer("registrationTime"))?;
		}
		if let Some(ultimate) = object.get("ultimate") {
			info.ultimate = ultimate.as_bool().ok_or(Error::Type {
				expected: "boolean",
				got: type_name(ultimate),
			})?;
		}
		if let Some(signatures) = object.get("signatures") {
			let signatures = signatures.as_array().ok_or(Error::Type {
				expected: "array of signatures",
				got: type_name(signatures),
			})?;
			info.signatures = signatures
				.iter()
				.map(Signature::from_json)
				.collect::<Result<_, _>>()?;
		}
		Ok(info)
	}
}

/// Compression methods CppNix accepts in a narinfo (`NIX_FOR_EACH_COMPRESSION_ALGO`).
pub const COMPRESSION_METHODS: &[&str] = &[
	"none", "br", "bzip2", "compress", "grzip", "gzip", "lrzip", "lz4", "lzip", "lzma", "lzop",
	"xz", "zstd",
];

/// CppNix's `UnkeyedNarInfo`: a path info plus where a binary cache keeps the
/// compressed NAR. An empty URL and a zero file size mean absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarInfo {
	pub info: PathInfo,
	pub url: String,
	pub compression: Option<String>,
	pub file_hash: Option<NixHash>,
	pub file_size: u64,
}

impl NarInfo {
	/// `UnkeyedNarInfo::toJSON`. The cache fields are impure information.
	pub fn to_json(&self, include_impure: bool, format: PathInfoFormat) -> Value {
		let mut value = self.info.to_json(include_impure, format);
		if include_impure {
			let object = value.as_object_mut().expect("path info is an object");
			if !self.url.is_empty() {
				object.insert("url".into(), json!(self.url));
			}
			if let Some(compression) = &self.compression {
				object.insert("compression".into(), json!(compression));
			}
			if let Some(hash) = &self.file_hash {
				object.insert("downloadHash".into(), hash_to_json(hash));
			}
			if self.file_size != 0 {
				object.insert("downloadSize".into(), json!(self.file_size));
			}
		}
		value
	}

	/// `UnkeyedNarInfo::fromJSON`.
	pub fn from_json(value: &Value, default_store_dir: &str) -> Result<Self, Error> {
		let info = PathInfo::from_json(value, default_store_dir)?;
		let object = as_object(value)?;
		let url = match object.get("url") {
			Some(url) => as_str(url, "URL string")?.to_owned(),
			None => String::new(),
		};
		let compression = match object.get("compression") {
			Some(method) => {
				let method = as_str(method, "compression method string")?;
				if !COMPRESSION_METHODS.contains(&method) {
					return Err(Error::Compression(method.to_owned()));
				}
				Some(method.to_owned())
			}
			None => None,
		};
		let file_hash = object.get("downloadHash").map(hash_from_json).transpose()?;
		let file_size = match object.get("downloadSize") {
			Some(size) => size.as_u64().ok_or(Error::Unsigned("downloadSize"))?,
			None => 0,
		};
		Ok(Self {
			info,
			url,
			compression,
			file_hash,
			file_size,
		})
	}
}

/// CppNix's `DrvOutput`: one output of a derivation, the key of a
/// realisation. The output name is not validated, as in CppNix.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DrvOutput {
	pub drv_path: StorePath,
	pub output_name: String,
}

impl DrvOutput {
	/// `adl_serializer<DrvOutput>::to_json`.
	pub fn to_json(&self) -> Value {
		json!({ "drvPath": store_path_to_json(&self.drv_path), "outputName": self.output_name })
	}

	/// `adl_serializer<DrvOutput>::from_json`.
	pub fn from_json(value: &Value) -> Result<Self, Error> {
		let object = as_object(value)?;
		Ok(Self {
			drv_path: store_path_from_json(field(object, "drvPath")?)?,
			output_name: as_str(field(object, "outputName")?, "output name string")?.to_owned(),
		})
	}
}

/// CppNix's `UnkeyedRealisation`: the output path a derivation output
/// realised to, with signatures over its fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnkeyedRealisation {
	pub out_path: StorePath,
	pub signatures: BTreeSet<Signature>,
}

impl UnkeyedRealisation {
	/// `adl_serializer<UnkeyedRealisation>::to_json`. Signatures use the
	/// object form.
	pub fn to_json(&self) -> Value {
		json!({
			"outPath": store_path_to_json(&self.out_path),
			"signatures": self.signatures.iter().map(Signature::to_json).collect::<Vec<_>>(),
		})
	}

	/// `adl_serializer<UnkeyedRealisation>::from_json`. Signatures may be
	/// missing, or use either the object or the `name:base64` form.
	pub fn from_json(value: &Value) -> Result<Self, Error> {
		let object = as_object(value)?;
		let signatures = match object.get("signatures") {
			None => BTreeSet::new(),
			Some(signatures) => signatures
				.as_array()
				.ok_or(Error::Type {
					expected: "array of signatures",
					got: type_name(signatures),
				})?
				.iter()
				.map(Signature::from_json)
				.collect::<Result<_, _>>()?,
		};
		Ok(Self {
			out_path: store_path_from_json(field(object, "outPath")?)?,
			signatures,
		})
	}

	/// `UnkeyedRealisation::fingerprint`: the compact JSON of the keyed
	/// realisation without its signatures, which is what gets signed.
	pub fn fingerprint(&self, key: &DrvOutput) -> String {
		json!({
			"key": key.to_json(),
			"value": { "outPath": store_path_to_json(&self.out_path) },
		})
		.to_string()
	}

	/// `UnkeyedRealisation::sign` without inserting the signature.
	pub fn sign<S>(&self, key: &DrvOutput, signer: &crate::narinfo::SigningKey<S>) -> Signature
	where
		S: ed25519::signature::Signer<ed25519::Signature>,
	{
		let signature = signer.sign(self.fingerprint(key).as_bytes());
		Signature {
			key_name: signature.name().to_string(),
			sig: signature.bytes().to_vec(),
		}
	}

	/// `UnkeyedRealisation::checkSignature` for one trusted key: the names
	/// must match and the signature must verify over the fingerprint.
	pub fn check_signature(
		&self,
		key: &DrvOutput,
		verifying_key: &crate::narinfo::VerifyingKey,
		signature: &Signature,
	) -> bool {
		let Ok(bytes) = <[u8; 64]>::try_from(signature.sig.as_slice()) else {
			return false;
		};
		let signature = crate::narinfo::SignatureRef::new(signature.key_name.as_str(), bytes);
		verifying_key.verify(&self.fingerprint(key), &signature)
	}
}

/// CppNix's `Realisation`: an [`UnkeyedRealisation`] with its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Realisation {
	pub id: DrvOutput,
	pub value: UnkeyedRealisation,
}

impl Realisation {
	/// `adl_serializer<Realisation>::to_json`.
	pub fn to_json(&self) -> Value {
		json!({ "key": self.id.to_json(), "value": self.value.to_json() })
	}

	/// `adl_serializer<Realisation>::from_json`.
	pub fn from_json(value: &Value) -> Result<Self, Error> {
		let object = as_object(value)?;
		Ok(Self {
			id: DrvOutput::from_json(field(object, "key")?)?,
			value: UnkeyedRealisation::from_json(field(object, "value")?)?,
		})
	}
}

/// `BuildResult::Success::Status` names.
pub const BUILD_SUCCESS_STATUSES: &[&str] = &[
	"Built",
	"Substituted",
	"AlreadyValid",
	"ResolvesToAlreadyValid",
];

/// `BuildResult::Failure::Status` names.
pub const BUILD_FAILURE_STATUSES: &[&str] = &[
	"PermanentFailure",
	"InputRejected",
	"OutputRejected",
	"TransientFailure",
	"CachedFailure",
	"TimedOut",
	"MiscFailure",
	"DependencyFailed",
	"LogLimitExceeded",
	"NotDeterministic",
	"NoSubstituters",
	"HashMismatch",
];

/// Whether a build succeeded, as the variant inside CppNix's `BuildResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildOutcome {
	Success {
		/// One of [`BUILD_SUCCESS_STATUSES`].
		status: String,
		built_outputs: BTreeMap<String, UnkeyedRealisation>,
	},
	Failure {
		/// One of [`BUILD_FAILURE_STATUSES`].
		status: String,
		error_msg: String,
		is_non_deterministic: bool,
	},
}

/// CppNix's `BuildResult`. CPU times are in microseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildResult {
	pub outcome: BuildOutcome,
	pub times_built: u64,
	pub start_time: u64,
	pub stop_time: u64,
	pub cpu_user: Option<u64>,
	pub cpu_system: Option<u64>,
}

impl BuildResult {
	/// `adl_serializer<BuildResult>::to_json`.
	pub fn to_json(&self) -> Value {
		let mut object = Map::new();
		object.insert("timesBuilt".into(), json!(self.times_built));
		object.insert("startTime".into(), json!(self.start_time));
		object.insert("stopTime".into(), json!(self.stop_time));
		if let Some(cpu_user) = self.cpu_user {
			object.insert("cpuUser".into(), json!(cpu_user));
		}
		if let Some(cpu_system) = self.cpu_system {
			object.insert("cpuSystem".into(), json!(cpu_system));
		}
		match &self.outcome {
			BuildOutcome::Success {
				status,
				built_outputs,
			} => {
				object.insert("success".into(), json!(true));
				object.insert("status".into(), json!(status));
				object.insert(
					"builtOutputs".into(),
					Value::Object(
						built_outputs
							.iter()
							.map(|(name, realisation)| (name.clone(), realisation.to_json()))
							.collect(),
					),
				);
			}
			BuildOutcome::Failure {
				status,
				error_msg,
				is_non_deterministic,
			} => {
				object.insert("success".into(), json!(false));
				object.insert("status".into(), json!(status));
				object.insert("errorMsg".into(), json!(error_msg));
				object.insert("isNonDeterministic".into(), json!(is_non_deterministic));
			}
		}
		Value::Object(object)
	}

	/// `adl_serializer<BuildResult>::from_json`.
	pub fn from_json(value: &Value) -> Result<Self, Error> {
		let object = as_object(value)?;
		let unsigned = |name: &'static str| -> Result<u64, Error> {
			field(object, name)?.as_u64().ok_or(Error::Unsigned(name))
		};
		let optional_unsigned = |name: &'static str| -> Result<Option<u64>, Error> {
			object
				.get(name)
				.map(|value| value.as_u64().ok_or(Error::Unsigned(name)))
				.transpose()
		};
		let boolean = |name: &'static str| -> Result<bool, Error> {
			let value = field(object, name)?;
			value.as_bool().ok_or(Error::Type {
				expected: "boolean",
				got: type_name(value),
			})
		};
		let status = as_str(field(object, "status")?, "status string")?.to_owned();
		let outcome = if boolean("success")? {
			if !BUILD_SUCCESS_STATUSES.contains(&status.as_str()) {
				return Err(Error::BuildStatus(status));
			}
			let outputs = as_object(field(object, "builtOutputs")?)?;
			BuildOutcome::Success {
				status,
				built_outputs: outputs
					.iter()
					.map(|(name, realisation)| {
						Ok((name.clone(), UnkeyedRealisation::from_json(realisation)?))
					})
					.collect::<Result<_, Error>>()?,
			}
		} else {
			if !BUILD_FAILURE_STATUSES.contains(&status.as_str()) {
				return Err(Error::BuildStatus(status));
			}
			BuildOutcome::Failure {
				status,
				error_msg: as_str(field(object, "errorMsg")?, "error message string")?.to_owned(),
				is_non_deterministic: boolean("isNonDeterministic")?,
			}
		};
		Ok(Self {
			outcome,
			times_built: unsigned("timesBuilt")?,
			start_time: unsigned("startTime")?,
			stop_time: unsigned("stopTime")?,
			cpu_user: optional_unsigned("cpuUser")?,
			cpu_system: optional_unsigned("cpuSystem")?,
		})
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
