//! Reproduces CppNix 2.35.2's `dummy-store` and `worker-substitution`
//! scenarios with noxi's real store and compares the resulting states with
//! the copied fixtures.
//!
//! The fixtures are the JSON form of CppNix's in-memory `DummyStore`
//! (`doc/manual/source/protocols/json/schema/store-v1.yaml`, documented as
//! used for testing), written by `dummy-store.cc` and
//! `worker-substitution.cc`. noxi's `dummy://` holds no state, so each
//! scenario here runs against a diverted [`LocalStore`] instead: the test
//! performs the same store operations the C++ test performs, then exports
//! the store as that JSON and compares it with the fixture byte for byte.
//! Store objects, their NAR hashes, sizes, content addresses and paths, the
//! derivation paths and the build trace are all what noxi's store
//! produced. The export moves `.drv` text objects to `derivations`, as
//! `DummyStore` keeps them apart, and writes `registrationTime` as null:
//! `DummyStore` records no time and a local store records the current one.
//! Nothing else is normalized.
//!
//! The two single-path substitution scenarios run noxi's substitution
//! goal (`LocalBackend::build_paths` with an opaque path) from a `file://`
//! binary cache holding the substituter's objects, mirroring the trusted
//! in-memory substituter of the C++ test. The two content-addressed
//! scenarios also upload the substituter's realisations to the cache's
//! `build-trace-v2` directory, as `nix copy` does, and run a derivation
//! goal for the floating output, which substitutes the realisation and its
//! output and, for issue 11928, resolves the root through its input's
//! realisation first. The
//! `one-derivation` state has a derivation without outputs, which a local
//! store refuses in CppNix as in noxi; it stays a gap until noxi has an
//! in-memory store. `upstream_dummy_store` separately checks that every
//! file decodes and re-encodes from `nix_compat` components.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use nix_compat::cppnix_json::derivation::{Derivation, Features};
use nix_compat::cppnix_json::derivation_hash::SingleDerivedPath;
use nix_compat::cppnix_json::{PathInfo, PathInfoFormat, Signature, UnkeyedRealisation};
use nix_compat::fso::File;
use nix_compat::nar::writer::sync as nar;
use nix_compat::narinfo::parse_keypair;
use nix_compat::nixhash::{CAHash, HashAlgo, NixHash};
use nix_compat::store_path::StorePath;
use noxi_build::backend::LocalBackend;
use noxi_build::settings::BuildSettings;
use noxi_store::binary_cache::BinaryCache;
use noxi_store::{
	DerivedPath, FileIngestionMethod, LocalStore, LocalStoreConfig, Realisation, Store,
};
use serde_json::{Map, Value, json};

const DATA: &str = concat!(
	env!("CARGO_MANIFEST_DIR"),
	"/../../tests/nix/src/libstore-tests/data"
);

/// Only used to give `upload_closure` a key type; no key is passed.
const TEST_SECRET_KEY: &str = "test-key:tU7tTvLcScf8pmz/eTV0BEtLmRsPpZfKaRcd0nCN+pysBZPHSeg61/u2oc7mIOewfuAY1V1BiX32homTaDJ2Jw==";

const FEATURES: Features = Features {
	ca_derivations: true,
	dynamic_derivations: false,
	impure_derivations: false,
	git_hashing: false,
};

/// A scratch directory under the test target directory. Store files are
/// read-only, so they are made writable before removal.
struct Scratch(PathBuf);

impl Scratch {
	fn new() -> Self {
		let dir = tempfile::Builder::new()
			.prefix("dummy-store-states-")
			.tempdir_in(env!("CARGO_TARGET_TMPDIR"))
			.unwrap()
			.keep();
		Scratch(dir)
	}

	fn path(&self, name: &str) -> PathBuf {
		self.0.join(name)
	}
}

fn make_writable(path: &Path) {
	let Ok(meta) = fs::symlink_metadata(path) else {
		return;
	};
	if meta.file_type().is_symlink() {
		return;
	}
	let mut permissions = meta.permissions();
	permissions.set_mode(permissions.mode() | 0o700);
	let _ = fs::set_permissions(path, permissions);
	if meta.is_dir()
		&& let Ok(entries) = fs::read_dir(path)
	{
		for entry in entries.flatten() {
			make_writable(&entry.path());
		}
	}
}

impl Drop for Scratch {
	fn drop(&mut self) {
		make_writable(&self.0);
		let _ = fs::remove_dir_all(&self.0);
	}
}

/// A fresh diverted store. Like the C++ test's stores it accepts unsigned
/// paths and has `ca-derivations` enabled where the scenario enables it.
fn open_store(root: &Path) -> LocalStore {
	let mut config = LocalStoreConfig::with_root(root);
	config.require_sigs = false;
	config.trusted_public_keys = Vec::new();
	config.ca_derivations = true;
	LocalStore::open(config).unwrap()
}

/// The fixture `DIR/STEM.json`.
fn read_fixture(dir: &str, stem: &str) -> String {
	let name = format!("{dir}/{stem}.json");
	fs::read_to_string(Path::new(DATA).join(&name)).expect(&name)
}

fn regular(contents: &str) -> File {
	File::Regular {
		executable: false,
		contents: contents.as_bytes().to_vec(),
	}
}

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

/// `Store::addToStore` with `NixArchive` and SHA-256, as each scenario
/// calls it.
fn add_to_store(
	store: &LocalStore,
	name: &str,
	file: &File,
	references: &BTreeSet<StorePath>,
) -> StorePath {
	let mut bytes = Vec::new();
	nar_of(file, nar::open(&mut bytes).unwrap());
	store
		.add_to_store_from_dump(
			&mut bytes.as_slice(),
			name,
			FileIngestionMethod::NixArchive,
			FileIngestionMethod::NixArchive,
			HashAlgo::Sha256,
			references,
			false,
		)
		.unwrap()
}

/// `writeDerivation`: the ATerm as a text object referring to the inputs.
fn write_derivation(store: &LocalStore, drv: &Derivation) -> StorePath {
	let aterm = drv.unparse(false, None).unwrap();
	let references: BTreeSet<StorePath> = drv
		.input_sources
		.iter()
		.chain(drv.input_derivations.keys())
		.cloned()
		.collect();
	let path = store
		.add_text_to_store(
			&format!("{}.drv", drv.name),
			aterm.as_bytes(),
			&references,
			false,
		)
		.unwrap();
	assert_eq!(path, drv.path().unwrap(), "{}: derivation path", drv.name);
	path
}

fn register(store: &LocalStore, drv: &StorePath, output: &str, out_path: &StorePath) {
	store
		.register_drv_output(
			drv,
			output,
			&Realisation {
				out_path: out_path.clone(),
				signatures: BTreeSet::new(),
			},
		)
		.unwrap();
}

/// A derivation as the C++ scenarios construct one: no builder, system,
/// arguments or environment.
fn derivation(name: &str, outputs: Value, inputs: Value) -> Derivation {
	Derivation::from_json(
		&json!({
			"args": [],
			"builder": "",
			"env": {},
			"inputs": inputs,
			"name": name,
			"outputs": outputs,
			"system": "",
			"version": 4,
		}),
		FEATURES,
	)
	.unwrap()
}

fn floating_ca(name: &str, inputs: Value) -> Derivation {
	derivation(
		name,
		json!({"out": {"hashAlgo": "sha256", "method": "nar"}}),
		inputs,
	)
}

fn no_inputs() -> Value {
	json!({"drvs": {}, "srcs": []})
}

fn read_tree(path: &Path) -> File {
	let meta = fs::symlink_metadata(path).unwrap();
	if meta.file_type().is_symlink() {
		File::Symlink {
			target: fs::read_link(path)
				.unwrap()
				.into_os_string()
				.into_string()
				.unwrap(),
		}
	} else if meta.is_dir() {
		let mut entries = BTreeMap::new();
		for entry in fs::read_dir(path).unwrap() {
			let entry = entry.unwrap();
			entries.insert(
				entry.file_name().into_string().unwrap(),
				read_tree(&entry.path()),
			);
		}
		File::Directory { entries }
	} else {
		File::Regular {
			executable: meta.permissions().mode() & 0o100 != 0,
			contents: fs::read(path).unwrap(),
		}
	}
}

/// Every realisation in the store's build trace. The table is read only to
/// enumerate the keys; each value comes from `query_realisation`.
fn build_trace(store: &LocalStore, root: &Path) -> Map<String, Value> {
	let db = rusqlite::Connection::open_with_flags(
		root.join("nix/var/nix/db/db.sqlite"),
		rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
	)
	.unwrap();
	let mut statement = db
		.prepare("select drvPath, outputName from BuildTraceV3")
		.unwrap();
	let keys: Vec<(String, String)> = statement
		.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
		.unwrap()
		.map(Result::unwrap)
		.collect();
	let mut trace: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
	for (drv, output) in keys {
		let drv_path = StorePath::from_bytes(drv.as_bytes()).unwrap();
		let realisation = store
			.query_realisation(&drv_path, &output)
			.unwrap()
			.unwrap();
		let value = UnkeyedRealisation {
			out_path: realisation.out_path,
			signatures: realisation
				.signatures
				.iter()
				.map(|signature| Signature::parse(signature).unwrap())
				.collect(),
		};
		trace
			.entry(drv)
			.or_default()
			.insert(output, value.to_json());
	}
	trace
		.into_iter()
		.map(|(drv, outputs)| (drv, Value::Object(outputs)))
		.collect()
}

/// The store as `DummyStore`'s JSON, written as CppNix's `dump(2)` with a
/// trailing newline.
fn export(store: &LocalStore, root: &Path) -> String {
	let mut contents = Map::new();
	let mut derivations = Map::new();
	for path in store.query_all_valid_paths().unwrap() {
		let info = store.query_path_info(&path).unwrap().unwrap();
		let real = store.real_path(&path);
		if let (Some(name), Some(CAHash::Text(_))) = (path.name().strip_suffix(".drv"), &info.ca) {
			let drv = Derivation::from_aterm(&fs::read(&real).unwrap(), name, FEATURES).unwrap();
			derivations.insert(path.to_string(), drv.to_json());
			continue;
		}
		let json_info = PathInfo {
			store_dir: "/nix/store".to_owned(),
			nar_hash: NixHash::Sha256(info.nar_hash),
			nar_size: info.nar_size,
			references: info.references.clone(),
			ca: info.ca.clone(),
			deriver: info.deriver.clone(),
			// `DummyStore` records no registration time.
			registration_time: 0,
			ultimate: info.ultimate,
			signatures: info
				.sigs
				.iter()
				.map(|signature| Signature::parse(signature).unwrap())
				.collect(),
		};
		contents.insert(
			path.to_string(),
			json!({
				"contents": serde_json::to_value(read_tree(&real)).unwrap(),
				"info": json_info.to_json(true, PathInfoFormat::V3),
			}),
		);
	}
	let document = json!({
		"buildTrace": build_trace(store, root),
		"config": {"store": "/nix/store"},
		"contents": contents,
		"derivations": derivations,
	});
	serde_json::to_string_pretty(&document).unwrap() + "\n"
}

fn assert_state(store: &LocalStore, root: &Path, dir: &str, stem: &str) {
	assert_eq!(
		export(store, root),
		read_fixture(dir, stem),
		"{dir}/{stem}.json"
	);
}

/// Uploads the substituter's objects, unsigned, to a `file://` cache.
fn cache_of(source: &LocalStore, paths: &[&StorePath], dir: &Path) -> String {
	let (key, _) = parse_keypair(TEST_SECRET_KEY).unwrap();
	let keys = [key];
	let url = format!("file://{}?compression=none", dir.display());
	let cache = BinaryCache::open(&url).unwrap();
	for path in paths {
		cache.upload_closure(source, path, &keys[..0]).unwrap();
	}
	url
}

fn backend(root: &Path, substituter: &str) -> LocalBackend {
	LocalBackend::new(
		open_store(root),
		BuildSettings::default(),
		vec![substituter.to_owned()],
	)
}

// dummy-store.cc

#[test]
fn empty_store() {
	let scratch = Scratch::new();
	let root = scratch.path("store");
	assert_state(&open_store(&root), &root, "dummy-store", "empty");
}

#[test]
fn one_flat_file() {
	let scratch = Scratch::new();
	let root = scratch.path("store");
	let store = open_store(&root);
	add_to_store(&store, "my-file", &regular("asdf"), &BTreeSet::new());
	assert_state(&store, &root, "dummy-store", "one-flat-file");
}

/// Gap: `DummyStore::writeDerivation` checks nothing, but a local store
/// checks each derivation it registers, and CppNix's own local store
/// rejects this output-less one just as noxi's does. Only an in-memory
/// store, which noxi lacks, can hold the `one-derivation.json` state. The
/// derivation's ATerm and path still match the fixture's key.
#[test]
fn one_derivation() {
	let scratch = Scratch::new();
	let root = scratch.path("store");
	let store = open_store(&root);
	let drv = derivation("foo", json!({}), no_inputs());
	let aterm = drv.unparse(false, None).unwrap();
	assert_eq!(aterm, r#"Derive([],[],[],"","",[],[])"#);
	let path = drv.path().unwrap();
	let document: Value =
		serde_json::from_str(&read_fixture("dummy-store", "one-derivation")).unwrap();
	assert!(document["derivations"].get(path.to_string()).is_some());

	let error = store
		.add_text_to_store("foo.drv", aterm.as_bytes(), &BTreeSet::new(), false)
		.unwrap_err()
		.to_string();
	assert_eq!(
		error,
		format!(
			"while checking derivation '{}': must have at least one output",
			path.to_absolute_path()
		)
	);
	assert_state(&store, &root, "dummy-store", "empty");

	// The reference's local store, given the same text object.
	let Some(bin) = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.or_else(|| {
			let target =
				Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin");
			target.exists().then_some(target)
		})
	else {
		panic!("the reference is missing; run `just reference` or set NOXI_REFERENCE_BIN");
	};
	fs::create_dir_all(scratch.path("home")).unwrap();
	fs::create_dir_all(scratch.path("conf")).unwrap();
	let output = std::process::Command::new(bin.join("nix"))
		.args([
			"eval",
			"--raw",
			"--store",
			&format!("local?root={}", scratch.path("reference").display()),
			"--expr",
			&format!(
				"builtins.toFile \"foo.drv\" {}",
				serde_json::to_string(&aterm).unwrap()
			),
		])
		.env_clear()
		.env("HOME", scratch.path("home"))
		.env("NIX_CONF_DIR", scratch.path("conf"))
		.env(
			"NIX_CONFIG",
			"experimental-features = nix-command\nbuild-users-group =\nsubstituters =\n",
		)
		.output()
		.unwrap();
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(!output.status.success(), "{stderr}");
	assert!(
		stderr.contains(&format!(
			"while checking derivation '{}'",
			path.to_absolute_path()
		)) && stderr.contains("error: must have at least one output"),
		"{stderr}"
	);
}

#[test]
fn one_realisation() {
	let scratch = Scratch::new();
	let root = scratch.path("store");
	let store = open_store(&root);
	let drv = StorePath::from_bytes(b"g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-bar.drv").unwrap();
	let out = StorePath::from_bytes(b"g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo").unwrap();
	assert_eq!(store.query_realisation(&drv, "out").unwrap(), None);
	register(&store, &drv, "out", &out);
	assert_state(&store, &root, "dummy-store", "one-realisation");
}

/// The `from_json` direction: each document loads into a fresh store
/// through the same operations, and the store exports the same bytes.
/// `one-derivation` cannot load into a local store (see `one_derivation`).
#[test]
fn dummy_store_documents_load_and_export() {
	for stem in ["empty", "one-flat-file", "one-realisation"] {
		let scratch = Scratch::new();
		let root = scratch.path("store");
		let store = open_store(&root);
		let fixture = format!("dummy-store/{stem}.json");
		let document: Value = serde_json::from_str(&read_fixture("dummy-store", stem)).unwrap();
		for (path, object) in document["contents"].as_object().unwrap() {
			let info = PathInfo::from_json(&object["info"], "/nix/store").unwrap();
			let file: File = serde_json::from_value(object["contents"].clone()).unwrap();
			let name = &path[33..];
			let added = add_to_store(&store, name, &file, &info.references);
			assert_eq!(added.to_string(), *path, "{fixture}");
		}
		for (path, value) in document["derivations"].as_object().unwrap() {
			let drv = Derivation::from_json(value, FEATURES).unwrap();
			assert_eq!(
				write_derivation(&store, &drv).to_string(),
				*path,
				"{fixture}"
			);
		}
		for (drv, outputs) in document["buildTrace"].as_object().unwrap() {
			let drv = StorePath::from_bytes(drv.as_bytes()).unwrap();
			for (output, value) in outputs.as_object().unwrap() {
				let realisation = UnkeyedRealisation::from_json(value).unwrap();
				register(&store, &drv, output, &realisation.out_path);
			}
		}
		assert_state(&store, &root, "dummy-store", stem);
	}
}

// worker-substitution.cc

#[test]
fn single_store_object_is_substituted() {
	let scratch = Scratch::new();
	let source_root = scratch.path("substituter");
	let substituter = open_store(&source_root);
	let path = add_to_store(
		&substituter,
		"hello",
		&regular("Hello, world!"),
		&BTreeSet::new(),
	);
	assert_state(
		&substituter,
		&source_root,
		"worker-substitution/single",
		"substituter",
	);

	let url = cache_of(&substituter, &[&path], &scratch.path("cache"));
	let root = scratch.path("store");
	let destination = backend(&root, &url);
	assert_state(&destination.store, &root, "dummy-store", "empty");
	assert!(!destination.is_valid_path(&path).unwrap());

	destination
		.build_paths(&[DerivedPath::Opaque(path.clone())])
		.unwrap();
	assert!(destination.is_valid_path(&path).unwrap());
	assert_state(
		&destination.store,
		&root,
		"worker-substitution/single",
		"substituter",
	);
}

#[test]
fn store_object_with_a_dependency_is_substituted() {
	let scratch = Scratch::new();
	let source_root = scratch.path("substituter");
	let substituter = open_store(&source_root);
	let dependency = add_to_store(
		&substituter,
		"dependency",
		&regular("I am a dependency"),
		&BTreeSet::new(),
	);
	let main = add_to_store(
		&substituter,
		"main",
		&regular(&format!("I depend on {}", dependency.to_absolute_path())),
		&BTreeSet::from([dependency.clone()]),
	);
	assert_state(
		&substituter,
		&source_root,
		"worker-substitution/with-dep",
		"substituter",
	);

	let url = cache_of(&substituter, &[&main], &scratch.path("cache"));
	let root = scratch.path("store");
	let destination = backend(&root, &url);
	assert_state(&destination.store, &root, "dummy-store", "empty");

	// Only the main path is requested; its reference comes with it.
	destination
		.build_paths(&[DerivedPath::Opaque(main.clone())])
		.unwrap();
	assert!(destination.is_valid_path(&dependency).unwrap());
	assert!(destination.is_valid_path(&main).unwrap());
	assert_state(
		&destination.store,
		&root,
		"worker-substitution/with-dep",
		"substituter",
	);
}

/// Uploads the substituter's realisation of `drv^out` to the cache, as
/// `nix copy` writes it.
fn upload_realisation(source: &LocalStore, drv: &StorePath, url: &str) {
	let realisation = source.query_realisation(drv, "out").unwrap().unwrap();
	BinaryCache::open(url)
		.unwrap()
		.upload_realisation(drv, "out", &realisation)
		.unwrap();
}

/// A derivation goal for `drv^out`, as `worker.makeDerivationGoal` runs it.
fn substitute_floating_output(destination: &LocalBackend, drv: &StorePath) {
	destination
		.build_paths(&[DerivedPath::Built {
			drv: drv.clone(),
			outputs: BTreeSet::from(["out".to_owned()]),
		}])
		.unwrap();
}

#[test]
fn floating_derivation_output() {
	let scratch = Scratch::new();
	let source_root = scratch.path("substituter");
	let substituter = open_store(&source_root);
	let root = scratch.path("store");

	let drv = floating_ca("test-ca-drv", no_inputs());
	// The cache is opened lazily, on the first realisation.
	let cache = scratch.path("cache");
	let url = format!("file://{}?compression=none", cache.display());
	let destination = backend(&root, &url);
	let drv_path = write_derivation(&destination.store, &drv);
	assert_state(
		&destination.store,
		&root,
		"worker-substitution/ca-drv",
		"store-before",
	);

	let output = add_to_store(
		&substituter,
		"test-ca-drv-out",
		&regular("I am the output of a CA derivation"),
		&BTreeSet::new(),
	);
	register(&substituter, &drv_path, "out", &output);
	assert_state(
		&substituter,
		&source_root,
		"worker-substitution/ca-drv",
		"substituter",
	);
	assert_eq!(
		destination
			.store
			.query_realisation(&drv_path, "out")
			.unwrap(),
		None
	);
	cache_of(&substituter, &[&output], &cache);
	upload_realisation(&substituter, &drv_path, &url);

	// The goal finds the realisation in the cache's build trace,
	// substitutes the output and registers the realisation.
	substitute_floating_output(&destination, &drv_path);
	assert!(destination.is_valid_path(&output).unwrap());
	assert_eq!(
		destination
			.store
			.query_realisation(&drv_path, "out")
			.unwrap()
			.map(|realisation| realisation.out_path),
		Some(output)
	);
	assert_state(
		&destination.store,
		&root,
		"worker-substitution/ca-drv",
		"store-after",
	);
}

/// CppNix issue 11928: substituting a floating output whose input is
/// another floating derivation.
#[test]
fn floating_derivation_output_with_a_dependency() {
	let scratch = Scratch::new();
	let source_root = scratch.path("substituter");
	let substituter = open_store(&source_root);
	let root = scratch.path("store");
	let cache = scratch.path("cache");
	let url = format!("file://{}?compression=none", cache.display());
	let destination = backend(&root, &url);

	let dep_drv = floating_ca("dep-drv", no_inputs());
	let dep_drv_path = write_derivation(&destination.store, &dep_drv);
	let dep_output = add_to_store(
		&substituter,
		"dep-drv-out",
		&regular("I am the dependency output"),
		&BTreeSet::new(),
	);
	register(&substituter, &dep_drv_path, "out", &dep_output);

	let root_drv = floating_ca(
		"root-drv",
		json!({
			"drvs": {dep_drv_path.to_string(): {"dynamicOutputs": {}, "outputs": ["out"]}},
			"srcs": [],
		}),
	);
	let root_drv_path = write_derivation(&destination.store, &root_drv);
	let resolved = root_drv
		.try_resolve(
			&|drv, output| {
				assert_eq!(*drv, SingleDerivedPath::Opaque(dep_drv_path.clone()));
				assert!(*output == "out");
				Some(dep_output.clone())
			},
			true,
			&|_| None,
			&mut Vec::new(),
		)
		.unwrap()
		.expect("the input resolves");
	let resolved_path = write_derivation(&substituter, &resolved);
	assert_state(
		&destination.store,
		&root,
		"worker-substitution/issue-11928",
		"store-before",
	);

	let root_output = add_to_store(
		&substituter,
		"root-drv-out",
		&regular(
			"I am the root output. I don't reference anything because the other derivation's output is just needed at build time.",
		),
		&BTreeSet::new(),
	);
	register(&substituter, &resolved_path, "out", &root_output);
	assert_state(
		&substituter,
		&source_root,
		"worker-substitution/issue-11928",
		"substituter",
	);
	cache_of(&substituter, &[&dep_output, &root_output], &cache);
	upload_realisation(&substituter, &dep_drv_path, &url);
	upload_realisation(&substituter, &resolved_path, &url);

	// The cache has no realisation of the unresolved root, so the goal
	// realises the input through its realisation, resolves the root and
	// substitutes the resolved derivation's output.
	substitute_floating_output(&destination, &root_drv_path);
	let realised = |drv: &StorePath| {
		destination
			.store
			.query_realisation(drv, "out")
			.unwrap()
			.map(|realisation| realisation.out_path)
	};
	assert!(destination.is_valid_path(&root_output).unwrap());
	assert_eq!(realised(&resolved_path), Some(root_output));
	assert_eq!(realised(&dep_drv_path), Some(dep_output.clone()));
	// TODO #11928 in the reference: the input's output is fetched although
	// the root output does not refer to it.
	assert!(destination.is_valid_path(&dep_output).unwrap());
	// No realisation is recorded for the unresolved root, and the resolved
	// derivation, which was never built, is not written.
	assert_eq!(realised(&root_drv_path), None);
	assert!(!destination.is_valid_path(&resolved_path).unwrap());
	assert_state(
		&destination.store,
		&root,
		"worker-substitution/issue-11928",
		"store-after",
	);
}
