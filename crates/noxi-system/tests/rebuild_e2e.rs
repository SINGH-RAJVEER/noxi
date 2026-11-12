//! `noxi system build` through the in-process compatibility builder, in a
//! diverted `local?root=` store with no substituters. The toplevel is a
//! `builtin:buildenv` derivation, which needs no builder executable inside
//! the store, linking a source directory that holds
//! `bin/switch-to-configuration`.
//!
//! The store, configuration, profile, running-system link and journal are
//! diverted through the process environment, so the tests here run one at a
//! time behind `isolate`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, OnceLock};

use noxi_system::rebuild;
use noxi_system::snapshot::{Action, SnapshotStore, Status};

static SERIAL: Mutex<()> = Mutex::new(());

/// Divert everything the build and the journal touch to one private
/// directory, once, before any test runs a build.
fn isolate() -> &'static Path {
	static ROOT: OnceLock<PathBuf> = OnceLock::new();
	ROOT.get_or_init(|| {
		let root = tempfile::Builder::new()
			.prefix("noxi-system-rebuild-")
			.tempdir_in(env!("CARGO_TARGET_TMPDIR"))
			.unwrap()
			.keep();
		let config = format!(
			"store = local?root={}\nsubstituters =\nflake-registry =\nwarn-dirty = false\nbuild-users-group =\n",
			root.join("store").display()
		);
		let dirs = [
			("HOME", root.join("home")),
			("XDG_CACHE_HOME", root.join("cache")),
			("XDG_CONFIG_HOME", root.join("config")),
			("XDG_DATA_HOME", root.join("data")),
			("XDG_STATE_HOME", root.join("state")),
			("NIX_CONF_DIR", root.join("etc")),
			("NOXI_SNAPSHOT_DIR", root.join("snapshots")),
		];
		// SAFETY: this runs once, before any build, and every test calls it
		// first while holding `SERIAL`.
		unsafe {
			for (name, value) in &dirs {
				fs::create_dir_all(value).unwrap();
				std::env::set_var(name, value);
			}
			std::env::set_var("NOXI_SYSTEM_PROFILE", root.join("profiles/system"));
			std::env::set_var("NOXI_CURRENT_SYSTEM", root.join("current-system"));
			std::env::set_var("NIX_USER_CONF_FILES", "");
			std::env::set_var("NIX_CONFIG", config);
			std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
			std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
			std::env::remove_var("NIX_REMOTE");
			std::env::remove_var("NIX_PATH");
			std::env::remove_var("NOXI_ACTIVATION_PROGRAM");
		}
		root
	})
}

fn run(args: &[&str]) -> (ExitCode, String, String) {
	let (mut out, mut err) = (Vec::new(), Vec::new());
	let code = rebuild::run(
		args.iter().map(|arg| (*arg).to_owned()).collect(),
		&mut out,
		&mut err,
	);
	(
		code,
		String::from_utf8(out).unwrap(),
		String::from_utf8(err).unwrap(),
	)
}

/// A directory `pkg` with an executable `bin/switch-to-configuration` and
/// an empty `manifest`, inside `dir`.
fn package(dir: &Path) {
	let bin = dir.join("pkg/bin");
	fs::create_dir_all(&bin).unwrap();
	let program = bin.join("switch-to-configuration");
	fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
	fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
	fs::write(dir.join("pkg/manifest"), "").unwrap();
}

const TOPLEVEL: &str = r#"derivation {
	name = "nixos-system-test";
	system = "builtin";
	builder = "builtin:buildenv";
	derivations = "true 5 1 ${PKG}";
	manifest = "${PKG}/manifest";
}"#;

/// Check that the out-link points at a store path whose
/// `bin/switch-to-configuration` exists, resolving `/nix/store` links
/// through the diverted store root.
fn assert_toplevel(root: &Path, link: &Path) -> PathBuf {
	let closure = fs::read_link(link).unwrap();
	assert!(closure.starts_with("/nix/store/"), "{}", closure.display());
	let physical = |path: &Path| root.join("store").join(path.strip_prefix("/").unwrap());
	let bin = fs::read_link(physical(&closure).join("bin")).unwrap();
	assert!(
		physical(&bin).join("switch-to-configuration").is_file(),
		"{}",
		bin.display()
	);
	closure
}

#[test]
fn file_build_realizes_the_toplevel_in_process() {
	let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
	let root = isolate();
	let dir = tempfile::tempdir().unwrap();
	package(dir.path());
	fs::write(
		dir.path().join("system.nix"),
		format!(
			"{{ host.config.system.build.toplevel = {}; }}",
			TOPLEVEL.replace("PKG", "./pkg")
		),
	)
	.unwrap();
	let link = dir.path().join("result");
	let (code, out, err) = run(&[
		"build",
		"--file",
		dir.path().to_str().unwrap(),
		"--attr",
		"host",
		"--out-link",
		link.to_str().unwrap(),
	]);
	assert_eq!(code, ExitCode::SUCCESS, "{err}");
	let closure = assert_toplevel(root, &link);
	assert!(
		out.contains(&format!("closure: {}", closure.display())),
		"{out}"
	);
	assert!(out.contains("no activation occurred"), "{out}");
	let record = SnapshotStore::default_location()
		.unwrap()
		.latest()
		.unwrap()
		.unwrap();
	assert_eq!(record.action, Action::Build);
	assert_eq!(record.outcome.status, Status::Success);
	assert_eq!(record.closure, Some(closure));
}

#[test]
fn flake_build_with_update_realizes_the_host_toplevel_in_process() {
	let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
	let root = isolate();
	let dir = tempfile::tempdir().unwrap();
	let extra = dir.path().join("extra");
	fs::create_dir(&extra).unwrap();
	fs::write(extra.join("value"), "1").unwrap();
	let flake = dir.path().join("flake");
	fs::create_dir(&flake).unwrap();
	package(&flake);
	fs::write(
		flake.join("flake.nix"),
		format!(
			"{{ inputs.extra = {{ url = \"path:{}\"; flake = false; }}; outputs = {{ self, extra }}: {{ nixosConfigurations.host.config.system.build.toplevel = {}; }}; }}",
			extra.display(),
			TOPLEVEL.replace("PKG", "self + \"/pkg\"")
		),
	)
	.unwrap();
	let link = dir.path().join("result");
	let reference = format!("{}#host", flake.display());
	let (code, out, err) = run(&[
		"build",
		"--flake",
		&reference,
		"--update",
		"--json",
		"--out-link",
		link.to_str().unwrap(),
	]);
	assert_eq!(code, ExitCode::SUCCESS, "{err}");
	assert!(err.contains("update: created "), "{err}");
	let closure = assert_toplevel(root, &link);
	let value: serde_json::Value = serde_json::from_str(&out).unwrap();
	assert_eq!(value["closure"], closure.to_str().unwrap());
	assert_eq!(value["update"]["changes"][0]["input"], "extra");
	assert_eq!(value["update"]["changes"][0]["kind"], "added");
	assert_eq!(
		value["source"]["attribute"],
		"nixosConfigurations.\"host\".config.system.build.toplevel"
	);
	assert!(flake.join("flake.lock").is_file());
	// The record keeps the lock the closure was built from.
	let journal = SnapshotStore::default_location().unwrap();
	let record = journal.latest().unwrap().unwrap();
	assert_eq!(record.outcome.status, Status::Success);
	assert_eq!(
		journal.lock_bytes(&record).unwrap(),
		fs::read(flake.join("flake.lock")).unwrap()
	);
}
