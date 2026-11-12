//! Dependency explanations are compared against an isolated reference store.
use noxi_store::{LocalStore, LocalStoreConfig};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn command(binary: &Path, root: &Path) -> Command {
	let mut command = Command::new(binary);
	command
		.env_clear()
		.env("HOME", root)
		.env("NIX_CONF_DIR", root.join("conf"))
		.env(
			"NIX_CONFIG",
			"experimental-features = nix-command\nbuild-users-group =\nsubstituters =\n",
		)
		.args(["--store", &format!("local?root={}", root.display())]);
	command
}

fn reference() -> PathBuf {
	std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix")
}

#[test]
fn local_url_initializes_a_missing_diverted_root() {
	let run = |binary: &Path| {
		let temp = tempfile::tempdir().unwrap();
		let root = temp.path().join("missing");
		let output = command(binary, temp.path())
			.args([
				"--store",
				&format!("local://?root={}", root.display()),
				"store",
				"ping",
			])
			.output()
			.unwrap();
		assert!(
			output.status.success(),
			"{}",
			String::from_utf8_lossy(&output.stderr)
		);
		assert!(root.join("nix/var/nix/db/db.sqlite").exists());
		(output.status.code(), output.stdout)
	};
	assert_eq!(run(Path::new(env!("CARGO_BIN_EXE_nix"))), run(&reference()));
}

#[test]
fn local_url_info_json_formats_match_reference() {
	let temp = tempfile::tempdir().unwrap();
	for scheme in ["local", "local://"] {
		for flags in [
			vec!["--json"],
			vec!["--json", "--pretty"],
			vec!["--json", "--no-pretty"],
		] {
			let run = |binary: &Path| {
				command(binary, temp.path())
					.args([
						"--store",
						&format!("{scheme}?root={}", temp.path().display()),
						"store",
						"info",
					])
					.args(&flags)
					.output()
					.unwrap()
			};
			let expected = run(&reference());
			let actual = run(Path::new(env!("CARGO_BIN_EXE_nix")));
			assert!(
				expected.status.success(),
				"{}",
				String::from_utf8_lossy(&expected.stderr)
			);
			assert_eq!(actual.status.code(), expected.status.code());
			assert_eq!(actual.stdout, expected.stdout);
			assert_eq!(actual.stderr, expected.stderr);
		}
	}
}

#[test]
fn dependency_paths_match_reference() {
	let temp = tempfile::tempdir().unwrap();
	let store = LocalStore::open(LocalStoreConfig::diverted(temp.path()).unwrap()).unwrap();
	let leaf = store
		.add_text_to_store("leaf", b"leaf", &BTreeSet::new(), false)
		.unwrap();
	let middle = store
		.add_text_to_store(
			"middle",
			leaf.to_absolute_path().as_bytes(),
			&BTreeSet::from([leaf.clone()]),
			false,
		)
		.unwrap();
	let top = store
		.add_text_to_store(
			"top",
			middle.to_absolute_path().as_bytes(),
			&BTreeSet::from([middle.clone()]),
			false,
		)
		.unwrap();
	for (package, dependency) in [(&top, &leaf), (&leaf, &top), (&leaf, &leaf)] {
		for flag in [None, Some("--all")] {
			let args = [
				"why-depends",
				&package.to_absolute_path(),
				&dependency.to_absolute_path(),
			];
			let expected = command(&reference(), temp.path())
				.args(args)
				.args(flag)
				.output()
				.unwrap();
			let actual = command(Path::new(env!("CARGO_BIN_EXE_nix")), temp.path())
				.args(args)
				.args(flag)
				.output()
				.unwrap();
			assert_eq!(actual.status.code(), expected.status.code());
			assert_eq!(
				actual.stdout,
				expected.stdout,
				"{}",
				String::from_utf8_lossy(&actual.stdout)
			);
			assert_eq!(actual.stderr, expected.stderr);
		}
	}
}

#[test]
fn repl_keeps_lazy_bindings_and_recovers_after_errors() {
	let input: &[u8] =
		b"x = 3\nx + 4\nx = x + 1\nx\n{\na = 1;\n}\nbad = throw \"deferred\"\nbad\n1 + 2\n:q\n";
	let session = |binary: &Path| {
		let temp = tempfile::tempdir().unwrap();
		let mut child = command(binary, temp.path())
			.args(["repl", "--impure"])
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap();
		child.stdin.take().unwrap().write_all(input).unwrap();
		child.wait_with_output().unwrap()
	};
	let expected = session(&reference());
	let output = session(Path::new(env!("CARGO_BIN_EXE_nix")));
	assert!(output.status.success());
	assert_eq!(output.status.code(), expected.status.code());
	// Bindings stay lazy and the session continues after the error.
	assert_eq!(
		String::from_utf8_lossy(&output.stdout),
		String::from_utf8_lossy(&expected.stdout)
	);
	assert_eq!(
		String::from_utf8_lossy(&output.stderr),
		String::from_utf8_lossy(&expected.stderr)
	);
	assert!(String::from_utf8_lossy(&output.stderr).contains("deferred"));
}

#[test]
fn store_init_creates_database_in_isolated_store() {
	let temp = tempfile::tempdir().unwrap();
	let output = Command::new(env!("CARGO_BIN_EXE_nix-store"))
		.env_clear()
		.env("HOME", temp.path())
		.env("NIX_CONF_DIR", temp.path().join("conf"))
		.env("NIX_CONFIG", "build-users-group =\nsubstituters =\n")
		.args([
			"--store",
			&format!("local?root={}", temp.path().display()),
			"--init",
		])
		.output()
		.unwrap();
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert!(temp.path().join("nix/var/nix/db/db.sqlite").exists());
}

#[test]
fn bundle_rejects_a_non_derivation_from_custom_bundler() {
	let temp = tempfile::tempdir().unwrap();
	let flake = temp.path().join("flake");
	std::fs::create_dir(&flake).unwrap();
	std::fs::write(
		flake.join("flake.nix"),
		"{ outputs = { self }: { bundlers.x86_64-linux.default = app: 42; }; }",
	)
	.unwrap();
	let output = command(Path::new(env!("CARGO_BIN_EXE_nix")), temp.path())
		.args([
			"--extra-experimental-features",
			"flakes",
			"bundle",
			"--impure",
			"--expr",
			"{}",
			"--bundler",
			&format!("path:{}", flake.display()),
		])
		.output()
		.unwrap();
	assert!(!output.status.success());
	assert!(
		String::from_utf8_lossy(&output.stderr).contains("does not produce a derivation"),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
}

#[test]
fn bundle_builds_custom_derivation_and_roots_named_output() {
	use std::io::Seek;
	let temp = tempfile::tempdir().unwrap();
	let busybox = std::fs::canonicalize(
		Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-busybox"),
	)
	.unwrap();
	let path = nix_compat::store_path::StorePath::from_absolute_path(
		busybox.as_os_str().as_encoded_bytes(),
	)
	.unwrap();
	let (hash, size) = noxi_store::nar::hash_path(&busybox).unwrap();
	let mut info = noxi_store::ValidPathInfo::new(path, hash);
	info.nar_size = size;
	let store = LocalStore::open(LocalStoreConfig::diverted(temp.path()).unwrap()).unwrap();
	let mut nar = tempfile::tempfile().unwrap();
	noxi_store::nar::dump_path(&mut nar, &busybox).unwrap();
	nar.rewind().unwrap();
	store
		.add_to_store(&info, &mut std::io::BufReader::new(nar), false, false)
		.unwrap();
	let flake = temp.path().join("flake");
	std::fs::create_dir(&flake).unwrap();
	std::fs::write(flake.join("flake.nix"), format!("{{ outputs = {{ self }}: {{ bundlers.x86_64-linux.default = app: let bb = builtins.storePath {}; in derivation {{ name = \"bundled-test\"; system = \"x86_64-linux\"; builder = \"${{bb}}/bin/sh\"; args = [ \"-c\" \"echo bundled > $out\" ]; }}; }}; }}", busybox.display())).unwrap();
	let link = temp.path().join("bundle");
	let output = command(Path::new(env!("CARGO_BIN_EXE_nix")), temp.path())
		.args([
			"--extra-experimental-features",
			"flakes",
			"bundle",
			"--impure",
			"--expr",
			"{}",
			"--bundler",
			&format!("path:{}", flake.display()),
			"--out-link",
			link.to_str().unwrap(),
		])
		.output()
		.unwrap();
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	let output_path = std::fs::read_link(link).unwrap();
	let output_path = nix_compat::store_path::StorePath::from_absolute_path(
		output_path.as_os_str().as_encoded_bytes(),
	)
	.unwrap();
	assert_eq!(
		std::fs::read_to_string(store.real_path(&output_path)).unwrap(),
		"bundled\n"
	);
	let output = command(Path::new(env!("CARGO_BIN_EXE_nix")), temp.path())
		.current_dir(temp.path())
		.args([
			"--extra-experimental-features",
			"flakes",
			"bundle",
			"--impure",
			"--expr",
			"{}",
			"--bundler",
			&format!("path:{}", flake.display()),
		])
		.output()
		.unwrap();
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert_eq!(
		std::fs::read_link(temp.path().join("bundled-test")).unwrap(),
		PathBuf::from(output_path.to_absolute_path())
	);
}

#[test]
fn config_check_reports_missing_nix_in_path() {
	let temp = tempfile::tempdir().unwrap();
	let output = command(Path::new(env!("CARGO_BIN_EXE_nix")), temp.path())
		.env("PATH", "")
		.args(["config", "check"])
		.output()
		.unwrap();
	assert_eq!(output.status.code(), Some(2));
	assert!(String::from_utf8_lossy(&output.stderr).contains("[FAIL] No nix-env found in PATH."));
}
