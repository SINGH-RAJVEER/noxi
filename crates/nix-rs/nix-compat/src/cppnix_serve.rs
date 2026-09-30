//! CppNix's serve protocol 2.x value codecs, from `serve-protocol.cc`.
//!
//! Wrap the CppNix JSON models in [`Serve`] and set the reader/writer's
//! [`ProtocolVersion`] explicitly. These codecs use `/nix/store`, as do the
//! existing wire store-path codecs. They do not implement serve commands.
//! Worker-protocol models and their 1.x version gates are deliberately separate.
//! Paths use Unix lexical normalization without filesystem or symlink access.
//! Signature key names must fit the existing JSON model's UTF-8 strings.
//!
//! The reference's lossy behaviour is preserved: old build-output maps lose
//! derivation hashes, dependencies and signatures, CPU times are not sent, and
//! path info omits registration time and ultimate and discards download size.

#![cfg(all(feature = "serde", feature = "wire"))]

use std::collections::{BTreeMap, BTreeSet};

use data_encoding::BASE64;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::cppnix_json::{
	BuildOutcome, BuildResult, DrvOutput, PathInfo, Realisation, Signature, UnkeyedRealisation,
};
use crate::nixhash::{CAHash, NixHash};
use crate::store_path::{STORE_DIR, StorePath};
use crate::wire::ProtocolVersion;
use crate::wire::de::{Error as _, NixDeserialize, NixRead};
use crate::wire::ser::{Error as _, NixSerialize, NixWrite};

/// Selects the serve encoding of a CppNix store value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Serve<T>(pub T);

fn read_version<R: ?Sized + NixRead>(reader: &R, minimum: u8) -> Result<(), R::Error> {
	let version = reader.version();
	if version.major() != 2 || version.minor() < minimum {
		return Err(R::Error::invalid_data(format!(
			"serve protocol {version} cannot encode this value; requires 2.{minimum} or newer"
		)));
	}
	Ok(())
}

fn write_version<W: NixWrite>(writer: &W, minimum: u8) -> Result<(), W::Error> {
	let version = writer.version();
	if version.major() != 2 || version.minor() < minimum {
		return Err(W::Error::unsupported_data(format!(
			"serve protocol {version} cannot encode this value; requires 2.{minimum} or newer"
		)));
	}
	Ok(())
}

fn bounded<E: crate::wire::de::Error>(number: u64, maximum: u64) -> Result<u64, E> {
	if number > maximum {
		Err(E::invalid_data("serialised integer is too large"))
	} else {
		Ok(number)
	}
}

async fn read_u32<R: ?Sized + NixRead>(reader: &mut R) -> Result<u64, R::Error> {
	bounded(reader.read_number().await?, u32::MAX as u64)
}

fn validate_store_path_name(path: &StorePath) -> Result<(), &'static str> {
	let name = path.name();
	// path.cc::checkName rejects '.' or '..' followed by end or a dash.
	// StorePath already enforces the remaining alphabet and length rules.
	if matches!(name, "." | "..") || name.starts_with(".-") || name.starts_with("..-") {
		return Err("invalid store path name: first dash-separated component is '.' or '..'");
	}
	Ok(())
}

fn parse_store_path_base<E: crate::wire::de::Error>(base: &[u8]) -> Result<StorePath, E> {
	let path = StorePath::from_bytes(base).map_err(E::invalid_data)?;
	validate_store_path_name(&path).map_err(E::invalid_data)?;
	Ok(path)
}

fn parse_store_path<E: crate::wire::de::Error>(text: &[u8]) -> Result<StorePath, E> {
	if !text.starts_with(b"/") {
		return Err(E::invalid_data("store path is not absolute"));
	}
	// Unix canonPathInner with resolveSymlinks=false: only lexical components,
	// no filesystem access. Use bytes so canceled non-UTF-8 components work too.
	let mut components = Vec::new();
	for component in text.split(|byte| *byte == b'/') {
		match component {
			b"" | b"." => {}
			b".." => {
				components.pop();
			}
			_ => components.push(component),
		}
	}
	if components.len() != 3 || components[0] != b"nix" || components[1] != b"store" {
		return Err(E::invalid_data("path is not in the Nix store"));
	}
	parse_store_path_base(components[2])
}

async fn read_store_paths<R: ?Sized + NixRead>(
	reader: &mut R,
) -> Result<BTreeSet<StorePath>, R::Error> {
	let mut paths = BTreeSet::new();
	for _ in 0..reader.read_number().await? {
		paths.insert(parse_store_path::<R::Error>(&reader.read_bytes().await?)?);
	}
	Ok(paths)
}

// local-keys.cc's parseColonBase64 with base-n.cc's permissive decoder.
// Signature's shared JSON parser intentionally has different base64 rules.
fn parse_signature<E: crate::wire::de::Error>(text: &[u8]) -> Result<Signature, E> {
	let colon = text
		.iter()
		.position(|byte| *byte == b':')
		.filter(|colon| *colon != 0)
		.ok_or_else(|| E::invalid_data("signature is corrupt"))?;
	let key_name = std::str::from_utf8(&text[..colon])
		.map_err(E::invalid_data)?
		.to_owned();
	let mut sig = Vec::new();
	let mut data = 0u32;
	let mut bits = 0;
	for &byte in &text[colon + 1..] {
		let digit = match byte {
			b'=' => break,
			b'\n' => continue,
			b'A'..=b'Z' => byte - b'A',
			b'a'..=b'z' => byte - b'a' + 26,
			b'0'..=b'9' => byte - b'0' + 52,
			b'+' => 62,
			b'/' => 63,
			_ => return Err(E::invalid_data("invalid character in Base64 signature")),
		};
		bits += 6;
		data = data.wrapping_shl(6) | u32::from(digit);
		if bits >= 8 {
			sig.push((data >> (bits - 8)) as u8);
			bits -= 8;
		}
	}
	if sig.is_empty() {
		return Err(E::invalid_data("signature is corrupt"));
	}
	Ok(Signature { key_name, sig })
}

async fn read_signatures<R: ?Sized + NixRead>(
	reader: &mut R,
) -> Result<BTreeSet<Signature>, R::Error> {
	let mut signatures = BTreeSet::new();
	for _ in 0..reader.read_number().await? {
		let text = reader.read_bytes().await?;
		signatures.insert(parse_signature::<R::Error>(&text)?);
	}
	Ok(signatures)
}

async fn write_signatures<W: NixWrite>(
	writer: &mut W,
	signatures: &BTreeSet<Signature>,
) -> Result<(), W::Error> {
	writer.write_number(signatures.len() as u64).await?;
	for signature in signatures {
		writer
			.write_value(&format!(
				"{}:{}",
				signature.key_name,
				BASE64.encode(&signature.sig)
			))
			.await?;
	}
	Ok(())
}

impl NixDeserialize for Serve<DrvOutput> {
	async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
	where
		R: ?Sized + NixRead + Send,
	{
		read_version(reader, 8)?;
		let Some(drv_path) = reader.try_read_bytes().await? else {
			return Ok(None);
		};
		Ok(Some(Self(DrvOutput {
			drv_path: parse_store_path::<R::Error>(&drv_path)?,
			output_name: reader.read_value().await?,
		})))
	}
}

impl NixSerialize for Serve<DrvOutput> {
	async fn serialize<W: NixWrite>(&self, writer: &mut W) -> Result<(), W::Error> {
		write_version(writer, 8)?;
		writer.write_value(&self.0.drv_path).await?;
		writer.write_value(&self.0.output_name).await
	}
}

impl NixDeserialize for Serve<UnkeyedRealisation> {
	async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
	where
		R: ?Sized + NixRead + Send,
	{
		read_version(reader, 8)?;
		let Some(out_path) = reader.try_read_bytes().await? else {
			return Ok(None);
		};
		Ok(Some(Self(UnkeyedRealisation {
			out_path: parse_store_path::<R::Error>(&out_path)?,
			signatures: read_signatures(reader).await?,
		})))
	}
}

impl NixSerialize for Serve<UnkeyedRealisation> {
	async fn serialize<W: NixWrite>(&self, writer: &mut W) -> Result<(), W::Error> {
		write_version(writer, 8)?;
		writer.write_value(&self.0.out_path).await?;
		write_signatures(writer, &self.0.signatures).await
	}
}

impl NixDeserialize for Serve<Realisation> {
	async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
	where
		R: ?Sized + NixRead + Send,
	{
		let Some(Serve(id)) = reader.try_read_value::<Serve<DrvOutput>>().await? else {
			return Ok(None);
		};
		let Serve(value) = reader.read_value().await?;
		Ok(Some(Self(Realisation { id, value })))
	}
}

impl NixSerialize for Serve<Realisation> {
	async fn serialize<W: NixWrite>(&self, writer: &mut W) -> Result<(), W::Error> {
		writer.write_value(&Serve(self.0.id.clone())).await?;
		writer.write_value(&Serve(self.0.value.clone())).await
	}
}

// CommonProto::Serialise<BuildResultStatus>, indexed by the wire number.
const BUILD_STATUSES: &[&str] = &[
	"Built",
	"Substituted",
	"AlreadyValid",
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
	"ResolvesToAlreadyValid",
	"NoSubstituters",
];

impl NixDeserialize for Serve<BuildResult> {
	async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
	where
		R: ?Sized + NixRead + Send,
	{
		read_version(reader, 0)?;
		let Some(status) = reader.try_read_number().await? else {
			return Ok(None);
		};
		let status = bounded::<R::Error>(status, u8::MAX as u64)? as usize;
		let name = BUILD_STATUSES
			.get(status)
			.ok_or_else(|| R::Error::invalid_data("invalid BuildResult status"))?
			.to_string();
		let error_msg = reader.read_value().await?;
		let mut result = BuildResult {
			outcome: BuildOutcome::Failure {
				status: name.clone(),
				error_msg,
				is_non_deterministic: false,
			},
			times_built: 0,
			start_time: 0,
			stop_time: 0,
			cpu_user: None,
			cpu_system: None,
		};
		if reader.version().minor() >= 3 {
			result.times_built = read_u32(reader).await?;
			if let BuildOutcome::Failure {
				is_non_deterministic,
				..
			} = &mut result.outcome
			{
				*is_non_deterministic = bounded(reader.read_number().await?, 1)? != 0;
			}
			result.start_time = bounded(reader.read_number().await?, i64::MAX as u64)?;
			result.stop_time = bounded(reader.read_number().await?, i64::MAX as u64)?;
		}
		let mut built_outputs = BTreeMap::new();
		if reader.version().minor() >= 8 {
			for _ in 0..reader.read_number().await? {
				let name = reader.read_value().await?;
				let Serve(value) = reader.read_value().await?;
				built_outputs.insert(name, value);
			}
		} else if reader.version().minor() >= 6 {
			let legacy: BTreeMap<String, String> = reader.read_value().await?;
			for (id, text) in legacy {
				let (_, name) = id
					.split_once('!')
					.ok_or_else(|| R::Error::invalid_data("invalid derivation output id"))?;
				let json: serde_json::Value =
					serde_json::from_str(&text).map_err(R::Error::invalid_data)?;
				let path = json
					.get("outPath")
					.ok_or_else(|| R::Error::invalid_data("missing realisation outPath"))?;
				let path = path
					.as_str()
					.ok_or_else(|| R::Error::invalid_data("expected store path string"))?;
				built_outputs.insert(
					name.to_owned(),
					UnkeyedRealisation {
						out_path: parse_store_path_base::<R::Error>(path.as_bytes())?,
						signatures: BTreeSet::new(),
					},
				);
			}
		}
		if matches!(status, 0..=2 | 13) {
			result.outcome = BuildOutcome::Success {
				status: name,
				built_outputs,
			};
		}
		Ok(Some(Self(result)))
	}
}

impl NixSerialize for Serve<BuildResult> {
	async fn serialize<W: NixWrite>(&self, writer: &mut W) -> Result<(), W::Error> {
		write_version(writer, 0)?;
		if writer.version().minor() >= 3
			&& (self.0.times_built > u32::MAX as u64
				|| self.0.start_time > i64::MAX as u64
				|| self.0.stop_time > i64::MAX as u64)
		{
			return Err(W::Error::unsupported_data(
				"build result integer out of range",
			));
		}
		let empty = BTreeMap::new();
		let (status, error_msg, non_deterministic, outputs) = match &self.0.outcome {
			BuildOutcome::Success {
				status,
				built_outputs,
			} => {
				if !crate::cppnix_json::BUILD_SUCCESS_STATUSES.contains(&status.as_str()) {
					return Err(W::Error::unsupported_data("invalid build success status"));
				}
				(status.as_str(), "", false, built_outputs)
			}
			BuildOutcome::Failure {
				status,
				error_msg,
				is_non_deterministic,
			} => {
				if !crate::cppnix_json::BUILD_FAILURE_STATUSES.contains(&status.as_str()) {
					return Err(W::Error::unsupported_data("invalid build failure status"));
				}
				(
					status.as_str(),
					error_msg.as_str(),
					*is_non_deterministic,
					&empty,
				)
			}
		};
		let status = if status == "HashMismatch" {
			"OutputRejected"
		} else {
			status
		};
		let number = BUILD_STATUSES
			.iter()
			.position(|name| *name == status)
			.ok_or_else(|| W::Error::unsupported_data("invalid build status"))?;
		writer.write_number(number as u64).await?;
		writer.write_value(error_msg).await?;
		if writer.version().minor() >= 3 {
			writer.write_number(self.0.times_built).await?;
			writer.write_value(&non_deterministic).await?;
			writer.write_number(self.0.start_time).await?;
			writer.write_number(self.0.stop_time).await?;
		}
		if writer.version().minor() >= 8 {
			writer.write_number(outputs.len() as u64).await?;
			for (name, value) in outputs {
				writer.write_value(name).await?;
				writer.write_value(&Serve(value.clone())).await?;
			}
		} else if writer.version().minor() >= 6 {
			let legacy: BTreeMap<_, _> = outputs
				.iter()
				.map(|(name, value)| {
					let id = format!("sha256:{}!{name}", "0".repeat(64));
					let json =
						serde_json::json!({ "id": id, "outPath": value.out_path.to_string() });
					(id, json.to_string())
				})
				.collect();
			writer.write_value(&legacy).await?;
		}
		Ok(())
	}
}

impl NixDeserialize for Serve<PathInfo> {
	async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
	where
		R: ?Sized + NixRead + Send,
	{
		read_version(reader, 0)?;
		let Some(deriver) = reader.try_read_bytes().await? else {
			return Ok(None);
		};
		let mut info = PathInfo::new(STORE_DIR, NixHash::Sha256([0; 32]));
		if !deriver.is_empty() {
			info.deriver = Some(parse_store_path::<R::Error>(&deriver)?);
		}
		info.references = read_store_paths(reader).await?;
		let _download_size = reader.read_number().await?;
		info.nar_size = reader.read_number().await?;
		if reader.version().minor() >= 4 {
			let hash: String = reader.read_value().await?;
			if !hash.is_empty() {
				info.nar_hash = NixHash::from_str(&hash, None).map_err(R::Error::invalid_data)?;
			}
			info.ca = reader.read_value::<Option<CAHash>>().await?;
			info.signatures = read_signatures(reader).await?;
		}
		Ok(Some(Self(info)))
	}
}

impl NixSerialize for Serve<PathInfo> {
	async fn serialize<W: NixWrite>(&self, writer: &mut W) -> Result<(), W::Error> {
		write_version(writer, 0)?;
		let info = &self.0;
		if info.store_dir != STORE_DIR {
			return Err(W::Error::unsupported_data(
				"serve codec requires /nix/store",
			));
		}
		writer.write_value(&info.deriver).await?;
		writer.write_value(&info.references).await?;
		writer.write_number(info.nar_size).await?;
		writer.write_number(info.nar_size).await?;
		if writer.version().minor() >= 4 {
			writer
				.write_value(&info.nar_hash.to_nix_nixbase32())
				.await?;
			writer.write_value(&info.ca).await?;
			write_signatures(writer, &info.signatures).await?;
		}
		Ok(())
	}
}

/// `ServeProto::BuildOptions`. Absent fields keep the reference's sentinel
/// defaults: -1 for times and sizes, and true for booleans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOptions {
	pub max_silent_time: i64,
	pub build_timeout: i64,
	pub max_log_size: u64,
	pub nr_repeats: u64,
	pub enforce_determinism: bool,
	pub keep_failed: bool,
}

impl Default for BuildOptions {
	fn default() -> Self {
		Self {
			max_silent_time: -1,
			build_timeout: -1,
			max_log_size: u64::MAX,
			nr_repeats: u64::MAX,
			enforce_determinism: true,
			keep_failed: true,
		}
	}
}

impl NixDeserialize for BuildOptions {
	async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
	where
		R: ?Sized + NixRead + Send,
	{
		read_version(reader, 0)?;
		let Some(max_silent_time) = reader.try_read_number().await? else {
			return Ok(None);
		};
		let mut options = Self {
			max_silent_time: bounded::<R::Error>(max_silent_time, u32::MAX as u64)? as i64,
			build_timeout: read_u32(reader).await? as i64,
			..Self::default()
		};
		if reader.version().minor() >= 2 {
			options.max_log_size = reader.read_number().await?;
		}
		if reader.version().minor() >= 3 {
			options.nr_repeats = read_u32(reader).await?;
			options.enforce_determinism = read_u32(reader).await? != 0;
		}
		if reader.version().minor() >= 7 {
			options.keep_failed = read_u32(reader).await? != 0;
		}
		Ok(Some(options))
	}
}

impl NixSerialize for BuildOptions {
	async fn serialize<W: NixWrite>(&self, writer: &mut W) -> Result<(), W::Error> {
		write_version(writer, 0)?;
		writer.write_number(self.max_silent_time as u64).await?;
		writer.write_number(self.build_timeout as u64).await?;
		if writer.version().minor() >= 2 {
			writer.write_number(self.max_log_size).await?;
		}
		if writer.version().minor() >= 3 {
			writer.write_number(self.nr_repeats).await?;
			writer.write_value(&self.enforce_determinism).await?;
		}
		if writer.version().minor() >= 7 {
			writer.write_value(&self.keep_failed).await?;
		}
		Ok(())
	}
}

pub const SERVE_MAGIC_1: u64 = 0x390c9deb;
pub const SERVE_MAGIC_2: u64 = 0x5452eecb;

// CppNix reads an unsigned int, then masks to the low 16 bits in fromWire.
async fn read_handshake_version<R: ?Sized + NixRead>(
	reader: &mut R,
) -> Result<ProtocolVersion, R::Error> {
	Ok(ProtocolVersion::from(read_u32(reader).await? as u16))
}

/// `ServeProto::BasicClientConnection::handshake`, including request and flush.
pub async fn client_handshake<R, W>(
	reader: &mut R,
	writer: &mut W,
	local_version: ProtocolVersion,
) -> Result<ProtocolVersion, std::io::Error>
where
	R: NixRead<Error = std::io::Error>,
	W: NixWrite<Error = std::io::Error> + AsyncWrite + Unpin,
{
	writer.write_number(SERVE_MAGIC_1).await?;
	writer.write_number(local_version.into()).await?;
	writer.flush().await?;
	if read_u32(reader).await? != SERVE_MAGIC_2 {
		return Err(std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			"serve protocol magic mismatch",
		));
	}
	let remote = read_handshake_version(reader).await?;
	if remote.major() != 2 || remote.minor() < 5 {
		return Err(std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			"unsupported serve protocol version",
		));
	}
	Ok(remote.min(local_version))
}

/// `ServeProto::BasicServerConnection::handshake`. Like the reference, the
/// server negotiates the minimum without imposing the client's 2.5 floor.
pub async fn server_handshake<R, W>(
	reader: &mut R,
	writer: &mut W,
	local_version: ProtocolVersion,
) -> Result<ProtocolVersion, std::io::Error>
where
	R: NixRead<Error = std::io::Error>,
	W: NixWrite<Error = std::io::Error> + AsyncWrite + Unpin,
{
	if read_u32(reader).await? != SERVE_MAGIC_1 {
		return Err(std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			"serve protocol magic mismatch",
		));
	}
	writer.write_number(SERVE_MAGIC_2).await?;
	writer.write_number(local_version.into()).await?;
	writer.flush().await?;
	Ok(read_handshake_version(reader).await?.min(local_version))
}
