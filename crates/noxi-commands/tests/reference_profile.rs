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
			command.env_clear().env("PATH", std::env::var_os("PATH").unwrap()).env("HOME", root).env("NIX_CONF_DIR", root.join("conf")).env("NIX_USER_CONF_FILES", "").env("NIX_CONFIG", "experimental-features = nix-command flakes\nsubstituters =\nflake-registry =\nbuild-users-group =\nrequire-drop-supplementary-groups = false\nsandbox = false\n").args(["--store", &format!("local?root={}/store", root.display())]).args(args).output().unwrap()
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
		vec!["profile", "history"],
		vec!["profile", "diff-closures"],
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
				String::from_utf8_lossy(&o.stderr)
					.replace(r.path().to_str().unwrap(), "ROOT")
					.replace(bins[0].to_str().unwrap(), "nix")
					.replace(bins[1].to_str().unwrap(), "nix")
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
	let clash = fixture.path().join("clash-1");
	fs::create_dir_all(clash.join("bin")).unwrap();
	fs::write(clash.join("bin/demo"), "different").unwrap();
	let mut clash_paths = Vec::new();
	for engine in 0..2 {
		let result = run(
			engine,
			&bins[engine],
			&["store", "add-path", clash.to_str().unwrap()],
		);
		assert!(result.status.success());
		clash_paths.push(String::from_utf8(result.stdout).unwrap().trim().to_owned());
	}
	assert_eq!(clash_paths[0], clash_paths[1]);

	for args in [
		vec!["profile", "list", "--json", "--pretty"],
		vec!["profile", "list", "--json", "--no-pretty"],
		vec!["profile", "add", clash_paths[0].as_str()],
		vec!["profile", "add", clash_paths[0].as_str(), "--priority", "4"],
		vec!["profile", "remove", "clash"],
		vec!["profile", "remove"],
		vec!["profile", "remove", "--all", "demo"],
		vec!["profile", "remove", "0"],
		vec!["profile", "remove", "absent"],
		vec!["profile", "upgrade", "demo"],
		vec!["profile", "remove", "--regex", "DeMo"],
		vec!["profile", "rollback", "--to", "1", "--dry-run"],
		vec!["profile", "rollback", "--to", "1"],
		vec!["profile", "wipe-history", "--dry-run"],
		vec!["profile", "wipe-history"],
		vec!["profile", "history"],
	] {
		let outputs: Vec<_> = (0..2)
			.map(|engine| {
				let profile = roots[engine].path().join("profile");
				let mut argv = args.clone();
				argv.extend(["--profile", profile.to_str().unwrap()]);
				run(engine, &bins[engine], &argv)
			})
			.collect();
		assert_eq!(
			outputs[0].status.code(),
			outputs[1].status.code(),
			"{args:?}"
		);
		assert_eq!(outputs[0].stdout, outputs[1].stdout, "{args:?}");
		let stderr: Vec<_> = outputs
			.iter()
			.zip(&roots)
			.map(|(o, r)| {
				String::from_utf8_lossy(&o.stderr)
					.replace(r.path().to_str().unwrap(), "ROOT")
					.replace(bins[0].to_str().unwrap(), "nix")
					.replace(bins[1].to_str().unwrap(), "nix")
			})
			.collect();
		assert_eq!(stderr[0], stderr[1], "{args:?}");
	}

	// A mutable local flake must retain its source and realize a changed output on upgrade.
	let flake = fixture.path().join("flake");
	fs::create_dir(&flake).unwrap();
	let url = format!("path:{}#demo^out", flake.display());
	for version in [1, 2] {
		fs::write(flake.join("flake.nix"), format!(r#"{{ outputs = {{ self }}: {{ packages.x86_64-linux.demo = derivation {{ name = "profile-demo-{version}"; system = "builtin"; builder = "builtin:buildenv"; derivations = ""; manifest = builtins.toFile "empty.nix" "[]"; }}; }}; }}"#)).unwrap();
		let action = if version == 1 {
			vec!["profile", "add", url.as_str()]
		} else {
			vec!["profile", "upgrade", "demo"]
		};
		for args in [
			action,
			vec!["profile", "list", "--json"],
			vec!["profile", "list"],
			vec!["profile", "history"],
			vec!["profile", "diff-closures"],
		] {
			let outputs: Vec<_> = (0..2)
				.map(|engine| {
					let profile = roots[engine].path().join("flake-profile");
					let mut argv = args.clone();
					argv.extend(["--profile", profile.to_str().unwrap()]);
					run(engine, &bins[engine], &argv)
				})
				.collect();
			for o in &outputs {
				assert!(
					o.status.success(),
					"{args:?}: {}",
					String::from_utf8_lossy(&o.stderr)
				);
			}
			assert_eq!(outputs[0].stdout, outputs[1].stdout, "{args:?}");
			assert_eq!(outputs[0].stderr, outputs[1].stderr, "{args:?}");
			// The installed manifests and generation paths must agree as well.
			let manifests: Vec<_> = roots
				.iter()
				.map(|r| {
					let generation = fs::read_link(r.path().join("flake-profile")).unwrap();
					let target = fs::read_link(r.path().join(generation)).unwrap();
					let path = r
						.path()
						.join("store")
						.join(target.strip_prefix("/").unwrap());
					(target, fs::read(path.join("manifest.json")).unwrap())
				})
				.collect();
			assert_eq!(manifests[0], manifests[1], "{args:?}");
		}
	}
	for version in [1, 2, 3, 0] {
		let source = fixture.path().join(format!("manifest-v{version}"));
		fs::create_dir(&source).unwrap();
		let manifest = if version == 0 {
			None
		} else {
			Some(
				serde_json::json!({"version":version,"elements":[{"active":false,"storePaths":[paths[0]]}]}),
			)
		};
		if let Some(manifest) = manifest {
			fs::write(source.join("manifest.json"), manifest.to_string()).unwrap();
		} else {
			let file = fixture.path().join("legacy-manifest.nix");
			fs::write(
				&file,
				format!(
					"[ {{ type = \"derivation\"; name = \"legacy-1\"; outPath = \"{}\"; }} ]",
					paths[0]
				),
			)
			.unwrap();
			let mut targets = Vec::new();
			for engine in 0..2 {
				let out = run(
					engine,
					&bins[engine],
					&["store", "add-path", file.to_str().unwrap()],
				);
				assert!(out.status.success());
				targets.push(String::from_utf8(out.stdout).unwrap().trim().to_owned());
			}
			assert_eq!(targets[0], targets[1]);
			std::os::unix::fs::symlink(&targets[0], source.join("manifest.nix")).unwrap();
		}
		for engine in 0..2 {
			let added = run(
				engine,
				&bins[engine],
				&["store", "add-path", source.to_str().unwrap()],
			);
			assert!(added.status.success());
			let target = String::from_utf8(added.stdout).unwrap();
			std::os::unix::fs::symlink(
				target.trim(),
				roots[engine].path().join(format!("old-{version}")),
			)
			.unwrap();
		}
		for args in [
			vec!["profile", "list", "--json"],
			vec!["profile", "add", clash_paths[0].as_str(), "--priority", "4"],
			vec!["profile", "list", "--json"],
		] {
			let outputs: Vec<_> = (0..2)
				.map(|engine| {
					let profile = roots[engine].path().join(format!("old-{version}"));
					let mut argv = args.clone();
					argv.extend(["--profile", profile.to_str().unwrap()]);
					run(engine, &bins[engine], &argv)
				})
				.collect();
			for o in &outputs {
				assert!(
					o.status.success(),
					"v{version} {args:?}: {}",
					String::from_utf8_lossy(&o.stderr)
				);
			}
			assert_eq!(outputs[0].stdout, outputs[1].stdout, "v{version} {args:?}");
			assert_eq!(outputs[0].stderr, outputs[1].stderr, "v{version} {args:?}");
		}
	}
}
