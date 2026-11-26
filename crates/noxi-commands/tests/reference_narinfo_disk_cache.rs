//! The narinfo disk cache, `binary-cache-v8.sqlite`, compared with CppNix
//! 2.35.2. Both engines create the same schema in the user's cache
//! directory and leave no write-ahead log behind, and each reuses what the
//! other recorded: a `.narinfo` the cache no longer has, and the absence of
//! a path the cache gained since, until `narinfo-cache-negative-ttl` says
//! otherwise. The caches are `file://` caches read as HTTP caches under
//! `_NIX_FORCE_HTTP=1`, as CppNix's tests read them; every store and home
//! directory is a scratch directory.
//!
//! The reference comes from `NOXI_REFERENCE_BIN`, else
//! `target/nix-reference/bin` (`just reference` creates it).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn reference_bin() -> PathBuf {
	if let Some(path) = std::env::var_os("NOXI_REFERENCE_BIN") {
		return PathBuf::from(path);
	}
	let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin");
	assert!(
		path.join("nix-store").exists(),
		"the reference is missing; run `just reference` or set NOXI_REFERENCE_BIN"
	);
	path
}

fn program(engine: &str, name: &str) -> PathBuf {
	match engine {
		"reference" => reference_bin().join(name),
		_ => PathBuf::from(match name {
			"nix" => env!("CARGO_BIN_EXE_nix"),
			"nix-store" => env!("CARGO_BIN_EXE_nix-store"),
			_ => panic!("no noxi executable for {name}"),
		}),
	}
}

struct Scene {
	dir: noxi_test_support::TempDir,
}

impl Scene {
	fn new() -> Self {
		Self {
			dir: noxi_test_support::tempdir().unwrap(),
		}
	}

	fn path(&self, name: &str) -> PathBuf {
		self.dir.path().join(name)
	}

	/// Run `engine`'s `name` with `home` as the home directory.
	fn run(&self, engine: &str, name: &str, home: &str, args: &[&str]) -> Output {
		fs::create_dir_all(self.path("conf")).unwrap();
		Command::new(program(engine, name))
			.env_clear()
			.env("HOME", self.path(home))
			.env("NIX_CONF_DIR", self.path("conf"))
			.env(
				"NIX_CONFIG",
				"experimental-features = nix-command\nsubstituters =\nbuild-users-group =\n",
			)
			.env("_NIX_FORCE_HTTP", "1")
			.args(args)
			.output()
			.unwrap()
	}

	fn store(&self, name: &str) -> String {
		format!("local?root={}", self.path(name).display())
	}

	fn cache(&self, name: &str) -> String {
		format!("file://{}", self.path(name).display())
	}

	/// A path added to the `source` store and copied, with the reference
	/// and without `_NIX_FORCE_HTTP`, to the `file://` cache `cache`.
	fn add(&self, file: &str, contents: &str, cache: &str) -> String {
		fs::write(self.path(file), contents).unwrap();
		let added = self.run(
			"reference",
			"nix",
			"setup-home",
			&[
				"store",
				"add-file",
				"--store",
				&self.store("source"),
				self.path(file).to_str().unwrap(),
			],
		);
		assert!(added.status.success(), "{added:?}");
		let path = String::from_utf8(added.stdout).unwrap().trim().to_owned();
		let copied = Command::new(reference_bin().join("nix"))
			.env_clear()
			.env("HOME", self.path("setup-home"))
			.env("NIX_CONF_DIR", self.path("conf"))
			.env("NIX_CONFIG", "experimental-features = nix-command\n")
			.args([
				"copy",
				"--store",
				&self.store("source"),
				"--to",
				&self.cache(cache),
				&path,
			])
			.output()
			.unwrap();
		assert!(copied.status.success(), "{copied:?}");
		path
	}

	fn database(&self, home: &str) -> PathBuf {
		self.path(home).join(".cache/nix/binary-cache-v8.sqlite")
	}
}

fn narinfo(path: &str) -> String {
	let base = path.rsplit('/').next().unwrap();
	format!("{}.narinfo", &base[..32])
}

/// `sqlite_master` and the journal mode of a database.
fn schema(database: &Path) -> (Vec<(String, String, String, Option<String>)>, String) {
	let db =
		rusqlite::Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
			.unwrap();
	let mut statement = db
		.prepare("select type, name, tbl_name, sql from sqlite_master order by name")
		.unwrap();
	let rows = statement
		.query_map([], |row| {
			Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
		})
		.unwrap()
		.map(Result::unwrap)
		.collect();
	let mode = db
		.query_row("pragma journal_mode", [], |row| row.get(0))
		.unwrap();
	(rows, mode)
}

#[test]
fn both_engines_create_the_same_disk_cache() {
	let scene = Scene::new();
	scene.add("file", "cached\n", "cache");
	let mut found = Vec::new();
	for engine in ["reference", "noxi"] {
		let info = scene.run(
			engine,
			"nix",
			engine,
			&["store", "info", "--store", &scene.cache("cache")],
		);
		assert!(info.status.success(), "{engine}: {info:?}");
		let database = scene.database(engine);
		// The database is closed at exit, which removes the log.
		let mut wal = database.clone().into_os_string();
		wal.push("-wal");
		assert!(!Path::new(&wal).exists(), "{engine}");
		found.push(schema(&database));
	}
	assert_eq!(found[0].1, "wal");
	assert_eq!(found[0], found[1]);
}

#[test]
fn each_engine_reuses_what_the_other_cached() {
	for (writer, reader) in [("reference", "noxi"), ("noxi", "reference")] {
		let scene = Scene::new();
		let path = scene.add("file", "cached\n", "cache");
		let substitute = |engine: &str, home: &str, store: &str| {
			scene.run(
				engine,
				"nix-store",
				home,
				&[
					"--store",
					&scene.store(store),
					"-r",
					&path,
					"--option",
					"substituters",
					&scene.cache("cache"),
					"--option",
					"require-sigs",
					"false",
				],
			)
		};
		let written = substitute(writer, "home", "first");
		assert!(written.status.success(), "{writer}: {written:?}");
		// The cache loses the `.narinfo`, but keeps the NAR.
		fs::remove_file(scene.path("cache").join(narinfo(&path))).unwrap();
		let reused = substitute(reader, "home", "second");
		assert!(
			reused.status.success(),
			"{reader} after {writer}: {reused:?}"
		);
		// Without the disk cache the path cannot be substituted.
		let without = substitute(reader, "other-home", "third");
		assert!(!without.status.success(), "{reader}: {without:?}");
	}
}

#[test]
fn a_recorded_absence_hides_a_later_upload() {
	for (writer, reader) in [("reference", "noxi"), ("noxi", "reference")] {
		let scene = Scene::new();
		scene.add("first", "first\n", "cache");
		// A path that is in the source store but not yet in the cache.
		let path = scene.add("second", "second\n", "elsewhere");
		let cache = scene.cache("cache");
		let query = |engine: &str, extra: &[&str]| {
			let mut args = vec!["path-info", "--store", &cache, &path];
			args.extend_from_slice(extra);
			scene.run(engine, "nix", "home", &args)
		};
		let missing = query(writer, &[]);
		assert!(!missing.status.success(), "{writer}: {missing:?}");
		let copied = Command::new(reference_bin().join("nix"))
			.env_clear()
			.env("HOME", scene.path("setup-home"))
			.env("NIX_CONF_DIR", scene.path("conf"))
			.env("NIX_CONFIG", "experimental-features = nix-command\n")
			.args([
				"copy",
				"--store",
				&scene.store("source"),
				"--to",
				&scene.cache("cache"),
				&path,
			])
			.output()
			.unwrap();
		assert!(copied.status.success(), "{copied:?}");
		let hidden = query(reader, &[]);
		assert!(
			!hidden.status.success(),
			"{reader} after {writer}: {hidden:?}"
		);
		let found = query(reader, &["--option", "narinfo-cache-negative-ttl", "0"]);
		assert!(found.status.success(), "{reader}: {found:?}");
		assert_eq!(String::from_utf8_lossy(&found.stdout).trim(), path);
	}
}
