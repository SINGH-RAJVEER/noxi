//! Offline registry resolution through the actual evaluator command.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

fn registry(path: &Path, target: &Path) {
	fs::write(path, json!({"version": 2, "flakes": [{"from": {"type": "indirect", "id": "local"}, "to": {"type": "path", "path": target}}]}).to_string()).unwrap();
}

#[test]
fn registry_precedence_and_override_flake_match_reference() {
	let fixture = tempfile::tempdir().unwrap();
	let dir = fixture.path();
	for sub in ["home", "config/nix", "system", "global", "user", "flag"] {
		fs::create_dir_all(dir.join(sub)).unwrap();
	}
	for (name, answer) in [("global", 1), ("system", 2), ("user", 3), ("flag", 4)] {
		fs::write(
			dir.join(name).join("flake.nix"),
			format!("{{ outputs = {{ self }}: {{ answer = {answer}; }}; }}"),
		)
		.unwrap();
	}
	let global_registry = dir.join("global.json");
	let system_registry = dir.join("system/registry.json");
	let user_registry = dir.join("config/nix/registry.json");
	registry(&global_registry, &dir.join("global"));
	let reference = std::env::var_os("NIXRS_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate");
	assert!(
		reference.exists(),
		"run just reference or set NIXRS_REFERENCE_BIN"
	);
	let run = |bin: &Path, flags: &[&str]| {
		let store = tempfile::tempdir().unwrap();
		Command::new(bin)
			.env_clear()
			.env("PATH", std::env::var_os("PATH").unwrap())
			.env("HOME", dir.join("home"))
			.env("XDG_CONFIG_HOME", dir.join("config"))
			.env("NIX_CONF_DIR", dir.join("system"))
			.env("NIX_USER_CONF_FILES", "")
			.env("NIX_PATH", "")
			.env(
				"NIX_CONFIG",
				format!(
					"experimental-features = nix-command flakes\nflake-registry = {}\nwarn-dirty = false\n",
					global_registry.display()
				),
			)
			.current_dir(dir)
			.args([
				"--store",
				&format!("local?root={}", store.path().display()),
				"--eval",
				"--strict",
				"--json",
				"--expr",
				"(builtins.getFlake \"local\").answer",
			])
			.args(flags)
			.output()
			.unwrap()
	};
	let check = |expected: u64, flags: &[&str]| {
		let reference = run(&reference, flags);
		let candidate = run(Path::new(env!("CARGO_BIN_EXE_nix-instantiate")), flags);
		assert!(
			reference.status.success(),
			"{}",
			String::from_utf8_lossy(&reference.stderr)
		);
		assert!(
			candidate.status.success(),
			"{}",
			String::from_utf8_lossy(&candidate.stderr)
		);
		assert_eq!(candidate.stdout, reference.stdout);
		assert_eq!(
			serde_json::from_slice::<u64>(&candidate.stdout).unwrap(),
			expected
		);
	};
	check(1, &[]);
	registry(&system_registry, &dir.join("system"));
	check(2, &[]);
	registry(&user_registry, &dir.join("user"));
	check(3, &[]);
	let flag = format!("path:{}", dir.join("flag").display());
	check(4, &["--override-flake", "local", &flag]);
}
