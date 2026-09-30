//! Serve 2.x values from the pinned `serve-protocol.cc` characterization tests.
//! JSON fixtures supply semantic expectations. Write fixtures must also match
//! byte for byte; the old 2.6 build-result fixture is explicitly read-only.

use std::collections::BTreeSet;
use std::fmt::Debug;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use nix_compat::cppnix_json::{
	BuildOutcome, BuildResult, DrvOutput, PathInfo, PathInfoFormat, Realisation, UnkeyedRealisation,
};
use nix_compat::cppnix_serve::{
	BuildOptions, SERVE_MAGIC_1, SERVE_MAGIC_2, Serve, client_handshake, server_handshake,
};
use nix_compat::store_path::StorePath;
use nix_compat::wire::ProtocolVersion;
use nix_compat::wire::de::{NixDeserialize, NixRead, NixReader};
use nix_compat::wire::ser::{NixSerialize, NixWrite, NixWriter};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn fixture_root() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../tests/nix/src/libstore-tests/data/serve-protocol")
}

fn binary(stem: &str) -> Vec<u8> {
	binary_file(&format!("{stem}.bin"))
}

fn binary_file(filename: &str) -> Vec<u8> {
	std::fs::read(fixture_root().join(filename)).expect("fixture binary")
}

fn fixture_json(stem: &str) -> Value {
	serde_json::from_slice(
		&std::fs::read(fixture_root().join(format!("{stem}.json"))).expect("fixture JSON"),
	)
	.expect("valid fixture JSON")
}

fn version(minor: u8) -> ProtocolVersion {
	ProtocolVersion::from_parts(2, minor)
}

fn reader(bytes: &[u8], version: ProtocolVersion) -> NixReader<Cursor<Vec<u8>>> {
	NixReader::builder()
		.set_version(version)
		.build(Cursor::new(bytes.to_vec()))
}

async fn encode<T: NixSerialize + Send + Sync>(
	values: &[T],
	version: ProtocolVersion,
) -> std::io::Result<Vec<u8>> {
	let mut bytes = Vec::new();
	let mut writer = NixWriter::builder().set_version(version).build(&mut bytes);
	for value in values {
		writer.write_value(value).await?;
	}
	writer.flush().await?;
	drop(writer);
	Ok(bytes)
}

trait JsonFixture: Debug + PartialEq + Sized {
	fn from_fixture(value: &Value) -> Self;
	fn fixture_value(&self) -> Value;
}

macro_rules! json_fixture {
	($($ty:ty),+ $(,)?) => {
		$(impl JsonFixture for $ty {
			fn from_fixture(value: &Value) -> Self {
				Self::from_json(value).expect("reference JSON model")
			}
			fn fixture_value(&self) -> Value {
				self.to_json()
			}
		})+
	};
}

json_fixture!(DrvOutput, UnkeyedRealisation, Realisation, BuildResult);

impl JsonFixture for PathInfo {
	fn from_fixture(value: &Value) -> Self {
		Self::from_json(value, "/nix/store").expect("reference path-info model")
	}

	fn fixture_value(&self) -> Value {
		self.to_json(true, PathInfoFormat::V3)
	}
}

async fn check_json<T>(stem: &str, minor: u8, writes: bool)
where
	T: JsonFixture + Send + Sync,
	Serve<T>: NixDeserialize + NixSerialize,
{
	let bytes = binary(stem);
	let mut json = fixture_json(stem);
	if stem == "build-result-2.6" {
		// This copied JSON predates the model change. The pinned read keeps
		// only outputName and outPath, dropping old ids, dependencies and sigs.
		for result in json.as_array_mut().expect("build-result tuple") {
			if let Some(outputs) = result.get_mut("builtOutputs") {
				for output in outputs.as_object_mut().expect("output map").values_mut() {
					*output = serde_json::json!({
						"outPath": output["outPath"],
						"signatures": [],
					});
				}
			}
		}
	}
	let values = match &json {
		Value::Array(values) => values.clone(),
		Value::Object(_) => vec![json],
		_ => panic!("unexpected fixture shape: {stem}"),
	};
	let mut reader = reader(&bytes, version(minor));
	let mut decoded = Vec::new();
	let mut expected = Vec::new();
	for (index, value) in values.iter().enumerate() {
		let actual: Serve<T> = reader
			.read_value()
			.await
			.unwrap_or_else(|error| panic!("{stem} value {index}: {error}"));
		assert_eq!(
			actual.0.fixture_value(),
			*value,
			"{stem} value {index} JSON"
		);
		let model = T::from_fixture(value);
		assert_eq!(actual.0, model, "{stem} value {index} model");
		decoded.push(actual);
		expected.push(Serve(model));
	}
	assert_eq!(
		reader.read(&mut [0]).await.unwrap(),
		0,
		"{stem} trailing bytes"
	);
	if writes {
		assert_eq!(
			encode(&decoded, version(minor)).await.unwrap(),
			bytes,
			"{stem} roundtrip"
		);
		assert_eq!(
			encode(&expected, version(minor)).await.unwrap(),
			bytes,
			"{stem} reference write"
		);
	}
}

#[tokio::test]
async fn drv_output_and_realisations_2_8() {
	check_json::<DrvOutput>("drv-output-2.8", 8, true).await;
	check_json::<UnkeyedRealisation>("unkeyed-realisation-2.8", 8, true).await;
	check_json::<Realisation>("realisation-2.8", 8, true).await;
}

#[tokio::test]
async fn build_results_at_reference_versions() {
	for (stem, minor, writes) in [
		("build-result-2.2", 2, true),
		("build-result-2.3", 3, true),
		("build-result-2.6", 6, false),
		("build-result-2.7-compat", 7, true),
		("build-result-2.8", 8, true),
	] {
		check_json::<BuildResult>(stem, minor, writes).await;
	}
}

#[tokio::test]
async fn path_info_at_reference_versions() {
	check_json::<PathInfo>("unkeyed-valid-path-info-2.3", 3, true).await;
	check_json::<PathInfo>("unkeyed-valid-path-info-2.4", 4, true).await;
}

#[tokio::test]
async fn build_options_at_reference_versions() {
	// These fixtures have no JSON. Values and absent-field defaults are from
	// ServeProtoTest and ServeProto::BuildOptions, not decoded from the bytes.
	for (minor, filename) in [
		(1, "build-options-2.1.bin"),
		(2, "build-options-2.2.bin"),
		(3, "build-options-2.3.bin"),
		(7, "build-options-2.7.bin"),
	] {
		let expected = BuildOptions {
			max_silent_time: 5,
			build_timeout: 6,
			max_log_size: if minor >= 2 { 7 } else { u64::MAX },
			nr_repeats: if minor >= 3 { 8 } else { u64::MAX },
			enforce_determinism: minor < 7,
			keep_failed: true,
		};
		let stem = format!("build-options-2.{minor}");
		let bytes = binary_file(filename);
		let mut reader = reader(&bytes, version(minor));
		let actual: BuildOptions = reader.read_value().await.unwrap();
		assert_eq!(actual, expected, "{stem}");
		assert_eq!(
			reader.read(&mut [0]).await.unwrap(),
			0,
			"{stem} trailing bytes"
		);
		assert_eq!(
			encode(&[actual], version(minor)).await.unwrap(),
			bytes,
			"{stem} roundtrip"
		);
		assert_eq!(
			encode(&[expected], version(minor)).await.unwrap(),
			bytes,
			"{stem} reference write"
		);
	}
}

async fn reject_versions<T>(stem: &str, versions: &[ProtocolVersion])
where
	T: JsonFixture + Send + Sync,
	Serve<T>: NixDeserialize + NixSerialize,
{
	let json = fixture_json(stem);
	let json = json.as_array().map_or(&json, |values| &values[0]);
	let value = Serve(T::from_fixture(json));
	for &version in versions {
		let mut reader = reader(&binary(stem), version);
		let error = reader.read_value::<Serve<T>>().await.unwrap_err();
		assert_eq!(
			error.kind(),
			std::io::ErrorKind::InvalidData,
			"{stem} {version}"
		);
		assert!(
			encode(std::slice::from_ref(&value), version).await.is_err(),
			"{stem} {version}"
		);
	}
}

#[tokio::test]
async fn serve_version_gates_reject_old_realisations_and_worker_versions() {
	let versions = [
		version(7),
		ProtocolVersion::from_parts(1, 38),
		ProtocolVersion::from_parts(3, 8),
	];
	reject_versions::<DrvOutput>("drv-output-2.8", &versions).await;
	reject_versions::<UnkeyedRealisation>("unkeyed-realisation-2.8", &versions).await;
	reject_versions::<Realisation>("realisation-2.8", &versions).await;
	reject_versions::<BuildResult>("build-result-2.8", &versions[1..]).await;
	reject_versions::<PathInfo>("unkeyed-valid-path-info-2.4", &versions[1..]).await;
	for &version in &versions[1..] {
		assert!(
			reader(&binary("build-options-2.7"), version)
				.read_value::<BuildOptions>()
				.await
				.is_err()
		);
		assert!(encode(&[BuildOptions::default()], version).await.is_err());
	}
}

#[tokio::test]
async fn realisation_truncations_are_errors() {
	for stem in [
		"drv-output-2.8",
		"unkeyed-realisation-2.8",
		"realisation-2.8",
	] {
		let bytes = binary(stem);
		for end in 0..bytes.len() {
			let mut reader = reader(&bytes[..end], version(8));
			let fails = match stem {
				"drv-output-2.8" => {
					// The fixture holds two consecutive outputs.
					match reader.read_value::<Serve<DrvOutput>>().await {
						Err(_) => true,
						Ok(_) => reader.read_value::<Serve<DrvOutput>>().await.is_err(),
					}
				}
				"unkeyed-realisation-2.8" => reader
					.read_value::<Serve<UnkeyedRealisation>>()
					.await
					.is_err(),
				_ => reader.read_value::<Serve<Realisation>>().await.is_err(),
			};
			assert!(fails, "{stem} truncation at {end}");
		}
	}
}

#[tokio::test]
async fn build_result_status_validation_and_hash_mismatch_fallback() {
	let fixture = fixture_json("build-result-2.8");
	let valid = Serve(BuildResult::from_json(&fixture[0]).unwrap());
	let complete = encode(&[valid], version(8)).await.unwrap();
	for status in [15u64, 255, 256, u64::MAX] {
		let mut bytes = complete.clone();
		bytes[..8].copy_from_slice(&status.to_le_bytes());
		let error = reader(&bytes, version(8))
			.read_value::<Serve<BuildResult>>()
			.await
			.unwrap_err();
		assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
		assert_eq!(
			error.to_string(),
			if status <= 255 {
				"invalid BuildResult status"
			} else {
				"serialised integer is too large"
			}
		);
	}
	let mut value = BuildResult::from_json(&fixture[0]).unwrap();
	let BuildOutcome::Failure { status, .. } = &mut value.outcome else {
		unreachable!()
	};
	*status = "HashMismatch".into();
	let bytes = encode(&[Serve(value)], version(8)).await.unwrap();
	let decoded: Serve<BuildResult> = reader(&bytes, version(8)).read_value().await.unwrap();
	assert_eq!(decoded.0.to_json(), fixture[0]);
}

const BOUNDARY_PATH: &str = "/nix/store/g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo";

async fn raw_realisation_bytes(path: &[u8], signatures: &[&[u8]]) -> Vec<u8> {
	let mut bytes = Vec::new();
	let mut writer = NixWriter::builder()
		.set_version(version(8))
		.build(&mut bytes);
	writer.write_slice(path).await.unwrap();
	writer.write_number(signatures.len() as u64).await.unwrap();
	for text in signatures {
		writer.write_slice(text).await.unwrap();
	}
	writer.flush().await.unwrap();
	drop(writer);
	bytes
}

#[tokio::test]
async fn signature_reference_base64_forms_have_canonical_output() {
	// local-keys.cc splits at the first colon. base-n.cc ignores only LF,
	// stops at the first '=', and discards leftover bits without validation.
	for text in [
		b"key:Zg".as_slice(),
		b"key:Z\ng\n==",
		b"key:Zg=ignored!",
		b"key:Zg=\xff",
		b"key:Zh==",
		b"key:ZgA",
	] {
		let bytes = raw_realisation_bytes(BOUNDARY_PATH.as_bytes(), &[text]).await;
		let decoded: Serve<UnkeyedRealisation> = reader(&bytes, version(8))
			.read_value()
			.await
			.unwrap_or_else(|error| panic!("{text:?}: {error}"));
		let signature = decoded.0.signatures.iter().next().unwrap();
		assert_eq!(signature.key_name, "key");
		let (expected_bytes, canonical) = if text == b"key:ZgA" {
			(b"f\0".as_slice(), b"key:ZgA=".as_slice())
		} else {
			(b"f".as_slice(), b"key:Zg==".as_slice())
		};
		assert_eq!(signature.sig, expected_bytes, "{text:?}");
		let expected = raw_realisation_bytes(BOUNDARY_PATH.as_bytes(), &[canonical]).await;
		assert_eq!(encode(&[decoded], version(8)).await.unwrap(), expected);
	}
}

#[tokio::test]
async fn signature_reference_rejects_empty_data_names_and_invalid_base64() {
	for text in [
		b"key:".as_slice(),
		b"key:\n",
		b"key:A",
		b"key:=Zg",
		b":Zg==",
		b"Zg==",
		b"key:Zg\r==",
		b"key:Zg ==",
		b"key:Zg\t==",
		b"key:Z!g",
	] {
		let bytes = raw_realisation_bytes(BOUNDARY_PATH.as_bytes(), &[text]).await;
		let error = reader(&bytes, version(8))
			.read_value::<Serve<UnkeyedRealisation>>()
			.await
			.expect_err("reference rejects this signature");
		assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{text:?}");
	}
}

async fn raw_path_info_bytes(deriver: &[u8], references: &[&[u8]], minor: u8) -> Vec<u8> {
	let mut bytes = Vec::new();
	let mut writer = NixWriter::builder()
		.set_version(version(minor))
		.build(&mut bytes);
	writer.write_slice(deriver).await.unwrap();
	writer.write_number(references.len() as u64).await.unwrap();
	for path in references {
		writer.write_slice(path).await.unwrap();
	}
	writer.write_number(0).await.unwrap(); // unused download size
	writer.write_number(0).await.unwrap(); // NAR size
	if minor >= 4 {
		writer.write_slice(b"").await.unwrap(); // absent hash retains Hash::dummy
		writer.write_slice(b"").await.unwrap(); // absent CA
		writer.write_number(0).await.unwrap(); // signatures
	}
	writer.flush().await.unwrap();
	drop(writer);
	bytes
}

async fn raw_build_result_output_bytes(path: &[u8], minor: u8) -> Vec<u8> {
	let mut bytes = Vec::new();
	let mut writer = NixWriter::builder()
		.set_version(version(minor))
		.build(&mut bytes);
	writer.write_number(0).await.unwrap(); // Built
	writer.write_slice(b"").await.unwrap(); // error message
	for _ in 0..4 {
		writer.write_number(0).await.unwrap(); // timesBuilt, nondeterminism, start, stop
	}
	writer.write_number(1).await.unwrap(); // output map
	if minor >= 8 {
		writer.write_slice(b"out").await.unwrap();
		writer.write_slice(path).await.unwrap();
		writer.write_number(0).await.unwrap(); // signatures
	} else {
		writer.write_slice(b"ignored-hash!out").await.unwrap();
		let json = serde_json::json!({ "outPath": std::str::from_utf8(path).unwrap() });
		writer.write_value(&json.to_string()).await.unwrap();
	}
	writer.flush().await.unwrap();
	drop(writer);
	bytes
}

async fn decode_path_positions(path: &[u8]) -> Vec<(&'static str, std::io::Result<StorePath>)> {
	// Layouts come from serve-protocol.cc, with primitive writes so a permissive
	// StorePath constructor cannot determine whether a malformed input passes.
	let mut results = Vec::new();
	let mut bytes = Vec::new();
	let mut writer = NixWriter::builder()
		.set_version(version(8))
		.build(&mut bytes);
	writer.write_slice(path).await.unwrap();
	writer.write_slice(b"out").await.unwrap();
	writer.flush().await.unwrap();
	drop(writer);
	results.push((
		"drv path",
		reader(&bytes, version(8))
			.read_value::<Serve<DrvOutput>>()
			.await
			.map(|value| value.0.drv_path),
	));
	let mut keyed = bytes.clone();
	bytes = raw_realisation_bytes(path, &[]).await;
	results.push((
		"unkeyed out path",
		reader(&bytes, version(8))
			.read_value::<Serve<UnkeyedRealisation>>()
			.await
			.map(|value| value.0.out_path),
	));
	keyed.extend(raw_realisation_bytes(BOUNDARY_PATH.as_bytes(), &[]).await);
	results.push((
		"keyed drv path",
		reader(&keyed, version(8))
			.read_value::<Serve<Realisation>>()
			.await
			.map(|value| value.0.id.drv_path),
	));
	keyed = encode(&[BOUNDARY_PATH.to_owned(), "out".to_owned()], version(8))
		.await
		.unwrap();
	keyed.extend(&bytes);
	results.push((
		"keyed out path",
		reader(&keyed, version(8))
			.read_value::<Serve<Realisation>>()
			.await
			.map(|value| value.0.value.out_path),
	));
	for minor in [3, 4] {
		bytes = raw_path_info_bytes(path, &[], minor).await;
		results.push((
			"deriver",
			reader(&bytes, version(minor))
				.read_value::<Serve<PathInfo>>()
				.await
				.map(|value| value.0.deriver.unwrap()),
		));
		bytes = raw_path_info_bytes(b"", &[path], minor).await;
		results.push((
			"reference",
			reader(&bytes, version(minor))
				.read_value::<Serve<PathInfo>>()
				.await
				.map(|value| value.0.references.into_iter().next().unwrap()),
		));
	}
	bytes = raw_build_result_output_bytes(path, 8).await;
	results.push((
		"build output",
		reader(&bytes, version(8))
			.read_value::<Serve<BuildResult>>()
			.await
			.map(|value| {
				let BuildOutcome::Success { built_outputs, .. } = value.0.outcome else {
					unreachable!()
				};
				built_outputs.into_values().next().unwrap().out_path
			}),
	));
	results
}

#[tokio::test]
async fn serve_paths_reject_reference_reserved_names_in_every_position() {
	// path.cc rejects '.' and '..' as the first dash-separated name component.
	for name in [".", "..", ".-foo", "..-foo"] {
		let path = format!("/nix/store/g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-{name}");
		for (position, result) in decode_path_positions(path.as_bytes()).await {
			let error = result.expect_err(&format!("{position}: {name}"));
			assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
		}
	}
	for name in [".foo", "..foo", "...", "...-foo", "foo.-bar"] {
		let path = format!("/nix/store/g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-{name}");
		for (position, result) in decode_path_positions(path.as_bytes()).await {
			assert_eq!(
				result
					.unwrap_or_else(|error| panic!("{position}: {error}"))
					.name(),
				name
			);
		}
	}
}

#[tokio::test]
async fn serve_paths_normalize_lexically_before_store_membership_checks() {
	// StoreDirConfig::parseStorePath calls canonPath with resolveSymlinks=false.
	// canonPathInner skips empty/dot components, pops '..', and clamps at root.
	for path in [
		BOUNDARY_PATH.to_owned(),
		BOUNDARY_PATH.replace("/nix/store/", "//nix///store//"),
		BOUNDARY_PATH.replace("/nix/store/", "/./nix/./store/"),
		BOUNDARY_PATH.replace("/nix/store/", "/ignored/../nix/other/../store/"),
		BOUNDARY_PATH.replace("/nix/store/", "/../../nix/store/"),
		format!("{BOUNDARY_PATH}///./"),
		format!("{BOUNDARY_PATH}/not-a-real-directory/.."),
	] {
		for (position, result) in decode_path_positions(path.as_bytes()).await {
			assert_eq!(
				result
					.unwrap_or_else(|error| panic!("{position}: {path}: {error}"))
					.to_absolute_path(),
				BOUNDARY_PATH
			);
		}
	}
	let raw = [b"/\xff/../".as_slice(), &BOUNDARY_PATH.as_bytes()[1..]].concat();
	for (position, result) in decode_path_positions(&raw).await {
		assert_eq!(
			result
				.unwrap_or_else(|error| panic!("{position}: {error}"))
				.to_absolute_path(),
			BOUNDARY_PATH
		);
	}
	for path in [
		String::new(),
		BOUNDARY_PATH[1..].to_owned(),
		BOUNDARY_PATH.replace("/nix/store/", "/nix/store-other/"),
		format!("{BOUNDARY_PATH}/child"),
		format!("{BOUNDARY_PATH}/../.."),
		"/nix/store".into(),
		"/".into(),
	] {
		// An empty deriver is the protocol's valid None sentinel, so test that
		// separately through non-optional path fields below.
		if path.is_empty() {
			let bytes = raw_realisation_bytes(b"", &[]).await;
			assert!(
				reader(&bytes, version(8))
					.read_value::<Serve<UnkeyedRealisation>>()
					.await
					.is_err()
			);
			continue;
		}
		for (position, result) in decode_path_positions(path.as_bytes()).await {
			assert_eq!(
				result.expect_err(&format!("{position}: {path}")).kind(),
				std::io::ErrorKind::InvalidData
			);
		}
	}
}

#[tokio::test]
async fn legacy_build_output_paths_validate_basenames_without_normalizing() {
	for minor in [6, 7] {
		for name in [".", "..", ".-foo", "..-foo"] {
			let base = format!("g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-{name}");
			let bytes = raw_build_result_output_bytes(base.as_bytes(), minor).await;
			let error = reader(&bytes, version(minor))
				.read_value::<Serve<BuildResult>>()
				.await
				.expect_err("reference StorePath rejects this name");
			assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
		}
		for path in [BOUNDARY_PATH, "./g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo"] {
			let bytes = raw_build_result_output_bytes(path.as_bytes(), minor).await;
			assert!(
				reader(&bytes, version(minor))
					.read_value::<Serve<BuildResult>>()
					.await
					.is_err()
			);
		}
		let bytes = raw_build_result_output_bytes(&BOUNDARY_PATH.as_bytes()[11..], minor).await;
		let value: Serve<BuildResult> = reader(&bytes, version(minor)).read_value().await.unwrap();
		let BuildOutcome::Success { built_outputs, .. } = value.0.outcome else {
			unreachable!()
		};
		assert_eq!(
			built_outputs["out"].out_path.to_absolute_path(),
			BOUNDARY_PATH
		);
	}
}

async fn replay_client(bytes: &[u8], local: ProtocolVersion) -> std::io::Result<ProtocolVersion> {
	let mut reader = reader(bytes, local);
	let mut output = Vec::new();
	let mut writer = NixWriter::builder().set_version(local).build(&mut output);
	client_handshake(&mut reader, &mut writer, local).await
}

#[tokio::test]
async fn handshake_transcript_matches_both_sides() {
	let local = version(5);
	let bytes = binary_file("handshake-to-client.bin");
	let mut reader = reader(&bytes, local);
	let mut request = Vec::new();
	let mut writer = NixWriter::builder().set_version(local).build(&mut request);
	assert_eq!(
		client_handshake(&mut reader, &mut writer, local)
			.await
			.unwrap(),
		local
	);
	assert_eq!(reader.read(&mut [0]).await.unwrap(), 0);
	drop(writer);
	let expected = [SERVE_MAGIC_1.to_le_bytes(), u64::from(local).to_le_bytes()].concat();
	assert_eq!(request, expected);

	let mut reader = self::reader(&request, local);
	let mut response = Vec::new();
	let mut writer = NixWriter::builder().set_version(local).build(&mut response);
	assert_eq!(
		server_handshake(&mut reader, &mut writer, local)
			.await
			.unwrap(),
		local
	);
	assert_eq!(reader.read(&mut [0]).await.unwrap(), 0);
	drop(writer);
	assert_eq!(response, bytes);
}

#[tokio::test]
async fn handshake_reference_truncations_corruption_and_negotiation() {
	let local = version(5);
	let bytes = binary_file("handshake-to-client.bin");
	for end in 0..bytes.len() {
		assert!(
			replay_client(&bytes[..end], local).await.is_err(),
			"truncation at {end}"
		);
	}
	for index in 0..bytes.len() {
		let mut corrupt = bytes.clone();
		corrupt[index] = corrupt[index].wrapping_mul(4).wrapping_add(1);
		let result = replay_client(&corrupt, local).await;
		if index < 8 || index == 9 || index >= 12 {
			assert!(result.is_err(), "corruption at {index}");
		} else {
			assert_eq!(result.unwrap(), local, "corruption at {index}");
		}
	}
	for (remote, accepted) in [
		(0x204u64, false),
		(0x205, true),
		(0x208, true),
		(0x308, false),
		(0x1000208, true),
		(1u64 << 32, false),
	] {
		let transcript = [SERVE_MAGIC_2.to_le_bytes(), remote.to_le_bytes()].concat();
		let result = replay_client(&transcript, version(8)).await;
		if accepted {
			assert_eq!(
				result.unwrap(),
				ProtocolVersion::from(remote as u16).min(version(8))
			);
		} else {
			assert!(result.is_err());
		}
	}
}

#[test]
fn every_serve_binary_has_an_explicit_disposition() {
	// Shared values are tested by upstream_shared_protocols. The three old
	// realisation fixtures are absent from the pinned C++ test definitions.
	let shared = [
		"string",
		"store-path",
		"content-address",
		"optional-store-path",
		"optional-content-address",
		"vector",
		"set",
	];
	let stale = ["drv-output", "realisation", "realisation-with-deps"];
	let covered = [
		"drv-output-2.8",
		"unkeyed-realisation-2.8",
		"realisation-2.8",
		"build-result-2.2",
		"build-result-2.3",
		"build-result-2.6",
		"build-result-2.7-compat",
		"build-result-2.8",
		"unkeyed-valid-path-info-2.3",
		"unkeyed-valid-path-info-2.4",
		"build-options-2.1",
		"build-options-2.2",
		"build-options-2.3",
		"build-options-2.7",
		"handshake-to-client",
	];
	let expected: BTreeSet<String> = shared
		.into_iter()
		.chain(stale)
		.chain(covered)
		.map(str::to_owned)
		.collect();
	let actual = std::fs::read_dir(fixture_root())
		.unwrap()
		.map(|entry| entry.unwrap().path())
		.filter(|path| path.extension().is_some_and(|extension| extension == "bin"))
		.map(|path| path.file_stem().unwrap().to_str().unwrap().to_owned())
		.collect();
	assert_eq!(
		expected, actual,
		"new serve fixtures need a disposition and a semantic codec test"
	);
}
