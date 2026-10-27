//! The local store checked against the pinned CppNix 2.35.2 reference in
//! both directions: stores noxi writes must open, verify and report the
//! same metadata in the reference, and stores the reference writes must
//! read back identically in noxi.
//!
//! The reference executables come from `NOXI_REFERENCE_BIN` or
//! `target/nix-reference/bin` (`just reference`). Every command runs
//! against a temporary `--store ROOT` with an empty configuration directory,
//! so neither implementation touches `/nix/store`.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nix_compat::nixhash::{CAHash, HashAlgo, NixHash};
use nix_compat::store_path::StorePath;
use noxi_store::{FileIngestionMethod, LocalStore, LocalStoreConfig, ValidPathInfo};
use serde_json::Value;

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

	fn root(&self) -> PathBuf {
		self.dir.path().join("store")
	}

	fn open(&self) -> LocalStore {
		let mut config = LocalStoreConfig::with_root(&self.root());
		config.require_sigs = false;
		LocalStore::open(config).unwrap()
	}

	fn reference(&self, program: &str, args: &[&str]) -> Output {
		let output = Command::new(reference_bin().join(program))
			.args(args)
			.env_clear()
			.env("HOME", self.dir.path().join("home"))
			.env("NIX_CONF_DIR", self.dir.path().join("conf"))
			.env(
				"NIX_CONFIG",
				"experimental-features = nix-command\nbuild-users-group =\nsandbox = false\nsubstituters =\n",
			)
			.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin")
			.output()
			.unwrap();
		output
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

	fn store_arg(&self) -> String {
		self.root().to_str().unwrap().to_owned()
	}

	/// `nix path-info --json` from the reference, keyed by absolute path.
	fn reference_path_infos(&self) -> serde_json::Map<String, Value> {
		let json = self.reference_ok(
			"nix",
			&[
				"path-info",
				"--store",
				&self.store_arg(),
				"--json",
				"--json-format",
				"1",
				"--all",
			],
		);
		match serde_json::from_str(&json).unwrap() {
			Value::Object(map) => map,
			other => panic!("unexpected path-info output {other}"),
		}
	}

	fn reference_verify(&self) {
		let output = self.reference(
			"nix-store",
			&["--store", &self.store_arg(), "--verify", "--check-contents"],
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
}

fn sp(abs: &str) -> StorePath {
	StorePath::from_absolute_path(abs.trim().as_bytes()).unwrap()
}

/// Check one noxi path info against the reference's JSON for it.
fn assert_matches_reference(info: &ValidPathInfo, json: &Value) {
	let path = info.path.to_absolute_path();
	assert_eq!(
		json["narHash"].as_str().unwrap(),
		NixHash::Sha256(info.nar_hash).to_sri_string(),
		"narHash of {path}"
	);
	assert_eq!(
		json["narSize"].as_u64().unwrap(),
		info.nar_size,
		"narSize of {path}"
	);
	let refs: BTreeSet<String> = json["references"]
		.as_array()
		.unwrap()
		.iter()
		.map(|r| r.as_str().unwrap().to_owned())
		.collect();
	let ours: BTreeSet<String> = info
		.references
		.iter()
		.map(|r| r.to_absolute_path())
		.collect();
	assert_eq!(refs, ours, "references of {path}");
	assert_eq!(
		json["ca"].as_str().map(str::to_owned),
		info.ca.as_ref().map(ToString::to_string),
		"ca of {path}"
	);
	assert_eq!(
		json["deriver"].as_str().map(str::to_owned),
		info.deriver.as_ref().map(|d| d.to_absolute_path()),
		"deriver of {path}"
	);
	assert_eq!(
		json["ultimate"].as_bool().unwrap(),
		info.ultimate,
		"ultimate of {path}"
	);
	assert_eq!(
		json["registrationTime"].as_i64().unwrap(),
		info.registration_time,
		"registrationTime of {path}"
	);
	let sigs: BTreeSet<String> = json["signatures"]
		.as_array()
		.unwrap()
		.iter()
		.map(|s| s.as_str().unwrap().to_owned())
		.collect();
	assert_eq!(sigs, info.sigs, "signatures of {path}");
}

/// The layout CppNix creates for an empty store, as recorded in
/// `tests/reference-inventory/empty-store.txt`.
fn expected_layout() -> Vec<(String, char, String)> {
	let inventory = Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../tests/reference-inventory/empty-store.txt");
	let text = fs::read_to_string(inventory).unwrap();
	text.lines()
		.take_while(|line| !line.starts_with("# nix/var/nix/db/schema"))
		.filter(|line| !line.starts_with('#'))
		.map(|line| {
			let mut parts = line.split(' ');
			let mode = parts.next().unwrap().to_owned();
			let kind = parts.next().unwrap().chars().next().unwrap();
			let path = parts.next().unwrap().to_owned();
			(mode, kind, path)
		})
		.collect()
}

fn mode_string(mode: u32, is_dir: bool) -> String {
	let mut s = String::from(if is_dir { "d" } else { "-" });
	for shift in [6, 3, 0] {
		let bits = (mode >> shift) & 7;
		s.push(if bits & 4 != 0 { 'r' } else { '-' });
		s.push(if bits & 2 != 0 { 'w' } else { '-' });
		s.push(if bits & 1 != 0 { 'x' } else { '-' });
	}
	s
}

fn actual_layout(root: &Path) -> Vec<(String, char, String)> {
	fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, char, String)>) {
		let mut entries: Vec<_> = fs::read_dir(dir)
			.unwrap()
			.map(|e| e.unwrap().path())
			.collect();
		entries.sort();
		for path in entries {
			let metadata = fs::symlink_metadata(&path).unwrap();
			let rel = path
				.strip_prefix(root)
				.unwrap()
				.to_str()
				.unwrap()
				.to_owned();
			let is_dir = metadata.is_dir();
			out.push((
				mode_string(metadata.permissions().mode() & 0o777, is_dir),
				if is_dir { 'd' } else { 'f' },
				rel,
			));
			if is_dir {
				walk(root, &path, out);
			}
		}
	}
	let mut out = Vec::new();
	walk(root, root, &mut out);
	out.sort_by(|a, b| a.2.cmp(&b.2));
	out
}

#[test]
fn fresh_store_matches_the_reference_layout_and_opens_in_the_reference() {
	let env = Env::new();
	drop(env.open());

	let mut expected = expected_layout();
	expected.sort_by(|a, b| a.2.cmp(&b.2));
	assert_eq!(actual_layout(&env.root()), expected);

	let db = env.root().join("nix/var/nix/db");
	assert_eq!(fs::read_to_string(db.join("schema")).unwrap(), "10");
	assert_eq!(
		fs::metadata(db.join("reserved")).unwrap().len(),
		8 * 1024 * 1024
	);

	// The reference opens it without migrating, and reopening is stable.
	env.reference_ok("nix", &["store", "info", "--store", &env.store_arg()]);
	env.reference_verify();
	drop(env.open());
	assert_eq!(fs::read_to_string(db.join("schema")).unwrap(), "10");
}

#[test]
fn noxi_paths_have_reference_paths_and_verify_in_the_reference() {
	let env = Env::new();
	let src = env.dir.path().join("src");
	fs::create_dir(&src).unwrap();
	fs::write(src.join("hello"), "hello\n").unwrap();
	fs::write(src.join("run"), "#!/bin/sh\n").unwrap();
	fs::set_permissions(src.join("run"), fs::Permissions::from_mode(0o755)).unwrap();
	std::os::unix::fs::symlink("hello", src.join("link")).unwrap();
	let flat = env.dir.path().join("flat");
	fs::write(&flat, "flat contents\n").unwrap();

	let store = env.open();
	let mut nar = Vec::new();
	noxi_store::nar::dump_path(&mut nar, &src).unwrap();
	let recursive = store
		.add_to_store_from_dump(
			&mut &nar[..],
			"src",
			FileIngestionMethod::NixArchive,
			FileIngestionMethod::NixArchive,
			HashAlgo::Sha256,
			&BTreeSet::new(),
			false,
		)
		.unwrap();
	let flat_path = store
		.add_to_store_from_dump(
			&mut fs::File::open(&flat).unwrap(),
			"flat",
			FileIngestionMethod::Flat,
			FileIngestionMethod::Flat,
			HashAlgo::Sha256,
			&BTreeSet::new(),
			false,
		)
		.unwrap();
	let text = store
		.add_text_to_store(
			"text",
			format!("{}\n", recursive.to_absolute_path()).as_bytes(),
			&BTreeSet::from([recursive.clone()]),
			false,
		)
		.unwrap();
	drop(store);

	// The same inputs added by the reference get the same paths.
	let reference_recursive = env.reference_ok(
		"nix-store",
		&["--store", &env.store_arg(), "--add", src.to_str().unwrap()],
	);
	assert_eq!(sp(&reference_recursive), recursive);
	let reference_flat = env.reference_ok(
		"nix-store",
		&[
			"--store",
			&env.store_arg(),
			"--add-fixed",
			"sha256",
			flat.to_str().unwrap(),
		],
	);
	assert_eq!(
		sp(&reference_flat),
		StorePath::from_bytes(
			format!("{}-flat", nix_compat::nixbase32::encode(flat_path.digest())).as_bytes()
		)
		.unwrap()
	);

	let infos = env.reference_path_infos();
	let store = env.open();
	for path in [&recursive, &flat_path, &text] {
		let info = store.query_path_info(path).unwrap().unwrap();
		assert_matches_reference(&info, &infos[&path.to_absolute_path()]);
		let mut ours = Vec::new();
		store.nar_from_path(path, &mut ours).unwrap();
		let output = env.reference(
			"nix",
			&[
				"store",
				"dump-path",
				"--store",
				&env.store_arg(),
				&path.to_absolute_path(),
			],
		);
		assert!(
			output.status.success(),
			"{}",
			String::from_utf8_lossy(&output.stderr)
		);
		assert_eq!(ours, output.stdout, "NAR of {}", path.to_absolute_path());
	}
	drop(store);
	env.reference_verify();

	// Store contents are canonical: read-only modes and mtime 1.
	let real = env
		.root()
		.join(recursive.to_absolute_path().trim_start_matches('/'));
	for (name, mode) in [("", 0o555), ("hello", 0o444), ("run", 0o555)] {
		let metadata = fs::metadata(real.join(name)).unwrap();
		assert_eq!(metadata.permissions().mode() & 0o7777, mode, "{name}");
		assert_eq!(metadata.mtime(), 1, "{name}");
	}
}

#[test]
fn reference_paths_and_derivations_read_back_in_noxi() {
	let env = Env::new();
	let file = env.dir.path().join("f");
	fs::write(&file, "hello\n").unwrap();
	let added = sp(&env.reference_ok(
		"nix-store",
		&["--store", &env.store_arg(), "--add", file.to_str().unwrap()],
	));
	let drv = sp(&env.reference_ok(
		"nix-instantiate",
		&[
			"--store",
			&env.store_arg(),
			"--expr",
			r#"derivation { name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; outputs = [ "out" "dev" ]; args = [ "-c" "echo ${builtins.toFile "t" "x"} > $out" ]; }"#,
		],
	));

	let infos = env.reference_path_infos();
	let store = env.open();
	let all = store.query_all_valid_paths().unwrap();
	assert_eq!(
		all.iter()
			.map(|p| p.to_absolute_path())
			.collect::<BTreeSet<_>>(),
		infos.keys().cloned().collect::<BTreeSet<_>>()
	);
	for path in &all {
		let info = store.query_path_info(path).unwrap().unwrap();
		assert_matches_reference(&info, &infos[&path.to_absolute_path()]);
	}
	assert!(store.is_valid_path(&added).unwrap());
	assert!(matches!(
		store.query_path_info(&added).unwrap().unwrap().ca,
		Some(CAHash::Nar(_))
	));

	// The reference recorded the derivation's outputs; noxi reads them,
	// and finds the derivation from each output.
	let outputs = store.query_derivation_outputs(&drv).unwrap();
	assert_eq!(outputs.keys().cloned().collect::<Vec<_>>(), ["dev", "out"]);
	let reference_outputs: BTreeSet<StorePath> = env
		.reference_ok(
			"nix-store",
			&[
				"--store",
				&env.store_arg(),
				"-q",
				"--outputs",
				&drv.to_absolute_path(),
			],
		)
		.lines()
		.map(sp)
		.collect();
	assert_eq!(
		outputs.values().cloned().collect::<BTreeSet<_>>(),
		reference_outputs
	);
	for output in outputs.values() {
		assert_eq!(
			store.query_valid_derivers(output).unwrap(),
			BTreeSet::from([drv.clone()])
		);
	}

	// The reference's text paths are reproduced by noxi, which then
	// registers the derivation's outputs itself after checking them.
	let drv_text = fs::read(
		env.root()
			.join(drv.to_absolute_path().trim_start_matches('/')),
	)
	.unwrap();
	let drv_info = store.query_path_info(&drv).unwrap().unwrap();
	drop(store);
	let fresh = Env::new();
	let store = fresh.open();
	let t = drv_info.references.iter().next().unwrap().clone();
	let t_text = fs::read(
		env.root()
			.join(t.to_absolute_path().trim_start_matches('/')),
	)
	.unwrap();
	assert_eq!(
		store
			.add_text_to_store("t", &t_text, &BTreeSet::new(), false)
			.unwrap(),
		t
	);
	let name = drv.name().to_owned();
	assert_eq!(
		store
			.add_text_to_store(&name, &drv_text, &drv_info.references, false)
			.unwrap(),
		drv
	);
	assert_eq!(store.query_derivation_outputs(&drv).unwrap(), outputs);
	assert_eq!(
		fresh
			.reference_ok(
				"nix-store",
				&[
					"--store",
					&fresh.store_arg(),
					"-q",
					"--outputs",
					&drv.to_absolute_path()
				]
			)
			.lines()
			.map(sp)
			.collect::<BTreeSet<_>>(),
		reference_outputs
	);
	drop(store);
	fresh.reference_verify();
}

#[test]
fn nar_imports_check_hashes_and_register_metadata() {
	let env = Env::new();
	let src = env.dir.path().join("dep");
	fs::write(&src, "dependency\n").unwrap();
	let dep = sp(&env.reference_ok(
		"nix-store",
		&["--store", &env.store_arg(), "--add", src.to_str().unwrap()],
	));

	// An input-addressed path with a reference, a deriver and a signature,
	// imported from its NAR.
	let source = env.dir.path().join("out");
	fs::create_dir(&source).unwrap();
	fs::write(source.join("ref"), dep.to_absolute_path()).unwrap();
	let mut nar = Vec::new();
	noxi_store::nar::dump_path(&mut nar, &source).unwrap();
	let (nar_hash, nar_size) = {
		let mut sink = noxi_store::nar::HashSink::new();
		std::io::Write::write_all(&mut sink, &nar).unwrap();
		sink.finish()
	};
	let path = StorePath::from_bytes(b"00000000000000000000000000000000-imported").unwrap();
	let mut info = ValidPathInfo::new(path.clone(), nar_hash);
	info.nar_size = nar_size;
	info.references = BTreeSet::from([dep.clone()]);
	info.deriver =
		Some(StorePath::from_bytes(b"11111111111111111111111111111111-imported.drv").unwrap());
	info.registration_time = 1_700_000_000;
	info.ultimate = true;
	info.sigs = BTreeSet::from(["test-1:AAAA".to_owned()]);

	let store = env.open();
	let mut bad = info.clone();
	bad.nar_hash[0] ^= 1;
	let error = store
		.add_to_store(&bad, &mut &nar[..], false, false)
		.unwrap_err()
		.to_string();
	assert!(
		error.starts_with(&format!(
			"hash mismatch importing path '{}'",
			path.to_absolute_path()
		)),
		"{error}"
	);
	assert!(!store.is_valid_path(&path).unwrap());

	let mut bad = info.clone();
	bad.nar_size += 1;
	let error = store
		.add_to_store(&bad, &mut &nar[..], false, false)
		.unwrap_err()
		.to_string();
	assert!(error.starts_with("size mismatch importing path"), "{error}");

	// Unsigned paths are refused when signatures are required.
	let mut strict = LocalStoreConfig::with_root(&env.root());
	strict.require_sigs = true;
	drop(store);
	let strict = LocalStore::open(strict).unwrap();
	let error = strict
		.add_to_store(&info, &mut &nar[..], false, true)
		.unwrap_err()
		.to_string();
	assert_eq!(
		error,
		format!(
			"cannot add path '{}' because it lacks a signature by a trusted key",
			path.to_absolute_path()
		)
	);
	drop(strict);

	let store = env.open();
	store
		.add_to_store(&info, &mut &nar[..], false, false)
		.unwrap();
	assert_eq!(store.query_path_info(&path).unwrap().unwrap(), info);
	assert_eq!(
		store.query_referrers(&dep).unwrap(),
		BTreeSet::from([path.clone()])
	);
	assert_eq!(
		store
			.query_path_from_hash_part("00000000000000000000000000000000")
			.unwrap(),
		Some(path.clone())
	);

	// The referenced path cannot be removed while the import refers to it.
	let error = store.invalidate_path_checked(&dep).unwrap_err().to_string();
	assert_eq!(
		error,
		format!(
			"cannot delete path '{}' because it is in use by '{}'",
			dep.to_absolute_path(),
			path.to_absolute_path()
		)
	);
	drop(store);

	let infos = env.reference_path_infos();
	assert_matches_reference(&info, &infos[&path.to_absolute_path()]);
	env.reference_verify();
	let referrers = env.reference_ok(
		"nix-store",
		&[
			"--store",
			&env.store_arg(),
			"-q",
			"--referrers",
			&dep.to_absolute_path(),
		],
	);
	assert_eq!(referrers.trim(), path.to_absolute_path());
}

#[test]
fn reference_cycles_and_vanished_paths_are_handled() {
	let env = Env::new();
	let store = env.open();
	let make = |name: &str| {
		let content = env.dir.path().join(name);
		fs::write(&content, name).unwrap();
		let mut nar = Vec::new();
		noxi_store::nar::dump_path(&mut nar, &content).unwrap();
		(
			nar,
			StorePath::from_bytes(
				format!("{}-{name}", "2".repeat(31) + &name.len().to_string()).as_bytes(),
			)
			.unwrap(),
		)
	};
	let (nar_a, a) = make("a");
	let (_, b) = make("bb");
	let (nar_hash, nar_size) = {
		let mut sink = noxi_store::nar::HashSink::new();
		std::io::Write::write_all(&mut sink, &nar_a).unwrap();
		sink.finish()
	};
	// Two paths referring to each other form a cycle and nothing is
	// registered.
	let mut info_a = ValidPathInfo::new(a.clone(), nar_hash);
	info_a.nar_size = nar_size;
	info_a.references.insert(b.clone());
	let mut info_b = ValidPathInfo::new(b.clone(), nar_hash);
	info_b.nar_size = nar_size;
	info_b.references.insert(a.clone());
	let error = store
		.register_valid_paths(&[info_a.clone(), info_b])
		.unwrap_err()
		.to_string();
	assert!(
		error.starts_with("cycle detected in the references of"),
		"{error}"
	);
	assert!(!store.is_valid_path(&a).unwrap());

	// A registered path whose contents vanished is dropped by verification.
	info_a.references.clear();
	store
		.add_to_store(&info_a, &mut &nar_a[..], false, false)
		.unwrap();
	let real = store.real_path(&a);
	fs::set_permissions(&real, fs::Permissions::from_mode(0o644)).unwrap();
	fs::remove_file(&real).unwrap();
	assert!(!store.verify_store(true, &mut |_, _| {}).unwrap());
	assert!(!store.is_valid_path(&a).unwrap());
	drop(store);
	env.reference_verify();
}

#[test]
fn handles_to_one_store_share_its_temporary_roots() {
	let env = Env::new();
	let first = env.open();
	let second = env.open();
	let a = sp("/nix/store/00000000000000000000000000000000-a");
	let b = sp("/nix/store/11111111111111111111111111111111-b");
	first.add_temp_root(&a).unwrap();
	second.add_temp_root(&b).unwrap();
	first.add_temp_root(&b).unwrap();
	let file = env
		.root()
		.join("nix/var/nix/temproots")
		.join(std::process::id().to_string());
	assert_eq!(
		fs::read(&file).unwrap(),
		[&a, &b, &b]
			.map(|path| path.to_absolute_path() + "\0")
			.concat()
			.into_bytes()
	);
	// As in CppNix, the last handle removes the file.
	drop(first);
	assert!(file.exists());
	drop(second);
	assert!(!file.exists());
}
