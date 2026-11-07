//! A copied CppNix store used by noxi and returned to CppNix.
//!
//! The pinned 2.35.2 reference builds a store with built outputs, unbuilt
//! derivations, an indirect root, a profile, signatures and unrooted
//! garbage. The store is copied, and noxi uses the copy: it reads every
//! path's metadata, collects garbage, imports NARs and text, builds a
//! derivation the reference instantiated, adds signatures and verifies the
//! store. The reference then takes the copy back: `nix-store --verify
//! --check-contents` must find nothing to repair, the metadata of every path
//! noxi did not change must be what the reference wrote, and the
//! reference must be able to keep building, adding and collecting garbage.
//!
//! The reference executables come from `NOXI_REFERENCE_BIN` or
//! `target/nix-reference/bin`, and the busybox builder from
//! `NOXI_TEST_BUSYBOX` or `target/test-busybox` (`just reference`).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nix_compat::nixhash::{HashAlgo, NixHash};
use nix_compat::store_path::StorePath;
use noxi_build::local::LocalBuilder;
use noxi_build::settings::BuildSettings;
use noxi_store::gc::{GcAction, GcOptions};
use noxi_store::{FileIngestionMethod, LocalStore, LocalStoreConfig, ValidPathInfo};
use serde_json::{Map, Value};

const SYSTEM_FEATURES: &str = "benchmark big-parallel kvm nixos-test uid-range";

fn from_env_or_target(var: &str, target: &str, probe: &str) -> PathBuf {
	if let Some(path) = std::env::var_os(var) {
		return PathBuf::from(path);
	}
	let path = Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../target")
		.join(target);
	assert!(
		path.join(probe).exists(),
		"{target} is missing; run `just reference` or set {var}"
	);
	path
}

fn reference_bin() -> PathBuf {
	from_env_or_target("NOXI_REFERENCE_BIN", "nix-reference/bin", "nix-store")
}

fn busybox() -> String {
	let path = from_env_or_target("NOXI_TEST_BUSYBOX", "test-busybox", "bin/busybox");
	fs::canonicalize(path)
		.unwrap()
		.to_string_lossy()
		.into_owned()
}

struct Env {
	dir: tempfile::TempDir,
	busybox: String,
}

impl Env {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		fs::create_dir(dir.path().join("conf")).unwrap();
		fs::create_dir(dir.path().join("home")).unwrap();
		fs::create_dir(dir.path().join("links")).unwrap();
		Env {
			dir,
			busybox: busybox(),
		}
	}

	fn root(&self, name: &str) -> PathBuf {
		self.dir.path().join(name)
	}

	fn store(&self, name: &str) -> String {
		self.root(name).to_str().unwrap().to_owned()
	}

	fn reference(&self, program: &str, args: &[&str]) -> Output {
		Command::new(reference_bin().join(program))
			.args(args)
			.env_clear()
			.env("HOME", self.dir.path().join("home"))
			.env("NIX_CONF_DIR", self.dir.path().join("conf"))
			.env(
				"NIX_CONFIG",
				format!(
					"experimental-features = nix-command\nbuild-users-group =\nsubstituters =\nsandbox = true\nsandbox-fallback = false\nsandbox-paths =\ncores = 1\nsystem-features = {SYSTEM_FEATURES}\n"
				),
			)
			.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin")
			.output()
			.unwrap()
	}

	fn reference_ok(&self, program: &str, args: &[&str]) -> String {
		let output = self.reference(program, args);
		assert!(
			output.status.success(),
			"{program} {args:?} failed:\n{}",
			String::from_utf8_lossy(&output.stderr)
		);
		String::from_utf8(output.stdout).unwrap()
	}

	fn write_expr(&self, name: &str, body: &str) -> String {
		let file = self.dir.path().join(name);
		fs::write(
			&file,
			format!(
				r#"let bb = builtins.storePath {}; mk = name: script: derivation {{ inherit name; system = "x86_64-linux"; builder = "${{bb}}/bin/sh"; args = [ "-c" script ]; }}; in {body}"#,
				self.busybox
			),
		)
		.unwrap();
		file.to_str().unwrap().to_owned()
	}

	fn instantiate(&self, store: &str, expr: &str, attr: &str) -> StorePath {
		let out = self.reference_ok(
			"nix-instantiate",
			&["--store", &self.store(store), expr, "-A", attr],
		);
		StorePath::from_absolute_path(out.trim().as_bytes()).unwrap()
	}

	fn open(&self, name: &str) -> LocalStore {
		let mut config = LocalStoreConfig::with_root(&self.root(name));
		config.require_sigs = false;
		LocalStore::open(config).unwrap()
	}

	/// `nix path-info --json --all` from the reference, keyed by path.
	fn path_infos(&self, name: &str) -> Map<String, Value> {
		let json = self.reference_ok(
			"nix",
			&[
				"path-info",
				"--json",
				"--json-format",
				"1",
				"--all",
				"--store",
				&self.store(name),
			],
		);
		match serde_json::from_str(&json).unwrap() {
			Value::Object(map) => map,
			other => panic!("unexpected path-info output {other}"),
		}
	}

	fn verify(&self, name: &str) {
		let output = self.reference(
			"nix-store",
			&["--store", &self.store(name), "--verify", "--check-contents"],
		);
		let stderr = String::from_utf8_lossy(&output.stderr);
		assert!(output.status.success(), "verify failed:\n{stderr}");
		for bad in [
			"error",
			"warning",
			"modified",
			"disappeared",
			"fixing",
			"updating",
		] {
			assert!(!stderr.contains(bad), "verify reported problems:\n{stderr}");
		}
	}

	/// The schema objects, named migrations and schema file, which a round
	/// trip must leave as the reference wrote them.
	fn schema(&self, name: &str) -> Vec<String> {
		let db_dir = self.root(name).join("nix/var/nix/db");
		let db = rusqlite::Connection::open(db_dir.join("db.sqlite")).unwrap();
		let mut out: Vec<String> = db
			.prepare(
				"select type || ' ' || name || ' ' || ifnull(sql, '') from sqlite_master order by name",
			)
			.unwrap()
			.query_map([], |row| row.get(0))
			.unwrap()
			.collect::<Result<_, _>>()
			.unwrap();
		out.extend(
			db.prepare("select migration from SchemaMigrations order by migration")
				.unwrap()
				.query_map([], |row| row.get::<_, String>(0))
				.unwrap()
				.map(Result::unwrap),
		);
		out.push(fs::read_to_string(db_dir.join("schema")).unwrap());
		out
	}
}

fn sp(abs: &str) -> StorePath {
	StorePath::from_absolute_path(abs.trim().as_bytes()).unwrap()
}

/// noxi's view of a path must be the reference's JSON for it.
fn assert_matches(info: &ValidPathInfo, json: &Value) {
	let path = info.path.to_absolute_path();
	assert_eq!(
		json["narHash"].as_str().unwrap(),
		NixHash::Sha256(info.nar_hash).to_sri_string(),
		"{path}"
	);
	assert_eq!(json["narSize"].as_u64().unwrap(), info.nar_size, "{path}");
	let refs: BTreeSet<&str> = json["references"]
		.as_array()
		.unwrap()
		.iter()
		.map(|r| r.as_str().unwrap())
		.collect();
	let ours: BTreeSet<String> = info
		.references
		.iter()
		.map(|r| r.to_absolute_path())
		.collect();
	assert_eq!(
		refs,
		ours.iter().map(String::as_str).collect::<BTreeSet<_>>(),
		"{path}"
	);
	assert_eq!(
		json["ca"].as_str().map(str::to_owned),
		info.ca.as_ref().map(ToString::to_string),
		"{path}"
	);
	assert_eq!(
		json["deriver"].as_str().map(str::to_owned),
		info.deriver.as_ref().map(|d| d.to_absolute_path()),
		"{path}"
	);
	assert_eq!(json["ultimate"].as_bool().unwrap(), info.ultimate, "{path}");
	assert_eq!(
		json["registrationTime"].as_i64().unwrap(),
		info.registration_time,
		"{path}"
	);
	let sigs: BTreeSet<String> = json["signatures"]
		.as_array()
		.unwrap()
		.iter()
		.map(|s| s.as_str().unwrap().to_owned())
		.collect();
	assert_eq!(sigs, info.sigs, "{path}");
}

#[test]
fn copied_reference_store_returns_to_the_reference_intact() {
	let env = Env::new();
	let original = env.store("original");
	env.reference_ok(
		"nix",
		&[
			"copy",
			"--no-check-sigs",
			"--to",
			&format!("local?root={original}"),
			&env.busybox,
		],
	);
	let expr = env.write_expr(
		"graph.nix",
		r#"rec {
			lib = mk "lib" "echo lib > $out";
			app = mk "app" "echo ${lib} ${builtins.toFile "app-source" "app"} > $out";
			profiled = mk "profiled" "echo profiled > $out";
			unbuilt = mk "unbuilt" "echo ${lib} > $out";
			garbage = mk "garbage" "echo garbage > $out";
		}"#,
	);
	let app = env.instantiate("original", &expr, "app");
	let profiled = env.instantiate("original", &expr, "profiled");
	let unbuilt = env.instantiate("original", &expr, "unbuilt");
	let garbage = env.instantiate("original", &expr, "garbage");
	let link = env.root("links/app");
	env.reference_ok(
		"nix-store",
		&[
			"--store",
			&original,
			"--realise",
			"--add-root",
			link.to_str().unwrap(),
			&app.to_absolute_path(),
		],
	);
	// With `--add-root` the reference prints the root, not the path.
	let app_out = fs::read_link(&link).unwrap().to_str().unwrap().to_owned();
	let profiled_out = env
		.reference_ok(
			"nix-store",
			&[
				"--store",
				&original,
				"--realise",
				&profiled.to_absolute_path(),
			],
		)
		.trim()
		.to_owned();
	env.reference_ok(
		"nix-env",
		&[
			"--store",
			&original,
			"-p",
			&format!("{original}/nix/var/nix/profiles/test"),
			"--set",
			&profiled_out,
		],
	);
	let garbage_out = env
		.reference_ok(
			"nix-store",
			&[
				"--store",
				&original,
				"--realise",
				&garbage.to_absolute_path(),
			],
		)
		.trim()
		.to_owned();
	let extra = env.dir.path().join("extra");
	fs::write(&extra, "extra\n").unwrap();
	let extra_path = env.reference_ok(
		"nix-store",
		&["--store", &original, "--add", extra.to_str().unwrap()],
	);
	let key = env.dir.path().join("key");
	let secret = env.reference_ok("nix", &["key", "generate-secret", "--key-name", "k-1"]);
	fs::write(&key, secret).unwrap();
	env.reference_ok(
		"nix",
		&[
			"store",
			"sign",
			"--store",
			&original,
			"--key-file",
			key.to_str().unwrap(),
			&app_out,
		],
	);
	std::os::unix::fs::symlink(
		&env.busybox,
		env.root("original/nix/var/nix/gcroots/busybox"),
	)
	.unwrap();
	std::os::unix::fs::symlink(
		unbuilt.to_absolute_path(),
		env.root("original/nix/var/nix/gcroots/unbuilt"),
	)
	.unwrap();

	let dead: BTreeSet<String> = env
		.reference_ok("nix-store", &["--store", &original, "--gc", "--print-dead"])
		.lines()
		.map(str::to_owned)
		.collect();
	assert!(dead.contains(&garbage_out) && dead.contains(extra_path.trim()));
	assert!(!dead.contains(&app_out) && !dead.contains(&profiled_out));

	let status = Command::new("cp")
		.args(["-a", &original, &env.store("copy")])
		.status()
		.unwrap();
	assert!(status.success());
	let profiles = env.root("copy/nix/var/nix/gcroots/profiles");
	if fs::symlink_metadata(&profiles).is_ok() {
		fs::remove_file(&profiles).unwrap();
		std::os::unix::fs::symlink(env.root("copy/nix/var/nix/profiles"), &profiles).unwrap();
	}
	let before = env.path_infos("copy");
	let schema = env.schema("copy");

	// noxi reads every path as the reference wrote it.
	let store = env.open("copy");
	let valid = store.query_all_valid_paths().unwrap();
	assert_eq!(
		valid
			.iter()
			.map(|p| p.to_absolute_path())
			.collect::<BTreeSet<_>>(),
		before.keys().cloned().collect::<BTreeSet<_>>()
	);
	for path in &valid {
		assert_matches(
			&store.query_path_info(path).unwrap().unwrap(),
			&before[&path.to_absolute_path()],
		);
	}

	// It collects the same garbage the reference would.
	let results = store
		.collect_garbage(&GcOptions::new(GcAction::DeleteDead), &mut |_| {})
		.unwrap();
	assert_eq!(results.paths, dead);

	// It adds paths that refer to reference-made ones, and builds a
	// derivation the reference instantiated on top of a reference output.
	let mut nar = Vec::new();
	let src = env.dir.path().join("noxi-src");
	fs::create_dir(&src).unwrap();
	fs::write(src.join("file"), "from noxi\n").unwrap();
	noxi_store::nar::dump_path(&mut nar, &src).unwrap();
	let imported = store
		.add_to_store_from_dump(
			&mut &nar[..],
			"noxi-src",
			FileIngestionMethod::NixArchive,
			FileIngestionMethod::NixArchive,
			HashAlgo::Sha256,
			&BTreeSet::new(),
			false,
		)
		.unwrap();
	let text = store
		.add_text_to_store(
			"noxi-text",
			format!("{app_out}\n").as_bytes(),
			&BTreeSet::from([sp(&app_out)]),
			false,
		)
		.unwrap();
	let settings = BuildSettings {
		sandbox_paths: Vec::new(),
		build_cores: 1,
		system_features: SYSTEM_FEATURES.split(' ').map(String::from).collect(),
		nix_command: true,
		..BuildSettings::default()
	};
	let built = LocalBuilder::new(&store, &settings)
		.realise(&unbuilt, &mut |_| {})
		.unwrap();
	let unbuilt_out = built["out"].clone();
	store
		.add_signatures(
			&sp(&profiled_out),
			&BTreeSet::from(["noxi-1:c2lnbmF0dXJl".to_owned()]),
		)
		.unwrap();
	let mut log = Vec::new();
	assert!(
		!store
			.verify_store(true, &mut |_, line| log.push(line.to_owned()))
			.unwrap(),
		"{log:?}"
	);
	drop(store);

	// The reference takes the store back without repairs or migrations.
	env.verify("copy");
	assert_eq!(env.schema("copy"), schema);
	let after = env.path_infos("copy");
	for (path, info) in &before {
		if dead.contains(path) {
			assert!(!after.contains_key(path), "{path} was collected");
		} else if *path == profiled_out {
			let mut expected = info.clone();
			let sigs = expected["signatures"].as_array_mut().unwrap();
			sigs.push(Value::String("noxi-1:c2lnbmF0dXJl".to_owned()));
			assert_eq!(after[path], expected, "{path}");
		} else {
			assert_eq!(after[path], *info, "{path}");
		}
	}
	for path in [&imported, &text, &unbuilt_out] {
		assert!(after.contains_key(&path.to_absolute_path()), "{path}");
	}
	assert_eq!(
		after[&text.to_absolute_path()]["references"],
		Value::Array(vec![Value::String(app_out.clone())])
	);
	assert_eq!(
		after[&unbuilt_out.to_absolute_path()]["deriver"],
		Value::String(unbuilt.to_absolute_path())
	);
	assert_eq!(
		fs::read_to_string(
			env.root("copy")
				.join(unbuilt_out.to_absolute_path().trim_start_matches('/'))
		)
		.unwrap(),
		fs::read_to_string(env.root("copy").join(app_out.trim_start_matches('/')))
			.unwrap()
			.split(' ')
			.next()
			.unwrap()
			.to_owned()
			+ "\n"
	);

	// The reference keeps writing to it: a new build, an addition, and a
	// collection that frees noxi's unrooted paths.
	let copy = env.store("copy");
	let more = env.write_expr("more.nix", r#"{ more = mk "more" "echo more > $out"; }"#);
	let more_drv = env.instantiate("copy", &more, "more");
	let more_out = env
		.reference_ok(
			"nix-store",
			&["--store", &copy, "--realise", &more_drv.to_absolute_path()],
		)
		.trim()
		.to_owned();
	assert_eq!(
		fs::read_to_string(env.root("copy").join(more_out.trim_start_matches('/'))).unwrap(),
		"more\n"
	);
	let collected: BTreeSet<String> = env
		.reference_ok("nix-store", &["--store", &copy, "--gc", "--print-dead"])
		.lines()
		.map(str::to_owned)
		.collect();
	for path in [&imported, &text, &unbuilt_out] {
		assert!(collected.contains(&path.to_absolute_path()), "{path}");
	}
	env.reference_ok("nix-store", &["--store", &copy, "--gc"]);
	env.verify("copy");
	assert_eq!(env.schema("copy"), schema);
}
