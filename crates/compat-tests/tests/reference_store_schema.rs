//! Store schema states from [expected store states] checked against the
//! pinned CppNix 2.35.2 reference: fresh initialization with and without
//! `ca-derivations`, upgrades of the legacy schemas 6 to 9, read-only opens,
//! and the schemas the reference refuses.
//!
//! Legacy stores are made by the reference and then taken back to an older
//! schema with SQLite, the way those schemas differ from version 10: no
//! `ca`, `ultimate` or `sigs` columns, a `FailedPaths` table, no
//! `SchemaMigrations` table and the `IndexReferrer` index. Each state is
//! copied; the reference opens one copy and noxi the other, and the
//! resulting databases must be identical.
//!
//! The reference executables come from `NOXI_REFERENCE_BIN` or
//! `target/nix-reference/bin` (`just reference`). Every store is a temporary
//! `--store ROOT`.
//!
//! [expected store states]: ../../../docs/compatibility.md#expected-store-states

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use noxi_store::{LocalStore, LocalStoreConfig, Realisation};

fn reference_bin() -> PathBuf {
	if let Some(dir) = std::env::var_os("NOXI_REFERENCE_BIN") {
		return PathBuf::from(dir);
	}
	let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin");
	assert!(
		dir.join("nix-store").exists(),
		"the CppNix reference is missing; run `just reference` or set NOXI_REFERENCE_BIN"
	);
	dir
}

struct Env {
	dir: tempfile::TempDir,
}

impl Env {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		fs::create_dir(dir.path().join("conf")).unwrap();
		fs::create_dir(dir.path().join("home")).unwrap();
		Env { dir }
	}

	fn root(&self, name: &str) -> PathBuf {
		self.dir.path().join(name)
	}

	fn reference(&self, features: &str, program: &str, args: &[&str]) -> Output {
		Command::new(reference_bin().join(program))
			.args(args)
			.env_clear()
			.env("HOME", self.dir.path().join("home"))
			.env("NIX_CONF_DIR", self.dir.path().join("conf"))
			.env(
				"NIX_CONFIG",
				format!(
					"experimental-features = nix-command read-only-local-store {features}\nbuild-users-group =\nsandbox = false\nsubstituters =\n"
				),
			)
			.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin")
			.output()
			.unwrap()
	}

	fn reference_ok(&self, features: &str, program: &str, args: &[&str]) -> String {
		let output = self.reference(features, program, args);
		assert!(
			output.status.success(),
			"{program} {args:?} failed:\n{}",
			String::from_utf8_lossy(&output.stderr)
		);
		String::from_utf8(output.stdout).unwrap()
	}

	/// Open `name` with the reference, as any store command does.
	fn reference_open(&self, features: &str, name: &str) -> Output {
		self.reference(
			features,
			"nix",
			&[
				"store",
				"info",
				"--store",
				self.root(name).to_str().unwrap(),
			],
		)
	}

	fn config(&self, name: &str, ca_derivations: bool) -> LocalStoreConfig {
		let mut config = LocalStoreConfig::with_root(&self.root(name));
		config.require_sigs = false;
		config.ca_derivations = ca_derivations;
		config
	}

	fn open(&self, name: &str, ca_derivations: bool) -> noxi_store::Result<LocalStore> {
		LocalStore::open(self.config(name, ca_derivations))
	}

	/// A reference-made store with a recursive, a flat and a text path,
	/// one of them signed.
	fn populate(&self, name: &str) {
		let src = self.dir.path().join("src");
		fs::create_dir_all(&src).unwrap();
		fs::write(src.join("file"), "contents\n").unwrap();
		let flat = self.dir.path().join("flat");
		fs::write(&flat, "flat\n").unwrap();
		let store = self.root(name);
		let store = store.to_str().unwrap();
		let added = self.reference_ok(
			"",
			"nix-store",
			&["--store", store, "--add", src.to_str().unwrap()],
		);
		self.reference_ok(
			"",
			"nix-store",
			&[
				"--store",
				store,
				"--add-fixed",
				"sha256",
				flat.to_str().unwrap(),
			],
		);
		let key = self.dir.path().join("key");
		let secret = self.reference_ok("", "nix", &["key", "generate-secret", "--key-name", "k"]);
		fs::write(&key, secret).unwrap();
		self.reference_ok(
			"",
			"nix",
			&[
				"store",
				"sign",
				"--store",
				store,
				"--key-file",
				key.to_str().unwrap(),
				added.trim(),
			],
		);
	}

	fn db(&self, name: &str) -> rusqlite::Connection {
		rusqlite::Connection::open(self.root(name).join("nix/var/nix/db/db.sqlite")).unwrap()
	}

	fn schema_file(&self, name: &str) -> PathBuf {
		self.root(name).join("nix/var/nix/db/schema")
	}

	/// Take a version-10 store back to `version`.
	fn downgrade(&self, name: &str, version: u32) {
		let db = self.db(name);
		db.execute_batch(
			"drop table SchemaMigrations; create index if not exists IndexReferrer on Refs(referrer);",
		)
		.unwrap();
		if version < 10 {
			db.execute_batch("alter table ValidPaths drop column ca")
				.unwrap();
		}
		if version < 9 {
			db.execute_batch(
				"create table FailedPaths (path text primary key not null, time integer not null);",
			)
			.unwrap();
		}
		if version < 8 {
			db.execute_batch(
				"alter table ValidPaths drop column sigs; alter table ValidPaths drop column ultimate;",
			)
			.unwrap();
		}
		drop(db);
		fs::write(self.schema_file(name), version.to_string()).unwrap();
	}

	fn copy(&self, from: &str, to: &str) {
		let status = Command::new("cp")
			.arg("-a")
			.arg(self.root(from))
			.arg(self.root(to))
			.status()
			.unwrap();
		assert!(status.success());
	}

	/// Everything that defines the state of a database: its schema objects,
	/// migrations and rows.
	fn dump(&self, name: &str) -> Vec<String> {
		let db = self.db(name);
		let mut out = Vec::new();
		let mut tables = Vec::new();
		let mut stmt = db
			.prepare("select type, name, tbl_name, sql from sqlite_master order by type, name")
			.unwrap();
		let rows = stmt
			.query_map([], |row| {
				Ok((
					row.get::<_, String>(0)?,
					row.get::<_, String>(1)?,
					row.get::<_, String>(2)?,
					row.get::<_, Option<String>>(3)?,
				))
			})
			.unwrap();
		for row in rows {
			let (kind, name, table, sql) = row.unwrap();
			if kind == "table" && !name.starts_with("sqlite_") {
				tables.push(name.clone());
			}
			out.push(format!("{kind} {name} {table} {sql:?}"));
		}
		for table in tables {
			let mut stmt = db
				.prepare(&format!("select * from {table} order by 1"))
				.unwrap();
			let columns = stmt.column_count();
			let rows = stmt
				.query_map([], |row| {
					(0..columns)
						.map(|i| row.get::<_, rusqlite::types::Value>(i))
						.collect::<Result<Vec<_>, _>>()
				})
				.unwrap();
			for row in rows {
				out.push(format!("{table}: {:?}", row.unwrap()));
			}
		}
		out.push(format!(
			"schema file: {:?}",
			fs::read_to_string(self.schema_file(name)).unwrap()
		));
		out
	}

	fn reference_verify(&self, name: &str) {
		let output = self.reference(
			"",
			"nix-store",
			&[
				"--store",
				self.root(name).to_str().unwrap(),
				"--verify",
				"--check-contents",
			],
		);
		let stderr = String::from_utf8_lossy(&output.stderr);
		assert!(output.status.success(), "verify failed:\n{stderr}");
		for bad in ["error", "warning", "modified", "disappeared"] {
			assert!(!stderr.contains(bad), "verify reported problems:\n{stderr}");
		}
	}
}

/// The message of a reference error, without `error: ` and the indentation
/// of continuation lines.
fn reference_error(output: &Output) -> String {
	assert!(!output.status.success());
	let stderr = String::from_utf8(output.stderr.clone()).unwrap();
	let message = stderr
		.trim_end()
		.strip_prefix("error: ")
		.unwrap_or_else(|| panic!("unexpected reference error: {stderr}"));
	message.replace("\n       ", "\n")
}

#[test]
fn fresh_stores_with_and_without_ca_derivations_match_the_reference() {
	let env = Env::new();
	for (features, ca) in [("", false), ("ca-derivations", true)] {
		let reference = format!("reference-{ca}");
		let noxi = format!("noxi-{ca}");
		env.reference_ok(
			features,
			"nix",
			&[
				"store",
				"info",
				"--store",
				env.root(&reference).to_str().unwrap(),
			],
		);
		drop(env.open(&noxi, ca).unwrap());
		assert_eq!(env.dump(&noxi), env.dump(&reference), "ca = {ca}");
		// Each engine reopens the other's store unchanged.
		let before = env.dump(&noxi);
		env.reference_ok(
			features,
			"nix",
			&[
				"store",
				"info",
				"--store",
				env.root(&noxi).to_str().unwrap(),
			],
		);
		assert_eq!(env.dump(&noxi), before, "ca = {ca}");
		drop(env.open(&reference, ca).unwrap());
		assert_eq!(env.dump(&reference), before, "ca = {ca}");
	}

	// Enabling the feature later applies the same migration in both.
	env.copy("reference-false", "late-reference");
	env.copy("reference-false", "late-noxi");
	assert!(
		env.reference_open("ca-derivations", "late-reference")
			.status
			.success()
	);
	drop(env.open("late-noxi", true).unwrap());
	assert_eq!(env.dump("late-noxi"), env.dump("late-reference"));
	assert_eq!(env.dump("late-noxi"), env.dump("reference-true"));
	// Disabling it again leaves the realisation tables alone.
	drop(env.open("late-noxi", false).unwrap());
	assert_eq!(env.dump("late-noxi"), env.dump("reference-true"));
}

#[test]
fn legacy_schemas_upgrade_like_the_reference() {
	let env = Env::new();
	env.populate("base");
	for version in [6, 7, 8, 9] {
		for (features, ca) in [("", false), ("ca-derivations", true)] {
			let old = format!("old-{version}-{ca}");
			let reference = format!("reference-{version}-{ca}");
			let noxi = format!("noxi-{version}-{ca}");
			env.copy("base", &old);
			env.downgrade(&old, version);
			env.copy(&old, &reference);
			env.copy(&old, &noxi);

			let output = env.reference_open(features, &reference);
			assert!(
				output.status.success(),
				"{}",
				String::from_utf8_lossy(&output.stderr)
			);
			drop(env.open(&noxi, ca).unwrap());
			assert_eq!(
				env.dump(&noxi),
				env.dump(&reference),
				"schema {version}, ca = {ca}"
			);

			// The upgraded store needs nothing more from either engine.
			let upgraded = env.dump(&noxi);
			env.reference_verify(&noxi);
			assert!(env.reference_open(features, &noxi).status.success());
			drop(env.open(&reference, ca).unwrap());
			assert_eq!(env.dump(&noxi), upgraded);
			assert_eq!(env.dump(&reference), upgraded);
		}
	}
}

#[test]
fn unsupported_schemas_are_refused_like_the_reference() {
	let env = Env::new();
	env.reference_ok(
		"",
		"nix",
		&[
			"store",
			"info",
			"--store",
			env.root("base").to_str().unwrap(),
		],
	);
	for (i, schema) in ["11", "5", "4", "-1", "abc", "10\n", " 10", "", "10x"]
		.into_iter()
		.enumerate()
	{
		let reference = format!("reference-{i}");
		let noxi = format!("noxi-{i}");
		for name in [&reference, &noxi] {
			env.copy("base", name);
			fs::write(env.schema_file(name), schema).unwrap();
		}
		let expected = reference_error(&env.reference_open("", &reference));
		let error = env.open(&noxi, false).unwrap_err();
		assert_eq!(
			error.to_string(),
			expected.replace(
				env.root(&reference).to_str().unwrap(),
				env.root(&noxi).to_str().unwrap()
			),
			"schema {schema:?}"
		);
		assert_eq!(
			env.dump(&noxi).last(),
			env.dump(&reference).last(),
			"schema {schema:?}"
		);
		assert_eq!(fs::read_to_string(env.schema_file(&noxi)).unwrap(), schema);
	}

	// A schema of 0 is a new store; the existing database is kept.
	for name in ["reference-zero", "noxi-zero"] {
		env.copy("base", name);
		fs::write(env.schema_file(name), "0").unwrap();
	}
	assert!(env.reference_open("", "reference-zero").status.success());
	drop(env.open("noxi-zero", false).unwrap());
	assert_eq!(env.dump("noxi-zero"), env.dump("reference-zero"));
}

#[test]
fn read_only_opens_match_the_reference() {
	let env = Env::new();
	env.populate("base");
	let read_only = |name: &str| format!("local?root={}&read-only=true", env.root(name).display());
	let open_read_only = |name: &str| {
		let mut config = env.config(name, false);
		config.read_only = true;
		LocalStore::open(config)
	};

	// A missing database and an old schema are refused.
	let output = env.reference(
		"",
		"nix",
		&["store", "info", "--store", &read_only("reference-missing")],
	);
	assert_eq!(
		open_read_only("noxi-missing").unwrap_err().to_string(),
		reference_error(&output)
	);
	for name in ["reference-9", "noxi-9"] {
		env.copy("base", name);
		env.downgrade(name, 9);
	}
	let before = env.dump("noxi-9");
	let output = env.reference(
		"",
		"nix",
		&["store", "info", "--store", &read_only("reference-9")],
	);
	assert_eq!(
		open_read_only("noxi-9").unwrap_err().to_string(),
		reference_error(&output)
	);
	assert_eq!(env.dump("noxi-9"), before);
	assert_eq!(env.dump("reference-9"), before);

	// A current store opens, answers queries and is not written, even
	// when a named migration would be missing.
	for name in ["reference-ro", "noxi-ro"] {
		env.copy("base", name);
		env.db(name)
			.execute_batch("drop table SchemaMigrations")
			.unwrap();
	}
	let db_files = |name: &str| -> Vec<(String, Vec<u8>)> {
		let dir = env.root(name).join("nix/var/nix/db");
		let mut files: Vec<_> = fs::read_dir(&dir)
			.unwrap()
			.map(|e| {
				let e = e.unwrap();
				(
					e.file_name().to_string_lossy().into_owned(),
					fs::read(e.path()).unwrap(),
				)
			})
			.collect();
		files.sort();
		files
	};
	let before = db_files("noxi-ro");
	let paths = env.reference_ok(
		"",
		"nix",
		&["path-info", "--all", "--store", &read_only("reference-ro")],
	);
	let store = open_read_only("noxi-ro").unwrap();
	let mut ours: Vec<String> = store
		.query_all_valid_paths()
		.unwrap()
		.iter()
		.map(|p| p.to_absolute_path())
		.collect();
	ours.sort();
	let mut theirs: Vec<&str> = paths.lines().collect();
	theirs.sort();
	assert_eq!(ours, theirs);
	assert!(
		store
			.add_text_to_store("x", b"x", &Default::default(), false)
			.is_err()
	);
	drop(store);
	assert_eq!(db_files("noxi-ro"), before);
	assert_eq!(env.dump("reference-ro"), env.dump("noxi-ro"));
}

#[test]
fn realisations_use_the_reference_table() {
	let env = Env::new();
	env.populate("base");
	let store = env.open("base", true).unwrap();
	let paths: Vec<_> = store.query_all_valid_paths().unwrap().into_iter().collect();
	let drv =
		nix_compat::store_path::StorePath::from_bytes(b"00000000000000000000000000000000-fake.drv")
			.unwrap();
	let realisation = Realisation {
		out_path: paths[0].clone(),
		signatures: Default::default(),
	};
	store
		.register_drv_output(&drv, "out", &realisation)
		.unwrap();
	// Registering again merges signatures; another path is refused.
	let signed = Realisation {
		signatures: ["k:c2ln".to_owned()].into(),
		..realisation.clone()
	};
	store.register_drv_output(&drv, "out", &signed).unwrap();
	assert_eq!(store.query_realisation(&drv, "out").unwrap(), Some(signed));
	let other = Realisation {
		out_path: paths[1].clone(),
		signatures: Default::default(),
	};
	let error = store
		.register_drv_output(&drv, "out", &other)
		.unwrap_err()
		.to_string();
	assert!(
		error.starts_with("Trying to register a realisation of"),
		"{error}"
	);
	drop(store);

	// The record has the reference's form: drv base name, output path, signatures.
	let rows: Vec<(String, String, String, String)> = env
		.db("base")
		.prepare("select drvPath, outputName, outputPath, signatures from BuildTraceV3")
		.unwrap()
		.query_map([], |row| {
			Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
		})
		.unwrap()
		.collect::<Result<_, _>>()
		.unwrap();
	assert_eq!(
		rows,
		[(
			drv.to_string(),
			"out".to_owned(),
			paths[0].to_absolute_path(),
			"k:c2ln".to_owned()
		)]
	);
	let without = env.open("base", false).unwrap();
	assert_eq!(without.query_realisation(&drv, "out").unwrap(), None);
	assert!(
		without
			.register_drv_output(&drv, "out", &realisation)
			.unwrap_err()
			.to_string()
			.contains("experimental Nix feature 'ca-derivations' is disabled")
	);
}
