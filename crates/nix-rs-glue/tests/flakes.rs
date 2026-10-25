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
	// JSON conversion coerces sets with outPath to strings. List each field
	// explicitly so metadata is compared alongside the store path.
	let expression = format!(
		"let result = ({expression}); in if builtins.isAttrs result && result ? outPath then builtins.map (key: {{ inherit key; value = result.${{key}}; }}) (builtins.attrNames result) else result"
	);
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
			&expression,
		])
		.output()
		.unwrap();
	assert!(
		output.status.success(),
		"{expression}: {}",
		String::from_utf8_lossy(&output.stderr)
	);
	let expected: Value = serde_json::from_slice(&output.stdout).unwrap();
	let actual = candidate(cwd, &expression);
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

#[derive(Clone)]
struct LockTest {
	store: std::rc::Rc<EvaluatorStore>,
	options: nix_rs_glue::fetch::flake::LockOptions,
	write: PathBuf,
}

#[nix_rs_eval::builtin_macros::builtins(state = "std::rc::Rc<LockTest>")]
mod lock_test_builtins {
	use super::*;
	use nix_rs_eval::generators::{Gen, GenCo};
	use nix_rs_eval::{ErrorKind, Value};
	#[builtin("testLock")]
	async fn builtin_test_lock(
		state: std::rc::Rc<LockTest>,
		co: GenCo,
		value: Value,
	) -> Result<Value, ErrorKind> {
		let reference = value.to_str()?.to_str()?.to_owned();
		let locked =
			nix_rs_glue::fetch::flake::lock_flake(&state.store, &co, &reference, &state.options)
				.await?;
		locked
			.write_lock_file(&state.write)
			.map_err(|e| ErrorKind::EvalError(e.to_string()))?;
		Ok(Value::from(locked.lock_file.render().unwrap()))
	}
}

fn lock_candidate(
	cwd: &Path,
	reference: &str,
	write: &Path,
	options: nix_rs_glue::fetch::flake::LockOptions,
) {
	let store = EvaluatorStore::new();
	store.configure_fetching(FetchConfig {
		cwd: cwd.to_owned(),
		..FetchConfig::default()
	});
	let state = std::rc::Rc::new(LockTest {
		store: store.clone(),
		options,
		write: write.to_owned(),
	});
	let result = configure(
		Evaluation::builder_pure().experimental_features(["flakes", "fetch-tree"]),
		store,
	)
	.add_builtins(lock_test_builtins::builtins(state))
	.mode(EvalMode::Strict)
	.build()
	.evaluate_in_dir(
		&format!("builtins.testLock {}", json!(reference)),
		cwd.to_owned(),
	);
	assert!(result.errors.is_empty(), "{:?}", result.errors);
}

fn reference_lock(cwd: &Path, flake_ref: &str, extra: &[&str]) -> bool {
	let Some(bin) = reference() else {
		return false;
	};
	let root = tempfile::tempdir().unwrap();
	let home = tempfile::tempdir().unwrap();
	let result = Command::new(bin.with_file_name("nix"))
		.current_dir(cwd)
		.env_clear()
		.env("PATH", std::env::var_os("PATH").unwrap())
		.env("HOME", home.path())
		.env("NIX_CONF_DIR", home.path())
		.env(
			"NIX_CONFIG",
			"experimental-features = nix-command flakes\nflake-registry =\nwarn-dirty = false\n",
		)
		.args([
			"--store",
			&format!("local?root={}", root.path().display()),
			"flake",
			"lock",
			flake_ref,
		])
		.args(extra)
		.output()
		.unwrap();
	assert!(
		result.status.success(),
		"{}",
		String::from_utf8_lossy(&result.stderr)
	);
	true
}

#[test]
fn relative_inputs_follows_and_outputs_match_reference() {
	let fixture = tempfile::tempdir().unwrap();
	let root = fixture.path().join("root");
	fs::create_dir_all(root.join("child")).unwrap();
	fs::write(root.join("flake.nix"), r#"{
	inputs.child.url = "path:./child";
	inputs.alias.follows = "child";
	outputs = { self, child, alias }: { answer = child.answer + alias.answer; recursive = self.answer; };
}"#).unwrap();
	fs::write(
		root.join("child/flake.nix"),
		"{ outputs = { self }: { answer = 21; }; }",
	)
	.unwrap();
	let reference = format!("path:{}", root.display());
	let candidate_lock = fixture.path().join("candidate.lock");
	lock_candidate(&root, &reference, &candidate_lock, Default::default());
	if !reference_lock(&root, &reference, &[]) {
		return;
	}
	assert_eq!(
		fs::read(&candidate_lock).unwrap(),
		fs::read(root.join("flake.lock")).unwrap()
	);
	let expression = format!(
		"let f = builtins.getFlake {}; in {{ inherit (f) answer recursive outPath sourceInfo; child = f.inputs.child.outPath; alias = f.inputs.alias.outPath; same = f.inputs.child == f.inputs.alias; }}",
		json!(reference)
	);
	compare(&root, &expression);
	lock_candidate(&root, &reference, &candidate_lock, Default::default());
	assert_eq!(
		fs::read(candidate_lock).unwrap(),
		fs::read(root.join("flake.lock")).unwrap()
	);
}

#[test]
fn git_lock_updates_match_reference_bytes() {
	let fixture = tempfile::tempdir().unwrap();
	let dependency = fixture.path().join("dependency");
	let root = fixture.path().join("root");
	fs::create_dir_all(&root).unwrap();
	fs::create_dir_all(&dependency).unwrap();
	repo(&dependency);
	fs::write(root.join("flake.nix"), format!("{{ inputs.dep.url = \"git+file://{}\"; outputs = {{ self, dep }}: {{ answer = dep.answer; }}; }}", dependency.display())).unwrap();
	let reference = format!("path:{}", root.display());
	let candidate_lock = fixture.path().join("candidate.lock");
	lock_candidate(&root, &reference, &candidate_lock, Default::default());
	if !reference_lock(&root, &reference, &[]) {
		return;
	}
	assert_eq!(
		fs::read(&candidate_lock).unwrap(),
		fs::read(root.join("flake.lock")).unwrap()
	);
	fs::write(
		dependency.join("flake.nix"),
		"{ outputs = { self }: { answer = 43; }; }",
	)
	.unwrap();
	git(&dependency, &["add", "."]);
	git(&dependency, &["commit", "-m", "second"]);
	lock_candidate(&root, &reference, &candidate_lock, Default::default());
	assert_eq!(
		fs::read(&candidate_lock).unwrap(),
		fs::read(root.join("flake.lock")).unwrap()
	);
	compare(
		&root,
		&format!("(builtins.getFlake {}).answer", json!(reference)),
	);
	let options = nix_rs_glue::fetch::flake::LockOptions {
		update_inputs: [vec!["dep".into()]].into(),
		..Default::default()
	};
	lock_candidate(&root, &reference, &candidate_lock, options);
	reference_lock(&root, &reference, &["--update-input", "dep"]);
	assert_eq!(
		fs::read(candidate_lock).unwrap(),
		fs::read(root.join("flake.lock")).unwrap()
	);
	compare(
		&root,
		&format!("(builtins.getFlake {}).answer", json!(reference)),
	);
}

#[test]
fn nested_input_overrides_match_reference() {
	let fixture = tempfile::tempdir().unwrap();
	let root = fixture.path().join("root");
	for name in ["root", "branch", "leaf"] {
		fs::create_dir_all(fixture.path().join(name)).unwrap();
	}
	fs::write(
		root.join("flake.nix"),
		format!(
			r#"{{
	inputs.branch.url = "path:{}";
	inputs.leaf.url = "path:{}";
	inputs.branch.inputs.leaf.follows = "leaf";
	outputs = {{ self, branch, leaf }}: {{ answer = branch.answer; }};
}}"#,
			fixture.path().join("branch").display(),
			fixture.path().join("leaf").display()
		),
	)
	.unwrap();
	fs::write(
		fixture.path().join("branch/flake.nix"),
		format!(
			"{{ inputs.leaf.url = \"path:{}\"; outputs = {{ self, leaf }}: {{ answer = leaf.answer; }}; }}",
			fixture.path().join("leaf").display()
		),
	)
	.unwrap();
	fs::write(
		fixture.path().join("leaf/flake.nix"),
		"{ outputs = { self }: { answer = 42; }; }",
	)
	.unwrap();
	let flake_ref = format!("path:{}", root.display());
	let candidate_lock = fixture.path().join("candidate.lock");
	lock_candidate(&root, &flake_ref, &candidate_lock, Default::default());
	if !reference_lock(&root, &flake_ref, &[]) {
		return;
	}
	assert_eq!(
		fs::read_to_string(&candidate_lock).unwrap(),
		fs::read_to_string(root.join("flake.lock")).unwrap()
	);
	compare(
		&root,
		&format!("(builtins.getFlake {}).answer", json!(flake_ref)),
	);
	// Removing an override must restore the dependency's own input graph.
	fs::write(root.join("flake.nix"), format!("{{ inputs.branch.url = \"path:{}\"; outputs = {{ self, branch }}: {{ answer = branch.answer; }}; }}", fixture.path().join("branch").display())).unwrap();
	lock_candidate(&root, &flake_ref, &candidate_lock, Default::default());
	reference_lock(&root, &flake_ref, &[]);
	assert_eq!(
		fs::read_to_string(&candidate_lock).unwrap(),
		fs::read_to_string(root.join("flake.lock")).unwrap()
	);
	compare(
		&root,
		&format!("(builtins.getFlake {}).answer", json!(flake_ref)),
	);
}

#[test]
fn initialized_git_submodules_match_reference() {
	let fixture = tempfile::tempdir().unwrap();
	let root = fixture.path().join("root");
	let child = fixture.path().join("child");
	fs::create_dir(&root).unwrap();
	fs::create_dir(&child).unwrap();
	repo(&root);
	repo(&child);
	git(
		&root,
		&[
			"-c",
			"protocol.file.allow=always",
			"submodule",
			"add",
			child.to_str().unwrap(),
			"child",
		],
	);
	git(&root, &["commit", "-m", "submodule"]);
	let url = json!(format!("file://{}", root.display()));
	for submodules in [false, true] {
		compare(
			&root,
			&format!(
				"builtins.fetchTree {{ type = \"git\"; url = {url}; submodules = {}; }}",
				if submodules { "true" } else { "false" }
			),
		);
	}
	fs::write(root.join("child/flake.nix"), "dirty child").unwrap();
	compare(
		&root,
		&format!("builtins.fetchGit {{ url = {url}; submodules = true; }}"),
	);
}
