//! Modern command comparisons against CppNix 2.35.2 in diverted stores.
use std::fs;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
	dir: tempfile::TempDir,
	reference: PathBuf,
}
impl Fixture {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let reference = std::env::var_os("NIXRS_REFERENCE_BIN")
			.map(PathBuf::from)
			.unwrap_or_else(|| {
				Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
			});
		assert!(
			reference.join("nix").exists(),
			"set NIXRS_REFERENCE_BIN to the pinned CppNix 2.35.2 bin directory"
		);
		Self { dir, reference }
	}
	fn run(&self, reference: bool, args: &[&str], features: &str) -> Output {
		let program = if reference {
			self.reference.join("nix")
		} else {
			PathBuf::from(env!("CARGO_BIN_EXE_nix"))
		};
		Command::new(program)
			.arg0("nix")
			.args([
				"--store",
				&format!("local?root={}", self.dir.path().join("root").display()),
			])
			.args(args)
			.current_dir(self.dir.path())
			.env_clear()
			.env("HOME", self.dir.path())
			.env("NIX_CONF_DIR", self.dir.path().join("etc"))
			.env(
				"NIX_CONFIG",
				format!(
					"experimental-features = {features}\nbuild-users-group =\nsubstituters =\nsandbox = true\n"
				),
			)
			.output()
			.unwrap()
	}
	fn same(&self, args: &[&str]) -> Output {
		let expected = self.run(true, args, "nix-command");
		let actual = self.run(false, args, "nix-command");
		assert_eq!(
			actual.status.code(),
			expected.status.code(),
			"{args:?}: {}",
			String::from_utf8_lossy(&actual.stderr)
		);
		assert_eq!(
			actual.status.signal(),
			expected.status.signal(),
			"signal: {args:?}"
		);
		assert_eq!(actual.stdout, expected.stdout, "stdout: {args:?}");
		assert_eq!(
			String::from_utf8_lossy(&actual.stderr),
			String::from_utf8_lossy(&expected.stderr),
			"stderr: {args:?}"
		);
		expected
	}
}

#[test]
fn eval_expressions_files_apply_and_formats() {
	let f = Fixture::new();
	for args in [
		vec!["eval", "--expr", "1 + 2"],
		vec![
			"eval",
			"--json",
			"--expr",
			"{ b = [ 1 true null ]; a = \"x\"; }",
		],
		vec!["eval", "--raw", "--expr", "\"a\\nb\""],
		vec!["eval", "--json", "--expr", "{ a.b = 2; }", "a.b"],
		vec![
			"eval",
			"--expr",
			"[ 1 2 ]",
			"--apply",
			"x: builtins.length x",
		],
		vec!["eval", "--raw", "--json", "--expr", "1"],
	] {
		f.same(&args);
	}
	fs::write(f.dir.path().join("input.nix"), "{ answer = 42; }").unwrap();
	f.same(&["eval", "--json", "--file", "input.nix", "answer"]);
	let lookup = format!("entry={}", f.dir.path().join("input.nix").display());
	f.same(&[
		"eval", "--json", "--file", "<entry>", "-I", &lookup, "answer",
	]);
}

#[test]
fn feature_gate_and_global_precedence() {
	let f = Fixture::new();
	for args in [
		vec!["eval", "--expr", "1"],
		vec![
			"eval",
			"--extra-experimental-features",
			"nix-command",
			"--expr",
			"1",
		],
		vec![
			"--option",
			"experimental-features",
			"nix-command",
			"eval",
			"--expr",
			"1",
		],
	] {
		let expected = f.run(true, &args, "");
		let actual = f.run(false, &args, "");
		assert_eq!(actual.status.code(), expected.status.code());
		assert_eq!(actual.stdout, expected.stdout);
		assert_eq!(actual.stderr, expected.stderr);
	}
	f.same(&["eval", "--expr", "1", "--not-a-flag"]);
}

#[test]
fn build_existing_paths_and_roots() {
	let f = Fixture::new();
	fs::write(f.dir.path().join("payload"), "payload\n").unwrap();
	let added = f.run(true, &["store", "add-file", "payload"], "nix-command");
	assert!(added.status.success());
	let path = String::from_utf8(added.stdout).unwrap();
	let path = path.trim();
	for args in [
		vec!["build", "--no-link", path],
		vec!["build", "--no-link", "--print-out-paths", path],
		vec!["build", "--no-link", "--json", path],
		vec!["build", "--no-link", "--json", "--print-out-paths", path],
		vec!["build", "--no-link", "--out-link", "ordered", path],
		vec!["build", "--out-link", "disabled", "--no-link", path],
		vec!["build", "--no-link", "--json", "--dry-run", path],
		vec!["build", "--out-link", "custom", "--print-out-paths", path],
	] {
		f.same(&args);
	}
	assert!(f.dir.path().join("ordered").is_symlink());
	assert!(!f.dir.path().join("disabled").exists());
	assert_eq!(
		fs::read_link(f.dir.path().join("custom")).unwrap(),
		Path::new(path)
	);
}

#[test]
fn store_add_inspect_verify_and_info() {
	let f = Fixture::new();
	fs::create_dir(f.dir.path().join("tree")).unwrap();
	fs::create_dir(f.dir.path().join("tree/sub")).unwrap();
	fs::write(f.dir.path().join("tree/a"), b"a\0b\n").unwrap();
	fs::write(f.dir.path().join("tree/sub/b"), "nested\n").unwrap();
	std::os::unix::fs::symlink("a", f.dir.path().join("tree/link")).unwrap();
	let added = f.same(&["store", "add-path", "tree"]);
	assert!(added.status.success());
	let path = String::from_utf8(added.stdout).unwrap();
	let path = path.trim();
	f.same(&["store", "add-file", "tree/a", "--name", "flat"]);
	for args in [
		vec!["path-info", path],
		vec!["path-info", "-rS", "--size", path],
		vec!["path-info", "--json", path],
		vec![
			"path-info",
			"--json",
			"--json-format",
			"2",
			"--closure-size",
			path,
		],
		vec!["path-info", "--json", "--json-format", "3", path],
		vec!["store", "ls", path],
		vec!["store", "ls", "-Rl", path],
		vec!["store", "ls", "--json", path],
		vec!["store", "ls", "--json", "-R", path],
		vec!["store", "cat", &format!("{path}/a")],
		vec!["store", "dump-path", path],
		vec!["store", "verify", path],
		vec!["store", "verify", "--no-trust", path],
		vec!["store", "info", "--json"],
		vec!["store", "info"],
	] {
		f.same(&args);
	}
}

#[test]
fn store_maintenance_and_ping_alias() {
	let f = Fixture::new();
	f.same(&["store", "ping", "--json"]);
	f.same(&["store", "gc", "--dry-run"]);
	f.same(&["store", "gc", "--max", "1", "--dry-run"]);
	f.same(&["store", "optimise"]);
	fs::write(f.dir.path().join("dead"), "unused\n").unwrap();
	let path = f.same(&["store", "add-file", "dead"]);
	let path = String::from_utf8(path.stdout).unwrap();
	let args = ["store", "delete", path.trim()];
	let expected = f.run(true, &args, "nix-command");
	let recreated = f.run(true, &["store", "add-file", "dead"], "nix-command");
	assert!(recreated.status.success());
	let actual = f.run(false, &args, "nix-command");
	assert_eq!(actual.status.code(), expected.status.code());
	assert_eq!(actual.stdout, expected.stdout);
	assert_eq!(actual.stderr, expected.stderr);
}

impl Fixture {
	/// Seed static BusyBox by reading its NAR directly. This never opens the host store.
	fn seed_busybox(&self) -> String {
		fs::create_dir_all(self.dir.path().join("root")).unwrap();
		let busybox = fs::canonicalize(
			std::env::var_os("NIXRS_TEST_BUSYBOX")
				.map(PathBuf::from)
				.unwrap_or_else(|| {
					Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-busybox")
				}),
		)
		.unwrap();
		let (hash, size) = nix_rs_store::nar::hash_path(&busybox).unwrap();
		let path = nix_compat::store_path::StorePath::from_absolute_path(
			busybox.to_str().unwrap().as_bytes(),
		)
		.unwrap();
		let mut info = nix_rs_store::ValidPathInfo::new(path, hash);
		info.nar_size = size;
		let store = nix_rs_store::LocalStore::open(
			nix_rs_store::LocalStoreConfig::diverted(&self.dir.path().join("root")).unwrap(),
		)
		.unwrap();
		let mut dump = tempfile::tempfile().unwrap();
		nix_rs_store::nar::dump_path(&mut dump, &busybox).unwrap();
		use std::io::Seek;
		dump.rewind().unwrap();
		store
			.add_to_store(&info, &mut std::io::BufReader::new(dump), false, false)
			.unwrap();
		busybox.to_string_lossy().into_owned()
	}
	fn derivation(&self) -> String {
		let busybox = self.seed_busybox();
		format!(
			r#"let bb = builtins.storePath {busybox}; in derivation {{
			name = "modern-test"; system = "x86_64-linux"; builder = "${{bb}}/bin/sh";
			args = [ "-c" "echo payload > $out; echo log-output >&2" ];
		}}"#
		)
	}
}

#[test]
fn derivation_show_and_local_logs() {
	let f = Fixture::new();
	let expr = f.derivation();
	let expected = f.same(&["derivation", "show", "--impure", "--expr", &expr]);
	assert!(expected.status.success());
	f.same(&["derivation", "show", "-r", "--impure", "--expr", &expr]);
	let value: serde_json::Value = serde_json::from_slice(&expected.stdout).unwrap();
	let name = value["derivations"]
		.as_object()
		.unwrap()
		.keys()
		.next()
		.unwrap();
	let path = format!("/nix/store/{name}");
	f.same(&["log", &path]);
	let store = nix_rs_store::LocalStore::open(
		nix_rs_store::LocalStoreConfig::diverted(&f.dir.path().join("root")).unwrap(),
	)
	.unwrap();
	store
		.add_build_log(
			&nix_compat::store_path::StorePath::from_absolute_path(path.as_bytes()).unwrap(),
			&mut b"log\0contents\n".as_slice(),
		)
		.unwrap();
	f.same(&["log", &path]);
}

#[test]
fn expression_builds_and_cached_json() {
	let f = Fixture::new();
	let expr = f.derivation();
	let expected = f.run(
		true,
		&[
			"build",
			"--impure",
			"--expr",
			&expr,
			"--no-link",
			"--print-out-paths",
		],
		"nix-command",
	);
	assert!(
		expected.status.success(),
		"{}",
		String::from_utf8_lossy(&expected.stderr)
	);
	nix_rs_store::delete_path(&f.dir.path().join("root")).unwrap();
	f.seed_busybox();
	let actual = f.run(
		false,
		&[
			"build",
			"--impure",
			"--expr",
			&expr,
			"--no-link",
			"--print-out-paths",
		],
		"nix-command",
	);
	assert!(
		actual.status.success(),
		"{}",
		String::from_utf8_lossy(&actual.stderr)
	);
	assert_eq!(actual.stdout, expected.stdout);
	assert_eq!(actual.stderr, expected.stderr);
	f.same(&["build", "--impure", "--expr", &expr, "--no-link", "--json"]);
	f.same(&[
		"build",
		"--impure",
		"--expr",
		&expr,
		"--out-link",
		"expression-result",
		"--print-out-paths",
	]);
}

#[test]
fn shell_and_run_expressions_match_execution_or_namespace_rejection() {
	let f = Fixture::new();
	let busybox = f.seed_busybox();
	let expr = format!(
		r#"let bb = builtins.storePath {busybox}; in derivation {{ name = "sh"; system = "x86_64-linux"; builder = "${{bb}}/bin/sh"; args = [ "-c" "${{bb}}/bin/mkdir -p $out/bin; ${{bb}}/bin/ln -s ${{bb}}/bin/sh $out/bin/sh" ]; }}"#
	);
	let built = f.run(
		true,
		&["build", "--impure", "--expr", &expr, "--no-link"],
		"nix-command",
	);
	assert!(
		built.status.success(),
		"{}",
		String::from_utf8_lossy(&built.stderr)
	);
	for args in [
		vec![
			"shell",
			"--impure",
			"--expr",
			&expr,
			"-c",
			"sh",
			"-c",
			"echo hello; exit 7",
		],
		vec![
			"shell",
			"--impure",
			"--expr",
			&expr,
			"-c",
			"sh",
			"-c",
			"echo $PATH",
		],
		vec![
			"shell",
			"--impure",
			"--expr",
			&expr,
			"--ignore-env",
			"--set-env-var",
			"CLI_TEST",
			"value",
			"-c",
			"sh",
			"-c",
			"echo $CLI_TEST",
		],
		vec![
			"run",
			"--impure",
			"--expr",
			&expr,
			"",
			"--",
			"-c",
			"echo run; exit 9",
		],
	] {
		let result = f.same(&args);
		if result.status.code() == Some(1)
			&& String::from_utf8_lossy(&result.stderr)
				.contains("setting up a private mount namespace: Operation not permitted")
		{
			// Compare the real platform boundary, but do not count it as child execution evidence.
			eprintln!("execution unavailable: private mount namespace rejected");
			continue;
		}
		if args[0] == "shell" {
			assert!(
				matches!(result.status.code(), Some(0 | 7)),
				"{}",
				String::from_utf8_lossy(&result.stderr)
			);
		} else {
			assert_eq!(result.status.code(), Some(9));
		}
		eprintln!(
			"verified {} child execution, exit {:?}",
			args[0],
			result.status.code()
		);
	}
}

#[test]
fn modern_purity_and_flag_order() {
	let f = Fixture::new();
	for args in [
		vec![
			"eval",
			"--json",
			"--expr",
			"[ (builtins ? currentSystem) (builtins ? currentTime) (builtins.builtins ? currentSystem) ]",
		],
		vec!["eval", "--raw", "--expr", "builtins.getEnv \"HOME\""],
		vec![
			"eval",
			"--raw",
			"--impure",
			"--expr",
			"builtins.getEnv \"HOME\"",
		],
		vec![
			"eval",
			"--raw",
			"--impure",
			"--pure-eval",
			"--expr",
			"builtins.getEnv \"HOME\"",
		],
		vec![
			"eval",
			"--raw",
			"--pure-eval",
			"--impure",
			"--expr",
			"builtins.getEnv \"HOME\"",
		],
	] {
		f.same(&args);
	}
	fs::write(f.dir.path().join("value.nix"), "builtins.getEnv \"HOME\"").unwrap();
	f.same(&["eval", "--raw", "--file", "value.nix"]);
	f.same(&["eval", "--file", "value.nix", "--pure-eval"]);
	f.same(&["eval", "--expr", "1", "--file", "value.nix"]);
}

#[test]
fn signed_file_copy_matches_cache_bytes_and_can_be_reimported() {
	let f = Fixture::new();
	let expr = f.derivation();
	let added = f.run(
		true,
		&[
			"build",
			"--impure",
			"--expr",
			&expr,
			"--no-link",
			"--print-out-paths",
		],
		"nix-command",
	);
	assert!(added.status.success());
	let path = String::from_utf8(added.stdout).unwrap();
	let path = path.trim();
	let key = f.run(
		true,
		&["key", "generate-secret", "--key-name", "cli-test"],
		"nix-command",
	);
	assert!(key.status.success());
	let keyfile = f.dir.path().join("secret-key");
	fs::write(&keyfile, &key.stdout).unwrap();
	let cache = f.dir.path().join("cache");
	let uri = format!(
		"file://{}?compression=none&secret-key={}",
		cache.display(),
		keyfile.display()
	);
	let args = ["copy", "--to", &uri, path];
	let expected = f.run(true, &args, "nix-command");
	assert!(
		expected.status.success(),
		"{}",
		String::from_utf8_lossy(&expected.stderr)
	);
	let store_path =
		nix_compat::store_path::StorePath::from_absolute_path(path.as_bytes()).unwrap();
	let cache_api = nix_rs_store::binary_cache::BinaryCache::open(&uri).unwrap();
	let metadata = cache_api.narinfo_text(&store_path).unwrap().unwrap();
	let mut nar = Vec::new();
	use std::io::Read;
	cache_api
		.nar(&cache_api.query_path_info(&store_path).unwrap().unwrap())
		.unwrap()
		.read_to_end(&mut nar)
		.unwrap();
	fs::remove_dir_all(&cache).unwrap();
	let actual = f.run(false, &args, "nix-command");
	assert_eq!(
		actual.status.code(),
		expected.status.code(),
		"{}",
		String::from_utf8_lossy(&actual.stderr)
	);
	assert_eq!(actual.stdout, expected.stdout);
	assert_eq!(actual.stderr, expected.stderr);
	let cache_api = nix_rs_store::binary_cache::BinaryCache::open(&uri).unwrap();
	assert_eq!(
		cache_api.narinfo_text(&store_path).unwrap().unwrap(),
		metadata
	);
	let mut actual_nar = Vec::new();
	cache_api
		.nar(&cache_api.query_path_info(&store_path).unwrap().unwrap())
		.unwrap()
		.read_to_end(&mut actual_nar)
		.unwrap();
	assert_eq!(actual_nar, nar);
	// Compare imports on fresh, identically named diverted stores.
	let root = f.dir.path().join("root");
	nix_rs_store::delete_path(&root).unwrap();
	let args = ["copy", "--from", &uri, "--no-check-sigs", path];
	let expected = f.run(true, &args, "nix-command");
	assert!(
		expected.status.success(),
		"{}",
		String::from_utf8_lossy(&expected.stderr)
	);
	let expected_file = fs::read(root.join("nix/store").join(store_path.to_string())).unwrap();
	nix_rs_store::delete_path(&root).unwrap();
	let actual = f.run(false, &args, "nix-command");
	assert_eq!(
		actual.status.code(),
		expected.status.code(),
		"{}",
		String::from_utf8_lossy(&actual.stderr)
	);
	assert_eq!(actual.stdout, expected.stdout);
	assert_eq!(actual.stderr, expected.stderr);
	assert_eq!(
		fs::read(root.join("nix/store").join(store_path.to_string())).unwrap(),
		expected_file
	);
}

#[test]
fn unsupported_library_operations_fail_before_building_or_exporting() {
	let f = Fixture::new();
	let expr =
		"derivation { name = \"json-limit\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; }";
	let dry = f.same(&["build", "--expr", expr, "--no-link", "--dry-run", "--json"]);
	assert!(dry.status.success());
	let paths: serde_json::Value = serde_json::from_slice(&dry.stdout).unwrap();
	let output = paths[0]["outputs"]["out"].as_str().unwrap();
	let result = f.run(
		false,
		&["build", "--expr", expr, "--no-link", "--json"],
		"nix-command",
	);
	assert_eq!(result.status.code(), Some(1));
	assert!(result.stdout.is_empty());
	assert!(String::from_utf8_lossy(&result.stderr).contains("BuildResult metadata"));
	assert!(
		!f.dir
			.path()
			.join("root")
			.join(output.trim_start_matches('/'))
			.exists()
	);
	let result = f.run(
		false,
		&["eval", "--raw", "--expr", "builtins.readFile /etc/passwd"],
		"nix-command",
	);
	assert_eq!(result.status.code(), Some(1));
	assert!(result.stdout.is_empty());
	assert!(
		String::from_utf8_lossy(&result.stderr)
			.contains("pure filesystem evaluation is not supported")
	);
	let result = f.run(false, &["develop", "--expr", expr], "nix-command");
	assert_eq!(result.status.code(), Some(1));
	assert!(String::from_utf8_lossy(&result.stderr).contains("build-environment capture"));
}

#[test]
fn legacy_environment_entrypoints_and_explicit_limits() {
	let f = Fixture::new();
	let run = |reference: bool, program: &str, args: &[&str]| {
		let candidate = match program {
			"nix-shell" => env!("CARGO_BIN_EXE_nix-shell"),
			"nix-copy-closure" => env!("CARGO_BIN_EXE_nix-copy-closure"),
			_ => unreachable!(),
		};
		let executable = if reference {
			f.reference.join(program)
		} else {
			PathBuf::from(candidate)
		};
		Command::new(executable)
			.arg0(program)
			.args(args)
			.env_clear()
			.env("HOME", f.dir.path())
			.env("NIX_CONF_DIR", f.dir.path().join("etc"))
			.output()
			.unwrap()
	};
	for (program, args) in [
		("nix-shell", vec!["--version"]),
		("nix-copy-closure", vec!["--version"]),
		("nix-copy-closure", vec![]),
	] {
		let expected = run(true, program, &args);
		let actual = run(false, program, &args);
		assert_eq!(actual.status.code(), expected.status.code());
		assert_eq!(actual.stdout, expected.stdout);
		assert_eq!(actual.stderr, expected.stderr);
	}
	for (program, args, message) in [
		(
			"nix-shell",
			vec!["--expr", "{}", "--run", "true"],
			"build-environment capture",
		),
		(
			"nix-copy-closure",
			vec!["--to", "localhost"],
			"local mode is not supported",
		),
	] {
		let actual = run(false, program, &args);
		assert_eq!(actual.status.code(), Some(1));
		assert!(actual.stdout.is_empty());
		assert!(String::from_utf8_lossy(&actual.stderr).contains(message));
	}
}
