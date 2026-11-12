//! `noxi flake lock` and `noxi flake update` against offline `path:` and local
//! Git inputs, in a diverted store and cache.
use std::{
	fs,
	path::{Path, PathBuf},
	process::{Command, ExitCode},
	sync::OnceLock,
};

use noxi_system::flake::{self, ChangeKind, Operation, UpdateOptions};

/// Divert the store, configuration, cache and Git configuration of this test
/// process to one private directory before any lock operation runs.
fn isolate() {
	static ROOT: OnceLock<PathBuf> = OnceLock::new();
	ROOT.get_or_init(|| {
		let root = tempfile::Builder::new()
			.prefix("noxi-system-flake-")
			.tempdir_in(env!("CARGO_TARGET_TMPDIR"))
			.unwrap()
			.keep();
		let config = format!(
			"store = local?root={}\nsubstituters =\nflake-registry =\nwarn-dirty = false\nbuild-users-group =\n",
			root.join("store").display()
		);
		let vars = [
			("HOME", root.join("home")),
			("XDG_CACHE_HOME", root.join("cache")),
			("XDG_CONFIG_HOME", root.join("config")),
			("XDG_DATA_HOME", root.join("data")),
			("XDG_STATE_HOME", root.join("state")),
			("NIX_CONF_DIR", root.join("etc")),
		];
		// SAFETY: this runs once, before any lock operation, and every test
		// calls it first; the variables are not read concurrently with the
		// writes below.
		unsafe {
			for (name, value) in &vars {
				fs::create_dir_all(value).unwrap();
				std::env::set_var(name, value);
			}
			std::env::set_var("NIX_USER_CONF_FILES", "");
			std::env::set_var("NIX_CONFIG", config);
			std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
			std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
			std::env::remove_var("NIX_REMOTE");
			std::env::remove_var("NIX_PATH");
		}
		root
	});
}

fn run(args: &[&str]) -> (ExitCode, String, String) {
	isolate();
	let (mut out, mut err) = (Vec::new(), Vec::new());
	let code = flake::run(
		args.iter().map(|arg| arg.to_string()).collect(),
		&mut out,
		&mut err,
	);
	(
		code,
		String::from_utf8(out).unwrap(),
		String::from_utf8(err).unwrap(),
	)
}

fn update(flake: &Path, operation: Operation, inputs: &[&str]) -> flake::UpdateReport {
	isolate();
	flake::update(UpdateOptions {
		operation,
		flake: Some(flake.to_owned()),
		inputs: inputs.iter().map(|input| input.to_string()).collect(),
		..Default::default()
	})
	.unwrap()
}

/// A flake with two non-flake `path:` inputs `a` and `b`.
fn two_path_inputs(dir: &Path) -> PathBuf {
	for name in ["a", "b"] {
		fs::create_dir(dir.join(name)).unwrap();
		fs::write(dir.join(name).join("value"), "1").unwrap();
	}
	let root = dir.join("root");
	fs::create_dir(&root).unwrap();
	fs::write(
		root.join("flake.nix"),
		format!(
			"{{ inputs.a = {{ url = \"path:{}\"; flake = false; }}; inputs.b = {{ url = \"path:{}\"; flake = false; }}; outputs = {{ self, a, b }}: {{}}; }}",
			dir.join("a").display(),
			dir.join("b").display()
		),
	)
	.unwrap();
	root
}

fn kinds(report: &flake::UpdateReport) -> Vec<(String, ChangeKind)> {
	report
		.diff
		.changes
		.iter()
		.map(|change| (change.input.clone(), change.kind))
		.collect()
}

#[test]
fn initial_lock_adds_both_inputs() {
	let dir = tempfile::tempdir().unwrap();
	let root = two_path_inputs(dir.path());
	let (code, out, err) = run(&["lock", "--flake", root.to_str().unwrap()]);
	assert_eq!(code, ExitCode::SUCCESS, "{err}");
	assert!(root.join("flake.lock").is_file());
	let lines: Vec<&str> = out.lines().collect();
	assert_eq!(
		lines[0],
		format!("created {}", root.join("flake.lock").display())
	);
	assert!(lines[1].starts_with("added    a  narHash sha256-"), "{out}");
	assert!(lines[2].starts_with("added    b  narHash sha256-"), "{out}");
	assert_eq!(lines.len(), 3, "{out}");
}

#[test]
fn selective_and_full_updates_report_changes() {
	let dir = tempfile::tempdir().unwrap();
	let root = two_path_inputs(dir.path());
	update(&root, Operation::Lock, &[]);

	fs::write(dir.path().join("a/value"), "2").unwrap();
	fs::write(dir.path().join("b/value"), "2").unwrap();
	let report = update(&root, Operation::Update, &["a"]);
	assert_eq!(kinds(&report), [("a".to_owned(), ChangeKind::Changed)]);
	assert_eq!(report.diff.unchanged, ["b"]);
	let change = &report.diff.changes[0];
	let (old, new) = (change.old.as_ref().unwrap(), change.new.as_ref().unwrap());
	assert!(old.rev.is_none() && new.rev.is_none());
	assert_ne!(old.nar_hash, new.nar_hash);
	let mut text = Vec::new();
	flake::write_report(&report, &mut text).unwrap();
	let text = String::from_utf8(text).unwrap();
	assert!(text.contains("changed  a  narHash sha256-"), "{text}");
	assert!(text.contains(" -> sha256-"), "{text}");
	assert!(text.ends_with("unchanged: 1 input (b)\n"), "{text}");

	fs::write(dir.path().join("a/value"), "3").unwrap();
	let report = update(&root, Operation::Update, &[]);
	assert_eq!(
		kinds(&report),
		[
			("a".to_owned(), ChangeKind::Changed),
			("b".to_owned(), ChangeKind::Changed)
		]
	);
	assert!(report.diff.unchanged.is_empty());

	let lock = root.join("flake.lock");
	let (bytes, mtime) = (
		fs::read(&lock).unwrap(),
		fs::metadata(&lock).unwrap().modified().unwrap(),
	);
	let (code, out, err) = run(&["update", "--flake", root.to_str().unwrap()]);
	assert_eq!(code, ExitCode::SUCCESS, "{err}");
	assert_eq!(out, "no changes\n");
	assert_eq!(fs::read(&lock).unwrap(), bytes);
	assert_eq!(fs::metadata(&lock).unwrap().modified().unwrap(), mtime);
}

#[test]
fn git_input_reports_short_revisions() {
	let dir = tempfile::tempdir().unwrap();
	let repo = dir.path().join("repo");
	fs::create_dir(&repo).unwrap();
	let git = |date: &str, args: &[&str]| {
		let result = Command::new("git")
			.env("GIT_CONFIG_GLOBAL", "/dev/null")
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.env("GIT_AUTHOR_NAME", "Test")
			.env("GIT_AUTHOR_EMAIL", "test@example.invalid")
			.env("GIT_COMMITTER_NAME", "Test")
			.env("GIT_COMMITTER_EMAIL", "test@example.invalid")
			.env("GIT_AUTHOR_DATE", date)
			.env("GIT_COMMITTER_DATE", date)
			.arg("-C")
			.arg(&repo)
			.args(args)
			.output()
			.unwrap();
		assert!(
			result.status.success(),
			"{}",
			String::from_utf8_lossy(&result.stderr)
		);
		String::from_utf8(result.stdout).unwrap().trim().to_owned()
	};
	git("2001-01-01T00:00:00Z", &["init", "-b", "main"]);
	fs::write(repo.join("value"), "1").unwrap();
	git("2001-01-01T00:00:00Z", &["add", "value"]);
	git("2001-01-01T00:00:00Z", &["commit", "-m", "first"]);
	let first = git("2001-01-01T00:00:00Z", &["rev-parse", "HEAD"]);

	let root = dir.path().join("root");
	fs::create_dir(&root).unwrap();
	fs::write(
		root.join("flake.nix"),
		format!(
			"{{ inputs.dep = {{ url = \"git+file://{}\"; flake = false; }}; outputs = {{ self, dep }}: {{}}; }}",
			repo.display()
		),
	)
	.unwrap();
	let report = update(&root, Operation::Lock, &[]);
	assert_eq!(kinds(&report), [("dep".to_owned(), ChangeKind::Added)]);

	fs::write(repo.join("value"), "2").unwrap();
	git("2002-02-02T00:00:00Z", &["commit", "-am", "second"]);
	let second = git("2002-02-02T00:00:00Z", &["rev-parse", "HEAD"]);
	let (code, out, err) = run(&["update", "dep", "--flake", root.to_str().unwrap()]);
	assert_eq!(code, ExitCode::SUCCESS, "{err}");
	assert_eq!(
		out.lines().nth(1).unwrap(),
		format!(
			"changed  dep  rev {} -> {}, last modified 2001-01-01 -> 2002-02-02",
			&first[..7],
			&second[..7]
		),
		"{out}"
	);
}

#[test]
fn json_report_has_the_documented_shape() {
	let dir = tempfile::tempdir().unwrap();
	let root = two_path_inputs(dir.path());
	let (code, out, err) = run(&["lock", "--flake", root.to_str().unwrap(), "--json"]);
	assert_eq!(code, ExitCode::SUCCESS, "{err}");
	let value: serde_json::Value = serde_json::from_str(&out).unwrap();
	let root = fs::canonicalize(&root).unwrap();
	assert_eq!(value["flake"], root.to_str().unwrap());
	assert_eq!(value["lock"], root.join("flake.lock").to_str().unwrap());
	assert_eq!(value["unchanged"], 0);
	let changes = value["changes"].as_array().unwrap();
	assert_eq!(changes.len(), 2);
	assert_eq!(changes[0]["input"], "a");
	assert_eq!(changes[0]["kind"], "added");
	assert!(changes[0]["old"].is_null());
	assert!(
		changes[0]["new"]["narHash"]
			.as_str()
			.unwrap()
			.starts_with("sha256-")
	);
	assert!(
		changes[0]["new"]["url"]
			.as_str()
			.unwrap()
			.starts_with("path:")
	);

	let (code, out, err) = run(&["update", "--flake", root.to_str().unwrap(), "--json"]);
	assert_eq!(code, ExitCode::SUCCESS, "{err}");
	let value: serde_json::Value = serde_json::from_str(&out).unwrap();
	assert_eq!(value["changes"], serde_json::json!([]));
	assert_eq!(value["unchanged"], 2);
}

#[test]
fn errors_name_the_phase_and_usage_errors_exit_two() {
	let dir = tempfile::tempdir().unwrap();
	let missing = dir.path().join("missing");
	let (code, out, err) = run(&["update", "--flake", missing.to_str().unwrap()]);
	assert_eq!(code, ExitCode::from(1));
	assert!(out.is_empty());
	assert!(
		err.starts_with("error: resolve: cannot open flake directory"),
		"{err}"
	);

	let broken = dir.path().join("broken");
	fs::create_dir(&broken).unwrap();
	fs::write(
		broken.join("flake.nix"),
		format!(
			"{{ inputs.gone = {{ url = \"path:{}\"; flake = false; }}; outputs = {{ self, gone }}: {{}}; }}",
			missing.display()
		),
	)
	.unwrap();
	let (code, out, err) = run(&["lock", "--flake", broken.to_str().unwrap()]);
	assert_eq!(code, ExitCode::from(1));
	assert!(out.is_empty());
	assert!(err.starts_with("error: lock: "), "{err}");
	assert!(!broken.join("flake.lock").exists());

	let (code, out, err) = run(&["update", "--frobnicate"]);
	assert_eq!(code, ExitCode::from(2));
	assert!(out.is_empty());
	assert!(
		err.starts_with("error: unknown flag '--frobnicate'\nusage: noxi flake update"),
		"{err}"
	);
}

#[test]
fn diff_resolves_follows_and_reports_each_node_once() {
	let old = serde_json::json!({
		"nodes": {
			"root": { "inputs": { "a": "a", "b": "b" } },
			"a": { "inputs": { "b": ["b"] }, "locked": { "type": "path", "path": "/a", "narHash": "sha256-A" }, "original": { "type": "path", "path": "/a" } },
			"b": { "locked": { "type": "path", "path": "/b", "narHash": "sha256-B" }, "original": { "type": "path", "path": "/b" } },
			"c": { "locked": { "type": "path", "path": "/c", "narHash": "sha256-C" }, "original": { "type": "path", "path": "/c" } }
		},
		"root": "root",
		"version": 7
	});
	let mut new = old.clone();
	new["nodes"]["root"]["inputs"] = serde_json::json!({ "a": "a", "b": "b_2" });
	new["nodes"]["b_2"] = new["nodes"]["b"].take();
	new["nodes"]["b_2"]["locked"]["narHash"] = "sha256-B2".into();
	new["nodes"]["a"]["inputs"] = serde_json::json!({ "b": ["b"], "c": "c" });
	let diff = flake::diff_locks(&old, &new);
	assert_eq!(diff.unchanged, ["a"]);
	let changes: Vec<_> = diff
		.changes
		.iter()
		.map(|change| (change.input.as_str(), change.kind))
		.collect();
	assert_eq!(
		changes,
		[("b", ChangeKind::Changed), ("a/c", ChangeKind::Added)]
	);
}
