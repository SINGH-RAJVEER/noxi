//! Registry bytes, precedence and resolution against pinned CppNix.
use std::{
	fs,
	path::{Path, PathBuf},
	process::Command,
};

#[test]
fn registry_mutations_and_resolution_match_reference() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix");
	let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
	let source = tempfile::tempdir().unwrap();
	fs::write(source.path().join("flake.nix"), "{ outputs = {self}: {}; }").unwrap();
	let source_url = format!("path:{}", source.path().display());
	for args in [
		vec!["registry", "add", "example", "github:owner/repo"],
		vec!["registry", "list"],
		vec!["registry", "resolve", "example", "example/branch"],
		vec!["registry", "add", "example", "github:other/repo?dir=sub"],
		vec!["registry", "resolve", "example"],
		vec!["registry", "pin", "local", &source_url],
		vec!["registry", "remove", "local"],
		vec!["registry", "remove", "example"],
		vec!["registry", "list"],
	] {
		let outputs: Vec<_> = [reference.as_path(), Path::new(env!("CARGO_BIN_EXE_nix"))]
			.iter()
			.zip(&roots)
			.map(|(bin, root)| {
				Command::new(bin)
					.env_clear()
					.env("HOME", root.path())
					.env("NIX_CONF_DIR", root.path().join("conf"))
					.env("NIX_USER_CONF_FILES", "")
					.env(
						"NIX_CONFIG",
						"experimental-features = nix-command flakes\nflake-registry =\n",
					)
					.args(&args)
					.output()
					.unwrap()
			})
			.collect();
		for output in &outputs {
			assert!(
				output.status.success(),
				"{args:?}: {}",
				String::from_utf8_lossy(&output.stderr)
			);
		}
		assert_eq!(outputs[0].stdout, outputs[1].stdout, "{args:?}");
		assert_eq!(outputs[0].stderr, outputs[1].stderr, "{args:?}");
		assert_eq!(
			fs::read(roots[0].path().join(".config/nix/registry.json")).unwrap(),
			fs::read(roots[1].path().join(".config/nix/registry.json")).unwrap(),
			"{args:?}"
		);
	}
}
