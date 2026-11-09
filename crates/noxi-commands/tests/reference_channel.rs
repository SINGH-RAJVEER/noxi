//! Channel subscription files, with private homes and no channel updates.
use std::{
	fs,
	path::{Path, PathBuf},
	process::Command,
};
#[test]
fn subscription_files_and_xdg_precedence() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-channel");
	for xdg in [false, true] {
		let homes = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
		for args in [
			vec!["--add", "https://example.org/foo/"],
			vec!["--add", "file:///tmp/source", "local"],
			vec!["--add", "https://example.org/replacement", "foo"],
			vec!["--list"],
			vec!["--remove", "foo"],
			vec!["--remove", "missing"],
			vec!["--list"],
		] {
			let results: Vec<_> = [
				reference.as_path(),
				Path::new(env!("CARGO_BIN_EXE_nix-channel")),
			]
			.iter()
			.zip(&homes)
			.map(|(bin, home)| {
				Command::new(bin)
					.env_clear()
					.env("HOME", home.path())
					.env("NIX_CONF_DIR", home.path())
					.env("NIX_USER_CONF_FILES", "")
					.env("NIX_CONFIG", format!("use-xdg-base-directories = {xdg}\n"))
					.args(&args)
					.output()
					.unwrap()
			})
			.collect();
			assert!(
				results[0].status.success(),
				"{}",
				String::from_utf8_lossy(&results[0].stderr)
			);
			assert!(
				results[1].status.success(),
				"{}",
				String::from_utf8_lossy(&results[1].stderr)
			);
			assert_eq!(results[0].stdout, results[1].stdout, "{args:?}");
			let file = if xdg {
				".local/state/nix/channels"
			} else {
				".nix-channels"
			};
			assert_eq!(
				fs::read(homes[0].path().join(file)).unwrap(),
				fs::read(homes[1].path().join(file)).unwrap(),
				"{args:?}"
			);
		}
	}
}
