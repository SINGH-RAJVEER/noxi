//! Native channel subscriptions and lifecycle in private homes and stores.
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

#[test]
fn channel_updates_preserve_reference_paths_contents_and_generations() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		});
	let fixture = tempfile::tempdir().unwrap();
	let source = fixture.path().join("nixexprs");
	fs::create_dir(&source).unwrap();
	let archive = fixture.path().join("channel.tar.gz");
	let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
	let binaries = [
		reference.join("nix-channel"),
		PathBuf::from(env!("CARGO_BIN_EXE_nix-channel")),
	];
	let run = |engine: usize, args: &[&str]| {
		let root = roots[engine].path();
		Command::new(&binaries[engine])
			.env_clear()
			.env("PATH", std::env::var_os("PATH").unwrap())
			.env("HOME", root)
			.env("NIX_CONF_DIR", root.join("conf"))
			.env("NIX_USER_CONF_FILES", "")
			.env("NIX_STATE_DIR", root.join("state"))
			.env(
				"NIX_CONFIG",
				format!(
					"store = local?root={}/store\nsubstituters =\nbuild-users-group =\nsandbox = false\ntarball-ttl = 0\n",
					root.display()
				),
			)
			.args(args)
			.output()
			.unwrap()
	};
	let same = |args: &[&str]| {
		let outputs = [run(0, args), run(1, args)];
		for output in &outputs {
			assert!(
				output.status.success(),
				"{args:?}: {}",
				String::from_utf8_lossy(&output.stderr)
			);
		}
		assert_eq!(outputs[0].stdout, outputs[1].stdout, "{args:?}");
		assert_eq!(outputs[0].stderr, outputs[1].stderr, "{args:?}");
	};
	let url = format!("file://{}", archive.display());
	same(&["--add", &url, "demo"]);
	for answer in [42, 43] {
		fs::write(
			source.join("default.nix"),
			format!("{{ answer = {answer}; }}"),
		)
		.unwrap();
		assert!(
			Command::new("tar")
				.args([
					"-czf",
					archive.to_str().unwrap(),
					"-C",
					fixture.path().to_str().unwrap(),
					"nixexprs"
				])
				.status()
				.unwrap()
				.success()
		);
		same(&["--update"]);
		let profiles: Vec<_> = roots
			.iter()
			.map(|root| root.path().join(".local/state/nix/profiles/channels"))
			.collect();
		assert_eq!(
			fs::read_link(&profiles[0]).unwrap(),
			fs::read_link(&profiles[1]).unwrap()
		);
		let generations: Vec<_> = profiles
			.iter()
			.map(|p| p.parent().unwrap().join(fs::read_link(p).unwrap()))
			.collect();
		let targets: Vec<_> = generations
			.iter()
			.map(|p| fs::read_link(p).unwrap())
			.collect();
		assert_eq!(targets[0], targets[1]);
		// Exact NAR bytes include the manifest and channel links, independent of timestamps.
		let dumps: Vec<_> = roots
			.iter()
			.zip(&targets)
			.map(|(root, target)| {
				let physical = root
					.path()
					.join("store")
					.join(target.strip_prefix("/").unwrap());
				let mut bytes = Vec::new();
				noxi_store::nar::dump_path(&mut bytes, &physical).unwrap();
				bytes
			})
			.collect();
		assert_eq!(dumps[0], dumps[1]);
	}
	same(&["--rollback"]);
	same(&["--rollback", "2"]);
}
