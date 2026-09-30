//! Checks the shared CppNix protocol fixtures against `nix-compat`'s wire types.
//! The fixtures are single values concatenated without an outer tuple frame.

use std::collections::BTreeSet;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use nix_compat::nixhash::CAHash;
use nix_compat::store_path::StorePath;
use nix_compat::wire::ProtocolVersion;
use nix_compat::wire::de::{NixDeserialize, NixRead, NixReader};
use nix_compat::wire::ser::{NixSerialize, NixWrite, NixWriter};
use tokio::io::AsyncWriteExt;

const COMMON: &str = "common-protocol";
const SERVE: &str = "serve-protocol";
const SHARED: &[&str] = &[
	"string",
	"store-path",
	"content-address",
	"optional-store-path",
	"optional-content-address",
	"vector",
	"set",
];

const STALE_COMMON: &[&str] = &["drv-output", "realisation", "realisation-with-deps"];

fn fixture(group: &str, stem: &str, extension: &str) -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../tests/nix/src/libstore-tests/data")
		.join(group)
		.join(format!("{stem}.{extension}"))
}

async fn round_trip<T>(bytes: &[u8], count: usize, version: ProtocolVersion) -> Result<(), String>
where
	T: NixDeserialize + NixSerialize + Send + Sync,
{
	let mut reader = NixReader::builder()
		.set_version(version)
		.build(Cursor::new(bytes.to_vec()));
	let mut values = Vec::with_capacity(count);
	for index in 0..count {
		values.push(
			reader
				.read_value::<T>()
				.await
				.map_err(|error| format!("value {index}: {error}"))?,
		);
	}
	let mut output = Vec::new();
	let mut writer = NixWriter::builder().set_version(version).build(&mut output);
	for value in &values {
		writer
			.write_value(value)
			.await
			.map_err(|error| format!("encoding: {error}"))?;
	}
	writer.flush().await.map_err(|error| error.to_string())?;
	drop(writer);
	if output != bytes {
		return Err(format!(
			"{} bytes decoded to {} bytes",
			bytes.len(),
			output.len()
		));
	}
	Ok(())
}

async fn nested<Flat, Nested>(bytes: &[u8], version: ProtocolVersion) -> Result<(), String>
where
	Flat: NixDeserialize + NixSerialize + Send + Sync,
	Nested: NixDeserialize + NixSerialize + Send + Sync,
{
	let mut reader = NixReader::builder()
		.set_version(version)
		.build(Cursor::new(bytes.to_vec()));
	let mut flat = Vec::new();
	for index in 0..3 {
		flat.push(
			reader
				.read_value::<Flat>()
				.await
				.map_err(|error| format!("value {index}: {error}"))?,
		);
	}
	let last = reader
		.read_value::<Nested>()
		.await
		.map_err(|error| format!("value 3: {error}"))?;
	let mut output = Vec::new();
	let mut writer = NixWriter::builder().set_version(version).build(&mut output);
	for value in &flat {
		writer
			.write_value(value)
			.await
			.map_err(|error| format!("encoding: {error}"))?;
	}
	writer
		.write_value(&last)
		.await
		.map_err(|error| format!("encoding: {error}"))?;
	writer.flush().await.map_err(|error| error.to_string())?;
	drop(writer);
	if output != bytes {
		return Err(format!(
			"{} bytes decoded to {} bytes",
			bytes.len(),
			output.len()
		));
	}
	Ok(())
}

#[tokio::test]
async fn shared_fixtures_match_cppnix() {
	for (group, version) in [
		(COMMON, ProtocolVersion::from_parts(1, 10)),
		(SERVE, ProtocolVersion::from_parts(2, 5)),
	] {
		for stem in SHARED {
			let bytes = std::fs::read(fixture(group, stem, "bin")).expect("fixture binary");
			let json: serde_json::Value = serde_json::from_slice(
				&std::fs::read(fixture(group, stem, "json")).expect("fixture JSON"),
			)
			.expect("valid fixture JSON");
			let count = json.as_array().expect("tuple fixture").len();
			let result = match *stem {
				"string" => round_trip::<String>(&bytes, count, version).await,
				"store-path" => round_trip::<StorePath>(&bytes, count, version).await,
				"content-address" => round_trip::<CAHash>(&bytes, count, version).await,
				"optional-store-path" => {
					round_trip::<Option<StorePath>>(&bytes, count, version).await
				}
				"optional-content-address" => {
					round_trip::<Option<CAHash>>(&bytes, count, version).await
				}
				"vector" => nested::<Vec<String>, Vec<Vec<String>>>(&bytes, version).await,
				"set" => {
					nested::<BTreeSet<String>, BTreeSet<BTreeSet<String>>>(&bytes, version).await
				}
				_ => unreachable!("listed codec"),
			};
			result.unwrap_or_else(|error| panic!("{group}/{stem}: {error}"));
		}
	}
}

#[test]
fn common_fixtures_have_explicit_reference_dispositions() {
	let source = include_str!("../../../tests/nix/src/libstore-tests/common-protocol.cc");
	for stem in SHARED {
		assert!(
			source.contains(&format!("\"{stem}\"")),
			"active case {stem}"
		);
	}
	for stem in STALE_COMMON {
		assert!(
			!source.contains(&format!("\"{stem}\"")),
			"{stem} is no longer stale; run and reclassify it"
		);
	}
	let expected: BTreeSet<_> = SHARED
		.iter()
		.chain(STALE_COMMON)
		.flat_map(|stem| [format!("{stem}.bin"), format!("{stem}.json")])
		.collect();
	let actual = std::fs::read_dir(fixture(COMMON, "string", "bin").parent().unwrap())
		.unwrap()
		.map(|entry| entry.unwrap().file_name().into_string().unwrap())
		.collect();
	assert_eq!(expected, actual, "new common fixtures need a disposition");
}
