//! Reads CppNix 2.35.2's `dummy-store` and `worker-substitution` fixtures.
//!
//! Each file is the JSON state of a `DummyStore`, CppNix's in-memory store:
//! its configuration, store objects (file trees with their path info),
//! derivations and build trace. `dummy-store.cc` round-trips the four
//! `dummy-store` files, and `worker-substitution.cc` checkpoints a
//! substituter and a destination store around substitution goals run by the
//! build worker.
//!
//! noxi has neither part: `dummy://` opens no store at all, nothing reads
//! or writes a whole store as this JSON, and noxi-build's substitution goals
//! cannot run against an in-memory substituter. Those files are known gaps.
//! What can be checked is checked here, against `nix_compat`: every store
//! object's NAR hash, NAR size and content-addressed store path are
//! recomputed from its file tree and references, every path info, file
//! tree, derivation and realisation decodes and re-encodes to the file's
//! bytes, and the whole document is rebuilt from those parts byte for byte.
//! `worker-substitution/with-dep/store.json` is not checkpointed by any
//! 2.35.2 test (it also still uses path-info version 2), so it is recorded
//! as a reference anomaly.

use std::collections::BTreeSet;
use std::io::Cursor;
use std::path::Path;

use nix_compat::cppnix_json::derivation::{Derivation, Features};
use nix_compat::cppnix_json::{PathInfo, PathInfoFormat, UnkeyedRealisation};
use nix_compat::fso::File;
use nix_compat::nar::writer::sync as nar;
use nix_compat::nixhash::{CAHash, NixHash};
use nix_compat::store_path::{StorePath, StorePathRef, build_ca_path};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const DATA: &str = concat!(
	env!("CARGO_MANIFEST_DIR"),
	"/../../tests/nix/src/libstore-tests/data"
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
	/// noxi has no in-memory store or substitution goal for this state.
	Gap,
	/// No 2.35.2 test reads or writes the file.
	Anomaly,
}

/// Every file in both directories, with its class. The `"stem` spellings
/// are what `fixture_inventory` looks for.
const FIXTURES: &[(&str, &str, Class)] = &[
	("dummy-store", "empty", Class::Gap),
	("dummy-store", "one-derivation", Class::Gap),
	("dummy-store", "one-flat-file", Class::Gap),
	("dummy-store", "one-realisation", Class::Gap),
	("worker-substitution/ca-drv", "store-after", Class::Gap),
	("worker-substitution/ca-drv", "store-before", Class::Gap),
	("worker-substitution/ca-drv", "substituter", Class::Gap),
	("worker-substitution/issue-11928", "store-after", Class::Gap),
	(
		"worker-substitution/issue-11928",
		"store-before",
		Class::Gap,
	),
	("worker-substitution/issue-11928", "substituter", Class::Gap),
	("worker-substitution/single", "substituter", Class::Gap),
	("worker-substitution/with-dep", "store", Class::Anomaly),
	("worker-substitution/with-dep", "substituter", Class::Gap),
];

fn nar_of(file: &File, node: nar::Node<'_, Vec<u8>>) {
	match file {
		File::Regular {
			executable,
			contents,
		} => node
			.file(
				*executable,
				contents.len() as u64,
				&mut Cursor::new(contents.as_slice()),
			)
			.unwrap(),
		File::Symlink { target } => node.symlink(target.as_bytes()).unwrap(),
		File::Directory { entries } => {
			let mut directory = node.directory().unwrap();
			for (name, entry) in entries {
				nar_of(entry, directory.entry(name.as_bytes()).unwrap());
			}
			directory.close().unwrap();
		}
	}
}

fn path_info_format(value: &Value) -> PathInfoFormat {
	match value["version"].as_u64() {
		Some(1) => PathInfoFormat::V1,
		Some(2) => PathInfoFormat::V2,
		Some(3) => PathInfoFormat::V3,
		other => panic!("unknown path-info version {other:?}"),
	}
}

/// Checks one store object and returns its JSON rebuilt from the decoded
/// parts.
fn store_object(path: &str, object: &Value, context: &str) -> Value {
	let store_path = StorePath::from_bytes(path.as_bytes()).expect(context);
	let file: File = serde_json::from_value(object["contents"].clone()).expect(context);
	let info = PathInfo::from_json(&object["info"], "/nix/store").expect(context);

	let mut bytes = Vec::new();
	nar_of(&file, nar::open(&mut bytes).unwrap());
	let digest: [u8; 32] = Sha256::digest(&bytes).into();
	assert_eq!(
		info.nar_hash,
		NixHash::Sha256(digest),
		"{context}: NAR hash"
	);
	assert_eq!(info.nar_size, bytes.len() as u64, "{context}: NAR size");

	// The store path follows from the content address and references.
	let Some(CAHash::Nar(hash)) = &info.ca else {
		panic!("{context}: expected a NAR content address");
	};
	let references: Vec<String> = info
		.references
		.iter()
		.filter(|reference| *reference != &store_path)
		.map(|reference| reference.to_string())
		.collect();
	let refs = references
		.iter()
		.map(|reference| StorePathRef::from_bytes(reference.as_bytes()).unwrap());
	let has_self = info.references.contains(&store_path);
	let computed = build_ca_path(store_path.name(), true, hash, refs, has_self).expect(context);
	assert_eq!(computed.to_string(), path, "{context}: store path");

	json!({
		"contents": serde_json::to_value(&file).unwrap(),
		"info": info.to_json(true, path_info_format(&object["info"])),
	})
}

/// Checks a whole dummy-store document and rebuilds it from its parts.
fn rebuild(document: &Value, context: &str) -> Value {
	let object = document.as_object().expect(context);
	let keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
	assert_eq!(
		keys,
		BTreeSet::from(["buildTrace", "config", "contents", "derivations"]),
		"{context}"
	);
	assert_eq!(
		document["config"],
		json!({ "store": "/nix/store" }),
		"{context}"
	);

	let mut contents = Map::new();
	for (path, value) in document["contents"].as_object().expect(context) {
		contents.insert(
			path.clone(),
			store_object(path, value, &format!("{context} {path}")),
		);
	}
	let mut derivations = Map::new();
	for (path, value) in document["derivations"].as_object().expect(context) {
		StorePath::from_bytes(path.as_bytes()).expect(context);
		// Upstream's worker fixtures include floating CA derivations.
		let derivation = Derivation::from_json(
			value,
			Features {
				ca_derivations: true,
				..Features::default()
			},
		)
		.expect(context);
		derivations.insert(path.clone(), derivation.to_json());
	}
	let mut trace = Map::new();
	for (drv, outputs) in document["buildTrace"].as_object().expect(context) {
		StorePath::from_bytes(drv.as_bytes()).expect(context);
		let mut by_output = Map::new();
		for (output, value) in outputs.as_object().expect(context) {
			let realisation = UnkeyedRealisation::from_json(value).expect(context);
			by_output.insert(output.clone(), realisation.to_json());
		}
		trace.insert(drv.clone(), Value::Object(by_output));
	}
	json!({
		"buildTrace": trace,
		"config": document["config"],
		"contents": contents,
		"derivations": derivations,
	})
}

#[test]
fn dummy_store_documents_decode_and_rebuild() {
	for (dir, stem, _) in FIXTURES {
		let context = format!("{dir}/{stem}.json");
		let text = std::fs::read_to_string(Path::new(DATA).join(&context)).expect(&context);
		let document: Value = serde_json::from_str(&text).expect(&context);
		let rebuilt = rebuild(&document, &context);
		// CppNix writes these with `dump(2)` and a trailing newline.
		let written = serde_json::to_string_pretty(&rebuilt).unwrap() + "\n";
		assert_eq!(written, text, "{context}: rebuilt bytes");
	}
}

/// The fixture list names every file in both directories, and the classes
/// match what `expectations/fixtures.txt` records.
#[test]
fn every_fixture_is_listed() {
	let mut on_disk = BTreeSet::new();
	for top in ["dummy-store", "worker-substitution"] {
		let mut stack = vec![Path::new(DATA).join(top)];
		while let Some(dir) = stack.pop() {
			for entry in std::fs::read_dir(&dir).unwrap() {
				let path = entry.unwrap().path();
				if path.is_dir() {
					stack.push(path);
				} else {
					let relative = path.strip_prefix(DATA).unwrap();
					on_disk.insert(relative.to_string_lossy().into_owned());
				}
			}
		}
	}
	let listed: BTreeSet<String> = FIXTURES
		.iter()
		.map(|(dir, stem, _)| format!("{dir}/{stem}.json"))
		.collect();
	assert_eq!(listed, on_disk);

	let manifest = include_str!("../expectations/fixtures.txt");
	for (dir, stem, class) in FIXTURES {
		let path = format!("src/libstore-tests/data/{dir}/{stem}.json");
		let recorded = manifest
			.lines()
			.find(|line| line.split_whitespace().nth(1) == Some(path.as_str()))
			.and_then(|line| line.split_whitespace().next());
		let expected = match class {
			Class::Gap => "gap",
			Class::Anomaly => "anomaly",
		};
		assert_eq!(recorded, Some(expected), "{path}");
	}
}
