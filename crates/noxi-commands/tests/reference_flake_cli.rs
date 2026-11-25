//! Modern flake dispatch and lock effects through the installed command entry point.
use std::{
	fs,
	path::{Path, PathBuf},
	process::{Command, Output},
};
fn run(bin: &Path, root: &Path, args: &[&str]) -> Output {
	Command::new(bin).env_clear().env("PATH", std::env::var_os("PATH").unwrap()).env("HOME", root).env("NIX_CONF_DIR", root).env("NIX_USER_CONF_FILES", "").env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00Z").env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z").env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").env("NIX_CONFIG", "experimental-features = nix-command flakes\nsubstituters =\nflake-registry =\nwarn-dirty = false\nbuild-users-group =\n").args(["--store", &format!("local?root={}", root.display())]).args(args).output().unwrap()
}

#[test]
fn dirty_git_tree_does_not_warn_and_can_still_be_rejected() {
	let fixture = noxi_test_support::tempdir().unwrap();
	let repo = fixture.path();
	assert!(
		Command::new("git")
			.args(["init", "-q"])
			.current_dir(repo)
			.status()
			.unwrap()
			.success()
	);
	fs::write(
		repo.join("flake.nix"),
		"{ outputs = { self }: { answer = 42; }; }",
	)
	.unwrap();
	assert!(
		Command::new("git")
			.args(["add", "flake.nix"])
			.current_dir(repo)
			.status()
			.unwrap()
			.success()
	);
	assert!(
		Command::new("git")
			.args([
				"-c",
				"core.hooksPath=/dev/null",
				"-c",
				"user.name=Test",
				"-c",
				"user.email=test@example.com",
				"commit",
				"-qm",
				"fixture"
			])
			.current_dir(repo)
			.status()
			.unwrap()
			.success()
	);
	fs::write(
		repo.join("flake.nix"),
		"{ outputs = { self }: { answer = 43; }; }",
	)
	.unwrap();
	let root = noxi_test_support::tempdir().unwrap();
	let url = format!("{}#answer", repo.display());
	let bin = Path::new(env!("CARGO_BIN_EXE_nix"));
	let result = run(bin, root.path(), &["eval", "--json", "--warn-dirty", &url]);
	assert!(
		result.status.success(),
		"{}",
		String::from_utf8_lossy(&result.stderr)
	);
	assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), "43");
	assert!(!String::from_utf8_lossy(&result.stderr).contains("is dirty"));
	let result = run(
		bin,
		root.path(),
		&["eval", "--json", "--no-allow-dirty", &url],
	);
	assert!(!result.status.success());
	assert!(String::from_utf8_lossy(&result.stderr).contains("is dirty"));
}

#[test]
fn get_flake_path_values_respect_evaluation_access() {
	let reference = PathBuf::from(std::env::var_os("NOXI_REFERENCE_BIN").unwrap()).join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	let flake = fixture.path().join("host-flake");
	fs::create_dir(&flake).unwrap();
	fs::write(flake.join("flake.nix"), "{ outputs = _: { x = 42; }; }").unwrap();
	let expression = format!("(builtins.getFlake {}).x", flake.display());
	for (flags, diagnostic) in [
		(vec!["--pure-eval"], "is forbidden in pure evaluation mode"),
		(
			vec!["--impure", "--restrict-eval"],
			"is forbidden in restricted mode",
		),
		(vec!["--impure"], "is not in the Nix store"),
	] {
		let mut args = vec!["eval", "--offline", "--expr", &expression];
		args.extend(flags);
		let a = run(&reference, fixture.path(), &args);
		let b = run(Path::new(env!("CARGO_BIN_EXE_nix")), fixture.path(), &args);
		for output in [&a, &b] {
			assert_eq!(output.status.code(), Some(1), "{args:?}");
			let stderr = String::from_utf8_lossy(&output.stderr);
			assert!(stderr.contains(diagnostic), "{args:?}: {stderr}");
		}
		assert_eq!(a.stdout, b.stdout);
		assert_eq!(a.stderr, b.stderr);
	}
}

#[test]
fn pure_generated_text_can_be_read_without_allowing_host_files() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix");
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
	for expression in [
		"builtins.readFile (builtins.toFile \"generated\" \"hello\")",
		"import (builtins.toFile \"generated.nix\" \"42\")",
	] {
		let args = ["eval", "--json", "--expr", expression];
		let a = run(&reference, roots[0].path(), &args);
		let b = run(Path::new(env!("CARGO_BIN_EXE_nix")), roots[1].path(), &args);
		assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
		assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
		assert_eq!(a.stdout, b.stdout);
		assert_eq!(a.stderr, b.stderr);
	}
}

#[test]
fn pure_flake_imports_a_native_builder_output() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	let source = fixture.path().join("nixexprs");
	fs::create_dir(&source).unwrap();
	fs::write(source.join("default.nix"), "42").unwrap();
	assert!(
		Command::new("tar")
			.args([
				"-czf",
				fixture.path().join("source.tar.gz").to_str().unwrap(),
				"-C",
				fixture.path().to_str().unwrap(),
				"nixexprs"
			])
			.status()
			.unwrap()
			.success()
	);
	fs::write(fixture.path().join("flake.nix"), "{ outputs = { self }: let d = derivation { name = \"pure-ifd\"; builder = \"builtin:unpack-channel\"; system = \"builtin\"; channelName = \"demo\"; src = self + \"/source.tar.gz\"; }; in { answer = import (d + \"/demo\"); }; }").unwrap();
	let url = format!("path:{}#answer", fixture.path().display());
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
	let args = ["eval", "--json", &url];
	let a = run(&reference, roots[0].path(), &args);
	let b = run(Path::new(env!("CARGO_BIN_EXE_nix")), roots[1].path(), &args);
	assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
	assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
	assert_eq!(a.stdout, b.stdout);
	assert_eq!(a.stderr, b.stderr);
}

#[test]
fn flake_lookup_path_imports_source_files() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	fs::write(
		fixture.path().join("flake.nix"),
		"{ outputs = { self }: {}; }",
	)
	.unwrap();
	fs::write(fixture.path().join("default.nix"), "42").unwrap();
	let entry = format!("demo=flake:path:{}", fixture.path().display());
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
	let args = [
		"eval",
		"--impure",
		"--json",
		"-I",
		&entry,
		"--expr",
		"import <demo>",
	];
	let a = run(&reference, roots[0].path(), &args);
	let b = run(Path::new(env!("CARGO_BIN_EXE_nix")), roots[1].path(), &args);
	assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
	assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
	assert_eq!(a.stdout, b.stdout);
	assert_eq!(a.stderr, b.stderr);
}

#[test]
fn explicit_empty_lock_files_match_reference_bytes() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	fs::write(
		fixture.path().join("flake.nix"),
		"{ outputs = { self }: {}; }",
	)
	.unwrap();
	let url = format!("path:{}", fixture.path().display());
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
	let mut bytes = Vec::new();
	for (engine, bin) in [reference.as_path(), Path::new(env!("CARGO_BIN_EXE_nix"))]
		.iter()
		.enumerate()
	{
		let output = roots[engine].path().join("selected.lock");
		let args = [
			"flake",
			"lock",
			&url,
			"--output-lock-file",
			output.to_str().unwrap(),
		];
		let result = run(bin, roots[engine].path(), &args);
		assert!(
			result.status.success(),
			"{}",
			String::from_utf8_lossy(&result.stderr)
		);
		assert!(result.stdout.is_empty());
		assert!(result.stderr.is_empty());
		bytes.push(fs::read(&output).unwrap());
		let result = run(
			bin,
			roots[engine].path(),
			&[
				"flake",
				"metadata",
				&url,
				"--json",
				"--reference-lock-file",
				output.to_str().unwrap(),
				"--no-update-lock-file",
			],
		);
		assert!(
			result.status.success(),
			"{}",
			String::from_utf8_lossy(&result.stderr)
		);
		assert!(result.stderr.is_empty());
		assert!(!fixture.path().join("flake.lock").exists());
	}
	assert_eq!(bytes[0], bytes[1]);
}

#[test]
fn flake_text_metadata_prefetch_and_archive_match_reference() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	fs::write(
		fixture.path().join("flake.nix"),
		"{ description = \"demo\"; outputs = { self }: {}; }",
	)
	.unwrap();
	let url = format!("path:{}", fixture.path().display());
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
	for operation in ["metadata", "info", "prefetch", "archive"] {
		let args = ["flake", operation, &url];
		let a = run(&reference, roots[0].path(), &args);
		let b = run(Path::new(env!("CARGO_BIN_EXE_nix")), roots[1].path(), &args);
		assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
		assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
		assert_eq!(a.stdout, b.stdout, "{operation}");
		assert_eq!(a.stderr, b.stderr, "{operation}");
	}
}
#[test]
fn modern_flake_metadata_lock_and_installable_selection() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
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
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
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
	let project = noxi_test_support::tempdir().unwrap();
	fs::write(project.path().join("flake.nix"), "{ outputs = { self }: let d = derivation { name = \"test\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo hi > $out\" ]; }; in { packages.x86_64-linux.default = d; checks.x86_64-linux.test = d; }; }").unwrap();
	let url = format!("path:{}", project.path().display());
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
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
	let project = noxi_test_support::tempdir().unwrap();
	fs::write(project.path().join("flake.nix"), "{ outputs = { self }: { nixosConfigurations.\"two\".config.system.build.toplevel = derivation { name = \"stored-flake-test\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo hi > $out\" ]; }; }; }").unwrap();
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
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

/// A non-flake `path:` input that is already a store path, imported as a
/// context-free string, as the qualification VM's NixOS flake imports
/// Nixpkgs. The reference reads stored sources through the host
/// filesystem, so it gets the physical path of the same stored tree.
#[test]
fn stored_path_input_imports_as_plain_string() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	let source = fixture.path().join("source");
	fs::create_dir_all(&source).unwrap();
	fs::write(source.join("default.nix"), "{ value = \"ok\"; }").unwrap();
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
	let mut outputs = Vec::new();
	for (index, (bin, root)) in [reference.as_path(), Path::new(env!("CARGO_BIN_EXE_nix"))]
		.iter()
		.zip(&roots)
		.enumerate()
	{
		let added = run(
			&reference,
			root.path(),
			&[
				"store",
				"add",
				"--mode",
				"nar",
				"--name",
				"source",
				source.to_str().unwrap(),
			],
		);
		assert!(
			added.status.success(),
			"{}",
			String::from_utf8_lossy(&added.stderr)
		);
		let logical = String::from_utf8(added.stdout).unwrap().trim().to_owned();
		let input = if index == 0 {
			root.path()
				.join(logical.trim_start_matches('/'))
				.display()
				.to_string()
		} else {
			logical
		};
		let flake = fixture.path().join(format!("flake{index}"));
		fs::create_dir_all(&flake).unwrap();
		fs::write(
			flake.join("flake.nix"),
			format!(
				"{{ inputs.src = {{ url = \"path:{input}\"; flake = false; }}; outputs = inputs: let plain = builtins.unsafeDiscardStringContext inputs.src.outPath; in {{ value = (import plain).value; source = plain; }}; }}"
			),
		)
		.unwrap();
		let mut output = Vec::new();
		for attr in ["value", "source"] {
			let result = run(
				bin,
				root.path(),
				&[
					"eval",
					"--raw",
					"--no-write-lock-file",
					&format!("path:{}#{attr}", flake.display()),
				],
			);
			assert!(
				result.status.success(),
				"{}",
				String::from_utf8_lossy(&result.stderr)
			);
			output.push(String::from_utf8(result.stdout).unwrap());
		}
		outputs.push(output);
	}
	assert_eq!(outputs[0][0], "ok");
	assert_eq!(outputs[0], outputs[1]);
}

/// What Nixpkgs and Home Manager do in a pure flake evaluation: look for
/// overlays under an empty HOME, which `pathExists` answers false for, and
/// use the built-in `<nix/fetchurl.nix>`.
#[test]
fn pure_flakes_see_forbidden_paths_as_missing_and_find_corepkgs() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	fs::write(
		fixture.path().join("flake.nix"),
		"{ outputs = { self }: { overlays = builtins.pathExists (builtins.getEnv \"HOME\" + \"/.config/nixpkgs/overlays.nix\"); etc = builtins.pathExists /etc/passwd; fetchurl = builtins.isFunction (import <nix/fetchurl.nix>); }; }",
	)
	.unwrap();
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
	let mut outputs = Vec::new();
	for (bin, root) in [reference.as_path(), Path::new(env!("CARGO_BIN_EXE_nix"))]
		.iter()
		.zip(&roots)
	{
		let mut output = Vec::new();
		for attr in ["overlays", "etc", "fetchurl"] {
			let result = run(
				bin,
				root.path(),
				&[
					"eval",
					"--no-write-lock-file",
					&format!("path:{}#{attr}", fixture.path().display()),
				],
			);
			output.push((
				result.status.code(),
				String::from_utf8_lossy(&result.stdout).into_owned(),
				String::from_utf8_lossy(&result.stderr).into_owned(),
			));
		}
		outputs.push(output);
	}
	assert_eq!(outputs[0][2].1, "true\n");
	assert_eq!(outputs[1], outputs[0]);
}

/// `builtins.path` and `builtins.filterSource` in pure flakes: allowed for a
/// stored input and the flake's own source, with a filter, as Nixpkgs'
/// filesets and source cleaning use them, and refused for a host path.
#[test]
fn pure_flake_path_imports_allow_only_fetched_sources() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	let source = fixture.path().join("source");
	fs::create_dir_all(source.join("sub")).unwrap();
	fs::write(source.join("sub/keep"), "keep\n").unwrap();
	fs::write(source.join("sub/drop"), "drop\n").unwrap();
	std::os::unix::fs::symlink("/etc/passwd", source.join("escape")).unwrap();
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
	let mut outputs = Vec::new();
	for (index, (bin, root)) in [reference.as_path(), Path::new(env!("CARGO_BIN_EXE_nix"))]
		.iter()
		.zip(&roots)
		.enumerate()
	{
		let added = run(
			&reference,
			root.path(),
			&[
				"store",
				"add",
				"--mode",
				"nar",
				"--name",
				"source",
				source.to_str().unwrap(),
			],
		);
		assert!(
			added.status.success(),
			"{}",
			String::from_utf8_lossy(&added.stderr)
		);
		let logical = String::from_utf8(added.stdout).unwrap().trim().to_owned();
		let input = if index == 0 {
			root.path()
				.join(logical.trim_start_matches('/'))
				.display()
				.to_string()
		} else {
			logical
		};
		let flake = fixture.path().join(format!("flake{index}"));
		fs::create_dir_all(flake.join("own")).unwrap();
		fs::write(flake.join("own/file"), "own\n").unwrap();
		fs::write(
			flake.join("flake.nix"),
			format!(
				"{{ inputs.src = {{ url = \"path:{input}\"; flake = false; }}; outputs = inputs: {{ filtered = builtins.path {{ path = inputs.src + \"/sub\"; name = \"filtered\"; filter = path: type: baseNameOf path != \"drop\"; }}; source = builtins.filterSource (path: type: true) (inputs.src + \"/sub\"); own = builtins.path {{ path = ./own; }}; host = builtins.path {{ path = /etc/passwd; }}; link = builtins.path {{ path = inputs.src + \"/escape\"; }}; linkFiltered = builtins.filterSource (path: type: true) (inputs.src + \"/escape\"); climb = builtins.path {{ path = inputs.src + \"/sub/../../../../../../etc/passwd\"; name = \"climb\"; }}; readLink = builtins.readFile (inputs.src + \"/escape\"); readClimb = builtins.readFile (inputs.src + \"/sub/../../../../../../etc/passwd\"); }}; }}"
			),
		)
		.unwrap();
		let mut output = Vec::new();
		for attr in [
			"filtered",
			"source",
			"own",
			"host",
			"link",
			"linkFiltered",
			"climb",
			"readLink",
			"readClimb",
		] {
			let result = run(
				bin,
				root.path(),
				&[
					"eval",
					"--raw",
					"--no-write-lock-file",
					&format!("path:{}#{attr}", flake.display()),
				],
			);
			output.push((
				result.status.code(),
				String::from_utf8_lossy(&result.stdout).into_owned(),
				String::from_utf8_lossy(&result.stderr)
					.contains("forbidden in pure evaluation mode"),
			));
		}
		outputs.push(output);
	}
	assert_eq!(outputs[0][0].0, Some(0), "{outputs:?}");
	assert_eq!(outputs[0][3], (Some(1), String::new(), true));
	assert_eq!(outputs[1], outputs[0]);
}

/// A pure flake reads a path its evaluation copied to the store, through
/// a string with context or a context-free path value, as Nixpkgs reads
/// `/${./11}/patches/...`, because CppNix's `copyPathToStore` allows it.
/// A store path the evaluation neither fetched, copied nor built stays
/// forbidden, and so does the derivation it wrote, whose context alone
/// grants nothing.
#[test]
fn pure_flake_reads_copied_paths_but_no_other_store_path() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	let flake = fixture.path().join("flake");
	fs::create_dir_all(flake.join("dir")).unwrap();
	fs::write(flake.join("dir/file"), "copied\n").unwrap();
	fs::write(fixture.path().join("other"), "other\n").unwrap();
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
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
				"--name",
				"other",
				fixture.path().join("other").to_str().unwrap(),
			],
		);
		assert!(
			added.status.success(),
			"{}",
			String::from_utf8_lossy(&added.stderr)
		);
		let other = String::from_utf8(added.stdout).unwrap().trim().to_owned();
		fs::write(
			flake.join("flake.nix"),
			format!(
				"{{ outputs = {{ self }}: {{ string = builtins.readFile \"${{./dir}}/file\"; path = builtins.readFile /${{./dir}}/file; other = builtins.readFile \"{other}\"; drv = builtins.readFile (derivation {{ name = \"drv\"; builder = \"/bin/sh\"; system = \"x86_64-linux\"; }}).drvPath; }}; }}"
			),
		)
		.unwrap();
		let mut output = Vec::new();
		for attr in ["string", "path", "other", "drv"] {
			let result = run(
				bin,
				root.path(),
				&[
					"eval",
					"--raw",
					"--no-write-lock-file",
					&format!("path:{}#{attr}", flake.display()),
				],
			);
			// Positions name the flake's physical copy in each root.
			output.push((
				result.status.code(),
				String::from_utf8_lossy(&result.stdout).into_owned(),
				String::from_utf8_lossy(&result.stderr)
					.replace(root.path().to_str().unwrap(), "<root>"),
			));
		}
		outputs.push(output);
	}
	for allowed in &outputs[0][..2] {
		assert_eq!((allowed.0, allowed.1.as_str()), (Some(0), "copied\n"));
	}
	for denied in &outputs[0][2..] {
		assert_eq!(denied.0, Some(1));
		assert!(
			denied
				.2
				.contains("is forbidden in pure evaluation mode (use '--impure' to override)"),
			"{}",
			denied.2
		);
	}
	assert_eq!(outputs[1], outputs[0]);
}

/// Pure `pathExists` and `readFileType` lstat a flake symlink whose target
/// is outside the allow list, as CppNix does, while reading through the
/// symlink stays forbidden.
#[test]
fn pure_flake_inspects_a_symlink_without_reading_its_target() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	let flake = fixture.path().join("flake");
	fs::create_dir_all(&flake).unwrap();
	std::os::unix::fs::symlink("/etc/passwd", flake.join("link")).unwrap();
	fs::write(
		flake.join("flake.nix"),
		"{ outputs = { self }: { exists = builtins.pathExists ./link; type = builtins.readFileType ./link; read = builtins.readFile ./link; }; }",
	)
	.unwrap();
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
	let mut outputs = Vec::new();
	for (bin, root) in [reference.as_path(), Path::new(env!("CARGO_BIN_EXE_nix"))]
		.iter()
		.zip(&roots)
	{
		let mut output = Vec::new();
		for attr in ["exists", "type", "read"] {
			let result = run(
				bin,
				root.path(),
				&[
					"eval",
					"--json",
					"--no-write-lock-file",
					&format!("path:{}#{attr}", flake.display()),
				],
			);
			output.push((
				result.status.code(),
				String::from_utf8_lossy(&result.stdout).into_owned(),
				String::from_utf8_lossy(&result.stderr)
					.replace(root.path().to_str().unwrap(), "<root>"),
			));
		}
		outputs.push(output);
	}
	assert_eq!(
		(outputs[0][0].0, outputs[0][0].1.as_str()),
		(Some(0), "true\n")
	);
	assert_eq!(
		(outputs[0][1].0, outputs[0][1].1.as_str()),
		(Some(0), "\"symlink\"\n")
	);
	assert_eq!(outputs[0][2].0, Some(1));
	assert!(
		outputs[0][2]
			.2
			.contains("access to absolute path '/etc/passwd' is forbidden in pure evaluation mode"),
		"{}",
		outputs[0][2].2
	);
	assert_eq!(outputs[1], outputs[0]);
}

/// The pure root serves only the store, as in CppNix: `/` is an empty
/// directory, `/nix` does not exist, and copying `/` yields an empty tree
/// instead of walking the host filesystem.
#[test]
fn pure_flake_root_serves_only_the_store() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	let flake = fixture.path().join("flake");
	fs::create_dir_all(&flake).unwrap();
	fs::write(
		flake.join("flake.nix"),
		"{ outputs = { self }: { root = builtins.readDir /.; exists = builtins.pathExists /nix; type = builtins.readFileType /nix; copy = builtins.path { path = /nix; name = \"nix\"; }; empty = builtins.path { path = /.; name = \"root\"; }; }; }",
	)
	.unwrap();
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
	let mut outputs = Vec::new();
	for (bin, root) in [reference.as_path(), Path::new(env!("CARGO_BIN_EXE_nix"))]
		.iter()
		.zip(&roots)
	{
		let mut output = Vec::new();
		for attr in ["root", "exists", "type", "copy", "empty"] {
			let result = run(
				bin,
				root.path(),
				&[
					"eval",
					"--json",
					"--read-only",
					"--no-write-lock-file",
					&format!("path:{}#{attr}", flake.display()),
				],
			);
			output.push((
				result.status.code(),
				String::from_utf8_lossy(&result.stdout).into_owned(),
				String::from_utf8_lossy(&result.stderr)
					.replace(root.path().to_str().unwrap(), "<root>"),
			));
		}
		outputs.push(output);
	}
	assert_eq!(
		(outputs[0][0].0, outputs[0][0].1.as_str()),
		(Some(0), "{}\n")
	);
	assert_eq!(
		(outputs[0][1].0, outputs[0][1].1.as_str()),
		(Some(0), "false\n")
	);
	for missing in &outputs[0][2..4] {
		assert_eq!(missing.0, Some(1));
		assert!(
			missing.2.contains("path '/nix' does not exist"),
			"{}",
			missing.2
		);
	}
	assert_eq!(outputs[0][4].0, Some(0));
	assert_eq!(outputs[1], outputs[0]);
}

#[test]
fn show_text_and_json_preserve_lazy_outputs_and_omissions() {
	let reference = PathBuf::from(std::env::var_os("NOXI_REFERENCE_BIN").unwrap()).join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	fs::write(fixture.path().join("flake.nix"), r#"{ outputs = { self }: let d = { type = "derivation"; name = "demo-1"; meta.description = "demo"; }; in {
		packages.x86_64-linux.default = d;
		packages.aarch64-linux.default = d;
		packages.empty = {};
		devShells.x86_64-linux.default = d;
		checks.x86_64-linux.test = d;
		legacyPackages.x86_64-linux.demo = d;
		apps.x86_64-linux.default = { type = "app"; meta.description = "app description"; };
		templates.default.description = "template description";
		nixosConfigurations.host = throw "lazy";
		nixosModules.default = throw "lazy";
		overlays.default = throw "lazy";
		unknown = throw "lazy";
	}; }"#).unwrap();
	let url = format!("path:{}", fixture.path().display());
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
	for flags in [
		vec![],
		vec!["--all-systems"],
		vec!["--legacy"],
		vec!["--json"],
		vec!["--json", "--all-systems", "--legacy"],
		vec!["--json", "--pretty"],
		vec!["--json", "--pretty", "--no-pretty"],
		vec!["--quiet"],
		vec!["--json", "--quiet"],
	] {
		let mut args = vec!["flake", "show", &url];
		args.extend(flags);
		let a = run(&reference, roots[0].path(), &args);
		let b = run(Path::new(env!("CARGO_BIN_EXE_nix")), roots[1].path(), &args);
		assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
		assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
		if args.contains(&"--pretty") {
			assert_eq!(a.stdout, b.stdout);
		}
		if args.contains(&"--json") {
			assert_eq!(
				serde_json::from_slice::<serde_json::Value>(&a.stdout).unwrap(),
				serde_json::from_slice::<serde_json::Value>(&b.stdout).unwrap()
			);
		} else {
			assert_eq!(
				String::from_utf8_lossy(&a.stdout),
				String::from_utf8_lossy(&b.stdout),
				"{args:?}"
			);
		}
		assert_eq!(
			String::from_utf8_lossy(&a.stderr),
			String::from_utf8_lossy(&b.stderr),
			"{args:?}"
		);
	}
}

#[test]
fn lock_creation_and_refusal_diagnostics_match() {
	let reference = PathBuf::from(std::env::var_os("NOXI_REFERENCE_BIN").unwrap()).join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	let dep = fixture.path().join("dep");
	fs::create_dir(&dep).unwrap();
	fs::write(dep.join("flake.nix"), "{ outputs = { self }: {}; }").unwrap();
	let project = fixture.path().join("project");
	fs::create_dir(&project).unwrap();
	let url = format!("path:{}?lastModified=1", project.display());
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
	for flags in [
		vec!["--no-write-lock-file"],
		vec!["--no-update-lock-file"],
		vec![],
		vec!["--json"],
	] {
		let mut results = Vec::new();
		let mut locks = Vec::new();
		for (engine, bin) in [reference.as_path(), Path::new(env!("CARGO_BIN_EXE_nix"))]
			.iter()
			.enumerate()
		{
			let lock = project.join("flake.lock");
			let _ = fs::remove_file(&lock);
			fs::write(
				project.join("flake.nix"),
				format!(
					"{{ inputs.dep.url = \"path:{}\"; outputs = {{ self, dep }}: {{}}; }}",
					dep.display()
				),
			)
			.unwrap();
			let mut args = vec!["flake", "metadata", &url];
			args.extend(flags.iter().copied());
			let result = run(bin, roots[engine].path(), &args);
			locks.push(fs::read(&lock).ok());
			results.push(result);
		}
		assert_eq!(
			results[0].status.code(),
			results[1].status.code(),
			"{flags:?}"
		);
		assert_eq!(locks[0], locks[1]);
		if flags.contains(&"--no-update-lock-file") {
			assert!(!results[0].status.success());
		} else {
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
		}
		assert_eq!(
			String::from_utf8_lossy(&results[0].stderr),
			String::from_utf8_lossy(&results[1].stderr),
			"{flags:?}"
		);
		if flags.contains(&"--json") {
			assert_eq!(
				serde_json::from_slice::<serde_json::Value>(&results[0].stdout).unwrap(),
				serde_json::from_slice::<serde_json::Value>(&results[1].stdout).unwrap()
			);
		} else {
			assert_eq!(
				String::from_utf8_lossy(&results[0].stdout),
				String::from_utf8_lossy(&results[1].stdout)
			);
		}
	}
}

#[test]
fn archive_destination_dry_run_and_prefetch_link_match() {
	let reference = PathBuf::from(std::env::var_os("NOXI_REFERENCE_BIN").unwrap()).join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	fs::write(
		fixture.path().join("flake.nix"),
		"{ outputs = { self }: {}; }",
	)
	.unwrap();
	let url = format!("path:{}", fixture.path().display());
	for operation in ["archive", "prefetch"] {
		for dry in [false, true] {
			if operation == "prefetch" && dry {
				continue;
			}
			let roots = [
				noxi_test_support::tempdir().unwrap(),
				noxi_test_support::tempdir().unwrap(),
			];
			let mut jsons = Vec::new();
			let mut diagnostics = Vec::new();
			for (engine, bin) in [reference.as_path(), Path::new(env!("CARGO_BIN_EXE_nix"))]
				.iter()
				.enumerate()
			{
				let root = roots[engine].path();
				let link = root.join("result");
				let destination = format!("local?root={}/destination", root.display());
				let mut args = vec!["flake", operation, &url, "--json"];
				if operation == "archive" {
					args.extend(["--to", &destination]);
					if dry {
						args.push("--dry-run");
					}
				} else {
					args.extend(["--out-link", link.to_str().unwrap()]);
				}
				let result = run(bin, root, &args);
				assert!(
					result.status.success(),
					"{args:?}: {}",
					String::from_utf8_lossy(&result.stderr)
				);
				let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
				let path = json[if operation == "prefetch" {
					"storePath"
				} else {
					"path"
				}]
				.as_str()
				.unwrap();
				if operation == "prefetch" {
					assert_eq!(fs::read_link(link).unwrap().to_str().unwrap(), path);
				} else {
					assert_eq!(
						root.join("destination")
							.join(path.trim_start_matches('/'))
							.exists(),
						!dry
					);
				}
				jsons.push(json);
				diagnostics.push(
					String::from_utf8(result.stderr)
						.unwrap()
						.replace(&destination, "DESTINATION"),
				);
			}
			assert_eq!(jsons[0], jsons[1]);
			assert_eq!(diagnostics[0], diagnostics[1]);
		}
	}
}

#[test]
fn command_registry_overrides_and_inputs_from_resolve_locally() {
	let reference = PathBuf::from(std::env::var_os("NOXI_REFERENCE_BIN").unwrap()).join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	let dep = fixture.path().join("dep");
	fs::create_dir(&dep).unwrap();
	fs::write(dep.join("flake.nix"), "{ outputs = { self }: {}; }").unwrap();
	let dep_url = format!("path:{}", dep.display());
	let provider = fixture.path().join("provider");
	fs::create_dir(&provider).unwrap();
	fs::write(
		provider.join("flake.nix"),
		format!("{{ inputs.demo.url = \"{dep_url}\"; outputs = {{ self, demo }}: {{}}; }}"),
	)
	.unwrap();
	let provider_url = format!("path:{}", provider.display());
	let root = noxi_test_support::tempdir().unwrap();
	let locked = run(&reference, root.path(), &["flake", "lock", &provider_url]);
	assert!(locked.status.success());
	let prefetch_roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
	let mut fetches = Vec::new();
	for (engine, bin) in [reference.as_path(), Path::new(env!("CARGO_BIN_EXE_nix"))]
		.iter()
		.enumerate()
	{
		let result = run(
			bin,
			prefetch_roots[engine].path(),
			&["flake", "prefetch-inputs", &provider_url],
		);
		assert!(
			result.status.success(),
			"{}",
			String::from_utf8_lossy(&result.stderr)
		);
		fetches.push((result.stdout, result.stderr));
	}
	assert_eq!(fetches[0], fetches[1]);
	for flags in [
		vec!["--override-flake", "demo", &dep_url],
		vec!["--inputs-from", &provider_url],
	] {
		let roots = [
			noxi_test_support::tempdir().unwrap(),
			noxi_test_support::tempdir().unwrap(),
		];
		let mut outputs = Vec::new();
		for (engine, bin) in [reference.as_path(), Path::new(env!("CARGO_BIN_EXE_nix"))]
			.iter()
			.enumerate()
		{
			let mut args = vec!["flake", "metadata", "demo", "--json"];
			args.extend(flags.iter().copied());
			let result = run(bin, roots[engine].path(), &args);
			assert!(
				result.status.success(),
				"{args:?}: {}",
				String::from_utf8_lossy(&result.stderr)
			);
			outputs.push(serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap());
			assert!(
				result.stderr.is_empty(),
				"{}",
				String::from_utf8_lossy(&result.stderr)
			);
		}
		assert_eq!(outputs[0], outputs[1]);
	}
}

#[test]
fn git_root_lock_intent_to_add_and_commit_match_reference() {
	let reference = PathBuf::from(std::env::var_os("NOXI_REFERENCE_BIN").unwrap()).join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	let dep = fixture.path().join("dep");
	fs::create_dir(&dep).unwrap();
	fs::write(dep.join("flake.nix"), "{ outputs = { self }: {}; }").unwrap();
	let git = |repo: &Path, args: &[&str]| {
		let result = Command::new("git")
			.env("GIT_CONFIG_GLOBAL", "/dev/null")
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00Z")
			.env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z")
			.arg("-C")
			.arg(repo)
			.args(args)
			.output()
			.unwrap();
		assert!(
			result.status.success(),
			"{}",
			String::from_utf8_lossy(&result.stderr)
		);
		result.stdout
	};
	for commit in [false, true] {
		let mut results = Vec::new();
		for (engine, bin) in [reference.as_path(), Path::new(env!("CARGO_BIN_EXE_nix"))]
			.iter()
			.enumerate()
		{
			let repo = fixture.path().join(format!("repo-{commit}-{engine}"));
			fs::create_dir(&repo).unwrap();
			git(&repo, &["init", "-b", "main"]);
			git(&repo, &["config", "user.email", "test@example.invalid"]);
			git(&repo, &["config", "user.name", "Test"]);
			fs::write(
				repo.join("flake.nix"),
				format!(
					"{{ inputs.dep.url = \"path:{}?lastModified=1\"; outputs = {{ self, dep }}: {{}}; }}",
					dep.display()
				),
			)
			.unwrap();
			git(&repo, &["add", "flake.nix"]);
			git(&repo, &["commit", "-m", "initial"]);
			let root = noxi_test_support::tempdir().unwrap();
			let url = format!("git+file://{}", repo.display());
			let mut args = vec!["flake", "lock", &url];
			if commit {
				args.push("--commit-lock-file");
			}
			let result = run(bin, root.path(), &args);
			assert!(
				result.status.success(),
				"{}",
				String::from_utf8_lossy(&result.stderr)
			);
			results.push((
				result.stdout,
				String::from_utf8(result.stderr)
					.unwrap()
					.replace(repo.to_str().unwrap(), "REPO"),
				fs::read(repo.join("flake.lock")).unwrap(),
				git(&repo, &["status", "--porcelain"]),
				git(&repo, &["log", "-1", "--format=%H%n%B"]),
			));
		}
		assert_eq!(results[0], results[1], "commit {commit}");
	}
}

#[test]
fn selective_flake_update_keeps_other_git_lock_bytes() {
	let reference = PathBuf::from(std::env::var_os("NOXI_REFERENCE_BIN").unwrap()).join("nix");
	let fixture = noxi_test_support::tempdir().unwrap();
	let git = |repo: &Path, args: &[&str]| {
		let out = Command::new("git")
			.env("GIT_CONFIG_GLOBAL", "/dev/null")
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00Z")
			.env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z")
			.arg("-C")
			.arg(repo)
			.args(args)
			.output()
			.unwrap();
		assert!(
			out.status.success(),
			"{}",
			String::from_utf8_lossy(&out.stderr)
		);
	};
	for name in ["a", "b"] {
		let repo = fixture.path().join(name);
		fs::create_dir(&repo).unwrap();
		git(&repo, &["init", "-b", "main"]);
		git(&repo, &["config", "user.email", "test@example.invalid"]);
		git(&repo, &["config", "user.name", "Test"]);
		fs::write(
			repo.join("flake.nix"),
			"{ outputs = { self }: { answer = 1; }; }",
		)
		.unwrap();
		git(&repo, &["add", "."]);
		git(&repo, &["commit", "-m", "one"]);
	}
	let project = fixture.path().join("project");
	fs::create_dir(&project).unwrap();
	fs::write(project.join("flake.nix"), format!("{{ inputs.a.url = \"git+file://{}/a\"; inputs.b.url = \"git+file://{}/b\"; outputs = {{ self, a, b }}: {{}}; }}", fixture.path().display(), fixture.path().display())).unwrap();
	let url = format!("path:{}?lastModified=1", project.display());
	let roots = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
	let initial = run(&reference, roots[0].path(), &["flake", "lock", &url]);
	assert!(initial.status.success());
	let initial = fs::read(project.join("flake.lock")).unwrap();
	for name in ["a", "b"] {
		let repo = fixture.path().join(name);
		fs::write(
			repo.join("flake.nix"),
			"{ outputs = { self }: { answer = 2; }; }",
		)
		.unwrap();
		git(&repo, &["add", "."]);
		git(&repo, &["commit", "-m", "two"]);
	}
	let mut results = Vec::new();
	for (engine, bin) in [reference.as_path(), Path::new(env!("CARGO_BIN_EXE_nix"))]
		.iter()
		.enumerate()
	{
		fs::write(project.join("flake.lock"), &initial).unwrap();
		let result = run(
			bin,
			roots[engine].path(),
			&["flake", "update", "a", "--flake", &url],
		);
		assert!(
			result.status.success(),
			"{}",
			String::from_utf8_lossy(&result.stderr)
		);
		let lock = fs::read(project.join("flake.lock")).unwrap();
		let old: serde_json::Value = serde_json::from_slice(&initial).unwrap();
		let new: serde_json::Value = serde_json::from_slice(&lock).unwrap();
		assert_eq!(old["nodes"]["b"], new["nodes"]["b"]);
		assert_ne!(old["nodes"]["a"]["locked"], new["nodes"]["a"]["locked"]);
		results.push((result.stdout, result.stderr, lock));
	}
	assert_eq!(results[0], results[1]);
}
