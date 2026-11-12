//! Store, archive, key and dependency commands compared with CppNix 2.35.2.
//!
//! Each engine gets its own working directory and `local?root=` store,
//! seeded identically by the reference, so commands that change the store
//! (signing, signature import, log copies, content-addressed rewrites) can
//! be compared on their output and on the state they leave. The reference
//! comes from `NOXI_REFERENCE_BIN`, else `target/nix-reference/bin`; when
//! neither exists the tests print a notice and return.
use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn reference_bin() -> Option<PathBuf> {
	let dir = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		});
	let nix = dir.join("nix");
	if nix.exists() {
		Some(nix)
	} else {
		eprintln!("skipping: no reference nix at {}", nix.display());
		None
	}
}

struct Pair {
	reference: PathBuf,
	dirs: [tempfile::TempDir; 2],
	features: &'static str,
}

impl Pair {
	fn new() -> Option<Self> {
		Some(Self {
			reference: reference_bin()?,
			dirs: [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()],
			features: "nix-command",
		})
	}

	fn dir(&self, engine: usize) -> &Path {
		self.dirs[engine].path()
	}

	fn command(&self, engine: usize, binary: bool) -> Command {
		let program = if binary {
			self.reference.clone()
		} else {
			PathBuf::from(env!("CARGO_BIN_EXE_nix"))
		};
		let dir = self.dir(engine);
		let mut command = Command::new(program);
		command
			.arg0("nix")
			.current_dir(dir)
			.env_clear()
			.env("HOME", dir)
			.env("NIX_CONF_DIR", dir.join("etc"))
			.env("NIX_USER_CONF_FILES", "")
			.env(
				"NIX_CONFIG",
				format!(
					"experimental-features = {}\nbuild-users-group =\nsubstituters =\n",
					self.features
				),
			)
			.args([
				"--store",
				&format!("local?root={}", dir.join("root").display()),
			])
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped());
		command
	}

	/// Run the reference (`reference`) or noxi in `engine`'s directory.
	fn run_in(&self, engine: usize, reference: bool, args: &[&str], input: &[u8]) -> Output {
		let mut child = self.command(engine, reference).args(args).spawn().unwrap();
		let mut stdin = child.stdin.take().unwrap();
		let _ = stdin.write_all(input);
		drop(stdin);
		child.wait_with_output().unwrap()
	}

	/// Run the reference in both directories, expecting the same output.
	fn seed(&self, args: &[&str]) -> String {
		let a = self.run_in(0, true, args, b"");
		let b = self.run_in(1, true, args, b"");
		assert!(
			a.status.success(),
			"seed {args:?}: {}",
			String::from_utf8_lossy(&a.stderr)
		);
		assert_eq!(a.stdout, b.stdout, "seed {args:?}");
		String::from_utf8(a.stdout).unwrap().trim().to_owned()
	}

	/// Write the same file into both directories.
	fn file(&self, name: &str, contents: &[u8]) {
		for engine in 0..2 {
			let path = self.dir(engine).join(name);
			fs::create_dir_all(path.parent().unwrap()).unwrap();
			fs::write(path, contents).unwrap();
		}
	}

	/// The reference in directory 0 and noxi in directory 1 must agree.
	fn same_input(&self, args: &[&str], input: &[u8]) -> Output {
		let expected = self.run_in(0, true, args, input);
		let actual = self.run_in(1, false, args, input);
		let norm = |engine: usize, bytes: &[u8]| {
			String::from_utf8_lossy(bytes).replace(&self.dir(engine).display().to_string(), "DIR")
		};
		let show = |o: &Output| String::from_utf8_lossy(&o.stderr).into_owned();
		assert_eq!(
			actual.status.code(),
			expected.status.code(),
			"status {args:?}\nreference: {}\nnoxi: {}",
			show(&expected),
			show(&actual)
		);
		assert_eq!(
			norm(1, &actual.stdout),
			norm(0, &expected.stdout),
			"stdout {args:?}"
		);
		assert_eq!(
			norm(1, &actual.stderr),
			norm(0, &expected.stderr),
			"stderr {args:?}"
		);
		expected
	}

	fn same(&self, args: &[&str]) -> Output {
		self.same_input(args, b"")
	}

	/// Whether each engine created its store root.
	fn roots_exist(&self) -> [bool; 2] {
		[0, 1].map(|e| self.dir(e).join("root").exists())
	}

	/// The reference's view of path metadata in both stores, which leaves
	/// out registration times.
	fn same_state(&self, args: &[&str]) {
		let a = self.run_in(0, true, args, b"");
		let b = self.run_in(1, true, args, b"");
		let strip = |bytes: &[u8]| {
			let text = String::from_utf8_lossy(bytes).into_owned();
			let mut out = String::new();
			let mut rest = text.as_str();
			while let Some(at) = rest.find("\"registrationTime\":") {
				out.push_str(&rest[..at]);
				rest = rest[at..].trim_start_matches("\"registrationTime\":");
				rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
			}
			out.push_str(rest);
			out
		};
		assert_eq!(strip(&b.stdout), strip(&a.stdout), "state {args:?}");
		assert_eq!(b.status.code(), a.status.code(), "state {args:?}");
	}
}

/// A store graph without builds: `toFile` paths that refer to each other.
const GRAPH: &str = r#"let
	dep = builtins.toFile "dep" "dep";
	b = builtins.toFile "b" "b ${dep}";
	c = builtins.toFile "c" "c ${dep}";
	a = builtins.toFile "a" "a ${b} ${c}";
	chain = builtins.toFile "chain" "top ${b}";
in { inherit dep b c a chain; }"#;

fn graph_path(pair: &Pair, attr: &str) -> String {
	pair.seed(&[
		"eval",
		"--raw",
		"--impure",
		"--expr",
		&format!("({GRAPH}).{attr}"),
	])
}

#[test]
fn key_generation_conversion_and_errors() {
	let Some(pair) = Pair::new() else { return };
	let generated = pair.run_in(0, true, &["key", "generate-secret", "--key-name", "k"], b"");
	assert!(generated.status.success());
	pair.same_input(&["key", "convert-secret-to-public"], &generated.stdout);
	let mut with_newline = generated.stdout.clone();
	with_newline.push(b'\n');
	pair.same_input(&["key", "convert-secret-to-public"], &with_newline);
	for input in [b"".as_slice(), b"foo:!!!", b"foo:YWJj", b":abc", b"noColon"] {
		pair.same_input(&["key", "convert-secret-to-public"], input);
	}
	// libsodium copies the public half of the secret key.
	let text = String::from_utf8(generated.stdout).unwrap();
	let (name, data) = text.split_once(':').unwrap();
	let mut bytes = data_decode(data);
	bytes[40] ^= 0xff;
	pair.same_input(
		&["key", "convert-secret-to-public"],
		format!("{name}:{}", data_encode(&bytes)).as_bytes(),
	);
	pair.same(&["key", "generate-secret"]);
	// noxi's keys are accepted by the reference, and the other way round.
	let ours = pair.run_in(
		1,
		false,
		&["key", "generate-secret", "--key-name", "ours"],
		b"",
	);
	assert!(ours.status.success());
	let ours_text = String::from_utf8(ours.stdout.clone()).unwrap();
	assert!(ours_text.starts_with("ours:") && !ours_text.ends_with('\n'));
	assert_eq!(data_decode(&ours_text[5..]).len(), 64);
	pair.same_input(&["key", "convert-secret-to-public"], &ours.stdout);
	assert_eq!(pair.roots_exist(), [false, false]);
}

fn data_decode(s: &str) -> Vec<u8> {
	let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
	let mut out = Vec::new();
	let (mut data, mut bits) = (0u32, 0);
	for c in s.bytes().take_while(|&c| c != b'=') {
		let v = table.iter().position(|&t| t == c).unwrap() as u32;
		data = (data << 6) | v;
		bits += 6;
		if bits >= 8 {
			out.push((data >> (bits - 8)) as u8);
			bits -= 8;
		}
	}
	out
}

fn data_encode(bytes: &[u8]) -> String {
	let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
	let mut out = String::new();
	for chunk in bytes.chunks(3) {
		let n = chunk
			.iter()
			.enumerate()
			.fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
		for i in 0..4 {
			if i <= chunk.len() {
				out.push(table[(n >> (18 - 6 * i) & 63) as usize] as char);
			} else {
				out.push('=');
			}
		}
	}
	out
}

#[test]
fn nar_listing_cat_and_pack() {
	let Some(pair) = Pair::new() else { return };
	pair.file("tree/a", b"a\0b\n");
	pair.file("tree/sub/b", b"nested\n");
	pair.file("tree/run", b"#!/bin/sh\n");
	pair.file("plain", b"hello\n");
	pair.file("garbage", b"0123456789abcdefghij");
	for engine in 0..2 {
		let dir = pair.dir(engine);
		use std::os::unix::fs::PermissionsExt;
		fs::set_permissions(dir.join("tree/run"), fs::Permissions::from_mode(0o755)).unwrap();
		std::os::unix::fs::symlink("a", dir.join("tree/link")).unwrap();
	}
	let packed = pair.same(&["nar", "pack", "tree"]);
	pair.same(&["nar", "dump-path", "tree"]);
	pair.same(&["nar", "pack", "missing"]);
	pair.same(&["nar", "pack", "/dev/null"]);
	pair.file("t.nar", &packed.stdout);
	let single = pair.same(&["nar", "pack", "plain"]);
	pair.file("plain.nar", &single.stdout);
	for args in [
		vec!["nar", "ls", "t.nar", "/"],
		vec!["nar", "ls", "-l", "t.nar", "/"],
		vec!["nar", "ls", "-R", "t.nar", "/"],
		vec!["nar", "ls", "-lR", "t.nar", "/sub"],
		vec!["nar", "ls", "-d", "t.nar", "/"],
		vec!["nar", "ls", "-ld", "t.nar", "/sub"],
		vec!["nar", "ls", "--json", "t.nar", "/"],
		vec!["nar", "ls", "--json", "-R", "t.nar", "/"],
		vec!["nar", "ls", "--json", "t.nar", "/run"],
		vec!["nar", "ls", "--json", "t.nar", "/link"],
		vec!["nar", "ls", "--json", "-d", "t.nar", "/"],
		vec!["nar", "ls", "t.nar", "/a"],
		vec!["nar", "ls", "t.nar", "a"],
		vec!["nar", "ls", "t.nar", "//sub/../a"],
		vec!["nar", "ls", "t.nar", "/sub/"],
		vec!["nar", "ls", "t.nar", "/nope"],
		vec!["nar", "ls", "t.nar", "/a/x"],
		vec!["nar", "ls", "plain.nar", "/"],
		vec!["nar", "ls", "-l", "plain.nar", "/"],
		vec!["nar", "ls", "plain", "/"],
		vec!["nar", "ls", "garbage", "/"],
		vec!["nar", "ls", "missing.nar", "/"],
		vec!["nar", "ls", "t.nar"],
	] {
		pair.same(&args);
	}
	assert_eq!(pair.roots_exist(), [false, false]);
	for args in [
		vec!["nar", "cat", "t.nar", "/a"],
		vec!["nar", "cat", "t.nar", "sub/b"],
		vec!["nar", "cat", "t.nar", "/sub"],
		vec!["nar", "cat", "t.nar", "/nope"],
		vec!["nar", "cat", "t.nar", "/link"],
		vec!["nar", "cat", "plain.nar", "/"],
		vec!["nar", "cat", "missing.nar", "/a"],
	] {
		pair.same(&args);
	}
}

#[test]
fn signing_and_signature_import() {
	let Some(pair) = Pair::new() else { return };
	pair.file("payload", b"payload\n");
	let path = pair.seed(&["store", "add-file", "payload"]);
	let k = pair.run_in(0, true, &["key", "generate-secret", "--key-name", "k"], b"");
	let k1 = pair.run_in(
		0,
		true,
		&["key", "generate-secret", "--key-name", "k-1"],
		b"",
	);
	pair.file("k.sec", &k.stdout);
	let mut k1_text = k1.stdout.clone();
	k1_text.push(b'\n');
	pair.file("k1.sec", &k1_text);
	pair.file("bad.sec", b"payload");
	let invalid = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x";
	for args in [
		vec!["store", "sign", path.as_str()],
		vec!["store", "sign", "-k", "missing", path.as_str()],
		vec!["store", "sign", "-k", "bad.sec", path.as_str()],
		vec!["store", "sign", "-k", "k.sec", invalid],
		vec!["store", "sign", "-k", "k.sec", path.as_str()],
		vec!["store", "sign", "-k", "k.sec", path.as_str()],
		vec![
			"store",
			"sign",
			"--key-file",
			"k1.sec",
			"--quiet",
			path.as_str(),
		],
		vec!["store", "sign", "-k", "k1.sec", path.as_str()],
		vec!["path-info", "--sigs", path.as_str()],
	] {
		pair.same(&args);
	}
	pair.same_state(&["path-info", "--sigs", &path]);
	pair.same_state(&["path-info", "--json", "--json-format", "2", &path]);

	// Signatures from another store of the same path, and none for a
	// store whose copy differs.
	let other = Pair::new().unwrap();
	other.file("payload", b"payload\n");
	other.seed(&["store", "add-file", "payload"]);
	other.file("k.sec", &k.stdout);
	other.seed(&["store", "sign", "-k", "k.sec", &path]);
	let source = |engine: usize| format!("local?root={}", other.dir(engine).join("root").display());
	let fresh = Pair::new().unwrap();
	fresh.file("payload", b"payload\n");
	fresh.seed(&["store", "add-file", "payload"]);
	// Each engine imports from its own copy, whose URI differs only in
	// the directory, so compare status and the resulting signatures.
	let expected = fresh.run_in(
		0,
		true,
		&["store", "copy-sigs", "-s", &source(0), &path],
		b"",
	);
	let actual = fresh.run_in(
		1,
		false,
		&["store", "copy-sigs", "-s", &source(1), &path],
		b"",
	);
	assert_eq!(actual.status.code(), expected.status.code());
	assert_eq!(actual.stdout, expected.stdout);
	assert_eq!(
		String::from_utf8_lossy(&actual.stderr),
		String::from_utf8_lossy(&expected.stderr)
	);
	assert_eq!(
		String::from_utf8_lossy(&expected.stderr),
		"imported 1 signatures\n"
	);
	fresh.same_state(&["path-info", "--sigs", &path]);
	fresh.same(&["store", "copy-sigs", &path]);
	let none = format!("local?root={}", fresh.dir(0).join("empty").display());
	let expected = fresh.run_in(0, true, &["store", "copy-sigs", "-s", &none, &path], b"");
	let actual = fresh.run_in(1, false, &["store", "copy-sigs", "-s", &none, &path], b"");
	assert_eq!(actual.status.code(), expected.status.code());
	assert_eq!(actual.stderr, expected.stderr);
}

#[test]
fn hash_part_lookup_closure_diffs_and_content_addressing() {
	let Some(pair) = Pair::new() else { return };
	let a = graph_path(&pair, "a");
	let chain = graph_path(&pair, "chain");
	let dep = graph_path(&pair, "dep");
	for args in [
		vec!["store", "path-from-hash-part", &a[11..43]],
		vec![
			"store",
			"path-from-hash-part",
			"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
		],
		vec!["store", "path-from-hash-part", "abc"],
		vec!["store", "path-from-hash-part", &a[11..44]],
	] {
		pair.same(&args);
	}
	// Versions and sizes: two package versions and a large addition.
	let big = "x".repeat(20000);
	let before = pair.seed(&[
		"eval",
		"--raw",
		"--impure",
		"--expr",
		r#"let lib = builtins.toFile "libfoo-1.0" "1"; in builtins.toFile "env" "${lib} ${builtins.toFile "tool-2.1" "t"}""#,
	]);
	let after = pair.seed(&[
		"eval",
		"--raw",
		"--impure",
		"--expr",
		&format!(
			r#"let lib = builtins.toFile "libfoo-1.1" "1"; in builtins.toFile "env" "${{lib}} ${{builtins.toFile "big-data" "{big}"}} ${{builtins.toFile "tool-dev" "t"}}""#
		),
	]);
	for args in [
		vec!["store", "diff-closures", before.as_str(), after.as_str()],
		vec!["store", "diff-closures", after.as_str(), before.as_str()],
		vec!["store", "diff-closures", a.as_str(), a.as_str()],
		vec!["store", "diff-closures", a.as_str(), dep.as_str()],
	] {
		pair.same(&args);
	}
	for args in [
		vec!["store", "make-content-addressed", chain.as_str()],
		vec!["store", "make-content-addressed", "--json", a.as_str()],
		vec![
			"store",
			"make-content-addressed",
			"-r",
			"--json",
			dep.as_str(),
		],
	] {
		pair.same(&args);
	}
	pair.same_state(&["path-info", "--all", "--json", "--json-format", "2"]);
}

#[test]
fn why_depends_trees() {
	let Some(pair) = Pair::new() else { return };
	let a = graph_path(&pair, "a");
	let chain = graph_path(&pair, "chain");
	let b = graph_path(&pair, "b");
	let dep = graph_path(&pair, "dep");
	for args in [
		vec!["why-depends", a.as_str(), dep.as_str()],
		vec!["why-depends", "-a", a.as_str(), dep.as_str()],
		vec!["why-depends", "--precise", a.as_str(), dep.as_str()],
		vec!["why-depends", "--precise", "-a", a.as_str(), dep.as_str()],
		vec!["why-depends", chain.as_str(), dep.as_str()],
		vec!["why-depends", "--precise", chain.as_str(), dep.as_str()],
		vec!["why-depends", chain.as_str(), b.as_str()],
		vec!["why-depends", dep.as_str(), a.as_str()],
		vec!["why-depends", a.as_str(), a.as_str()],
		vec!["why-depends", "-a", a.as_str(), a.as_str()],
		vec!["why-depends", a.as_str()],
	] {
		pair.same(&args);
	}
}

#[test]
fn prefetch_file_outputs_and_errors() {
	let Some(pair) = Pair::new() else { return };
	pair.file("hello", b"hello\n");
	let mut paths = Vec::new();
	for engine in 0..2 {
		paths.push(pair.dir(engine).join("hello"));
	}
	// The URL contains the engine's directory, so compare each engine's
	// output with the directory replaced.
	let check = |args: &[&str], url_name: &str| {
		let outputs: Vec<Output> = (0..2)
			.map(|engine| {
				let url = format!("file://{}", pair.dir(engine).join(url_name).display());
				let mut full: Vec<&str> = args.to_vec();
				full.push(&url);
				pair.run_in(engine, engine == 0, &full, b"")
			})
			.collect();
		let norm = |engine: usize, bytes: &[u8]| {
			String::from_utf8_lossy(bytes).replace(&pair.dir(engine).display().to_string(), "DIR")
		};
		assert_eq!(
			outputs[1].status.code(),
			outputs[0].status.code(),
			"{args:?}"
		);
		assert_eq!(
			norm(1, &outputs[1].stdout),
			norm(0, &outputs[0].stdout),
			"{args:?}"
		);
		let (expected, actual) = (norm(0, &outputs[0].stderr), norm(1, &outputs[1].stderr));
		if expected.contains("hash mismatch for '/") {
			// The reference names its random temporary directory.
			let re = |s: &str| s.split("'/").next().unwrap().to_owned();
			assert_eq!(re(&actual), re(&expected), "{args:?}");
		} else {
			assert_eq!(actual, expected, "{args:?}");
		}
	};
	for (args, name) in [
		(vec!["store", "prefetch-file"], "hello"),
		(vec!["store", "prefetch-file", "--quiet"], "hello"),
		(vec!["store", "prefetch-file", "--json"], "hello"),
		(
			vec![
				"store",
				"prefetch-file",
				"--executable",
				"--hash-type",
				"sha512",
			],
			"hello",
		),
		(
			vec!["store", "prefetch-file", "--name", "bad/name"],
			"hello",
		),
		(vec!["store", "prefetch-file", "--name", "x@y"], "hello"),
		(vec!["store", "prefetch-file"], "a%20b"),
		(vec!["store", "prefetch-file"], "missing"),
		(
			vec![
				"store",
				"prefetch-file",
				"--expected-hash",
				"sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
			],
			"hello",
		),
		(
			vec![
				"store",
				"prefetch-file",
				"--json",
				"--expected-hash",
				"sha256-WJG1tSLV3whtD/CxEPvZ0hu0/HFjrzTQgoai6Eb2vgM=",
			],
			"hello",
		),
	] {
		check(&args, name);
	}
	pair.same_state(&["path-info", "--all", "--json", "--json-format", "2"]);
}

#[test]
fn derivation_add_round_trip_and_output_checks() {
	let Some(pair) = Pair::new() else { return };
	let show = pair.run_in(
		0,
		true,
		&[
			"derivation",
			"show",
			"--expr",
			r#"derivation { name = "x"; system = "x86_64-linux"; builder = "/bin/sh"; args = [ "-c" "echo" ]; src = builtins.toFile "s" "s"; }"#,
		],
		b"",
	);
	assert!(show.status.success());
	let value: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
	let drv = value["derivations"]
		.as_object()
		.unwrap()
		.values()
		.next()
		.unwrap()
		.clone();
	let src = drv["inputs"]["srcs"][0].as_str().unwrap().to_owned();
	pair.seed(&[
		"eval",
		"--raw",
		"--impure",
		"--expr",
		r#"builtins.toFile "s" "s""#,
	]);
	let variants = {
		let mut wrong = drv.clone();
		wrong["outputs"]["out"]["path"] = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x".into();
		let mut renamed = drv.clone();
		renamed["name"] = "y".into();
		let mut deferred = drv.clone();
		deferred["outputs"]["out"] = serde_json::json!({});
		let mut emptied = deferred.clone();
		emptied["env"]["out"] = "".into();
		vec![drv.clone(), wrong, renamed, deferred, emptied]
	};
	for variant in &variants {
		pair.same_input(
			&["derivation", "add", "--dry-run"],
			variant.to_string().as_bytes(),
		);
		pair.same_input(&["derivation", "add"], variant.to_string().as_bytes());
	}
	let added = pair.same_input(&["derivation", "add"], drv.to_string().as_bytes());
	let drv_path = String::from_utf8(added.stdout).unwrap();
	let drv_path = drv_path.trim().trim_start_matches('/');
	assert_eq!(
		fs::read(pair.dir(1).join("root").join(drv_path)).unwrap(),
		fs::read(pair.dir(0).join("root").join(drv_path)).unwrap()
	);
	pair.same_state(&[
		"path-info",
		"--json",
		"--json-format",
		"2",
		&format!("/{drv_path}"),
	]);
	let _ = src;
}

#[test]
fn logs_repair_roots_daemon_and_build_traces() {
	let Some(pair) = Pair::new() else { return };
	let dep = graph_path(&pair, "dep");
	let show = pair.run_in(
		0,
		true,
		&[
			"derivation",
			"show",
			"--expr",
			r#"derivation { name = "logged"; system = "x86_64-linux"; builder = "/bin/sh"; }"#,
		],
		b"",
	);
	let value: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
	let drv = value["derivations"]
		.as_object()
		.unwrap()
		.values()
		.next()
		.unwrap()
		.to_string();
	let added = pair.run_in(0, true, &["derivation", "add"], drv.as_bytes());
	pair.run_in(1, true, &["derivation", "add"], drv.as_bytes());
	let drv_path = String::from_utf8(added.stdout).unwrap().trim().to_owned();
	let name = drv_path.trim_start_matches("/nix/store/");
	for engine in 0..2 {
		let log = pair
			.dir(engine)
			.join("root/nix/var/log/nix/drvs")
			.join(&name[..2]);
		fs::create_dir_all(&log).unwrap();
		fs::write(log.join(&name[2..]), "build output\n").unwrap();
	}
	let to = |engine: usize| format!("local?root={}", pair.dir(engine).join("other").display());
	pair.same(&["store", "copy-log", &drv_path]);
	pair.same(&["store", "copy-log", "--to", "local?root=other", &dep]);
	pair.same(&[
		"store",
		"copy-log",
		"--to",
		"local?root=other",
		"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv",
	]);
	for engine in 0..2 {
		let output = pair.run_in(
			engine,
			engine == 0,
			&["store", "copy-log", "--to", &to(engine), &drv_path],
			b"",
		);
		assert!(
			output.status.success(),
			"{}",
			String::from_utf8_lossy(&output.stderr)
		);
		assert!(output.stderr.is_empty() && output.stdout.is_empty());
	}
	for engine in 0..2 {
		let output = pair.run_in(
			engine,
			true,
			&["--store", &to(engine), "log", &drv_path],
			b"",
		);
		assert_eq!(String::from_utf8_lossy(&output.stdout), "build output\n");
	}
	pair.same(&["store", "repair", &dep]);
	pair.same(&[
		"store",
		"repair",
		"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x",
	]);
	pair.same(&["--store", "dummy://", "store", "roots-daemon"]);
	pair.same(&["store", "build-trace", "info", &dep]);
	let mut ca = Pair::new().unwrap();
	ca.features = "nix-command ca-derivations";
	let dep = graph_path(&ca, "dep");
	ca.same(&["store", "build-trace", "info", &dep]);
	ca.same(&["store", "build-trace", "info", "--json", &dep]);
}
