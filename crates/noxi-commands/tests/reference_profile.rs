//! Modern profile store-path lifecycle against pinned CppNix.
use std::{
	fs,
	path::{Path, PathBuf},
	process::Command,
};

#[test]
fn modern_profiles_match_manifest_and_output_paths() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		});
	let fixture = tempfile::tempdir().unwrap();
	let pkg = fixture.path().join("demo-1");
	fs::create_dir_all(pkg.join("bin")).unwrap();
	fs::write(pkg.join("bin/demo"), "hello").unwrap();
	let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
	let bins = [
		reference.join("nix"),
		PathBuf::from(env!("CARGO_BIN_EXE_nix")),
	];
	let run = |engine: usize, bin: &Path, args: &[&str]| {
		let root = roots[engine].path();
		{
			let mut command = if engine == 0 && args.first() == Some(&"profile") {
				let mut command = Command::new("unshare");
				command.args([
					"-Ur",
					"-m",
					"--",
					"bash",
					"-c",
					"mount --bind \"$1\" /nix/store; shift; exec \"$@\"",
					"profile-test",
				]);
				command.arg(root.join("store/nix/store")).arg(bin);
				command
			} else {
				Command::new(bin)
			};
			command.env_clear().env("PATH", std::env::var_os("PATH").unwrap()).env("HOME", root).env("NIX_CONF_DIR", root.join("conf")).env("NIX_USER_CONF_FILES", "").env("NIX_CONFIG", "experimental-features = nix-command flakes\nsubstituters =\nflake-registry =\nbuild-users-group =\n").args(["--store", &format!("local?root={}/store", root.display())]).args(args).output().unwrap()
		}
	};
	for root in &roots {
		let package = fs::canonicalize(&reference).unwrap();
		let package = package.parent().unwrap();
		let copied = Command::new(reference.join("nix"))
			.env_clear()
			.env("HOME", root.path())
			.env("NIX_CONF_DIR", root.path().join("conf"))
			.env("NIX_USER_CONF_FILES", "")
			.env("NIX_CONFIG", "experimental-features = nix-command")
			.args([
				"copy",
				"--no-check-sigs",
				"--to",
				&format!("local?root={}/store", root.path().display()),
			])
			.arg(package)
			.output()
			.unwrap();
		assert!(
			copied.status.success(),
			"{}",
			String::from_utf8_lossy(&copied.stderr)
		);
	}
	let mut paths = Vec::new();
	for engine in 0..2 {
		let out = run(
			engine,
			&bins[engine],
			&["store", "add-path", pkg.to_str().unwrap()],
		);
		assert!(
			out.status.success(),
			"{}",
			String::from_utf8_lossy(&out.stderr)
		);
		paths.push(String::from_utf8(out.stdout).unwrap().trim().to_owned());
	}
	assert_eq!(paths[0], paths[1]);
	for args in [
		vec!["profile", "add", paths[0].as_str()],
		vec!["profile", "list", "--json"],
		vec!["profile", "list"],
		vec!["profile", "remove", "demo"],
		vec!["profile", "list", "--json"],
		vec!["profile", "rollback"],
		vec!["profile", "list", "--json"],
	] {
		let outputs: Vec<_> = (0..2)
			.map(|engine| {
				let profile = roots[engine].path().join("profile");
				let mut argv = args.clone();
				argv.extend(["--profile", profile.to_str().unwrap()]);
				run(engine, &bins[engine], &argv)
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
		// Profile paths in rollback notices differ only by the isolated home.
		let stderr: Vec<_> = outputs
			.iter()
			.zip(&roots)
			.map(|(o, r)| {
				String::from_utf8_lossy(&o.stderr).replace(r.path().to_str().unwrap(), "ROOT")
			})
			.collect();
		assert_eq!(stderr[0], stderr[1], "{args:?}");
		let targets: Vec<_> = roots
			.iter()
			.map(|r| {
				let link = fs::read_link(r.path().join("profile")).unwrap();
				fs::read_link(r.path().join(link)).unwrap()
			})
			.collect();
		assert_eq!(targets[0], targets[1]);
	}
}
