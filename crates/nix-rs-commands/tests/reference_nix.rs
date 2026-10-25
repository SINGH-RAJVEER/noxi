//! Modern command comparisons against CppNix 2.35.2 in diverted stores.
use std::fs;
use std::os::unix::process::CommandExt;
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
		vec!["build", "--no-link", "--json", "--dry-run", path],
		vec!["build", "--out-link", "custom", "--print-out-paths", path],
	] {
		f.same(&args);
	}
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
