//! Fetcher attribute builtins checked against the pinned reference process.

use nix_compat::nixcpp::settings::Settings;
use noxi_commands::instantiate;
use std::path::Path;
use std::process::{Command, ExitCode};

#[test]
fn flake_reference_attribute_builtins_match_reference_values() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(std::path::PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		});
	let scratch = tempfile::tempdir().unwrap();
	let expressions = [
		r#"builtins.parseFlakeRef "github:NixOS/nixpkgs/23.05?dir=lib""#,
		r#"builtins.parseFlakeRef "gitlab:owner/repo/main?host=git.example.org""#,
		r#"builtins.parseFlakeRef "sourcehut:~owner/repo/main""#,
		r#"builtins.parseFlakeRef "git+https://example.org/repo.git?ref=main&shallow=1""#,
		r#"builtins.parseFlakeRef "git+https://example.org/repo.git?shallow=true&ref=one&ref=two""#,
		r#"builtins.parseFlakeRef "path:/tmp?lastModified=123""#,
		r#"builtins.parseFlakeRef "flake:nixpkgs""#,
		r#"builtins.flakeRefToString { type = "github"; owner = "NixOS"; repo = "nixpkgs"; ref = "23.05"; dir = "lib"; }"#,
		r#"builtins.flakeRefToString { type = "git"; url = "https://example.org/repo.git"; ref = "main"; shallow = true; }"#,
		r#"builtins.flakeRefToString { type = "path"; path = "/tmp"; lastModified = 123; }"#,
		r#"builtins.flakeRefToString { type = "file"; url = "https://example.org/file.txt"; }"#,
	];
	for expression in expressions {
		let output = Command::new(reference.join("nix-instantiate"))
			.args([
				"--eval",
				"--strict",
				"--extra-experimental-features",
				"flakes",
				"--expr",
				expression,
			])
			.env_clear()
			.env("HOME", scratch.path())
			.env("NIX_CONF_DIR", scratch.path())
			.env("NIX_USER_CONF_FILES", scratch.path().join("absent.conf"))
			.env("NIX_CONFIG", "")
			.env("NIX_PATH", "")
			.env("NIX_REMOTE", "dummy://")
			.current_dir(scratch.path())
			.output()
			.unwrap();
		assert!(
			output.status.success(),
			"{expression}: {}",
			String::from_utf8_lossy(&output.stderr)
		);
		let options = instantiate::parse_args(
			[
				"--eval",
				"--strict",
				"--extra-experimental-features",
				"flakes",
				"--expr",
				expression,
			]
			.map(str::to_owned),
			Settings::default(),
		)
		.unwrap();
		let mut actual = Vec::new();
		assert_eq!(
			instantiate::run(&options, scratch.path(), &mut actual),
			ExitCode::SUCCESS,
			"{expression}"
		);
		assert_eq!(actual, output.stdout, "{expression}");
	}
}
