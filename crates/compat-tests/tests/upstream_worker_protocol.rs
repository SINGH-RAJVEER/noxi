//! Checks `nix-compat`'s daemon wire encoding against CppNix's worker protocol
//! fixtures.
//!
//! Each `<stem>.bin` holds the values of a tuple written back to back at one
//! protocol version (see `VersionedProtoTest` in CppNix's test support), and
//! `<stem>.json` lists the same values. For every fixture this decodes exactly
//! the number of values in the JSON (optional values need not signal EOF), and
//! requires that writing them again reproduces the file byte for byte.
//!
//! Every `.bin` file must appear in [`EXPECTATIONS`], so a fixture whose
//! behaviour changes fails the test until the table is updated.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use nix_compat::derived_path::DerivedPath;
use nix_compat::nix_daemon::types::{
	BuildMode, BuildResult, KeyedBuildResult, UnkeyedValidPathInfo, ValidPathInfo,
};
use nix_compat::nixhash::CAHash;
use nix_compat::realisation::{DrvOutput, Realisation};
use nix_compat::store_path::StorePath;
use nix_compat::wire::ProtocolVersion;
use nix_compat::wire::de::{NixDeserialize, NixRead, NixReader};
use nix_compat::wire::ser::{NixSerialize, NixWrite, NixWriter};
use tokio::io::AsyncWriteExt;

const FIXTURE_DIR: &str = "../../tests/nix/src/libstore-tests/data/worker-protocol";

/// The oldest version CppNix's tests use for serialisers that ignore it.
const DEFAULT: (u8, u8) = (1, 10);

/// What a fixture is decoded as.
#[derive(Debug, Clone, Copy)]
enum Codec {
	String,
	StorePath,
	OptionalStorePath,
	ContentAddress,
	OptionalContentAddress,
	DerivedPath,
	BuildMode,
	DrvOutput,
	Realisation,
	BuildResult,
	KeyedBuildResult,
	UnkeyedValidPathInfo,
	ValidPathInfo,
	/// Three `Vec<String>`, then one `Vec<Vec<String>>`.
	Vectors,
	/// Three string sets, then one set of string sets.
	Sets,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
	/// Decodes the listed number of values and re-encodes byte for byte.
	RoundTrip,
	/// CppNix only reads this (a lossy read of an old format), so neither
	/// implementation writes it back; decoding must succeed.
	ReadOnly,
	/// Not used by the snapshot's C++ tests: left over from older CppNix
	/// releases, which sent realisations without protocol features.
	Stale,
	/// Does not match CppNix yet.
	Gap(&'static str),
	/// No `nix-compat` type exists for this fixture.
	Unsupported(&'static str),
}

use Expect::*;

const PATH_NOT_HASH: &str = "protocol 1.38 with the realisation-with-path-not-hash feature: nix-compat has no protocol feature negotiation";

const EXPECTATIONS: &[(&str, (u8, u8), Option<Codec>, Expect)] = &[
	("string", DEFAULT, Some(Codec::String), RoundTrip),
	("store-path", DEFAULT, Some(Codec::StorePath), RoundTrip),
	(
		"optional-store-path",
		DEFAULT,
		Some(Codec::OptionalStorePath),
		RoundTrip,
	),
	(
		"content-address",
		DEFAULT,
		Some(Codec::ContentAddress),
		RoundTrip,
	),
	(
		"optional-content-address",
		DEFAULT,
		Some(Codec::OptionalContentAddress),
		RoundTrip,
	),
	(
		"derived-path-1.29",
		(1, 29),
		Some(Codec::DerivedPath),
		RoundTrip,
	),
	(
		"derived-path-1.30",
		(1, 30),
		Some(Codec::DerivedPath),
		RoundTrip,
	),
	("build-mode", DEFAULT, Some(Codec::BuildMode), RoundTrip),
	("drv-output", DEFAULT, None, Stale),
	("realisation", DEFAULT, None, Stale),
	("realisation-with-deps", DEFAULT, None, Stale),
	(
		"build-result-1.27",
		(1, 27),
		Some(Codec::BuildResult),
		RoundTrip,
	),
	(
		"build-result-1.28",
		(1, 28),
		Some(Codec::BuildResult),
		ReadOnly,
	),
	(
		"build-result-1.29",
		(1, 29),
		Some(Codec::BuildResult),
		ReadOnly,
	),
	(
		"build-result-1.29-compat",
		(1, 29),
		Some(Codec::BuildResult),
		RoundTrip,
	),
	(
		"build-result-1.37",
		(1, 37),
		Some(Codec::BuildResult),
		ReadOnly,
	),
	(
		"keyed-build-result-1.29",
		(1, 29),
		Some(Codec::KeyedBuildResult),
		RoundTrip,
	),
	(
		"unkeyed-valid-path-info-1.15",
		(1, 15),
		Some(Codec::UnkeyedValidPathInfo),
		RoundTrip,
	),
	(
		"valid-path-info-1.15",
		(1, 15),
		Some(Codec::ValidPathInfo),
		RoundTrip,
	),
	(
		"valid-path-info-1.16",
		(1, 16),
		Some(Codec::ValidPathInfo),
		RoundTrip,
	),
	("vector", DEFAULT, Some(Codec::Vectors), RoundTrip),
	("set", DEFAULT, Some(Codec::Sets), RoundTrip),
	(
		"build-result-realisation-with-path-not-hash",
		(1, 38),
		Some(Codec::BuildResult),
		Gap(PATH_NOT_HASH),
	),
	(
		"drv-output-realisation-with-path-not-hash",
		(1, 38),
		Some(Codec::DrvOutput),
		Gap(PATH_NOT_HASH),
	),
	(
		"realisation-realisation-with-path-not-hash",
		(1, 38),
		Some(Codec::Realisation),
		Gap(PATH_NOT_HASH),
	),
	(
		"unkeyed-realisation-realisation-with-path-not-hash",
		(1, 38),
		None,
		Unsupported("UnkeyedRealisation"),
	),
	(
		"optional-trusted-flag",
		DEFAULT,
		None,
		Unsupported("std::optional<TrustedFlag>"),
	),
	(
		"client-handshake-info_1_30",
		(1, 30),
		None,
		Unsupported("ClientHandshakeInfo"),
	),
	(
		"client-handshake-info_1_33",
		(1, 33),
		None,
		Unsupported("ClientHandshakeInfo"),
	),
	(
		"client-handshake-info_1_35",
		(1, 35),
		None,
		Unsupported("ClientHandshakeInfo"),
	),
	(
		"handshake-to-client",
		DEFAULT,
		None,
		Unsupported("handshake transcript"),
	),
];

fn fixture_root() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_DIR)
}

/// Number of values the JSON fixture lists, if there is one. The C++ fixture
/// uses an object for a single realisation and arrays for tuple fixtures.
fn json_len(stem: &str) -> Option<usize> {
	let bytes = std::fs::read(fixture_root().join(format!("{stem}.json"))).ok()?;
	let value: serde_json::Value = serde_json::from_slice(&bytes).expect("fixture JSON");
	match value {
		serde_json::Value::Array(values) => Some(values.len()),
		serde_json::Value::Object(_) => Some(1),
		_ => None,
	}
}

fn reader(bytes: &[u8], version: ProtocolVersion) -> NixReader<Cursor<Vec<u8>>> {
	NixReader::builder()
		.set_version(version)
		.build(Cursor::new(bytes.to_vec()))
}

async fn read_all<T>(bytes: &[u8], version: ProtocolVersion, count: usize) -> Result<Vec<T>, String>
where
	T: NixDeserialize + Send,
{
	let mut reader = reader(bytes, version);
	let mut values = Vec::with_capacity(count);
	for index in 0..count {
		values.push(
			reader
				.read_value::<T>()
				.await
				.map_err(|e| format!("decoding value {index}: {e}"))?,
		);
	}
	Ok(values)
}

async fn write_all<T>(values: &[T], version: ProtocolVersion) -> Result<Vec<u8>, String>
where
	T: NixSerialize + Send + Sync,
{
	let mut out = Vec::new();
	let mut writer = NixWriter::builder().set_version(version).build(&mut out);
	for value in values {
		writer
			.write_value(value)
			.await
			.map_err(|e| format!("encoding: {e}"))?;
	}
	writer.flush().await.map_err(|e| format!("flushing: {e}"))?;
	drop(writer);
	Ok(out)
}

fn compare(expected: &[u8], actual: &[u8]) -> Result<(), String> {
	if expected == actual {
		return Ok(());
	}
	let offset = expected
		.iter()
		.zip(actual)
		.position(|(a, b)| a != b)
		.unwrap_or(expected.len().min(actual.len()));
	Err(format!(
		"re-encodes differently: {} bytes expected, {} written, first difference at byte {offset}",
		expected.len(),
		actual.len()
	))
}

async fn round_trip<T>(
	bytes: &[u8],
	version: ProtocolVersion,
	count: usize,
	write_back: bool,
) -> Result<usize, String>
where
	T: NixDeserialize + NixSerialize + Send + Sync,
{
	let values = read_all::<T>(bytes, version, count).await?;
	if write_back {
		compare(bytes, &write_all(&values, version).await?)?;
	}
	Ok(values.len())
}

/// Decodes `vector` or `set`: three flat collections, then one nested one.
async fn nested<Flat, Nested>(bytes: &[u8], version: ProtocolVersion) -> Result<usize, String>
where
	Flat: NixDeserialize + NixSerialize + Send + Sync,
	Nested: NixDeserialize + NixSerialize + Send + Sync,
{
	let mut reader = reader(bytes, version);
	let mut flat = Vec::new();
	for n in 0..3 {
		flat.push(
			reader
				.read_value::<Flat>()
				.await
				.map_err(|e| format!("decoding value {n}: {e}"))?,
		);
	}
	let last = reader
		.read_value::<Nested>()
		.await
		.map_err(|e| format!("decoding value 3: {e}"))?;

	let mut written = write_all(&flat, version).await?;
	written.extend(write_all(&[last], version).await?);
	compare(bytes, &written)?;
	Ok(4)
}

/// Returns how many values were decoded.
async fn check(
	codec: Codec,
	bytes: &[u8],
	version: ProtocolVersion,
	count: usize,
	write_back: bool,
) -> Result<usize, String> {
	match codec {
		Codec::String => round_trip::<String>(bytes, version, count, write_back).await,
		Codec::StorePath => round_trip::<StorePath>(bytes, version, count, write_back).await,
		Codec::OptionalStorePath => {
			round_trip::<Option<StorePath>>(bytes, version, count, write_back).await
		}
		Codec::ContentAddress => round_trip::<CAHash>(bytes, version, count, write_back).await,
		Codec::OptionalContentAddress => {
			round_trip::<Option<CAHash>>(bytes, version, count, write_back).await
		}
		Codec::DerivedPath => round_trip::<DerivedPath>(bytes, version, count, write_back).await,
		Codec::BuildMode => round_trip::<BuildMode>(bytes, version, count, write_back).await,
		Codec::DrvOutput => round_trip::<DrvOutput>(bytes, version, count, write_back).await,
		Codec::Realisation => round_trip::<Realisation>(bytes, version, count, write_back).await,
		Codec::BuildResult => round_trip::<BuildResult>(bytes, version, count, write_back).await,
		Codec::KeyedBuildResult => {
			round_trip::<KeyedBuildResult>(bytes, version, count, write_back).await
		}
		Codec::UnkeyedValidPathInfo => {
			round_trip::<UnkeyedValidPathInfo>(bytes, version, count, write_back).await
		}
		Codec::ValidPathInfo => {
			round_trip::<ValidPathInfo>(bytes, version, count, write_back).await
		}
		Codec::Vectors => nested::<Vec<String>, Vec<Vec<String>>>(bytes, version).await,
		Codec::Sets => nested::<BTreeSet<String>, BTreeSet<BTreeSet<String>>>(bytes, version).await,
	}
}

#[test]
fn every_fixture_is_classified() {
	let mut found: Vec<String> = std::fs::read_dir(fixture_root())
		.expect("fixture directory")
		.map(|entry| entry.expect("entry").path())
		.filter(|path| path.extension().is_some_and(|e| e == "bin"))
		.map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
		.collect();
	found.sort();

	let mut listed: Vec<String> = EXPECTATIONS.iter().map(|e| e.0.to_string()).collect();
	listed.sort();

	assert_eq!(
		found, listed,
		"EXPECTATIONS must list exactly the .bin fixtures"
	);
}

#[tokio::test]
async fn fixtures_match_expectations() {
	let mut mismatches = BTreeMap::new();

	for (stem, (major, minor), codec, expect) in EXPECTATIONS {
		let Some(codec) = codec else { continue };
		let bytes = std::fs::read(fixture_root().join(format!("{stem}.bin"))).expect("fixture");
		let version = ProtocolVersion::from_parts(*major, *minor);

		let count = json_len(stem).expect("classified fixture JSON array");

		let result = check(*codec, &bytes, version, count, *expect != ReadOnly)
			.await
			.and_then(|count| {
				match json_len(stem) {
					Some(expected) if matches!(codec, Codec::Vectors | Codec::Sets) => {
						// The JSON lists the four collections.
						(expected == count)
							.then_some(count)
							.ok_or_else(|| format!("decoded {count} values, JSON lists {expected}"))
					}
					Some(expected) if expected != count => {
						Err(format!("decoded {count} values, JSON lists {expected}"))
					}
					_ => Ok(count),
				}
			});

		let ok = match expect {
			RoundTrip | ReadOnly => result.is_ok(),
			Gap(_) => result.is_err(),
			Unsupported(_) | Stale => unreachable!("no codec"),
		};
		if !ok {
			mismatches.insert(*stem, format!("expected {expect:?}, got {result:?}"));
		}
	}

	assert!(
		mismatches.is_empty(),
		"fixtures changed behaviour: {mismatches:#?}"
	);
}

/// Decoded values must be the ones CppNix's test constructs, not merely
/// re-encode to the same bytes.
#[tokio::test]
async fn decoded_values_match_cppnix() {
	let read =
		|stem: &str| std::fs::read(fixture_root().join(format!("{stem}.bin"))).expect("fixture");

	// NAR hashes travel as Base16; protocol 1.15 has no ultimate, signatures or CA.
	let infos = read_all::<UnkeyedValidPathInfo>(
		&read("unkeyed-valid-path-info-1.15"),
		ProtocolVersion::from_parts(1, 15),
		json_len("unkeyed-valid-path-info-1.15").unwrap(),
	)
	.await
	.expect("decodes");
	let expected_hash = "sha256-FePFYIlMuycIXPZbWi7LGEiMmZSX9FMbaQenWBzm1Sc=";
	for info in &infos {
		let sri = format!(
			"sha256-{}",
			data_encoding::BASE64.encode(&info.nar_hash[..])
		);
		assert_eq!(sri, expected_hash);
		assert_eq!(info.registration_time, 23423);
		assert_eq!(info.nar_size, 34878);
		assert!(!info.ultimate && info.signatures.is_empty() && info.ca.is_none());
	}
	assert_eq!(infos[0].deriver, None);
	assert_eq!(
		infos[1]
			.deriver
			.as_ref()
			.map(ToString::to_string)
			.as_deref(),
		Some("g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-bar.drv")
	);

	// The old-protocol form of built outputs keys them by output name.
	let results = read_all::<BuildResult>(
		&read("build-result-1.29-compat"),
		ProtocolVersion::from_parts(1, 29),
		json_len("build-result-1.29-compat").unwrap(),
	)
	.await
	.expect("decodes");
	let built: Vec<(String, String)> = results[2]
		.built_outputs
		.0
		.iter()
		.map(|(name, realisation)| (name.to_string(), realisation.out_path.to_string()))
		.collect();
	assert_eq!(
		built,
		vec![
			(
				"bar".to_string(),
				"g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-bar".to_string()
			),
			(
				"foo".to_string(),
				"g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo".to_string()
			),
		]
	);
	assert!(results[0].built_outputs.0.is_empty());
}
