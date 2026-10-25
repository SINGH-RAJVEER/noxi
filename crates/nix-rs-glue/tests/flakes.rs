use bstr::ByteSlice;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use nix_rs_eval::{EvalMode, Evaluation};
use nix_rs_glue::fetch::FetchConfig;
use nix_rs_glue::{EvaluatorStore, configure};
use serde_json::{Value, json};

fn reference() -> Option<PathBuf> {
	let bin = std::env::var_os("NIXRS_REFERENCE_NIX")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR"))
				.join("../../target/nix-reference/bin/nix-instantiate")
		});
	if !bin.exists() {
		eprintln!("CppNix comparison skipped: run just reference or set NIXRS_REFERENCE_NIX");
		return None;
	}
	Some(bin)
}

fn candidate(cwd: &Path, expression: &str) -> Value {
	let store = EvaluatorStore::new();
	store.configure_fetching(FetchConfig {
		cwd: cwd.to_owned(),
		..FetchConfig::default()
	});
	let result = configure(
		Evaluation::builder_pure().experimental_features(["flakes", "fetch-tree"]),
		store,
	)
	.mode(EvalMode::Strict)
	.build()
	.evaluate_in_dir(&format!("builtins.toJSON ({expression})"), cwd.to_owned());
	assert!(
		result.errors.is_empty(),
		"{expression}: {:?}",
		result.errors
	);
	serde_json::from_str(
		result
			.value
			.unwrap()
			.to_contextful_str()
			.unwrap()
			.to_str()
			.unwrap(),
	)
	.unwrap()
}

fn compare(cwd: &Path, expression: &str) -> Option<Value> {
	let reference = reference()?;
	let root = tempfile::tempdir().unwrap();
	let home = tempfile::tempdir().unwrap();
	let output = Command::new(reference)
		.env_clear()
		.env("PATH", std::env::var_os("PATH").unwrap())
		.env("HOME", home.path())
		.env("NIX_CONF_DIR", home.path())
		.env(
			"NIX_CONFIG",
			"experimental-features = nix-command flakes\nflake-registry =\nwarn-dirty = false\n",
		)
		.env("NIX_PATH", "")
		.current_dir(cwd)
		.args([
			"--store",
			&format!("local?root={}", root.path().display()),
			"--eval",
			"--strict",
			"--json",
			"--expr",
			expression,
		])
		.output()
		.unwrap();
	assert!(
		output.status.success(),
		"{expression}: {}",
		String::from_utf8_lossy(&output.stderr)
	);
	let expected: Value = serde_json::from_slice(&output.stdout).unwrap();
	let actual = candidate(cwd, expression);
	assert_eq!(actual, expected, "{expression}");
	Some(actual)
}

fn git(path: &Path, args: &[&str]) -> String {
	let output = Command::new("git")
		.arg("-C")
		.arg(path)
		.args(args)
		.env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
		.env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
		.output()
		.unwrap();
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn repo(path: &Path) {
	git(path, &["init", "-b", "main"]);
	git(path, &["config", "user.name", "Fetcher test"]);
	git(path, &["config", "user.email", "fetcher@example.invalid"]);
	fs::write(
		path.join("flake.nix"),
		"{ outputs = { self }: { answer = 42; }; }",
	)
	.unwrap();
	git(path, &["add", "."]);
	git(path, &["commit", "-m", "initial"]);
}

#[test]
fn local_path_and_file_fetches_match_reference() {
	let fixture = tempfile::tempdir().unwrap();
	fs::write(fixture.path().join("file"), "hello\n").unwrap();
	std::os::unix::fs::symlink("file", fixture.path().join("link")).unwrap();
	for expression in [
		format!("builtins.fetchTree {{ type = \"path\"; path = {}; }}", json!(fixture.path())),
		format!("builtins.fetchTree {{ type = \"file\"; url = {}; }}", json!(format!("file://{}/file", fixture.path().display()))),
		format!("builtins.fetchurl {}", json!(format!("file://{}/file", fixture.path().display()))),
		"builtins.path { path = ./.; name = \"source\"; }".into(),
		"builtins.filterSource (p: t: t != \"symlink\") ./.".into(),
		"let t = builtins.fetchTree { type = \"path\"; path = ./.; }; in builtins.readFile (t.outPath + \"/file\")".into(),
	] { compare(fixture.path(), &expression); }
}

#[test]
fn git_worktrees_and_commits_match_reference() {
	let fixture = tempfile::tempdir().unwrap();
	repo(fixture.path());
	fs::write(fixture.path().join("untracked"), "hidden").unwrap();
	let url = json!(format!("file://{}", fixture.path().display()));
	let expressions = [
		format!("builtins.fetchGit {{ url = {url}; }}"),
		format!("builtins.fetchTree {{ type = \"git\"; url = {url}; }}"),
		format!("builtins.fetchTree {{ type = \"git\"; url = {url}; shallow = false; }}"),
	];
	for expression in &expressions {
		compare(fixture.path(), expression);
	}
	let rev = git(fixture.path(), &["rev-parse", "HEAD"]);
	fs::write(fixture.path().join("flake.nix"), "dirty\n").unwrap();
	for expression in &expressions {
		compare(fixture.path(), expression);
	}
	compare(
		fixture.path(),
		&format!("builtins.fetchGit {{ url = {url}; rev = \"{rev}\"; }}"),
	);
}

#[test]
fn file_tarballs_match_reference() {
	let fixture = tempfile::tempdir().unwrap();
	let mut bytes = Vec::new();
	{
		let mut archive = tar::Builder::new(&mut bytes);
		let mut header = tar::Header::new_gnu();
		header.set_size(6);
		header.set_mode(0o755);
		header.set_mtime(1577836800);
		header.set_cksum();
		archive
			.append_data(&mut header, "root/file", &b"hello\n"[..])
			.unwrap();
		archive.finish().unwrap();
	}
	fs::write(fixture.path().join("source.tar"), &bytes).unwrap();
	let url = json!(format!("file://{}/source.tar", fixture.path().display()));
	compare(
		fixture.path(),
		&format!("builtins.fetchTree {{ type = \"tarball\"; url = {url}; }}"),
	);
	compare(fixture.path(), &format!("builtins.fetchTarball {url}"));
	compare(
		fixture.path(),
		&format!("builtins.readFile ((builtins.fetchTarball {url}) + \"/file\")"),
	);
}
