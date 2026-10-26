//! Optional real-flake evaluation from already-cached immutable sources.
use bstr::ByteSlice;
use nix_compat::store_path::build_ca_path;
use nix_rs_eval::{EvalMode, Evaluation};
use nix_rs_flakeref::{input::FetchSettings, lockfile::LockFile};
use nix_rs_glue::{EvaluatorStore, configure, fetch::FetchConfig};
use nix_rs_store::{LocalStore, LocalStoreConfig};
use std::{fs, path::Path, process::Command, rc::Rc};

fn compare_repository_flake(subdir: &str, outputs: &str) {
	let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../..")
		.canonicalize()
		.unwrap();
	let lock = LockFile::parse(
		&FetchSettings {
			flakes: true,
			..Default::default()
		},
		&fs::read_to_string(repo.join(subdir).join("flake.lock")).unwrap(),
		"flake.lock",
	)
	.unwrap();
	// Open existing host sources read-only. Preserve its reserved-space length so
	// LocalStore's initialization has no reason to resize that existing file.
	let mut config = LocalStoreConfig::system();
	config.read_only = true;
	config.reserved_size = fs::metadata(config.db_dir().join("reserved"))
		.unwrap()
		.len();
	for path in [
		config.real_store_dir.clone(),
		config.real_store_dir.join(".links"),
		config.db_dir(),
		config.state_dir.join("profiles/per-user"),
		config.state_dir.join("gcroots/per-user"),
		config.state_dir.join("temproots"),
	] {
		assert!(
			path.is_dir(),
			"missing existing source-store directory {}",
			path.display()
		);
	}
	let owner = Rc::new(LocalStore::open(config).unwrap());
	let mut missing = Vec::new();
	for node in &lock.nodes {
		let Some(node) = &node.locked else {
			continue;
		};
		if node.locked_ref.input.is_relative().is_some() {
			continue;
		}
		let input = &node.locked_ref.input;
		let hash = input
			.get_nar_hash()
			.unwrap()
			.expect("repository input must have a locked NAR hash");
		let name = input.get_name();
		let path = build_ca_path(&name, true, &hash, std::iter::empty(), false)
			.unwrap()
			.to_owned();
		if !owner.is_valid_path(&path).unwrap() {
			missing.push(path.to_absolute_path());
		}
	}
	assert!(
		missing.is_empty(),
		"populate the pinned source fixtures before this offline comparison: {missing:?}"
	);
	let rev = Command::new("git")
		.arg("-C")
		.arg(&repo)
		.args(["rev-parse", "HEAD"])
		.output()
		.unwrap();
	assert!(rev.status.success());
	let rev = String::from_utf8(rev.stdout).unwrap();
	let reference = format!(
		"git+file://{}?rev={}&dir={subdir}",
		repo.display(),
		rev.trim()
	);
	let expression = format!(
		"let f = builtins.getFlake {}; in {{ source = f.outPath; inputs = builtins.mapAttrs (_: input: input.outPath) f.inputs; inherit (f) {}; selected = ({outputs}); }}",
		serde_json::json!(reference),
		"lastModifiedDate"
	);
	let home = tempfile::tempdir().unwrap();
	let reference_result=Command::new(repo.join("target/nix-reference/bin/nix-instantiate")).env_clear().env("PATH",std::env::var_os("PATH").unwrap()).env("HOME",home.path()).env("NIX_CONF_DIR",home.path()).env("NIX_CONFIG","experimental-features = flakes nix-command read-only-local-store\nflake-registry =\nsubstituters =\nwarn-dirty = false\n")
		.args(["--store","local?read-only=true","--eval","--strict","--json","--expr",&expression]).output().unwrap();
	assert!(
		reference_result.status.success(),
		"{}",
		String::from_utf8_lossy(&reference_result.stderr)
	);
	let expected: serde_json::Value = serde_json::from_slice(&reference_result.stdout).unwrap();
	let store = EvaluatorStore::with_read_store(owner, None);
	store.configure_fetching(FetchConfig {
		cwd: repo.clone(),
		cache_dir: Some(home.path().join("candidate-cache")),
		offline: true,
		..Default::default()
	});
	let actual = configure(
		Evaluation::builder_pure().experimental_features(["flakes"]),
		store,
	)
	.mode(EvalMode::Strict)
	.build()
	.evaluate_in_dir(&format!("builtins.toJSON ({expression})"), repo);
	assert!(actual.errors.is_empty(), "{:?}", actual.errors);
	let actual: serde_json::Value = serde_json::from_str(
		actual
			.value
			.unwrap()
			.to_contextful_str()
			.unwrap()
			.to_str()
			.unwrap(),
	)
	.unwrap();
	assert_eq!(actual, expected);
}

#[test]
#[ignore = "requires every pinned repository source to already be present in the read-only host store"]
fn repository_flake_package_out_paths_match_reference_offline() {
	compare_repository_flake(
		"",
		"{ package = f.packages.x86_64-linux.default.outPath; reference = f.packages.x86_64-linux.nix-reference.outPath; }",
	);
}
#[test]
#[ignore = "requires every pinned qualification source to already be present in the read-only host store"]
fn qualification_flake_system_out_paths_match_reference_offline() {
	compare_repository_flake(
		"tests/qualification",
		"{ system = f.nixosConfigurations.reference.config.system.build.toplevel.outPath; vm = f.packages.x86_64-linux.reference-vm.outPath; }",
	);
}
