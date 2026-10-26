use bstr::ByteSlice;
use nix_rs_eval::{EvalMode, Evaluation};
use nix_rs_flake::{CheckOptions, LockOptions, ShowOptions};
use nix_rs_glue::{EvaluatorStore, configure, fetch::FetchConfig};
use serde_json::{Value as Json, json};
use std::{
	fs,
	path::{Path, PathBuf},
	process::Command,
	rc::Rc,
};

struct Operation {
	store: Rc<EvaluatorStore>,
	reference: String,
	command: &'static str,
	options: LockOptions,
	write: Option<PathBuf>,
	all_systems: bool,
}
#[nix_rs_eval::builtin_macros::builtins(state = "Rc<Operation>")]
mod test_builtins {
	use super::*;
	use nix_rs_eval::{
		ErrorKind, Value,
		generators::{Gen, GenCo},
	};
	#[builtin("runOperation")]
	async fn builtin_operation(
		state: Rc<Operation>,
		co: GenCo,
		_arg: Value,
	) -> Result<Value, ErrorKind> {
		let result = if state.command == "prefetch" {
			let result = nix_rs_flake::prefetch(&state.store, &state.reference)?;
			let path = nix_compat::store_path::StorePath::from_absolute_path(
				result["storePath"].as_str().unwrap().as_bytes(),
			)
			.unwrap();
			assert!(state.store.store().unwrap().is_valid_path(&path).unwrap());
			assert!(state.store.store().unwrap().real_path(&path).exists());
			result
		} else {
			let flake = if state.command == "update" {
				nix_rs_flake::update(&state.store, &co, &state.reference, &state.options).await?
			} else {
				nix_rs_flake::lock(&state.store, &co, &state.reference, &state.options).await?
			};
			if let Some(path) = &state.write {
				flake
					.write_lock_file(path)
					.map_err(|e| ErrorKind::EvalError(e.to_string()))?;
			}
			match state.command {
				"lock" | "update" => json!(flake.lock_file.render().unwrap() + "\n"),
				"metadata" => nix_rs_flake::metadata(&state.store, &flake)?,
				"metadata-twice" => {
					let first = nix_rs_flake::metadata(&state.store, &flake)?;
					let second =
						nix_rs_flake::lock(&state.store, &co, &state.reference, &state.options)
							.await?;
					assert_eq!(first, nix_rs_flake::metadata(&state.store, &second)?);
					first
				}
				"archive" => nix_rs_flake::archive(&state.store, &flake)?,
				"show" => {
					nix_rs_flake::show(
						&state.store,
						&co,
						&flake,
						&ShowOptions {
							all_systems: state.all_systems,
							..Default::default()
						},
					)
					.await?
					.json
				}
				"check" => {
					let report = nix_rs_flake::check(
						&state.store,
						&co,
						&flake,
						&CheckOptions {
							build: true,
							all_systems: state.all_systems,
							..Default::default()
						},
					)
					.await?;
					for drv in &report.check_builds {
						let drv =
							nix_compat::store_path::StorePath::from_absolute_path(drv.as_bytes())
								.unwrap();
						let store = state.store.store().unwrap();
						for output in store.query_derivation_outputs(&drv).unwrap().values() {
							assert_eq!(fs::read(store.real_path(output)).unwrap(), b"checked\n");
						}
					}
					json!({"derivations":report.derivations,"builds":report.check_builds,"warnings":report.warnings})
				}
				_ => unreachable!(),
			}
		};
		Ok(Value::from(serde_json::to_string(&result).unwrap()))
	}
}
fn git(path: &Path, args: &[&str]) -> String {
	let result = Command::new("git")
		.arg("-C")
		.arg(path)
		.args(args)
		.env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
		.env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
		.output()
		.unwrap();
	assert!(
		result.status.success(),
		"{}",
		String::from_utf8_lossy(&result.stderr)
	);
	String::from_utf8(result.stdout).unwrap().trim().into()
}
fn repo(path: &Path) {
	git(path, &["init", "-b", "main"]);
	git(path, &["config", "user.name", "Flake test"]);
	git(path, &["config", "user.email", "flake@example.invalid"]);
	git(path, &["add", "."]);
	git(path, &["commit", "-m", "initial"]);
}
fn reference(cwd: &Path, operation: &str, reference: &str, flags: &[&str]) -> std::process::Output {
	let home = tempfile::tempdir().unwrap();
	let store = tempfile::tempdir().unwrap();
	Command::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin/nix")).env_clear().env("PATH",std::env::var_os("PATH").unwrap()).env("HOME",home.path()).env("NIX_CONF_DIR",home.path()).env("NIX_CONFIG","experimental-features = flakes nix-command\nsubstituters =\nflake-registry =\nwarn-dirty = false\n").current_dir(cwd)
	.args(["--store",&format!("local?root={}",store.path().display()),"flake",operation,reference]).args(if operation == "lock" {Vec::new()} else {vec!["--no-write-lock-file"]}).args(flags).output().unwrap()
}
fn run(
	cwd: &Path,
	command: &'static str,
	reference: &str,
	options: LockOptions,
	write: Option<PathBuf>,
	all_systems: bool,
) -> Result<Json, String> {
	run_with_overrides(
		cwd,
		command,
		reference,
		options,
		write,
		all_systems,
		Vec::new(),
	)
}
fn run_with_overrides(
	cwd: &Path,
	command: &'static str,
	reference: &str,
	options: LockOptions,
	write: Option<PathBuf>,
	all_systems: bool,
	overrides: Vec<(String, String)>,
) -> Result<Json, String> {
	let root = tempfile::tempdir().unwrap();
	let cache = tempfile::tempdir().unwrap();
	let store = if command == "check" {
		let config = nix_rs_store::LocalStoreConfig::diverted(root.path()).unwrap();
		let directory = config.real_store_dir.clone();
		let backend = nix_rs_build::backend::LocalBackend::new(
			nix_rs_store::LocalStore::open(config).unwrap(),
			nix_rs_build::settings::BuildSettings::default(),
			Vec::new(),
		);
		EvaluatorStore::with_store(Rc::new(backend), Some(directory))
	} else {
		EvaluatorStore::writable(root.path()).unwrap()
	};
	store.configure_fetching(FetchConfig {
		settings: nix_rs_flakeref::input::FetchSettings {
			flake_registry: String::new(),
			..Default::default()
		},
		offline: true,
		override_flakes: overrides,
		cwd: cwd.into(),
		cache_dir: Some(cache.path().into()),
		..Default::default()
	});
	let operation = Rc::new(Operation {
		store: store.clone(),
		command,
		reference: reference.into(),
		options,
		write,
		all_systems,
	});
	let result = configure(
		Evaluation::builder_pure().experimental_features(["flakes"]),
		store,
	)
	.add_builtins(test_builtins::builtins(operation))
	.mode(EvalMode::Strict)
	.build()
	.evaluate("builtins.runOperation null", None);
	if !result.errors.is_empty() {
		fn message(kind: &nix_rs_eval::ErrorKind) -> String {
			match kind {
				nix_rs_eval::ErrorKind::BytecodeError(error)
				| nix_rs_eval::ErrorKind::NativeError { err: error, .. } => message(&error.kind),
				nix_rs_eval::ErrorKind::WithContext { underlying, .. } => message(underlying),
				_ => kind.to_string(),
			}
		}
		return Err(message(&result.errors[0].kind));
	}
	Ok(serde_json::from_str(
		result
			.value
			.unwrap()
			.to_contextful_str()
			.unwrap()
			.to_str()
			.unwrap(),
	)
	.unwrap())
}
fn compare(
	cwd: &Path,
	command: &'static str,
	reference_value: &str,
	flags: &[&str],
	all_systems: bool,
) -> Json {
	let result = reference(cwd, command, reference_value, flags);
	assert!(
		result.status.success(),
		"{}",
		String::from_utf8_lossy(&result.stderr)
	);
	let expected: Json = serde_json::from_slice(&result.stdout).unwrap();
	let actual = run(
		cwd,
		command,
		reference_value,
		Default::default(),
		None,
		all_systems,
	)
	.unwrap();
	assert_eq!(actual, expected, "{command} {reference_value}");
	actual
}
#[test]
fn metadata_and_prefetch_json_match_reference() {
	let fixture = tempfile::tempdir().unwrap();
	fs::write(fixture.path().join("flake.nix"),"{ description = \"Fixture description\"; outputs = { self }: throw \"metadata must not evaluate outputs\"; }").unwrap();
	repo(fixture.path());
	let rev = git(fixture.path(), &["rev-parse", "HEAD"]);
	for input in [
		format!("path:{}", fixture.path().display()),
		format!("git+file://{}?rev={rev}", fixture.path().display()),
	] {
		compare(fixture.path(), "metadata", &input, &["--json"], false);
		compare(fixture.path(), "prefetch", &input, &["--json"], false);
	}
}
#[test]
fn archive_graph_json_matches_reference() {
	let fixture = tempfile::tempdir().unwrap();
	let child = fixture.path().join("child");
	let external = tempfile::tempdir().unwrap();
	fs::create_dir(&child).unwrap();
	fs::write(child.join("flake.nix"), "{ outputs = { self }: {}; }").unwrap();
	fs::write(external.path().join("file"), "non-flake source").unwrap();
	fs::write(fixture.path().join("flake.nix"),format!("{{ inputs.child.url = \"./child\"; inputs.source = {{ url = \"path:{}\"; flake = false; }}; inputs.alias.follows = \"child\"; outputs = {{ self, ... }}: {{}}; }}",external.path().display())).unwrap();
	let result = compare(
		fixture.path(),
		"archive",
		&format!("path:{}", fixture.path().display()),
		&["--json"],
		false,
	);
	assert!(result["inputs"]["child"].get("path").is_none());
	assert!(result["inputs"].get("alias").is_none());
}
#[test]
fn show_and_check_conventional_outputs_match_reference() {
	let fixture = tempfile::tempdir().unwrap();
	let busybox = Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../target/test-busybox")
		.canonicalize()
		.unwrap()
		.join("bin/busybox");
	fs::create_dir(fixture.path().join("bin")).unwrap();
	fs::copy(busybox, fixture.path().join("bin/busybox")).unwrap();
	let system = format!("{}-linux", std::env::consts::ARCH);
	fs::write(fixture.path().join("flake.nix"),format!(r#"{{ outputs = {{ self }}: let
	drv = builtins.derivation {{ name = "fixture"; system = "{system}"; builder = "${{builtins.path {{ path = ./bin; name = "builder"; }}}}/busybox"; args = ["sh" "-c" "echo checked > $out"]; }};
	in {{
		packages.{system}.default = drv // {{ meta.description = "Fixture package"; }};
		checks.{system}.smoke = drv;
		packages.other-system.foreign = drv;
		packages.empty-system = {{}};
		devShells.{system}.default = drv;
		apps.{system}.default = {{ type = "app"; program = "/bin/true"; meta.description = "Fixture app"; }};
		templates.default = {{ path = ./.; description = "Fixture template"; }};
		overlays.default = final: prev: {{}};
		nixosModules.default = {{}};
		unforced = throw "show must not force unknown output";
	}}; }}"#)).unwrap();
	let input = format!("path:{}", fixture.path().display());
	compare(fixture.path(), "show", &input, &["--json"], false);
	compare(
		fixture.path(),
		"show",
		&input,
		&["--json", "--all-systems"],
		true,
	);
	// Check evaluates every top-level output, including unknown outputs.
	let source = fs::read_to_string(fixture.path().join("flake.nix"))
		.unwrap()
		.replace("throw \"show must not force unknown output\"", "42");
	fs::write(fixture.path().join("flake.nix"), source).unwrap();
	let expected = reference(fixture.path(), "check", &input, &[]);
	assert!(
		expected.status.success(),
		"{}",
		String::from_utf8_lossy(&expected.stderr)
	);
	let report = run(
		fixture.path(),
		"check",
		&input,
		Default::default(),
		None,
		false,
	)
	.unwrap();
	assert_eq!(report["builds"].as_array().unwrap().len(), 1);
	assert_eq!(report["derivations"].as_object().unwrap().len(), 3);
}

#[test]
fn registry_metadata_preserves_resolved_reference_across_cache_hits() {
	let fixture = tempfile::tempdir().unwrap();
	fs::write(
		fixture.path().join("flake.nix"),
		"{ outputs = { self }: {}; }",
	)
	.unwrap();
	repo(fixture.path());
	let target = format!("git+file://{}", fixture.path().display());
	let expected = reference(
		fixture.path(),
		"metadata",
		"fixture",
		&["--json", "--override-flake", "fixture", &target],
	);
	assert!(
		expected.status.success(),
		"{}",
		String::from_utf8_lossy(&expected.stderr)
	);
	let expected: Json = serde_json::from_slice(&expected.stdout).unwrap();
	let actual = run_with_overrides(
		fixture.path(),
		"metadata-twice",
		"fixture",
		Default::default(),
		None,
		false,
		vec![("fixture".into(), target)],
	)
	.unwrap();
	assert_eq!(actual, expected);
}

#[test]
fn check_schema_errors_match_reference_messages() {
	let fixture = tempfile::tempdir().unwrap();
	let system = format!("{}-linux", std::env::consts::ARCH);
	for output in [
		format!("packages.{system}.default = 42;"),
		format!("apps.{system}.default = {{ program = \"/bin/true\"; }};"),
		format!(
			"apps.{system}.default = {{ type = \"app\"; program = \"/bin/true\"; unsupported = true; }};"
		),
		"templates.default = { description = \"Fixture\"; };".into(),
		"overlays.default = wrong: prev: {};".into(),
		format!("bundlers.{system}.default = 42;"),
	] {
		fs::write(
			fixture.path().join("flake.nix"),
			format!("{{ outputs = {{ self }}: {{ {output} }}; }}"),
		)
		.unwrap();
		let input = format!("path:{}", fixture.path().display());
		let expected = reference(fixture.path(), "check", &input, &["--no-build"]);
		assert!(!expected.status.success(), "{output}");
		let stderr = String::from_utf8(expected.stderr).unwrap();
		let expected = stderr.rsplit_once("error: ").unwrap().1.trim();
		let actual = run(
			fixture.path(),
			"check",
			&input,
			Default::default(),
			None,
			false,
		)
		.unwrap_err();
		assert_eq!(actual, expected, "{output}");
	}
}

#[test]
fn library_lock_selective_update_and_full_update_match_reference_bytes() {
	let fixture = tempfile::tempdir().unwrap();
	let root = fixture.path().join("root");
	let a = fixture.path().join("a");
	let b = fixture.path().join("b");
	for path in [&root, &a, &b] {
		fs::create_dir(path).unwrap();
		fs::write(path.join("flake.nix"), "{ outputs = { self }: {}; }").unwrap();
	}
	repo(&a);
	repo(&b);
	fs::write(root.join("flake.nix"),format!("{{ inputs.a.url = \"git+file://{}\"; inputs.b.url = \"git+file://{}\"; outputs = {{ self, ... }}: {{}}; }}",a.display(),b.display())).unwrap();
	let input = format!("path:{}", root.display());
	let initial = run(&root, "lock", &input, Default::default(), None, false).unwrap();
	let expected = reference(&root, "lock", &input, &[]);
	assert!(
		expected.status.success(),
		"{}",
		String::from_utf8_lossy(&expected.stderr)
	);
	assert_eq!(
		initial.as_str().unwrap(),
		fs::read_to_string(root.join("flake.lock")).unwrap()
	);
	for path in [&a, &b] {
		fs::write(path.join("new"), "new commit").unwrap();
		git(path, &["add", "."]);
		git(path, &["commit", "-m", "update"]);
	}
	let selected = run(
		&root,
		"update",
		&input,
		LockOptions {
			update_inputs: [vec!["a".into()]].into(),
			..Default::default()
		},
		None,
		false,
	)
	.unwrap();
	let expected = reference(&root, "lock", &input, &["--update-input", "a"]);
	assert!(
		expected.status.success(),
		"{}",
		String::from_utf8_lossy(&expected.stderr)
	);
	assert_eq!(
		selected.as_str().unwrap(),
		fs::read_to_string(root.join("flake.lock")).unwrap()
	);
	let all = run(&root, "update", &input, Default::default(), None, false).unwrap();
	let expected = reference(&root, "lock", &input, &["--recreate-lock-file"]);
	assert!(
		expected.status.success(),
		"{}",
		String::from_utf8_lossy(&expected.stderr)
	);
	assert_eq!(
		all.as_str().unwrap(),
		fs::read_to_string(root.join("flake.lock")).unwrap()
	);
}
