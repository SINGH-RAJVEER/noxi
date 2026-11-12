//! Native modern command comparisons with private stores.
use std::{
	fs,
	io::Write,
	path::{Path, PathBuf},
	process::{Command, Output, Stdio},
};
fn reference() -> PathBuf {
	std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix")
}
fn run(bin: &Path, root: &Path, args: &[&str], input: &[u8]) -> Output {
	let mut child = Command::new(bin)
		.env_clear()
		.env("HOME", root)
		.env("NIX_CONF_DIR", root)
		.env("NIX_USER_CONF_FILES", "")
		.env("NIX_CONFIG", "substituters =\nbuild-users-group =\n")
		.args([
			"--extra-experimental-features",
			"nix-command flakes",
			"--store",
			&format!("local?root={}", root.display()),
		])
		.args(args)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	child.stdin.take().unwrap().write_all(input).unwrap();
	child.wait_with_output().unwrap()
}
fn same(args: &[&str], input: &[u8]) -> Output {
	let a = tempfile::tempdir().unwrap();
	let b = tempfile::tempdir().unwrap();
	let expected = run(&reference(), a.path(), args, input);
	let actual = run(Path::new(env!("CARGO_BIN_EXE_nix")), b.path(), args, input);
	assert!(
		expected.status.success(),
		"reference {args:?}: {}",
		String::from_utf8_lossy(&expected.stderr)
	);
	assert_eq!(
		actual.status.code(),
		expected.status.code(),
		"{args:?}: {}",
		String::from_utf8_lossy(&actual.stderr)
	);
	assert_eq!(actual.stdout, expected.stdout, "{args:?}");
	assert_eq!(actual.stderr, expected.stderr, "{args:?}");
	actual
}
#[test]
fn nar_pack_list_cat_and_key_conversion() {
	let d = tempfile::tempdir().unwrap();
	let tree = d.path().join("tree");
	fs::create_dir(&tree).unwrap();
	fs::write(tree.join("hello"), b"hello\n").unwrap();
	std::os::unix::fs::symlink("hello", tree.join("link")).unwrap();
	let packed = same(&["nar", "pack", tree.to_str().unwrap()], &[]);
	let archive = d.path().join("tree.nar");
	fs::write(&archive, &packed.stdout).unwrap();
	for flags in [
		vec![],
		vec!["--json"],
		vec!["--recursive"],
		vec!["--long"],
		vec!["--json", "--recursive"],
	] {
		let mut args = vec!["nar", "ls"];
		args.extend(flags);
		args.extend([archive.to_str().unwrap(), "/"]);
		same(&args, &[]);
	}
	same(&["nar", "cat", archive.to_str().unwrap(), "/hello"], &[]);
	let a = tempfile::tempdir().unwrap();
	let generated = run(
		&reference(),
		a.path(),
		&["key", "generate-secret", "--key-name", "test"],
		&[],
	);
	assert!(generated.status.success());
	same(&["key", "convert-secret-to-public"], &generated.stdout);
}
#[test]
fn native_prefetch_hashes_paths_and_permissions() {
	let d = tempfile::tempdir().unwrap();
	let source = d.path().join("hello");
	fs::write(&source, b"hello\n").unwrap();
	let url = format!("file://{}", source.display());
	for flags in [
		vec![],
		vec!["--json"],
		vec!["--json", "--executable"],
		vec!["--json", "--hash-type", "sha512"],
		vec!["--json", "--name", "renamed"],
	] {
		let mut args = vec!["store", "prefetch-file"];
		args.extend(flags);
		args.push(&url);
		same(&args, &[]);
	}
}

#[test]
fn derivation_json_import_and_upgrade_dry_run() {
	let a = tempfile::tempdir().unwrap();
	let expression = "derivation { name = \"hello\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; args = []; }";
	let show = run(
		&reference(),
		a.path(),
		&["derivation", "show", "--expr", expression],
		&[],
	);
	assert!(
		show.status.success(),
		"{}",
		String::from_utf8_lossy(&show.stderr)
	);
	let json: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
	let drv = json["derivations"]
		.as_object()
		.unwrap()
		.values()
		.next()
		.unwrap();
	for flags in [
		vec!["derivation", "add"],
		vec!["derivation", "add", "--dry-run"],
	] {
		same(&flags, drv.to_string().as_bytes());
	}
	let fallback = a.path().join("fallback.nix");
	fs::write(
		&fallback,
		"{ x86_64-linux = \"/nix/store/00000000000000000000000000000000-nix-9.9\"; }",
	)
	.unwrap();
	let url = format!("file://{}", fallback.display());
	same(
		&[
			"upgrade-nix",
			"--dry-run",
			"--profile",
			"/test/profile",
			"--nix-store-paths-url",
			&url,
		],
		&[],
	);
}

#[test]
fn store_signatures_hash_lookup_and_content_address_rewrite() {
	let fixture = tempfile::tempdir().unwrap();
	let source = fixture.path().join("hello-1.0");
	fs::write(&source, b"hello\n").unwrap();
	let a = tempfile::tempdir().unwrap();
	let b = tempfile::tempdir().unwrap();
	let reference = reference();
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix"));
	let generated = run(
		&reference,
		a.path(),
		&["key", "generate-secret", "--key-name", "test"],
		&[],
	);
	assert!(generated.status.success());
	let key = fixture.path().join("secret");
	fs::write(&key, &generated.stdout).unwrap();
	let expected = run(
		&reference,
		a.path(),
		&["store", "add-file", source.to_str().unwrap()],
		&[],
	);
	let actual = run(
		candidate,
		b.path(),
		&["store", "add-file", source.to_str().unwrap()],
		&[],
	);
	assert_eq!(expected.stdout, actual.stdout);
	let path = String::from_utf8(expected.stdout).unwrap();
	let path = path.trim();
	for args in [
		vec!["store", "sign", "--key-file", key.to_str().unwrap(), path],
		vec!["path-info", "--sigs", path],
		vec!["store", "path-from-hash-part", &path[11..43]],
		vec!["store", "make-content-addressed", "--json", path],
		vec!["store", "diff-closures", path, path],
	] {
		let expected = run(&reference, a.path(), &args, &[]);
		let actual = run(candidate, b.path(), &args, &[]);
		assert!(
			expected.status.success(),
			"{args:?}: {}",
			String::from_utf8_lossy(&expected.stderr)
		);
		assert_eq!(
			actual.status.code(),
			expected.status.code(),
			"{args:?}: {}",
			String::from_utf8_lossy(&actual.stderr)
		);
		assert_eq!(actual.stdout, expected.stdout, "{args:?}");
		assert_eq!(actual.stderr, expected.stderr, "{args:?}");
	}
}

#[test]
fn search_expression_names_descriptions_and_exclusions() {
	let expression = r#"{ hello = { type = "derivation"; name = "hello-1.2"; system = "x86_64-linux"; meta.description = "A friendly greeting"; }; other = { type = "derivation"; name = "other-2"; system = "x86_64-linux"; meta.description = "A tool"; }; }"#;
	for flags in [
		vec!["--json", "^"],
		vec!["--json", "greeting"],
		vec!["--json", "--exclude", "other", "^"],
		vec!["--json", "hello", "friendly"],
	] {
		let mut args = vec!["search", "--expr", expression, ""];
		args.extend(flags);
		same(&args, &[]);
	}
}

#[test]
fn repair_restores_corrupt_registered_contents_from_binary_cache() {
	use std::os::unix::fs::PermissionsExt;
	let fixture = tempfile::tempdir().unwrap();
	let file = fixture.path().join("hello");
	fs::write(&file, b"original\n").unwrap();
	for bin in [reference(), PathBuf::from(env!("CARGO_BIN_EXE_nix"))] {
		let root = tempfile::tempdir().unwrap();
		let cache = tempfile::tempdir().unwrap();
		let add = run(
			&bin,
			root.path(),
			&["store", "add-file", file.to_str().unwrap()],
			&[],
		);
		assert!(add.status.success());
		let path = String::from_utf8(add.stdout).unwrap();
		let path = path.trim();
		let uri = format!("file://{}?compression=none", cache.path().display());
		let copy = run(
			&reference(),
			root.path(),
			&["copy", "--to", &uri, path],
			&[],
		);
		assert!(
			copy.status.success(),
			"{}",
			String::from_utf8_lossy(&copy.stderr)
		);
		let physical = root.path().join(path.trim_start_matches('/'));
		fs::set_permissions(&physical, fs::Permissions::from_mode(0o644)).unwrap();
		fs::write(&physical, b"corrupt!").unwrap();
		let repaired = run(
			&bin,
			root.path(),
			&["--option", "substituters", &uri, "store", "repair", path],
			&[],
		);
		assert!(
			repaired.status.success(),
			"{}",
			String::from_utf8_lossy(&repaired.stderr)
		);
		assert_eq!(fs::read(&physical).unwrap(), b"original\n");
	}
}

#[test]
fn help_matches_reference_lookup_and_fails_explicitly_for_pages() {
	// `showHelp`: unknown pages are the reference's usage error; existing
	// pages are rendered from CppNix's embedded manual, which noxi does not
	// embed, so rendering fails explicitly.
	let root = tempfile::tempdir().unwrap();
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix"));
	for args in [vec!["help", "nonexistent"], vec!["help", "a", "b"]] {
		let expected = run(&reference(), root.path(), &args, &[]);
		let actual = run(candidate, root.path(), &args, &[]);
		assert_eq!(actual.status.code(), expected.status.code(), "{args:?}");
		// The second line names the executable's own path.
		let first = |o: &Output| {
			String::from_utf8_lossy(&o.stderr)
				.lines()
				.next()
				.map(str::to_owned)
		};
		assert_eq!(first(&actual), first(&expected), "{args:?}");
	}
	for args in [vec!["help", "store", "sign"], vec!["help-stores"]] {
		let output = run(candidate, root.path(), &args, &[]);
		assert!(!output.status.success());
		assert!(String::from_utf8_lossy(&output.stderr).contains("is not supported by noxi yet"));
	}
}

#[test]
fn compressed_command_uploads_import_with_reference() {
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix"));
	let source = tempfile::tempdir().unwrap();
	let file = source.path().join("payload");
	fs::write(&file, b"compressed command upload\n").unwrap();
	let added = run(
		candidate,
		source.path(),
		&["store", "add-file", file.to_str().unwrap()],
		&[],
	);
	assert!(
		added.status.success(),
		"{}",
		String::from_utf8_lossy(&added.stderr)
	);
	let path = String::from_utf8(added.stdout).unwrap();
	let path = path.trim();
	let key = noxi_store::keys::SecretKey::generate("command-test").unwrap();
	let secret = source.path().join("secret");
	fs::write(&secret, key.to_text()).unwrap();
	for codec in ["none", "gzip", "bzip2", "zstd"] {
		let cache = tempfile::tempdir().unwrap();
		let uri = format!(
			"file://{}?compression={codec}&secret-key={}",
			cache.path().display(),
			secret.display()
		);
		let copied = run(candidate, source.path(), &["copy", "--to", &uri, path], &[]);
		assert!(
			copied.status.success(),
			"{codec}: {}",
			String::from_utf8_lossy(&copied.stderr)
		);
		let destination = tempfile::tempdir().unwrap();
		let uri = format!("file://{}", cache.path().display());
		let imported = run(
			&reference(),
			destination.path(),
			&["copy", "--no-check-sigs", "--from", &uri, path],
			&[],
		);
		assert!(
			imported.status.success(),
			"{codec}: {}",
			String::from_utf8_lossy(&imported.stderr)
		);
		assert_eq!(
			fs::read(destination.path().join(path.trim_start_matches('/'))).unwrap(),
			b"compressed command upload\n"
		);
		let verified = run(
			&reference(),
			destination.path(),
			&["store", "verify", "--no-trust", path],
			&[],
		);
		assert!(
			verified.status.success(),
			"{codec}: {}",
			String::from_utf8_lossy(&verified.stderr)
		);
	}
}
