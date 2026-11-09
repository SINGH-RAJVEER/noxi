//! Modern flake dispatch and lock effects through the installed command entry point.
use std::{
	fs,
	path::{Path, PathBuf},
	process::{Command, Output},
};
fn run(bin: &Path, root: &Path, args: &[&str]) -> Output {
	Command::new(bin).env_clear().env("PATH", std::env::var_os("PATH").unwrap()).env("HOME", root).env("NIX_CONF_DIR", root).env("NIX_USER_CONF_FILES", "").env("NIX_CONFIG", "experimental-features = nix-command flakes\nsubstituters =\nflake-registry =\nwarn-dirty = false\nbuild-users-group =\n").args(["--store", &format!("local?root={}", root.display())]).args(args).output().unwrap()
}
#[test]
fn modern_flake_metadata_lock_and_installable_selection() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix");
	let fixture = tempfile::tempdir().unwrap();
	let dep = fixture.path().join("project/dep");
	let project = fixture.path().join("project");
	fs::create_dir_all(&dep).unwrap();
	fs::create_dir_all(&project).unwrap();
	fs::write(
		dep.join("flake.nix"),
		"{ outputs = { self }: { answer = 42; }; }",
	)
	.unwrap();
	fs::write(project.join("flake.nix"), "{ inputs.dep.url = \"path:./dep\"; outputs = { self, dep }: { answer = dep.answer; packages.x86_64-linux.default = dep.answer; }; }").unwrap();
	let url = format!("path:{}", project.display());
	let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
	let blocked_args = ["flake", "metadata", &url, "--json", "--no-update-lock-file"];
	for (bin, root) in [reference.as_path(), Path::new(env!("CARGO_BIN_EXE_nix"))]
		.iter()
		.zip(&roots)
	{
		let output = run(bin, root.path(), &blocked_args);
		assert_eq!(output.status.code(), Some(1));
		assert!(output.stdout.is_empty());
		assert!(!project.join("flake.lock").exists());
	}
	for operation in ["metadata", "archive"] {
		let args = ["flake", operation, &url, "--json", "--no-write-lock-file"];
		let a = run(&reference, roots[0].path(), &args);
		let b = run(Path::new(env!("CARGO_BIN_EXE_nix")), roots[1].path(), &args);
		assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
		assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
		assert_eq!(
			serde_json::from_slice::<serde_json::Value>(&a.stdout).unwrap(),
			serde_json::from_slice::<serde_json::Value>(&b.stdout).unwrap(),
			"{operation}"
		);
		assert!(!project.join("flake.lock").exists());
	}
	let a = run(&reference, roots[0].path(), &["flake", "lock", &url]);
	assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
	let lock = fs::read(project.join("flake.lock")).unwrap();
	fs::remove_file(project.join("flake.lock")).unwrap();
	let b = run(
		Path::new(env!("CARGO_BIN_EXE_nix")),
		roots[1].path(),
		&["flake", "lock", &url],
	);
	assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
	assert_eq!(lock, fs::read(project.join("flake.lock")).unwrap());
	for suffix in ["#answer", ""] {
		let selection = format!("{url}{suffix}");
		let args = ["eval", "--json", &selection, "--no-write-lock-file"];
		let a = run(&reference, roots[0].path(), &args);
		let b = run(Path::new(env!("CARGO_BIN_EXE_nix")), roots[1].path(), &args);
		assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
		assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
		assert_eq!(a.stdout, b.stdout);
	}
	let alternate = fixture.path().join("alternate");
	fs::create_dir(&alternate).unwrap();
	fs::write(
		alternate.join("flake.nix"),
		"{ outputs = { self }: { answer = 43; }; }",
	)
	.unwrap();
	let alternate_url = format!("path:{}", alternate.display());
	let selection = format!("{url}#answer");
	let flags = [
		"eval",
		"--json",
		&selection,
		"--no-write-lock-file",
		"--override-input",
		"dep",
		&alternate_url,
	];
	let a = run(&reference, roots[0].path(), &flags);
	let b = run(
		Path::new(env!("CARGO_BIN_EXE_nix")),
		roots[1].path(),
		&flags,
	);
	assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
	assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
	assert_eq!(a.stdout, b.stdout);
	assert_eq!(a.stdout, b"43\n");
	assert_eq!(lock, fs::read(project.join("flake.lock")).unwrap());
}

#[test]
fn flake_show_check_update_and_lock_policy() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix");
	let project = tempfile::tempdir().unwrap();
	fs::write(project.path().join("flake.nix"), "{ outputs = { self }: let d = derivation { name = \"test\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo hi > $out\" ]; }; in { packages.x86_64-linux.default = d; checks.x86_64-linux.test = d; }; }").unwrap();
	let url = format!("path:{}", project.path().display());
	let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
	for flags in [
		vec!["flake", "show", &url, "--json", "--no-write-lock-file"],
		vec!["flake", "check", &url, "--no-build", "--no-write-lock-file"],
		vec!["flake", "update", "--flake", &url],
		vec![
			"eval",
			&format!("{url}#packages.x86_64-linux.default.name"),
			"--json",
			"--no-update-lock-file",
		],
	] {
		let a = run(&reference, roots[0].path(), &flags);
		let b = run(
			Path::new(env!("CARGO_BIN_EXE_nix")),
			roots[1].path(),
			&flags,
		);
		assert!(
			a.status.success(),
			"{flags:?}: {}",
			String::from_utf8_lossy(&a.stderr)
		);
		assert!(
			b.status.success(),
			"{flags:?}: {}",
			String::from_utf8_lossy(&b.stderr)
		);
		if flags[1] == "show" {
			assert_eq!(
				serde_json::from_slice::<serde_json::Value>(&a.stdout).unwrap(),
				serde_json::from_slice::<serde_json::Value>(&b.stdout).unwrap()
			);
		} else {
			assert_eq!(a.stdout, b.stdout);
		}
		assert!(!project.path().join("flake.lock").exists());
	}
}

#[test]
fn stored_flake_fragments_select_build_installables() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix");
	let project = tempfile::tempdir().unwrap();
	fs::write(project.path().join("flake.nix"), "{ outputs = { self }: { nixosConfigurations.\"two\".config.system.build.toplevel = derivation { name = \"stored-flake-test\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo hi > $out\" ]; }; }; }").unwrap();
	let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
	let mut outputs = Vec::new();
	for (bin, root) in [reference.as_path(), Path::new(env!("CARGO_BIN_EXE_nix"))]
		.iter()
		.zip(&roots)
	{
		let added = run(
			&reference,
			root.path(),
			&[
				"store",
				"add",
				"--mode",
				"nar",
				project.path().to_str().unwrap(),
			],
		);
		assert!(
			added.status.success(),
			"{}",
			String::from_utf8_lossy(&added.stderr)
		);
		// Flake source traversal in the reference uses the host filesystem,
		// so use the physical path inside this diverted store. The VM gate
		// exercises the corresponding logical /nix/store path.
		let added = String::from_utf8(added.stdout).unwrap();
		let source = root.path().join(added.trim().trim_start_matches('/'));
		let installable = format!(
			"{}#nixosConfigurations.\"two\".config.system.build.toplevel",
			source.display()
		);
		let output = run(
			bin,
			root.path(),
			&[
				"build",
				&installable,
				"--dry-run",
				"--no-link",
				"--json",
				"--no-write-lock-file",
			],
		);
		assert!(
			output.status.success(),
			"{}",
			String::from_utf8_lossy(&output.stderr)
		);
		outputs.push(output.stdout);
	}
	assert_eq!(outputs[0], outputs[1]);
}
