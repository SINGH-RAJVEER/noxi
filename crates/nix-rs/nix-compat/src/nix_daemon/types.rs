use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::derivation::{
	DerivationBuilder, DerivationError, OutputName, Outputs, UnverifiedDerivation,
};
use crate::derived_path::DerivedPath;

use crate::wire::de::Error;
use crate::{
	narinfo::Signature,
	nixhash::CAHash,
	store_path::StorePath,
	wire::{
		de::{NixDeserialize, NixRead},
		ser::{NixSerialize, NixWrite},
	},
};
use bstr::BString;
use bytes::Bytes;
use nix_compat_derive::{NixDeserialize, NixSerialize};

/// Marker type that consumes/sends and ignores a u64.
#[derive(Clone, Debug, NixDeserialize, NixSerialize)]
#[nix(from = "u64", into = "u64")]
pub struct IgnoredZero;
impl From<u64> for IgnoredZero {
	fn from(_: u64) -> Self {
		IgnoredZero
	}
}

impl From<IgnoredZero> for u64 {
	fn from(_: IgnoredZero) -> Self {
		0
	}
}

#[derive(
	Debug,
	Clone,
	Copy,
	PartialEq,
	Eq,
	PartialOrd,
	Ord,
	Hash,
	num_enum::TryFromPrimitive,
	num_enum::IntoPrimitive,
	NixDeserialize,
	NixSerialize,
)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[nix(try_from = "u16", into = "u16")]
#[repr(u16)]
pub enum BuildMode {
	Normal = 0,
	Repair = 1,
	Check = 2,
}

#[derive(
	Debug,
	Clone,
	Copy,
	PartialEq,
	Eq,
	PartialOrd,
	Ord,
	Hash,
	num_enum::TryFromPrimitive,
	num_enum::IntoPrimitive,
	NixDeserialize,
	NixSerialize,
)]
#[nix(try_from = "u16", into = "u16")]
#[repr(u16)]
pub enum BuildStatus {
	Built = 0,
	Substituted = 1,
	AlreadyValid = 2,
	PermanentFailure = 3,
	InputRejected = 4,
	OutputRejected = 5,
	TransientFailure = 6,
	CachedFailure = 7,
	TimedOut = 8,
	MiscFailure = 9,
	DependencyFailed = 10,
	LogLimitExceeded = 11,
	NotDeterministic = 12,
	ResolvesToAlreadyValid = 13,
	NoSubstituters = 14,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, NixDeserialize, NixSerialize)]
#[repr(transparent)]
pub struct Microseconds(i64);

impl From<i64> for Microseconds {
	fn from(value: i64) -> Self {
		Microseconds(value)
	}
}

impl From<Microseconds> for Duration {
	fn from(value: Microseconds) -> Self {
		Duration::from_micros(value.0.unsigned_abs())
	}
}

impl TryFrom<Duration> for Microseconds {
	type Error = std::num::TryFromIntError;
	fn try_from(value: Duration) -> Result<Self, Self::Error> {
		Ok(Microseconds(value.as_micros().try_into()?))
	}
}

impl From<Microseconds> for i64 {
	fn from(value: Microseconds) -> Self {
		value.0
	}
}

impl NixDeserialize for Option<Microseconds> {
	async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
	where
		R: ?Sized + NixRead + Send,
	{
		if let Some(tag) = reader.try_read_value::<u8>().await? {
			match tag {
				0 => Ok(Some(None)),
				1 => Ok(Some(Some(reader.read_value::<Microseconds>().await?))),
				_ => Err(R::Error::invalid_data("invalid optional tag from remote")),
			}
		} else {
			Ok(None)
		}
	}
}

impl NixSerialize for Option<Microseconds> {
	async fn serialize<W>(&self, writer: &mut W) -> Result<(), W::Error>
	where
		W: NixWrite,
	{
		if let Some(value) = self.as_ref() {
			writer.write_number(1).await?;
			writer.write_value(value).await
		} else {
			writer.write_number(0).await
		}
	}
}

#[derive(Debug, NixSerialize)]
pub struct TraceLine {
	have_pos: IgnoredZero,
	hint: String,
}

/// Represents an error returned by the nix-daemon to its client.
///
/// Adheres to the format described in serialization.md
#[derive(NixSerialize)]
pub struct NixError {
	#[nix(version = "26..")]
	type_: &'static str,

	#[nix(version = "26..")]
	level: u64,

	#[nix(version = "26..")]
	name: &'static str,

	msg: String,
	#[nix(version = "26..")]
	have_pos: IgnoredZero,

	#[nix(version = "26..")]
	traces: Vec<TraceLine>,

	#[nix(version = "..=25")]
	exit_status: u64,
}

impl NixError {
	pub fn new(msg: String) -> Self {
		Self {
			type_: "Error",
			level: 0, // error
			name: "Error",
			msg,
			have_pos: IgnoredZero {},
			traces: vec![],
			exit_status: 1,
		}
	}
}

impl NixSerialize for Option<UnkeyedValidPathInfo> {
	async fn serialize<W>(&self, writer: &mut W) -> Result<(), W::Error>
	where
		W: NixWrite,
	{
		match self {
			Some(value) => {
				writer.write_value(&true).await?;
				writer.write_value(value).await
			}
			None => writer.write_value(&false).await,
		}
	}
}

#[derive(NixSerialize, NixDeserialize, Debug, Clone, PartialEq)]
pub struct UnkeyedValidPathInfo {
	pub deriver: Option<StorePath>,
	pub nar_hash: NarHash,
	pub references: Vec<StorePath>,
	pub registration_time: u64,
	pub nar_size: u64,
	// CppNix only sends the following from protocol 1.16 on.
	#[nix(version = "16..")]
	pub ultimate: bool,
	#[nix(version = "16..")]
	pub signatures: Vec<Signature<String>>,
	#[nix(version = "16..")]
	pub ca: Option<CAHash>,
}

/// Request tuple for [super::worker_protocol::Operation::QueryValidPaths]
#[derive(NixDeserialize)]
pub struct QueryValidPaths {
	// Paths to query
	pub paths: Vec<StorePath>,

	// Whether to try and substitute the paths.
	#[nix(version = "27..")]
	pub substitute: bool,
}

/// Request tuple for [super::worker_protocol::Operation::BuildPaths]
#[derive(NixDeserialize)]
pub struct BuildPaths {
	// Paths to build
	pub paths: Vec<DerivedPath>,

	// How to build the paths
	pub mode: BuildMode,
}

/// newtype wrapper for the byte array that correctly implements NixSerialize, NixDeserialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NarHash([u8; 32]);

impl NarHash {
	pub fn from_digest(digest: [u8; 32]) -> Self {
		NarHash(digest)
	}
}

impl std::ops::Deref for NarHash {
	type Target = [u8; 32];

	fn deref(&self) -> &Self::Target {
		&self.0
	}
}

impl NixDeserialize for NarHash {
	async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
	where
		R: ?Sized + NixRead + Send,
	{
		if let Some(bytes) = reader.try_read_bytes().await? {
			let result = data_encoding::HEXLOWER
				.decode(bytes.as_ref())
				.map_err(R::Error::invalid_data)?;
			Ok(Some(NarHash(result.try_into().map_err(|_| {
				R::Error::invalid_data("incorrect length")
			})?)))
		} else {
			Ok(None)
		}
	}
}

impl NixSerialize for NarHash {
	async fn serialize<W>(&self, writer: &mut W) -> Result<(), W::Error>
	where
		W: NixWrite,
	{
		// CppNix writes the NAR hash as Base16, and reads any encoding.
		data_encoding::HEXLOWER
			.encode(&self.0)
			.serialize(writer)
			.await
	}
}

/// Info type used by [super::worker_protocol::Operation::AddToStoreNar] and [super::worker_protocol::Operation::AddMultipleToStore]
///
/// Carries the path, signatures, and metadata returned by the daemon protocol.
#[derive(NixDeserialize, NixSerialize, Debug, Clone, PartialEq)]
pub struct ValidPathInfo {
	// - path :: [StorePath][se-StorePath]
	pub path: StorePath,
	pub info: UnkeyedValidPathInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, NixDeserialize, NixSerialize)]
pub struct BuildResult {
	pub status: BuildStatus,
	pub error_msg: Bytes,
	#[nix(version = "29..")]
	pub times_built: u32,
	#[nix(version = "29..")]
	pub is_non_deterministic: bool,
	#[nix(version = "29..")]
	pub start_time: u64,
	#[nix(version = "29..")]
	pub stop_time: u64,
	#[nix(version = "37..")]
	pub cpu_user: Option<Microseconds>,
	#[nix(version = "37..")]
	pub cpu_system: Option<Microseconds>,
	#[nix(version = "28..")]
	pub built_outputs: BuiltOutputs,
}

/// The path and signatures of one built output, as in CppNix's
/// `UnkeyedRealisation`. The output it belongs to is its key in [`BuiltOutputs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnkeyedRealisation {
	pub out_path: StorePath,
	pub signatures: Vec<Signature<String>>,
}

/// The outputs of a build, by output name, as CppNix's `BuildResult` keeps them.
///
/// Without the `realisation-with-path-not-hash` protocol feature (which
/// nix-compat does not negotiate), CppNix sends these from protocol 1.28 on as
/// a string map. Each key is `sha256:<zero hash>!<output name>` and each value
/// the JSON `{"id":<key>,"outPath":<path>}`. When reading, it only uses the
/// output name and the path, so signatures do not travel in this form.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BuiltOutputs(pub BTreeMap<OutputName, UnkeyedRealisation>);

/// The key CppNix writes for an output in the string-map form.
fn compat_output_id(output_name: &OutputName) -> String {
	format!("sha256:{}!{output_name}", "0".repeat(64))
}

impl NixSerialize for BuiltOutputs {
	async fn serialize<W>(&self, writer: &mut W) -> Result<(), W::Error>
	where
		W: NixWrite,
	{
		use crate::wire::ser::Error;
		writer.write_value(&self.0.len()).await?;
		for (output_name, realisation) in &self.0 {
			let id = compat_output_id(output_name);
			// Keys in the order CppNix's JSON library writes them.
			let json = format!(
				"{{\"id\":{},\"outPath\":{}}}",
				serde_json::to_string(&id).map_err(W::Error::custom)?,
				serde_json::to_string(&realisation.out_path.to_string())
					.map_err(W::Error::custom)?,
			);
			writer.write_value(&id).await?;
			writer.write_value(&json).await?;
		}
		Ok(())
	}
}

impl NixDeserialize for BuiltOutputs {
	async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
	where
		R: ?Sized + NixRead + Send,
	{
		let Some(entries) = reader.try_read_value::<BTreeMap<String, String>>().await? else {
			return Ok(None);
		};
		let mut outputs = BTreeMap::new();
		for (id, json) in entries {
			let (_, output_name) = id.split_once('!').ok_or_else(|| {
				R::Error::invalid_data(format!("invalid derivation output id {id}"))
			})?;
			let output_name: OutputName = output_name.parse().map_err(R::Error::invalid_data)?;
			let value: serde_json::Value =
				serde_json::from_str(&json).map_err(R::Error::invalid_data)?;
			let out_path = value
				.get("outPath")
				.and_then(serde_json::Value::as_str)
				.ok_or_else(|| R::Error::invalid_data(format!("realisation {id} has no outPath")))?
				.parse::<StorePath>()
				.map_err(R::Error::invalid_data)?;
			outputs.insert(
				output_name,
				UnkeyedRealisation {
					out_path,
					signatures: Vec::new(),
				},
			);
		}
		Ok(Some(BuiltOutputs(outputs)))
	}
}

pub type KeyedBuildResults = Vec<KeyedBuildResult>;
#[derive(Debug, Clone, PartialEq, Eq, NixDeserialize, NixSerialize)]
pub struct KeyedBuildResult {
	pub path: DerivedPath,
	pub result: BuildResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, NixDeserialize, NixSerialize)]
pub struct QueryMissingResult {
	pub will_build: Vec<StorePath>,
	pub will_substitute: Vec<StorePath>,
	pub unknown: Vec<StorePath>,
	pub download_size: u64,
	pub nar_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, NixDeserialize, NixSerialize)]
pub struct BasicDerivation {
	pub outputs: Outputs,
	pub input_sources: BTreeSet<StorePath>,
	pub system: String,
	pub command: String,
	pub arguments: Vec<String>,
	pub environment: BTreeMap<String, BString>,
}

impl TryFrom<BasicDerivation> for UnverifiedDerivation {
	type Error = DerivationError;

	fn try_from(value: BasicDerivation) -> Result<Self, Self::Error> {
		DerivationBuilder {
			input_sources: value.input_sources,
			system: value.system,
			command: value.command,
			arguments: value.arguments,
			environment: value.environment,
			..Default::default()
		}
		.build_unverified(value.outputs)
	}
}
