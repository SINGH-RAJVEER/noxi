//! Binary caches and substitution checked against the pinned CppNix 2.35.2
//! reference.
//!
//! The reference builds a path that refers to the test busybox, then copies
//! its closure into `file://` caches, signed or not and with each
//! compression method. Narinfo files must read and print back byte for
//! byte, and substituting from each cache into a fresh store must register
//! the same metadata in noxi as in the reference.
//!
//! The reference executables and the busybox come from the same places as
//! in `reference_build` (`just reference`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nix_compat::narinfo::{NarInfo, parse_keypair};
use nix_compat::nixcpp::narinfo as cppnix;
use nix_compat::store_path::StorePath;
use noxi_store::binary_cache::BinaryCache;
use noxi_store::{LocalStore, LocalStoreConfig};
use serde_json::Value;

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

struct Env {
	dir: tempfile::TempDir,
	public_key: String,
	/// The built path and the busybox it refers to.
	out: StorePath,
	busybox: StorePath,
}

impl Env {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		for sub in ["conf", "home"] {
			fs::create_dir(dir.path().join(sub)).unwrap();
		}
		let busybox = fs::canonicalize(from_env_or_target(
			"NOXI_TEST_BUSYBOX",
			"test-busybox",
			"bin/busybox",
		))
		.unwrap();
		let mut env = Env {
			dir,
			public_key: String::new(),
			busybox: StorePath::from_absolute_path(busybox.to_str().unwrap().as_bytes()).unwrap(),
			out: StorePath::from_absolute_path(busybox.to_str().unwrap().as_bytes()).unwrap(),
		};
		let secret = env.reference_ok("nix", &["key", "generate-secret", "--key-name", "test-1"]);
		fs::write(env.secret_key_file(), &secret).unwrap();
		env.public_key = env
			.reference_with_input("nix", &["key", "convert-secret-to-public"], &secret)
			.trim()
			.to_owned();

		let src = env.root("src");
		env.reference_ok(
			"nix",
			&[
				"copy",
				"--no-check-sigs",
				"--to",
				&format!("local?root={}", src.display()),
				&env.busybox.to_absolute_path(),
			],
		);
		let expr = env.dir.path().join("expr.nix");
		fs::write(
			&expr,
			format!(
				r#"let bb = builtins.storePath {}; in derivation {{
					name = "cached";
					system = "x86_64-linux";
					builder = "${{bb}}/bin/sh";
					args = [ "-c" "${{bb}}/bin/mkdir $out; echo ${{bb}} > $out/ref; ${{bb}}/bin/ln -s $out $out/self" ];
				}}"#,
				env.busybox.to_absolute_path()
			),
		)
		.unwrap();
		let out = env.reference_ok(
			"nix-build",
			&[
				"--store",
				src.to_str().unwrap(),
				"--no-out-link",
				expr.to_str().unwrap(),
			],
		);
		env.out = StorePath::from_absolute_path(out.trim().as_bytes()).unwrap();
		env
	}

	fn secret_key_file(&self) -> PathBuf {
		self.dir.path().join("secret-key")
	}

	fn root(&self, name: &str) -> PathBuf {
		self.dir.path().join(name)
	}

	fn command(&self, program: &str, args: &[&str]) -> Command {
		let bin = from_env_or_target("NOXI_REFERENCE_BIN", "nix-reference/bin", "nix-store");
		let mut command = Command::new(bin.join(program));
		command
			.args(args)
			.env_clear()
			.env("HOME", self.dir.path().join("home"))
			.env("NIX_CONF_DIR", self.dir.path().join("conf"))
			.env(
				"NIX_CONFIG",
				"experimental-features = nix-command\nbuild-users-group =\nsubstituters =\nsandbox = true\nsandbox-fallback = false\nsandbox-paths =\ncores = 1\n",
			)
			.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin");
		command
	}

	fn reference(&self, program: &str, args: &[&str]) -> Output {
		self.command(program, args).output().unwrap()
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

	fn reference_with_input(&self, program: &str, args: &[&str], input: &str) -> String {
		use std::io::Write;
		let mut child = self
			.command(program, args)
			.stdin(std::process::Stdio::piped())
			.stdout(std::process::Stdio::piped())
			.spawn()
			.unwrap();
		child
			.stdin
			.take()
			.unwrap()
			.write_all(input.as_bytes())
			.unwrap();
		let output = child.wait_with_output().unwrap();
		assert!(output.status.success());
		String::from_utf8(output.stdout).unwrap()
	}

	/// Copy the built path's closure into a new cache. Returns its URL.
	fn make_cache(&self, name: &str, params: &str, sign: bool) -> String {
		let dir = self.root(name);
		let mut url = format!("file://{}?{params}", dir.display());
		if sign {
			url += &format!("&secret-key={}", self.secret_key_file().display());
		}
		let src = self.root("src");
		self.reference_ok(
			"nix",
			&[
				"copy",
				"--store",
				src.to_str().unwrap(),
				"--to",
				&url,
				&self.out.to_absolute_path(),
			],
		);
		format!("file://{}", dir.display())
	}

	fn reference_substitute(&self, store: &str, cache: &str, path: &StorePath) -> Output {
		let root = self.root(store);
		self.reference(
			"nix-store",
			&[
				"--store",
				root.to_str().unwrap(),
				"--realise",
				&path.to_absolute_path(),
				"--option",
				"substituters",
				cache,
				"--option",
				"trusted-public-keys",
				&self.public_key,
			],
		)
	}

	fn noxi_store(&self, store: &str) -> LocalStore {
		let mut config = LocalStoreConfig::with_root(&self.root(store));
		config.trusted_public_keys = vec![self.public_key.clone()];
		LocalStore::open(config).unwrap()
	}

	fn path_info(&self, store: &str) -> Value {
		let root = self.root(store);
		let paths = [self.out.to_absolute_path(), self.busybox.to_absolute_path()];
		let mut json: Value = serde_json::from_str(&self.reference_ok(
			"nix",
			&[
				"path-info",
				"--json",
				"--store",
				root.to_str().unwrap(),
				&paths[0],
				&paths[1],
			],
		))
		.unwrap();
		for info in json.as_object_mut().unwrap().values_mut() {
			info.as_object_mut().unwrap().remove("registrationTime");
		}
		json
	}

	fn verify(&self, store: &str) {
		let root = self.root(store);
		let output = self.reference(
			"nix-store",
			&[
				"--store",
				root.to_str().unwrap(),
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

#[test]
fn signed_local_upload_matches_reference_and_round_trips() {
	use std::io::Read;

	let env = Env::new();
	let source = env.noxi_store("src");
	let original = source.query_path_info(&env.out).unwrap().unwrap();
	let (key, _) =
		parse_keypair(fs::read_to_string(env.secret_key_file()).unwrap().trim()).unwrap();
	let keys = [key];
	let url = format!("file://{}", env.root("rust-upload").display());
	let cache = BinaryCache::open(&url).unwrap();
	cache.upload_closure(&source, &env.out, &keys).unwrap();
	let reference_url = env.make_cache("reference-upload", "compression=none", true);
	let reference_cache = BinaryCache::open(&reference_url).unwrap();
	assert_eq!(
		fs::read_to_string(env.root("rust-upload/nix-cache-info")).unwrap(),
		"StoreDir: /nix/store\n"
	);
	for path in [&env.out, &env.busybox] {
		let text = cache.narinfo_text(path).unwrap().unwrap();
		assert_eq!(text, reference_cache.narinfo_text(path).unwrap().unwrap());
		let mut actual = Vec::new();
		cache
			.nar(&cache.query_path_info(path).unwrap().unwrap())
			.unwrap()
			.read_to_end(&mut actual)
			.unwrap();
		let mut expected = Vec::new();
		reference_cache
			.nar(&reference_cache.query_path_info(path).unwrap().unwrap())
			.unwrap()
			.read_to_end(&mut expected)
			.unwrap();
		assert_eq!(actual, expected);
		assert!(text.ends_with('\n'));
	}
	let first = cache.narinfo_text(&env.out).unwrap();
	cache.upload_closure(&source, &env.out, &keys).unwrap();
	assert_eq!(cache.narinfo_text(&env.out).unwrap(), first);
	assert_eq!(source.query_path_info(&env.out).unwrap().unwrap(), original);

	let output = env.reference_substitute("reference-from-upload", &url, &env.out);
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	env.noxi_store("rust-from-upload")
		.substitute(&[cache], &env.out)
		.unwrap();
	assert_eq!(
		env.path_info("rust-from-upload"),
		env.path_info("reference-from-upload")
	);
	env.verify("rust-from-upload");
}

#[test]
fn local_upload_preserves_signatures_across_key_rotation() {
	let mut env = Env::new();
	let inherited =
		BinaryCache::open(&env.make_cache("inherited", "compression=none", true)).unwrap();
	let source = env.noxi_store("src");
	assert!(
		source
			.query_path_info(&env.out)
			.unwrap()
			.unwrap()
			.ca
			.is_none()
	);
	for path in [&env.out, &env.busybox] {
		source
			.add_signatures(
				path,
				&inherited.query_path_info(path).unwrap().unwrap().info.sigs,
			)
			.unwrap();
	}
	let secret = env.reference_ok("nix", &["key", "generate-secret", "--key-name", "test"]);
	fs::write(env.secret_key_file(), &secret).unwrap();
	let (key, _) = parse_keypair(secret.trim()).unwrap();
	let cache =
		BinaryCache::open(&format!("file://{}", env.root("rotate-upload").display())).unwrap();
	cache.upload_closure(&source, &env.out, &[key]).unwrap();
	let reference =
		BinaryCache::open(&env.make_cache("reference-rotate", "compression=none", true)).unwrap();
	assert_eq!(
		cache.narinfo_text(&env.out).unwrap(),
		reference.narinfo_text(&env.out).unwrap()
	);

	let rotated = env.reference_ok("nix", &["key", "generate-secret", "--key-name", "rotate-1"]);
	let (key, _) = parse_keypair(rotated.trim()).unwrap();
	let keys = [key];
	cache.upload_closure(&source, &env.out, &keys).unwrap();
	let text = cache.narinfo_text(&env.out).unwrap().unwrap();
	let signatures = NarInfo::parse(&text).unwrap().signatures;
	assert_eq!(signatures.len(), 3);
	assert_eq!(
		signatures.iter().map(|sig| *sig.name()).collect::<Vec<_>>(),
		["rotate-1", "test", "test-1"]
	);
	cache.upload_closure(&source, &env.out, &keys).unwrap();
	assert_eq!(cache.narinfo_text(&env.out).unwrap().unwrap(), text);
	assert_eq!(
		source
			.query_path_info(&env.out)
			.unwrap()
			.unwrap()
			.sigs
			.len(),
		1
	);

	let wrong = env.reference_ok("nix", &["key", "generate-secret", "--key-name", "wrong-1"]);
	env.public_key = env
		.reference_with_input("nix", &["key", "convert-secret-to-public"], &wrong)
		.trim()
		.to_owned();
	let output = env.reference_substitute("wrong-key-reference", cache.uri(), &env.out);
	assert!(!output.status.success());
	assert!(String::from_utf8_lossy(&output.stderr).contains("not signed by any of the keys"));
	let destination = env.noxi_store("wrong-key-rust");
	assert!(destination.substitute(&[cache], &env.out).is_err());
	assert!(!destination.is_valid_path(&env.out).unwrap());
}

#[test]
fn local_cache_rejects_unsafe_nar_locations_like_reference() {
	let env = Env::new();
	let url = env.make_cache("unsafe-locations", "compression=none", true);
	let cache = BinaryCache::open(&url).unwrap();
	let dir = env.root("unsafe-locations");
	let name = format!(
		"{}.narinfo",
		nix_compat::nixbase32::encode(env.out.digest())
	);
	let original = cache.narinfo_text(&env.out).unwrap().unwrap();
	let original_url = NarInfo::parse(&original).unwrap().url.to_owned();
	fs::copy(dir.join(&original_url), env.root("outside.nar")).unwrap();
	for (index, location) in [
		"../outside.nar".to_owned(),
		format!("nar/./{}", original_url.strip_prefix("nar/").unwrap()),
		"nar/../../outside.nar".to_owned(),
		env.root("outside.nar").display().to_string(),
	]
	.iter()
	.enumerate()
	{
		let mut narinfo = NarInfo::parse(&original).unwrap();
		narinfo.url = location;
		fs::write(dir.join(&name), narinfo.to_string()).unwrap();
		let cached = cache.query_path_info(&env.out).unwrap().unwrap();
		let error = match cache.nar(&cached) {
			Ok(_) => panic!("accepted unsafe local cache location: {location}"),
			Err(error) => error.to_string(),
		};
		let output = env.reference_substitute(&format!("unsafe-reference-{index}"), &url, &env.out);
		assert!(!output.status.success());
		let stderr = String::from_utf8_lossy(&output.stderr);
		assert!(
			stderr.contains(&error),
			"noxi: {error}\nreference: {stderr}"
		);
	}
}

#[test]
fn local_upload_does_not_advertise_corrupt_source_or_leak_temporary_files() {
	use std::os::unix::fs::PermissionsExt;

	let env = Env::new();
	let source = env.noxi_store("src");
	let original = source.query_path_info(&env.out).unwrap().unwrap();
	let (key, _) =
		parse_keypair(fs::read_to_string(env.secret_key_file()).unwrap().trim()).unwrap();
	let keys = [key];
	for case in 0..4 {
		let mut bad = original.clone();
		match case {
			0 => bad.nar_hash[0] ^= 1,
			1 => bad.nar_size += 1,
			2 => bad.nar_size = 0,
			3 => {
				bad.sigs.insert("invalid-signature".into());
			}
			_ => unreachable!(),
		}
		source.register_valid_path(&bad).unwrap();
		let dir = env.root(&format!("bad-source-{case}"));
		let cache = BinaryCache::open(&format!("file://{}", dir.display())).unwrap();
		assert!(cache.upload_closure(&source, &env.out, &keys).is_err());
		assert!(cache.narinfo_text(&env.out).unwrap().is_none());
		assert!(cache.narinfo_text(&env.busybox).unwrap().is_some());
		assert!(fs::read_dir(dir.join("nar")).unwrap().all(|entry| {
			entry
				.unwrap()
				.path()
				.extension()
				.is_some_and(|ext| ext == "nar")
		}));
	}
	source.register_valid_path(&original).unwrap();
	let cache =
		BinaryCache::open(&format!("file://{}", env.root("preserve-good").display())).unwrap();
	cache.upload_closure(&source, &env.out, &keys).unwrap();
	let good = cache.narinfo_text(&env.out).unwrap();
	let contents = source.real_path(&env.out).join("ref");
	fs::set_permissions(&contents, fs::Permissions::from_mode(0o644)).unwrap();
	fs::write(&contents, b"corrupt source contents\n").unwrap();
	assert!(cache.upload_closure(&source, &env.out, &keys).is_err());
	assert_eq!(cache.narinfo_text(&env.out).unwrap(), good);
}

#[test]
fn local_upload_recovers_from_failed_publication_without_advertising_the_root() {
	let env = Env::new();
	let source = env.noxi_store("src");
	let (key, _) =
		parse_keypair(fs::read_to_string(env.secret_key_file()).unwrap().trim()).unwrap();
	let keys = [key];
	let info = source.query_path_info(&env.out).unwrap().unwrap();
	let dir = env.root("failed-publication");
	let blocker = dir.join(format!(
		"nar/{}.nar",
		nix_compat::nixbase32::encode(&info.nar_hash)
	));
	fs::create_dir_all(&blocker).unwrap();
	let cache = BinaryCache::open(&format!("file://{}", dir.display())).unwrap();
	assert!(cache.upload_closure(&source, &env.out, &keys).is_err());
	assert!(cache.narinfo_text(&env.out).unwrap().is_none());
	assert!(cache.narinfo_text(&env.busybox).unwrap().is_some());
	assert!(fs::read_dir(dir.join("nar")).unwrap().all(|entry| {
		entry
			.unwrap()
			.path()
			.extension()
			.is_some_and(|ext| ext == "nar")
	}));
	fs::remove_dir(blocker).unwrap();
	cache.upload_closure(&source, &env.out, &keys).unwrap();
	assert!(cache.narinfo_text(&env.out).unwrap().is_some());
	let output = env.reference_substitute("reference-after-retry", cache.uri(), &env.out);
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
}

#[test]
fn local_upload_roots_the_source_until_the_uploading_handle_is_dropped() {
	use noxi_store::gc::{GcAction, GcOptions};

	let env = Env::new();
	// Reference-created objects have no roots held by a Rust seeding handle.
	let source = env.noxi_store("src");
	let collector = env.noxi_store("src");
	let (key, _) =
		parse_keypair(fs::read_to_string(env.secret_key_file()).unwrap().trim()).unwrap();
	let cache = BinaryCache::open(&format!("file://{}", env.root("gc-upload").display())).unwrap();
	cache.upload_closure(&source, &env.out, &[key]).unwrap();
	let roots = collector.find_temp_roots(false, &mut |_| {}).unwrap();
	assert!(roots.contains_key(&env.out));
	assert!(roots.contains_key(&env.busybox));
	collector
		.collect_garbage(&GcOptions::new(GcAction::DeleteDead), &mut |_| {})
		.unwrap();
	assert!(collector.is_valid_path(&env.out).unwrap());
	assert!(collector.is_valid_path(&env.busybox).unwrap());
	drop(source);
	let deleted = collector
		.collect_garbage(&GcOptions::new(GcAction::DeleteDead), &mut |_| {})
		.unwrap();
	assert!(
		deleted.paths.contains(&env.out.to_absolute_path()),
		"{deleted:?}"
	);
}

#[test]
fn local_upload_rejects_unprotected_sources_and_http_destinations() {
	let env = Env::new();
	let source = env.noxi_store("src");
	let (key, _) =
		parse_keypair(fs::read_to_string(env.secret_key_file()).unwrap().trim()).unwrap();
	let keys = [key];
	let dir = env.root("rejected-upload");
	let cache = BinaryCache::open(&format!("file://{}", dir.display())).unwrap();
	assert!(cache.upload_closure(&source, &env.out, &keys[..0]).is_err());
	let mut config = LocalStoreConfig::with_root(&env.root("src"));
	config.read_only = true;
	let readonly = LocalStore::open(config).unwrap();
	assert!(cache.upload_closure(&readonly, &env.out, &keys).is_err());
	assert!(!dir.exists());
	let http = BinaryCache::open(&serve(env.root("nonexistent-http-cache"))).unwrap();
	let error = http
		.upload_closure(&source, &env.out, &keys)
		.unwrap_err()
		.to_string();
	assert!(error.contains("require a file:// cache"));
}

#[test]
fn narinfo_files_read_and_print_byte_for_byte() {
	let env = Env::new();
	for (name, params) in [("xz", ""), ("none", "compression=none")] {
		let cache = env.make_cache(name, params, true);
		let dir = PathBuf::from(cache.strip_prefix("file://").unwrap());
		let mut count = 0;
		for entry in fs::read_dir(&dir).unwrap() {
			let path = entry.unwrap().path();
			if path.extension().is_some_and(|e| e == "narinfo") {
				let text = fs::read_to_string(&path).unwrap();
				let narinfo = NarInfo::parse(&text).unwrap();
				assert_eq!(narinfo.to_string(), text, "{}", path.display());
				let name = path.file_name().unwrap().to_str().unwrap();
				let narinfo = cppnix::NarInfo::parse(text.as_bytes(), name).unwrap();
				assert_eq!(narinfo.to_string(), text, "{}", path.display());
				count += 1;
			}
		}
		assert_eq!(count, 2, "the built path and busybox");
	}
}

#[test]
fn signed_caches_substitute_like_the_reference() {
	let env = Env::new();
	for (name, params) in [
		("xz", ""),
		("none", "compression=none"),
		("bzip2", "compression=bzip2"),
		("zstd", "compression=zstd"),
		("gzip", "compression=gzip"),
		("br", "compression=br"),
	] {
		let cache = env.make_cache(name, params, true);
		let reference = format!("reference-{name}");
		let noxi = format!("noxi-{name}");
		let output = env.reference_substitute(&reference, &cache, &env.out);
		assert!(
			output.status.success(),
			"{name}: {}",
			String::from_utf8_lossy(&output.stderr)
		);
		env.noxi_store(&noxi)
			.substitute(&[BinaryCache::open(&cache).unwrap()], &env.out)
			.unwrap_or_else(|e| panic!("{name}: {e}"));
		let info = env.path_info(&noxi);
		assert_eq!(info, env.path_info(&reference), "{name}");
		let out = &info[env.out.to_absolute_path()];
		assert_eq!(out["signatures"].as_array().unwrap().len(), 1, "{info}");
		assert!(
			out["deriver"].as_str().unwrap().ends_with("-cached.drv"),
			"{info}"
		);
		assert_eq!(out["ultimate"], Value::Bool(false), "{info}");
		env.verify(&noxi);
	}
}

#[test]
fn unsigned_caches_are_refused_unless_trusted() {
	let env = Env::new();
	let cache = env.make_cache("unsigned", "", false);
	let output = env.reference_substitute("reference", &cache, &env.out);
	assert!(!output.status.success());
	let stderr = String::from_utf8_lossy(&output.stderr);
	let error = env
		.noxi_store("noxi")
		.substitute(&[BinaryCache::open(&cache).unwrap()], &env.out)
		.unwrap_err()
		.to_string();
	assert!(
		stderr.contains(&error),
		"noxi: {error}\nreference: {stderr}"
	);
	assert!(stderr.contains("as it's not signed by any of the keys in 'trusted-public-keys'"));

	let trusted = format!("{cache}?trusted=1");
	let output = env.reference_substitute("reference", &trusted, &env.out);
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	env.noxi_store("noxi")
		.substitute(&[BinaryCache::open(&trusted).unwrap()], &env.out)
		.unwrap();
	assert_eq!(env.path_info("noxi"), env.path_info("reference"));
	env.verify("noxi");
}

#[test]
fn corrupt_nars_are_rejected() {
	let env = Env::new();
	let cache = env.make_cache("corrupt", "compression=none", true);
	let dir = PathBuf::from(cache.strip_prefix("file://").unwrap());
	// Flip a byte inside the busybox NAR's file contents.
	let narinfo = fs::read_to_string(dir.join(format!(
		"{}.narinfo",
		nix_compat::nixbase32::encode(env.busybox.digest())
	)))
	.unwrap();
	let url = narinfo
		.lines()
		.find_map(|l| l.strip_prefix("URL: "))
		.unwrap();
	let nar = dir.join(url);
	let mut bytes = fs::read(&nar).unwrap();
	let middle = bytes.len() / 2;
	bytes[middle] ^= 0xff;
	fs::write(&nar, bytes).unwrap();

	let output = env.reference_substitute("reference", &cache, &env.out);
	assert!(!output.status.success());
	let stderr = String::from_utf8_lossy(&output.stderr);
	let error = env
		.noxi_store("noxi")
		.substitute(&[BinaryCache::open(&cache).unwrap()], &env.out)
		.unwrap_err()
		.to_string();
	// Both log the hash mismatch and then fail as having no substitute.
	assert!(stderr.contains("hash mismatch importing path"), "{stderr}");
	assert!(
		stderr.contains(&error),
		"noxi: {error}\nreference: {stderr}"
	);
}

/// Each change to the built path's narinfo, applied to the text the
/// reference wrote.
type Edit = fn(&str) -> String;

fn replace_line(text: &str, field: &str, line: &str) -> String {
	text.lines()
		.map(|l| {
			if l.starts_with(&format!("{field}:")) {
				line.to_owned()
			} else {
				l.to_owned()
			}
		})
		.map(|l| l + "\n")
		.collect()
}

fn drop_line(text: &str, field: &str) -> String {
	text.lines()
		.filter(|l| !l.starts_with(&format!("{field}:")))
		.map(|l| format!("{l}\n"))
		.collect()
}

const MALFORMED_NARINFO: &[(&str, Edit)] = &[
	("no colon", |_| "garbage".into()),
	("no final newline", |t| t.trim_end_matches('\n').into()),
	("a bad first line", |t| format!("X\n{t}")),
	("no StorePath", |t| drop_line(t, "StorePath")),
	("no NarHash", |t| drop_line(t, "NarHash")),
	("no URL", |t| drop_line(t, "URL")),
	("zero NarSize", |t| replace_line(t, "NarSize", "NarSize: 0")),
	("bad NarSize", |t| {
		replace_line(t, "NarSize", "NarSize: abc")
	}),
	("bad FileSize", |t| {
		replace_line(t, "FileSize", "FileSize: -1")
	}),
	("bad NarHash", |t| {
		replace_line(t, "NarHash", "NarHash: sha256:zz")
	}),
	("untyped NarHash", |t| {
		replace_line(t, "NarHash", "NarHash: abc")
	}),
	("unknown NarHash algorithm", |t| {
		replace_line(t, "NarHash", "NarHash: foo:abc")
	}),
	("BLAKE3 NarHash", |t| {
		replace_line(t, "NarHash", "NarHash: blake3:abc")
	}),
	("unknown Compression", |t| {
		replace_line(t, "Compression", "Compression: foo")
	}),
	("StorePath outside the store", |t| {
		replace_line(t, "StorePath", "StorePath: /foo/bar")
	}),
	("empty StorePath", |t| {
		replace_line(t, "StorePath", "StorePath: ")
	}),
	("short reference", |t| {
		replace_line(t, "References", "References: abc")
	}),
	("reference with a bad name", |t| {
		replace_line(
			t,
			"References",
			"References: xp0dpr9yz5h0fk5c5lpj7zzqjbdh3w8c-f!",
		)
	}),
	("reference with a bad hash", |t| {
		replace_line(
			t,
			"References",
			"References: eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-f",
		)
	}),
	("second References", |t| {
		let refs = t.lines().find(|l| l.starts_with("References:")).unwrap();
		format!("{t}{refs}\n")
	}),
	("bad Deriver", |t| replace_line(t, "Deriver", "Deriver: !x")),
	("signature without a key", |t| format!("{t}Sig: nokey\n")),
	("signature with bad base64", |t| format!("{t}Sig: k:!!!\n")),
	("empty signature", |t| format!("{t}Sig: k:\n")),
	("second CA", |t| {
		let ca = "CA: text:sha256:1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s";
		format!("{t}{ca}\n{ca}\n")
	}),
	("CA without a prefix", |t| format!("{t}CA: bogus\n")),
	("CA with an unknown prefix", |t| {
		format!("{t}CA: foo:sha256:abc\n")
	}),
	("CA without an algorithm", |t| {
		format!("{t}CA: fixed:r:sha256\n")
	}),
	("CA with an unknown algorithm", |t| {
		format!("{t}CA: fixed:r:foo:abc\n")
	}),
	("CA with a short hash", |t| {
		format!("{t}CA: fixed:r:sha256:zz\n")
	}),
	("CA with bad nix32", |t| {
		format!("{t}CA: fixed:r:sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee\n")
	}),
	("Git CA", |t| {
		format!("{t}CA: fixed:git:sha1:0000000000000000000000000000000000000000\n")
	}),
	// The value starts two bytes after the colon, so the NAR's URL loses
	// its first character and the download fails.
	("URL without a space", |t| t.replace("URL: ", "URL:")),
	// Read as bzip2, which the uncompressed NAR is not.
	("empty Compression", |t| {
		replace_line(t, "Compression", "Compression: ")
	}),
];

#[test]
fn malformed_narinfo_files_fail_like_the_reference() {
	let env = Env::new();
	let cache = env.make_cache("malformed", "compression=none", true);
	let dir = PathBuf::from(cache.strip_prefix("file://").unwrap());
	let file = dir.join(format!(
		"{}.narinfo",
		nix_compat::nixbase32::encode(env.out.digest())
	));
	let original = fs::read_to_string(&file).unwrap();
	for (index, (name, edit)) in MALFORMED_NARINFO.iter().enumerate() {
		fs::write(&file, edit(&original)).unwrap();
		let output = env.reference_substitute(&format!("reference-{index}"), &cache, &env.out);
		let stderr = String::from_utf8_lossy(&output.stderr);
		assert!(!output.status.success(), "{name}: the reference succeeded");
		let error = env
			.noxi_store(&format!("noxi-{index}"))
			.substitute(&[BinaryCache::open(&cache).unwrap()], &env.out)
			.expect_err(name)
			.to_string();
		assert!(
			stderr.contains(&format!("error: {error}\n")),
			"{name}\nnoxi: {error}\nreference: {stderr}"
		);
	}

	// A corrupt narinfo is logged, and the next cache is tried.
	fs::write(&file, "garbage").unwrap();
	let good = env.make_cache("well-formed", "compression=none", true);
	let both = format!("{cache} {good}");
	let output = env.reference_substitute("reference-fallthrough", &both, &env.out);
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(output.status.success(), "{stderr}");
	assert!(
		stderr.contains("is corrupt: expecting ':' at line 1"),
		"{stderr}"
	);
	env.noxi_store("noxi-fallthrough")
		.substitute(
			&[
				BinaryCache::open(&cache).unwrap(),
				BinaryCache::open(&good).unwrap(),
			],
			&env.out,
		)
		.unwrap();
	assert_eq!(
		env.path_info("noxi-fallthrough"),
		env.path_info("reference-fallthrough")
	);

	// Unknown fields are ignored, and the store path is made canonical.
	for (index, edit) in [(|t: &str| format!("{t}Foo: bar\n")) as Edit, |t| {
		t.replace("StorePath: /nix/store/", "StorePath: /nix/store/../store/")
	}]
	.iter()
	.enumerate()
	{
		fs::write(&file, edit(&original)).unwrap();
		let reference = format!("reference-ok-{index}");
		let output = env.reference_substitute(&reference, &cache, &env.out);
		assert!(
			output.status.success(),
			"{}",
			String::from_utf8_lossy(&output.stderr)
		);
		let noxi = format!("noxi-ok-{index}");
		env.noxi_store(&noxi)
			.substitute(&[BinaryCache::open(&cache).unwrap()], &env.out)
			.unwrap();
		assert_eq!(env.path_info(&noxi), env.path_info(&reference));
	}
}

/// Serve the files in `dir` over HTTP on a local port, answering 404 for
/// anything missing. Returns the base URL.
fn serve(dir: PathBuf) -> String {
	use std::io::{BufRead, BufReader, Write};
	let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
	let url = format!("http://{}", listener.local_addr().unwrap());
	std::thread::spawn(move || {
		for stream in listener.incoming() {
			let Ok(stream) = stream else { continue };
			let dir = dir.clone();
			std::thread::spawn(move || {
				let mut reader = BufReader::new(stream.try_clone().unwrap());
				let mut request = String::new();
				if reader.read_line(&mut request).is_err() {
					return;
				}
				loop {
					let mut line = String::new();
					if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
						break;
					}
				}
				let mut parts = request.split_whitespace();
				let method = parts.next().unwrap_or("");
				let path = parts.next().unwrap_or("/").trim_start_matches('/');
				let mut stream = stream;
				let file = (!path.contains(".."))
					.then(|| fs::read(dir.join(path)).ok())
					.flatten();
				let response = match file {
					Some(body) => {
						let mut head = format!(
							"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
							body.len()
						)
						.into_bytes();
						if method != "HEAD" {
							head.extend(body);
						}
						head
					}
					None => {
						b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
							.to_vec()
					}
				};
				let _ = stream.write_all(&response);
			});
		}
	});
	url
}

#[test]
fn http_caches_substitute_like_the_reference() {
	let env = Env::new();
	let cache = env.make_cache("http", "", true);
	let url = serve(PathBuf::from(cache.strip_prefix("file://").unwrap()));
	let output = env.reference_substitute("reference", &url, &env.out);
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	env.noxi_store("noxi")
		.substitute(&[BinaryCache::open(&url).unwrap()], &env.out)
		.unwrap();
	assert_eq!(env.path_info("noxi"), env.path_info("reference"));
	env.verify("noxi");

	// A path the cache does not have.
	let missing: StorePath = "00000000000000000000000000000000-missing".parse().unwrap();
	let output = env.reference_substitute("reference-missing", &url, &missing);
	let stderr = String::from_utf8_lossy(&output.stderr);
	let error = env
		.noxi_store("noxi-missing")
		.substitute(&[BinaryCache::open(&url).unwrap()], &missing)
		.unwrap_err()
		.to_string();
	assert!(
		stderr.contains(&error),
		"noxi: {error}\nreference: {stderr}"
	);
}

/// Substitute the test busybox from cache.nixos.org, checked with its real
/// signing key. Needs the network, so it only runs with `--run-ignored`.
#[test]
#[ignore = "downloads from cache.nixos.org"]
fn cache_nixos_org_substitutes_with_its_signing_key() {
	let busybox = fs::canonicalize(from_env_or_target(
		"NOXI_TEST_BUSYBOX",
		"test-busybox",
		"bin/busybox",
	))
	.unwrap();
	let busybox = StorePath::from_absolute_path(busybox.to_str().unwrap().as_bytes()).unwrap();
	let dir = tempfile::tempdir().unwrap();
	let mut config = LocalStoreConfig::with_root(dir.path());
	config.trusted_public_keys =
		vec!["cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=".to_owned()];
	let store = LocalStore::open(config).unwrap();
	store
		.substitute(
			&[BinaryCache::open("https://cache.nixos.org").unwrap()],
			&busybox,
		)
		.unwrap();
	let info = store.query_path_info(&busybox).unwrap().unwrap();
	assert!(
		info.sigs
			.iter()
			.any(|s| s.starts_with("cache.nixos.org-1:"))
	);
}
