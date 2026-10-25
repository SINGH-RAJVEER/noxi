//! Interrupted writes and concurrent clients of one local store, with the
//! pinned CppNix 2.35.2 reference as the other client.
//!
//! - NAR imports killed part way, by nix-rs and by the reference, leave an
//!   unregistered partial path that the other engine replaces.
//! - Truncated and corrupt NARs register nothing.
//! - nix-rs threads and processes and reference processes importing the
//!   same NAR and adding different paths at the same time all succeed, and
//!   the store verifies in the reference.
//! - Each engine waits for the other's lock on a path being written.
//!
//! nix-rs processes are this test binary run again with
//! `--ignored --exact child_process`; the variables `NIXRS_CHILD_*` say what
//! the child does. The reference executables come from `NIXRS_REFERENCE_BIN`
//! or `target/nix-reference/bin` (`just reference`).

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use nix_compat::store_path::StorePath;
use nix_rs_store::lock::PathLocks;
use nix_rs_store::{LocalStore, LocalStoreConfig, ValidPathInfo};
use sha2::{Digest, Sha256};

fn reference_bin() -> PathBuf {
	if let Some(dir) = std::env::var_os("NIXRS_REFERENCE_BIN") {
		return PathBuf::from(dir);
	}
	let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin");
	assert!(
		dir.join("nix-store").exists(),
		"the CppNix reference is missing; run `just reference` or set NIXRS_REFERENCE_BIN"
	);
	dir
}

fn open(root: &Path) -> LocalStore {
	let mut config = LocalStoreConfig::with_root(root);
	config.require_sigs = false;
	LocalStore::open(config).unwrap()
}

/// What a NAR import needs to know about the path.
#[derive(Clone)]
struct Nar {
	path: StorePath,
	bytes: Vec<u8>,
}

impl Nar {
	fn info(&self) -> ValidPathInfo {
		let mut info = ValidPathInfo::new(self.path.clone(), Sha256::digest(&self.bytes).into());
		info.nar_size = self.bytes.len() as u64;
		info
	}
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

	fn store(&self, name: &str) -> String {
		self.root(name).to_str().unwrap().to_owned()
	}

	fn reference_command(&self, program: &str, args: &[&str]) -> Command {
		let mut command = Command::new(reference_bin().join(program));
		command
			.args(args)
			.env_clear()
			.env("HOME", self.dir.path().join("home"))
			.env("NIX_CONF_DIR", self.dir.path().join("conf"))
			.env(
				"NIX_CONFIG",
				"experimental-features = nix-command\nbuild-users-group =\nsandbox = false\nsubstituters =\nrequire-sigs = false\n",
			)
			.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin");
		command
	}

	fn reference(&self, program: &str, args: &[&str]) -> Output {
		self.reference_command(program, args).output().unwrap()
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

	/// A fresh store made by the reference.
	fn init(&self, name: &str) {
		self.reference_ok("nix", &["store", "info", "--store", &self.store(name)]);
	}

	/// A path of a few megabytes in a source store, with its NAR and its
	/// `nix-store --export` serialisation.
	fn big_path(&self, label: &str) -> (Nar, Vec<u8>) {
		let src = self.dir.path().join(label);
		fs::create_dir(&src).unwrap();
		// Runtime roots are global even for diverted stores. Give each test
		// a distinct path so another test's importing process cannot root it.
		fs::write(
			src.join("test-id"),
			self.dir.path().as_os_str().as_encoded_bytes(),
		)
		.unwrap();
		for i in 0..8u8 {
			let block: Vec<u8> = (0..512 * 1024u32)
				.map(|j| (j.wrapping_mul(31) as u8) ^ i)
				.collect();
			fs::write(src.join(format!("part-{i}")), block).unwrap();
		}
		let source = self.store("source");
		let path = self.reference_ok(
			"nix-store",
			&["--store", &source, "--add", src.to_str().unwrap()],
		);
		let path = path.trim().to_owned();
		let bytes = self
			.reference_command("nix", &["store", "dump-path", "--store", &source, &path])
			.output()
			.unwrap()
			.stdout;
		let export = self
			.reference_command("nix-store", &["--store", &source, "--export", &path])
			.output()
			.unwrap()
			.stdout;
		let nar = Nar {
			path: StorePath::from_absolute_path(path.as_bytes()).unwrap(),
			bytes,
		};
		(nar, export)
	}

	fn real(&self, store: &str, path: &StorePath) -> PathBuf {
		self.root(store).join("nix/store").join(path.to_string())
	}

	fn is_valid(&self, store: &str, path: &StorePath) -> bool {
		self.reference(
			"nix-store",
			&[
				"--store",
				&self.store(store),
				"--check-validity",
				&path.to_absolute_path(),
			],
		)
		.status
		.success()
	}

	fn verify(&self, store: &str) {
		let output = self.reference(
			"nix-store",
			&[
				"--store",
				&self.store(store),
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

	/// `nix-store --import` from the reference, with the export on stdin.
	fn reference_import(&self, store: &str) -> Child {
		self.reference_command("nix-store", &["--store", &self.store(store), "--import"])
			.stdin(Stdio::piped())
			.stdout(Stdio::null())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap()
	}

	/// A nix-rs process importing `nar` into `store` from its stdin.
	fn nix_rs_import(&self, store: &str, nar: &Nar) -> Child {
		Command::new(std::env::current_exe().unwrap())
			.args(["--ignored", "--exact", "child_process", "--nocapture"])
			.env("NIXRS_CHILD_STORE", self.root(store))
			.env("NIXRS_CHILD_PATH", nar.path.to_absolute_path())
			.env("NIXRS_CHILD_NAR_SIZE", nar.bytes.len().to_string())
			.env(
				"NIXRS_CHILD_NAR_SHA256",
				data_encoding::HEXLOWER.encode(&Sha256::digest(&nar.bytes)),
			)
			.stdin(Stdio::piped())
			.stdout(Stdio::null())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap()
	}
}

/// What a stalled NAR download does next.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gate {
	Wait,
	Release,
	Abort,
}

/// An uncompressed HTTP binary cache holding one path, whose NAR download
/// stops half way until it is released or aborted. The reference streams
/// substituted NARs into the store, so it is caught part way through a
/// write.
struct StallingCache {
	url: String,
	gate: std::sync::Arc<(std::sync::Mutex<Gate>, std::sync::Condvar)>,
}

impl StallingCache {
	fn new(nar: &Nar) -> Self {
		use std::io::BufRead;
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		let gate =
			std::sync::Arc::new((std::sync::Mutex::new(Gate::Wait), std::sync::Condvar::new()));
		let hash = nix_compat::nixbase32::encode(nar.path.digest());
		let narinfo = format!(
			"StorePath: {}\nURL: nar/{hash}.nar\nCompression: none\nNarHash: sha256:{}\nNarSize: {}\nReferences: \n",
			nar.path.to_absolute_path(),
			nix_compat::nixbase32::encode(&Sha256::digest(&nar.bytes)),
			nar.bytes.len()
		);
		let files = std::sync::Arc::new(std::collections::BTreeMap::from([
			(
				"nix-cache-info".to_owned(),
				b"StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 30\n".to_vec(),
			),
			(format!("{hash}.narinfo"), narinfo.into_bytes()),
			(format!("nar/{hash}.nar"), nar.bytes.clone()),
		]));
		let server_gate = gate.clone();
		std::thread::spawn(move || {
			for stream in listener.incoming() {
				let Ok(mut stream) = stream else { continue };
				let files = files.clone();
				let gate = server_gate.clone();
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
					let name = parts.next().unwrap_or("/").trim_start_matches('/');
					let Some(body) = files.get(name) else {
						let _ = stream.write_all(
							b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
						);
						return;
					};
					let head = format!(
						"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
						body.len()
					);
					if stream.write_all(head.as_bytes()).is_err() || method == "HEAD" {
						return;
					}
					if !name.starts_with("nar/") {
						let _ = stream.write_all(body);
						return;
					}
					let half = body.len() / 2;
					if stream.write_all(&body[..half]).is_err() {
						return;
					}
					let _ = stream.flush();
					let (lock, cond) = &*gate;
					let mut state = lock.lock().unwrap();
					while *state == Gate::Wait {
						state = cond.wait(state).unwrap();
					}
					if *state == Gate::Release {
						let _ = stream.write_all(&body[half..]);
					}
				});
			}
		});
		StallingCache { url, gate }
	}

	fn set(&self, gate: Gate) {
		let (lock, cond) = &*self.gate;
		*lock.lock().unwrap() = gate;
		cond.notify_all();
	}

	/// The reference substituting the path into `store` from this cache.
	fn reference_copy(&self, env: &Env, store: &str, nar: &Nar) -> Child {
		env.reference_command(
			"nix",
			&[
				"copy",
				"--no-check-sigs",
				"--from",
				&self.url,
				"--to",
				&format!("local?root={}", env.store(store)),
				&nar.path.to_absolute_path(),
			],
		)
		.stdout(Stdio::null())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap()
	}
}

/// Wait until `cond` holds, failing after a minute.
fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
	let start = Instant::now();
	while !cond() {
		assert!(
			start.elapsed() < Duration::from_secs(60),
			"timed out waiting for {what}"
		);
		std::thread::sleep(Duration::from_millis(10));
	}
}

/// Number of bytes below `dir`, following no symlinks.
fn tree_size(dir: &Path) -> u64 {
	let Ok(metadata) = fs::symlink_metadata(dir) else {
		return 0;
	};
	if metadata.is_dir() {
		fs::read_dir(dir)
			.map(|entries| entries.map(|e| tree_size(&e.unwrap().path())).sum())
			.unwrap_or(0)
	} else {
		metadata.len()
	}
}

/// Feed the first half of `input` to `child`, wait until its write to
/// `real` has begun, and kill it.
fn kill_part_way(mut child: Child, input: &[u8], real: &Path) {
	let mut stdin = child.stdin.take().unwrap();
	stdin.write_all(&input[..input.len() / 2]).unwrap();
	stdin.flush().unwrap();
	wait_for("a partial write", || tree_size(real) > 0);
	child.kill().unwrap();
	child.wait().unwrap();
	drop(stdin);
}

/// The entry point of nix-rs child processes.
#[test]
#[ignore = "run as a child process by the other tests"]
fn child_process() {
	let root = PathBuf::from(std::env::var_os("NIXRS_CHILD_STORE").unwrap());
	let path = std::env::var("NIXRS_CHILD_PATH").unwrap();
	let size: u64 = std::env::var("NIXRS_CHILD_NAR_SIZE")
		.unwrap()
		.parse()
		.unwrap();
	let digest = data_encoding::HEXLOWER
		.decode(std::env::var("NIXRS_CHILD_NAR_SHA256").unwrap().as_bytes())
		.unwrap();
	let mut info = ValidPathInfo::new(
		StorePath::from_absolute_path(path.as_bytes()).unwrap(),
		digest.try_into().unwrap(),
	);
	info.nar_size = size;
	let store = open(&root);
	if std::env::var_os("NIXRS_CHILD_REGISTER").is_some() {
		let drv = StorePath::from_bytes(b"00000000000000000000000000000000-blocked.drv").unwrap();
		store
			.register_valid_paths(&[info, ValidPathInfo::new(drv, [0; 32])])
			.unwrap();
		return;
	}
	let mut stdin = BufReader::new(std::io::stdin());
	store.add_to_store(&info, &mut stdin, false, false).unwrap();
}

#[test]
fn killed_imports_are_replaced_by_either_engine() {
	let env = Env::new();
	let (nar, export) = env.big_path("big");
	let expected = env.reference_ok(
		"nix",
		&[
			"path-info",
			"--json",
			"--store",
			&env.store("source"),
			&nar.path.to_absolute_path(),
		],
	);
	let path_info = |store: &str| -> serde_json::Value {
		let json = env.reference_ok(
			"nix",
			&[
				"path-info",
				"--json",
				"--store",
				&env.store(store),
				&nar.path.to_absolute_path(),
			],
		);
		let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
		for info in value.as_object_mut().unwrap().values_mut() {
			let info = info.as_object_mut().unwrap();
			info.remove("registrationTime");
			info.remove("ultimate");
			// Neither an export nor a narinfo without `CA:` carries the content address.
			info.remove("ca");
		}
		value
	};
	let mut expected: serde_json::Value = serde_json::from_str(&expected).unwrap();
	for info in expected.as_object_mut().unwrap().values_mut() {
		let info = info.as_object_mut().unwrap();
		info.remove("registrationTime");
		info.remove("ultimate");
		info.remove("ca");
	}

	// A killed nix-rs import, finished by the reference.
	env.init("a");
	let real = env.real("a", &nar.path);
	kill_part_way(env.nix_rs_import("a", &nar), &nar.bytes, &real);
	assert!(!env.is_valid("a", &nar.path));
	assert!(tree_size(&real) < tree_size(&env.root("big")));
	let mut child = env.reference_import("a");
	child.stdin.take().unwrap().write_all(&export).unwrap();
	let output = child.wait_with_output().unwrap();
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert_eq!(path_info("a"), expected);
	env.verify("a");

	// A killed reference import, finished by nix-rs in another process.
	env.init("b");
	let real = env.real("b", &nar.path);
	let cache = StallingCache::new(&nar);
	let mut child = cache.reference_copy(&env, "b", &nar);
	wait_for("a partial write", || tree_size(&real) > 0);
	child.kill().unwrap();
	child.wait().unwrap();
	cache.set(Gate::Abort);
	assert!(!env.is_valid("b", &nar.path));
	let mut child = env.nix_rs_import("b", &nar);
	child.stdin.take().unwrap().write_all(&nar.bytes).unwrap();
	let output = child.wait_with_output().unwrap();
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert_eq!(path_info("b"), expected);
	env.verify("b");

	// A killed nix-rs import, finished by nix-rs; the dead process's lock
	// file and temporary roots are left behind and must not get in the way
	// of either engine's garbage collector.
	env.init("c");
	let real = env.real("c", &nar.path);
	kill_part_way(env.nix_rs_import("c", &nar), &nar.bytes, &real);
	assert!(!env.is_valid("c", &nar.path));
	let store = open(&env.root("c"));
	store
		.add_to_store(&nar.info(), &mut &nar.bytes[..], false, false)
		.unwrap();
	drop(store);
	assert_eq!(path_info("c"), expected);
	env.verify("c");
	env.reference_ok("nix-store", &["--store", &env.store("c"), "--gc"]);
	assert!(!env.is_valid("c", &nar.path));
	assert!(!real.exists());
	env.verify("c");
}

#[test]
fn killed_registration_rolls_back_the_whole_batch() {
	let env = Env::new();
	let (nar, export) = env.big_path("registration");
	env.init("store");
	let real = env.real("store", &nar.path);
	nix_rs_store::nar::restore_path(&real, &mut &nar.bytes[..]).unwrap();
	let fifo = env.root("store/nix/store/00000000000000000000000000000000-blocked.drv");
	assert!(
		Command::new("mkfifo")
			.arg(&fifo)
			.status()
			.unwrap()
			.success()
	);
	// The second row's derivation check blocks reading this FIFO after the
	// first row has been inserted. Keep a writer open without sending data.
	let writer = fs::OpenOptions::new()
		.read(true)
		.write(true)
		.open(&fifo)
		.unwrap();
	let mut child = Command::new(std::env::current_exe().unwrap())
		.args(["--ignored", "--exact", "child_process", "--nocapture"])
		.env("NIXRS_CHILD_STORE", env.root("store"))
		.env("NIXRS_CHILD_PATH", nar.path.to_absolute_path())
		.env("NIXRS_CHILD_NAR_SIZE", nar.bytes.len().to_string())
		.env(
			"NIXRS_CHILD_NAR_SHA256",
			data_encoding::HEXLOWER.encode(&Sha256::digest(&nar.bytes)),
		)
		.env("NIXRS_CHILD_REGISTER", "1")
		.stdout(Stdio::null())
		.spawn()
		.unwrap();
	let fds = PathBuf::from(format!("/proc/{}/fd", child.id()));
	wait_for("registration's second row", || {
		fs::read_dir(&fds)
			.unwrap()
			.any(|entry| fs::read_link(entry.unwrap().path()).ok().as_ref() == Some(&fifo))
	});
	assert!(!env.is_valid("store", &nar.path));
	child.kill().unwrap();
	child.wait().unwrap();
	drop(writer);
	fs::remove_file(fifo).unwrap();
	assert!(
		open(&env.root("store"))
			.query_all_valid_paths()
			.unwrap()
			.is_empty()
	);
	let mut child = env.reference_import("store");
	child.stdin.take().unwrap().write_all(&export).unwrap();
	assert!(child.wait_with_output().unwrap().status.success());
	assert!(env.is_valid("store", &nar.path));
	env.verify("store");
}

#[test]
fn truncated_and_corrupt_nars_register_nothing() {
	let env = Env::new();
	let (nar, export) = env.big_path("big");
	env.init("store");
	let store = open(&env.root("store"));
	let info = nar.info();

	let truncated = &nar.bytes[..nar.bytes.len() / 3];
	assert!(
		store
			.add_to_store(&info, &mut &truncated[..], false, false)
			.is_err()
	);
	assert!(!store.is_valid_path(&nar.path).unwrap());
	let mut corrupt = nar.bytes.clone();
	// Inside the contents of the second file, not the NAR structure.
	let at = corrupt.len() * 3 / 16;
	corrupt[at] ^= 1;
	let error = store
		.add_to_store(&info, &mut &corrupt[..], false, false)
		.unwrap_err()
		.to_string();
	assert!(error.starts_with("hash mismatch importing path"), "{error}");
	assert!(!store.is_valid_path(&nar.path).unwrap());
	drop(store);
	env.verify("store");

	// The reference fails the same way on a truncated export and the store
	// still takes the whole path afterwards, from either engine.
	let mut child = env.reference_import("store");
	child
		.stdin
		.take()
		.unwrap()
		.write_all(&export[..export.len() / 3])
		.unwrap();
	assert!(!child.wait_with_output().unwrap().status.success());
	assert!(!env.is_valid("store", &nar.path));
	let store = open(&env.root("store"));
	store
		.add_to_store(&info, &mut &nar.bytes[..], false, false)
		.unwrap();
	drop(store);
	env.verify("store");
}

#[test]
fn concurrent_clients_register_shared_and_distinct_paths() {
	let env = Env::new();
	let (nar, export) = env.big_path("big");
	env.init("store");
	let root = env.root("store");
	let rounds = 4;

	let files: Vec<PathBuf> = (0..rounds)
		.map(|i| {
			let file = env.dir.path().join(format!("reference-{i}"));
			fs::write(&file, format!("reference {i}\n")).unwrap();
			file
		})
		.collect();
	let mut reference_imports = Vec::new();
	let mut reference_adds = Vec::new();
	let mut nix_rs_processes = Vec::new();
	for file in &files {
		reference_imports.push(env.reference_import("store"));
		reference_adds.push(
			env.reference_command(
				"nix-store",
				&[
					"--store",
					&env.store("store"),
					"--add",
					file.to_str().unwrap(),
				],
			)
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap(),
		);
		nix_rs_processes.push(env.nix_rs_import("store", &nar));
	}
	let texts: Vec<StorePath> = std::thread::scope(|scope| {
		let threads: Vec<_> = (0..rounds)
			.map(|i| {
				let root = root.clone();
				let nar = nar.clone();
				scope.spawn(move || {
					let store = open(&root);
					store
						.add_to_store(&nar.info(), &mut &nar.bytes[..], false, false)
						.unwrap();
					store
						.add_text_to_store(
							&format!("text-{i}"),
							nar.path.to_absolute_path().as_bytes(),
							&BTreeSet::from([nar.path.clone()]),
							false,
						)
						.unwrap()
				})
			})
			.collect();
		// Feed every process concurrently. One may hold the path lock while
		// waiting for stdin; feeding another first can deadlock the test.
		for child in reference_imports.iter_mut() {
			let mut stdin = child.stdin.take().unwrap();
			let export = &export;
			scope.spawn(move || stdin.write_all(export).unwrap());
		}
		for child in nix_rs_processes.iter_mut() {
			let mut stdin = child.stdin.take().unwrap();
			let bytes = &nar.bytes;
			scope.spawn(move || stdin.write_all(bytes).unwrap());
		}
		threads.into_iter().map(|t| t.join().unwrap()).collect()
	});
	for child in reference_imports.into_iter().chain(nix_rs_processes) {
		let output = child.wait_with_output().unwrap();
		assert!(
			output.status.success(),
			"{}",
			String::from_utf8_lossy(&output.stderr)
		);
	}
	let mut added = BTreeSet::new();
	for child in reference_adds {
		let output = child.wait_with_output().unwrap();
		assert!(output.status.success());
		added.insert(String::from_utf8(output.stdout).unwrap().trim().to_owned());
	}

	let all: BTreeSet<String> = env
		.reference_ok(
			"nix",
			&["path-info", "--all", "--store", &env.store("store")],
		)
		.lines()
		.map(str::to_owned)
		.collect();
	let mut expected: BTreeSet<String> = added;
	expected.insert(nar.path.to_absolute_path());
	expected.extend(texts.iter().map(|p| p.to_absolute_path()));
	assert_eq!(all, expected);
	for text in &texts {
		assert_eq!(
			env.reference_ok(
				"nix-store",
				&[
					"--store",
					&env.store("store"),
					"--query",
					"--references",
					&text.to_absolute_path()
				],
			),
			format!("{}\n", nar.path.to_absolute_path())
		);
	}
	env.verify("store");
}

#[test]
fn path_locks_exclude_the_other_engine() {
	let env = Env::new();
	let (nar, _) = env.big_path("big");
	env.init("store");

	// The reference waits while nix-rs holds the lock on a path.
	let file = env.dir.path().join("locked");
	fs::write(&file, "locked\n").unwrap();
	let expected = env
		.reference_ok(
			"nix-store",
			&[
				"--store",
				&env.store("source"),
				"--add",
				file.to_str().unwrap(),
			],
		)
		.trim()
		.to_owned();
	let locked = StorePath::from_absolute_path(expected.as_bytes()).unwrap();
	let locks = PathLocks::lock([env.real("store", &locked)]).unwrap();
	let mut child = env
		.reference_command(
			"nix-store",
			&[
				"--store",
				&env.store("store"),
				"--add",
				file.to_str().unwrap(),
			],
		)
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	std::thread::sleep(Duration::from_secs(1));
	assert!(
		child.try_wait().unwrap().is_none(),
		"the reference did not wait"
	);
	assert!(!env.is_valid("store", &locked));
	drop(locks);
	let output = child.wait_with_output().unwrap();
	assert!(output.status.success());
	assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), expected);

	// nix-rs waits while the reference holds the lock during a streamed
	// substitution, then finds the path valid and consumes its NAR.
	let cache = StallingCache::new(&nar);
	let mut child = cache.reference_copy(&env, "store", &nar);
	let real = env.real("store", &nar.path);
	wait_for("the reference to start writing", || tree_size(&real) > 0);
	let root = env.root("store");
	let (done_tx, done_rx) = std::sync::mpsc::channel();
	let thread = {
		let nar = nar.clone();
		std::thread::spawn(move || {
			let store = open(&root);
			let result = store.add_to_store(&nar.info(), &mut &nar.bytes[..], false, false);
			done_tx.send(()).unwrap();
			result.map_err(|e| e.to_string())
		})
	};
	assert!(
		done_rx.recv_timeout(Duration::from_secs(1)).is_err(),
		"nix-rs did not wait for the reference's lock"
	);
	cache.set(Gate::Release);
	let mut stderr = String::new();
	child
		.stderr
		.take()
		.unwrap()
		.read_to_string(&mut stderr)
		.unwrap();
	assert!(child.wait().unwrap().success(), "{stderr}");
	thread.join().unwrap().unwrap();
	env.verify("store");
}
