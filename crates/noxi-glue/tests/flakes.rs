use bstr::ByteSlice;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use noxi_eval::{EvalMode, Evaluation};
use noxi_glue::fetch::FetchConfig;
use noxi_glue::{EvaluatorStore, configure};
use serde_json::{Value, json};

fn reference() -> Option<PathBuf> {
	let bin = std::env::var_os("NOXI_REFERENCE_NIX")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR"))
				.join("../../target/nix-reference/bin/nix-instantiate")
		});
	if !bin.exists() {
		eprintln!("CppNix comparison skipped: run just reference or set NOXI_REFERENCE_NIX");
		return None;
	}
	Some(bin)
}

fn candidate(cwd: &Path, expression: &str) -> Value {
	let store = EvaluatorStore::new();
	let cache = tempfile::tempdir().unwrap();
	store.configure_fetching(FetchConfig {
		cwd: cwd.to_owned(),
		cache_dir: Some(cache.path().to_owned()),
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
			"experimental-features = nix-command flakes\nflake-registry =\nsubstituters =\nwarn-dirty = false\n",
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
	let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
	gzip.write_all(&bytes).unwrap();
	let mut bzip = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
	bzip.write_all(&bytes).unwrap();
	for (extension, compressed) in [
		("gz", gzip.finish().unwrap()),
		("bz2", bzip.finish().unwrap()),
		(
			"zst",
			zstd::stream::encode_all(bytes.as_slice(), 1).unwrap(),
		),
	] {
		let archive = fixture.path().join(format!("source.tar.{extension}"));
		fs::write(&archive, compressed).unwrap();
		compare(
			fixture.path(),
			&format!(
				"builtins.fetchTree {{ type = \"tarball\"; url = {}; }}",
				json!(format!("file://{}", archive.display()))
			),
		);
	}
}

#[test]
fn fetched_git_tree_is_registered_in_the_diverted_store() {
	let fixture = tempfile::tempdir().unwrap();
	repo(fixture.path());
	let root = tempfile::tempdir().unwrap();
	let store = EvaluatorStore::writable(root.path()).unwrap();
	let expression = format!(
		"let tree = builtins.fetchTree {{ type = \"git\"; url = {}; }}; in {{ path = tree.outPath; hash = tree.narHash; }}",
		json!(format!("file://{}", fixture.path().display()))
	);
	let result = configure(
		Evaluation::builder_pure().experimental_features(["flakes", "fetch-tree"]),
		store.clone(),
	)
	.mode(EvalMode::Strict)
	.build()
	.evaluate_in_dir(
		&format!("builtins.toJSON ({expression})"),
		fixture.path().to_owned(),
	);
	assert!(result.errors.is_empty(), "{:?}", result.errors);
	let actual: Value = serde_json::from_str(
		result
			.value
			.unwrap()
			.to_contextful_str()
			.unwrap()
			.to_str()
			.unwrap(),
	)
	.unwrap();
	if let Some(expected) = compare(fixture.path(), &expression) {
		assert_eq!(actual, expected);
	}
	let path = nix_compat::store_path::StorePath::from_absolute_path(
		actual["path"].as_str().unwrap().as_bytes(),
	)
	.unwrap();
	let local = store.store().unwrap();
	let info = local.query_path_info(&path).unwrap().unwrap();
	assert_eq!(
		nix_compat::nixhash::NixHash::Sha256(info.nar_hash).to_sri_string(),
		actual["hash"].as_str().unwrap()
	);
	assert_eq!(
		fs::read(local.real_path(&path).join("flake.nix")).unwrap(),
		fs::read(fixture.path().join("flake.nix")).unwrap()
	);
	fs::remove_file(fixture.path().join("flake.nix")).unwrap();
	let result = configure(Evaluation::builder_pure(), store)
		.build()
		.evaluate(
			&format!(
				"builtins.readFile ({} + \"/flake.nix\")",
				json!(path.to_absolute_path())
			),
			None,
		);
	assert!(result.errors.is_empty(), "{:?}", result.errors);
	assert_eq!(
		result
			.value
			.unwrap()
			.to_contextful_str()
			.unwrap()
			.to_str()
			.unwrap(),
		"{ outputs = { self }: { answer = 42; }; }"
	);
}

#[test]
#[ignore = "downloads the pinned Nix source from GitHub; offline tests cover local sources"]
fn github_fetch_matches_reference_live() {
	let fixture = tempfile::tempdir().unwrap();
	compare(
		fixture.path(),
		"builtins.fetchTree \"github:NixOS/nix/2c73b59da29606068c0c98db015dd3a66955525d\"",
	);
}

#[test]
#[ignore = "requires GitHub API quota; the pinned-archive comparison does not use the API"]
fn github_tag_resolution_matches_reference_live() {
	let fixture = tempfile::tempdir().unwrap();
	compare(
		fixture.path(),
		"builtins.fetchTree \"github:NixOS/nix/2.35.2\"",
	);
}

#[test]
#[ignore = "fetches the pinned Nix Git revision over HTTPS; local transport tests are offline"]
fn remote_git_fetch_matches_reference_live() {
	let fixture = tempfile::tempdir().unwrap();
	compare(
		fixture.path(),
		"builtins.fetchTree { type = \"git\"; url = \"https://github.com/NixOS/nix\"; rev = \"2c73b59da29606068c0c98db015dd3a66955525d\"; shallow = true; }",
	);
}

#[derive(Clone)]
struct LockTest {
	store: std::rc::Rc<EvaluatorStore>,
	options: noxi_glue::fetch::flake::LockOptions,
	write: PathBuf,
}

#[noxi_eval::builtin_macros::builtins(state = "std::rc::Rc<LockTest>")]
mod lock_test_builtins {
	use super::*;
	use noxi_eval::generators::{Gen, GenCo};
	use noxi_eval::{ErrorKind, Value};
	#[builtin("testLock")]
	async fn builtin_test_lock(
		state: std::rc::Rc<LockTest>,
		co: GenCo,
		value: Value,
	) -> Result<Value, ErrorKind> {
		let reference = value.to_str()?.to_str()?.to_owned();
		let locked =
			noxi_glue::fetch::flake::lock_flake(&state.store, &co, &reference, &state.options)
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
	options: noxi_glue::fetch::flake::LockOptions,
) {
	let store = EvaluatorStore::new();
	let cache = tempfile::tempdir().unwrap();
	store.configure_fetching(FetchConfig {
		cwd: cwd.to_owned(),
		cache_dir: Some(cache.path().to_owned()),
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
			"experimental-features = nix-command flakes\nflake-registry =\nsubstituters =\nwarn-dirty = false\n",
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
	let options = noxi_glue::fetch::flake::LockOptions {
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

#[test]
fn self_submodules_and_deinitialized_pinned_submodules_match_reference() {
	let fixture = tempfile::tempdir().unwrap();
	let child = fixture.path().join("child");
	let root = fixture.path().join("root");
	fs::create_dir_all(&child).unwrap();
	fs::create_dir_all(&root).unwrap();
	repo(&child);
	repo(&root);
	git(
		&root,
		&[
			"-c",
			"protocol.file.allow=always",
			"submodule",
			"add",
			"../child",
			"child",
		],
	);
	fs::write(root.join("flake.nix"), r#"{
	inputs.self.submodules = true;
	outputs = { self }: {
		child = builtins.readFile (self.outPath + "/child/flake.nix");
		info = {
			inherit (self) outPath narHash lastModified lastModifiedDate rev revCount;
			sourceInfo = builtins.map (key: { inherit key; value = self.sourceInfo.${key}; }) (builtins.attrNames self.sourceInfo);
		};
	};
}"#).unwrap();
	git(&root, &["add", "."]);
	git(&root, &["commit", "-m", "self submodules"]);
	let rev = git(&root, &["rev-parse", "HEAD"]);
	let reference = format!("git+file://{}", root.display());
	compare(
		&root,
		&format!(
			"let f = builtins.getFlake {}; in {{ inherit (f) child info; }}",
			json!(reference)
		),
	);
	git(&root, &["submodule", "deinit", "-f", "--all"]);
	compare(
		&root,
		&format!(
			"builtins.fetchTree {{ type = \"git\"; url = {}; rev = \"{rev}\"; submodules = true; }}",
			json!(format!("file://{}", root.display()))
		),
	);
	compare(
		&root,
		&format!(
			"let f = builtins.getFlake {}; in {{ inherit (f) child info; }}",
			json!(format!("{reference}?rev={rev}"))
		),
	);
}

#[test]
fn explicit_cli_overrides_are_sticky_and_match_reference_lock_bytes() {
	let fixture = tempfile::tempdir().unwrap();
	let root = fixture.path().join("root");
	let first = fixture.path().join("first");
	let second = fixture.path().join("second");
	for path in [&root, &first, &second] {
		fs::create_dir(path).unwrap();
		repo(path);
	}
	fs::write(root.join("flake.nix"), format!("{{ inputs.dep.url = \"git+file://{}\"; outputs = {{ self, dep }}: {{ inherit (dep) answer; }}; }}", first.display())).unwrap();
	let reference = format!("path:{}", root.display());
	let replacement = format!("git+file://{}", second.display());
	let write = fixture.path().join("candidate.lock");
	let options = noxi_glue::fetch::flake::LockOptions {
		override_inputs: [(vec!["dep".into()], replacement.clone())].into(),
		..Default::default()
	};
	lock_candidate(&root, &reference, &write, options);
	assert!(reference_lock(
		&root,
		&reference,
		&["--override-input", "dep", &replacement]
	));
	assert_eq!(
		fs::read(&write).unwrap(),
		fs::read(root.join("flake.lock")).unwrap()
	);
	lock_candidate(&root, &reference, &write, Default::default());
	reference_lock(&root, &reference, &[]);
	assert_eq!(
		fs::read(write).unwrap(),
		fs::read(root.join("flake.lock")).unwrap()
	);
}

#[test]
fn graph_failures_match_reference_messages() {
	fn message(kind: &noxi_eval::ErrorKind) -> &str {
		match kind {
			noxi_eval::ErrorKind::EvalError(message) => message,
			noxi_eval::ErrorKind::BytecodeError(error)
			| noxi_eval::ErrorKind::NativeError { err: error, .. } => message(&error.kind),
			noxi_eval::ErrorKind::WithContext { underlying, .. } => message(underlying),
			_ => panic!("{kind:?}"),
		}
	}
	let Some(reference) = reference() else {
		return;
	};
	let fixture = tempfile::tempdir().unwrap();
	let home = tempfile::tempdir().unwrap();
	let root = tempfile::tempdir().unwrap();
	let inputs: Vec<String> = [
		"a.follows = \"missing\";",
		"a.follows = \"a\";",
		"a.follows = \"b\"; b.follows = \"a\";",
		"a.follows = \"b/child\"; b.follows = \"a\";",
		"a.follows = \"\"; b.follows = \"a/noSuchInput\";",
	]
	.into_iter()
	.map(str::to_owned)
	.chain([format!("a.url = \"path:{}\";", fixture.path().display())])
	.collect();
	for inputs in inputs {
		fs::write(
			fixture.path().join("flake.nix"),
			format!("{{ inputs = {{ {inputs} }}; outputs = {{ self, ... }}: {{}}; }}"),
		)
		.unwrap();
		let expression = format!(
			"builtins.getFlake {}",
			json!(format!("path:{}", fixture.path().display()))
		);
		let output = Command::new(&reference)
			.env_clear()
			.env("PATH", std::env::var_os("PATH").unwrap())
			.env("HOME", home.path())
			.env("NIX_CONF_DIR", home.path())
			.env(
				"NIX_CONFIG",
				"experimental-features = flakes nix-command\nflake-registry =\nsubstituters =\n",
			)
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
		assert!(!output.status.success(), "{inputs}");
		let stderr = String::from_utf8(output.stderr).unwrap();
		let expected = stderr.rsplit_once("error: ").unwrap().1.trim();
		let store = EvaluatorStore::new();
		store.configure_fetching(FetchConfig {
			cwd: fixture.path().into(),
			cache_dir: Some(home.path().join("candidate-cache")),
			..Default::default()
		});
		let result = configure(
			Evaluation::builder_pure().experimental_features(["flakes"]),
			store,
		)
		.build()
		.evaluate_in_dir(&expression, fixture.path().into());
		assert_eq!(result.errors.len(), 1, "{inputs}: {:?}", result.errors);
		let actual = message(&result.errors[0].kind);
		assert_eq!(actual, expected, "{inputs}");
	}
	fs::write(
		fixture.path().join("flake.nix"),
		"{ outputs = { self }: {}; }",
	)
	.unwrap();
	for inputs in [
		serde_json::json!({"a":"missing"}),
		serde_json::json!({"a":"root"}),
	] {
		fs::write(
			fixture.path().join("flake.lock"),
			serde_json::to_vec(
				&serde_json::json!({"nodes":{"root":{"inputs":inputs}},"root":"root","version":7}),
			)
			.unwrap(),
		)
		.unwrap();
		let expression = format!(
			"builtins.getFlake {}",
			json!(format!("path:{}", fixture.path().display()))
		);
		let output = Command::new(&reference)
			.env_clear()
			.env("PATH", std::env::var_os("PATH").unwrap())
			.env("HOME", home.path())
			.env("NIX_CONF_DIR", home.path())
			.env(
				"NIX_CONFIG",
				"experimental-features = flakes nix-command\nflake-registry =\nsubstituters =\n",
			)
			.args([
				"--store",
				&format!("local?root={}", root.path().display()),
				"--eval",
				"--expr",
				&expression,
			])
			.output()
			.unwrap();
		assert!(!output.status.success());
		let stderr = String::from_utf8(output.stderr).unwrap();
		let expected = stderr.rsplit_once("error: ").unwrap().1.trim();
		let store = EvaluatorStore::new();
		store.configure_fetching(FetchConfig {
			cwd: fixture.path().into(),
			cache_dir: Some(home.path().join("candidate-cache")),
			..Default::default()
		});
		let result = configure(
			Evaluation::builder_pure().experimental_features(["flakes"]),
			store,
		)
		.build()
		.evaluate(&expression, None);
		assert_eq!(message(&result.errors[0].kind), expected);
	}
	fs::remove_file(fixture.path().join("flake.lock")).unwrap();
	repo(fixture.path());
	fs::write(
		fixture.path().join("flake.nix"),
		format!(
			"{{ inputs.child.url = \"git+file://{}\"; outputs = {{ self, child }}: {{}}; }}",
			fixture.path().display()
		),
	)
	.unwrap();
	git(fixture.path(), &["add", "."]);
	git(fixture.path(), &["commit", "-m", "unlocked child"]);
	let rev = git(fixture.path(), &["rev-parse", "HEAD"]);
	let expression = format!(
		"builtins.getFlake {}",
		json!(format!("git+file://{}?rev={rev}", fixture.path().display()))
	);
	let output = Command::new(&reference)
		.env_clear()
		.env("PATH", std::env::var_os("PATH").unwrap())
		.env("HOME", home.path())
		.env("NIX_CONF_DIR", home.path())
		.env(
			"NIX_CONFIG",
			"experimental-features = flakes nix-command\nsubstituters =\nflake-registry =\n",
		)
		.args([
			"--store",
			&format!("local?root={}", root.path().display()),
			"--pure-eval",
			"--eval",
			"--expr",
			&expression,
		])
		.output()
		.unwrap();
	assert!(!output.status.success());
	let stderr = String::from_utf8(output.stderr).unwrap();
	let expected = stderr.rsplit_once("error: ").unwrap().1.trim();
	let store = EvaluatorStore::new();
	store.configure_fetching(FetchConfig {
		pure_eval: true,
		cache_dir: Some(home.path().join("candidate-cache")),
		..Default::default()
	});
	let result = configure(
		Evaluation::builder_pure().experimental_features(["flakes"]),
		store,
	)
	.build()
	.evaluate(&expression, None);
	assert_eq!(message(&result.errors[0].kind), expected);
}
