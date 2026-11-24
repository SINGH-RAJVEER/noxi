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
	let a = noxi_test_support::tempdir().unwrap();
	let b = noxi_test_support::tempdir().unwrap();
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
	let d = noxi_test_support::tempdir().unwrap();
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
	let a = noxi_test_support::tempdir().unwrap();
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
	let d = noxi_test_support::tempdir().unwrap();
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
	let a = noxi_test_support::tempdir().unwrap();
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
	let fixture = noxi_test_support::tempdir().unwrap();
	let source = fixture.path().join("hello-1.0");
	fs::write(&source, b"hello\n").unwrap();
	let a = noxi_test_support::tempdir().unwrap();
	let b = noxi_test_support::tempdir().unwrap();
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
	let fixture = noxi_test_support::tempdir().unwrap();
	let file = fixture.path().join("hello");
	fs::write(&file, b"original\n").unwrap();
	for bin in [reference(), PathBuf::from(env!("CARGO_BIN_EXE_nix"))] {
		let root = noxi_test_support::tempdir().unwrap();
		let cache = noxi_test_support::tempdir().unwrap();
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
	let root = noxi_test_support::tempdir().unwrap();
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
	let source = noxi_test_support::tempdir().unwrap();
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
		let cache = noxi_test_support::tempdir().unwrap();
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
		let destination = noxi_test_support::tempdir().unwrap();
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

/// Every file and directory under `dir`, with file contents.
fn tree(dir: &Path) -> Vec<(String, Option<Vec<u8>>)> {
	let mut entries = Vec::new();
	let mut pending = vec![dir.to_path_buf()];
	while let Some(current) = pending.pop() {
		for entry in fs::read_dir(&current).unwrap() {
			let path = entry.unwrap().path();
			let name = path.strip_prefix(dir).unwrap().display().to_string();
			if path.is_dir() {
				entries.push((name, None));
				pending.push(path);
			} else {
				entries.push((name, Some(fs::read(&path).unwrap())));
			}
		}
	}
	entries.sort();
	entries
}

#[test]
fn file_cache_copies_write_the_reference_layout_and_bytes() {
	let root = noxi_test_support::tempdir().unwrap();
	let file = root.path().join("payload");
	fs::write(&file, b"cache upload\n").unwrap();
	let tree_dir = root.path().join("tree");
	fs::create_dir(&tree_dir).unwrap();
	fs::write(tree_dir.join("a"), b"a\n").unwrap();
	std::os::unix::fs::symlink("a", tree_dir.join("l")).unwrap();
	let mut paths = Vec::new();
	for args in [
		["store", "add-file", file.to_str().unwrap()],
		["store", "add", tree_dir.to_str().unwrap()],
	] {
		let added = run(&reference(), root.path(), &args, &[]);
		assert!(added.status.success());
		paths.push(String::from_utf8(added.stdout).unwrap().trim().to_owned());
	}
	let key = run(
		&reference(),
		root.path(),
		&["key", "generate-secret", "--key-name", "cache-1"],
		&[],
	);
	let key_file = root.path().join("cache-key");
	fs::write(&key_file, &key.stdout).unwrap();
	let cache = root.path().join("cache");
	let key = key_file.display();
	for params in [
		String::new(),
		"?compression=none".to_owned(),
		format!("?write-nar-listing=1&secret-key={key}"),
		format!("?compression=zstd&unknown-setting=1&secret-keys={key},{key}"),
		"?compression=bzip2&index-debug-info=true".to_owned(),
		"?compression=xz&compression-level=0".to_owned(),
		"?compression=bogus".to_owned(),
		"?secret-key=/nonexistent/key".to_owned(),
	] {
		let uri = format!("file://{}{params}", cache.display());
		let mut args = vec!["copy", "--to", uri.as_str()];
		args.extend(paths.iter().map(String::as_str));
		let mut results = Vec::new();
		for bin in [reference(), PathBuf::from(env!("CARGO_BIN_EXE_nix"))] {
			let _ = fs::remove_dir_all(&cache);
			let first = run(&bin, root.path(), &args, &[]);
			// A second copy finds every path in the cache.
			let again = run(&bin, root.path(), &args, &[]);
			// The reference uploads in parallel, so the order of its
			// per-path messages varies; the usage hint names the program.
			let lines = |output: &Output| {
				let mut lines: Vec<String> = String::from_utf8_lossy(&output.stderr)
					.lines()
					.map(|line| line.replace(bin.to_str().unwrap(), "nix"))
					.collect();
				lines.sort();
				(output.status.code(), output.stdout.clone(), lines)
			};
			let files = cache.exists().then(|| tree(&cache));
			results.push((lines(&first), lines(&again), files));
		}
		assert_eq!(results[1].0, results[0].0, "{params}");
		assert_eq!(results[1].1, results[0].1, "{params}");
		assert!(results[1].2 == results[0].2, "{params}: cache files differ");
	}
}

/// Serve `status` with a short body to every connection until dropped.
struct FailingServer {
	port: u16,
	stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl FailingServer {
	fn new(status: &'static str) -> Self {
		use std::io::{BufRead, BufReader};
		use std::sync::atomic::Ordering;
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		listener.set_nonblocking(true).unwrap();
		let port = listener.local_addr().unwrap().port();
		let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
		let stopped = stop.clone();
		std::thread::spawn(move || {
			while !stopped.load(Ordering::Relaxed) {
				let Ok((stream, _)) = listener.accept() else {
					std::thread::sleep(std::time::Duration::from_millis(2));
					continue;
				};
				stream.set_nonblocking(false).unwrap();
				let mut reader = BufReader::new(stream);
				loop {
					let mut line = String::new();
					if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
						break;
					}
				}
				let _ = write!(
					reader.get_mut(),
					"HTTP/1.1 {status}\r\nContent-Length: 4\r\nConnection: close\r\n\r\noops"
				);
			}
		});
		FailingServer { port, stop }
	}
}

impl Drop for FailingServer {
	fn drop(&mut self) {
		self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
	}
}

#[test]
fn unreachable_http_substituters_retry_and_warn_like_reference() {
	let fixture = noxi_test_support::tempdir().unwrap();
	let file = fixture.path().join("payload");
	fs::write(&file, b"retry fixture\n").unwrap();
	let source = noxi_test_support::tempdir().unwrap();
	let added = run(
		&reference(),
		source.path(),
		&["store", "add-file", file.to_str().unwrap()],
		&[],
	);
	let path = String::from_utf8(added.stdout).unwrap().trim().to_owned();
	let cache = fixture.path().join("cache");
	let uri = format!("file://{}?compression=none", cache.display());
	assert!(
		run(
			&reference(),
			source.path(),
			&["copy", "--to", &uri, &path],
			&[]
		)
		.status
		.success()
	);
	// A second copy whose `nix-cache-info` sets a priority.
	let ranked = fixture.path().join("ranked");
	fs::create_dir(&ranked).unwrap();
	for entry in fs::read_dir(&cache).unwrap() {
		let entry = entry.unwrap();
		if entry.path().is_dir() {
			fs::create_dir(ranked.join(entry.file_name())).unwrap();
			for file in fs::read_dir(entry.path()).unwrap() {
				let file = file.unwrap();
				fs::copy(
					file.path(),
					ranked.join(entry.file_name()).join(file.file_name()),
				)
				.unwrap();
			}
		} else {
			fs::copy(entry.path(), ranked.join(entry.file_name())).unwrap();
		}
	}
	fs::write(
		ranked.join("nix-cache-info"),
		"StoreDir: /nix/store\nPriority: 10\n",
	)
	.unwrap();
	let ranked = format!("file://{}", ranked.display());
	let cache = format!("file://{}", cache.display());
	let error = FailingServer::new("500 Internal Server Error");
	let missing = FailingServer::new("404 Not Found");
	let refused = "http://127.0.0.1:1".to_owned();
	let failing = format!("http://127.0.0.1:{}", error.port);
	let empty = format!("http://127.0.0.1:{}", missing.port);
	let normalise = |text: &[u8]| {
		let text = String::from_utf8_lossy(text);
		let mut out = String::new();
		let mut rest = &*text;
		// curl reports how long the failed connection took.
		while let Some(at) = rest.find(" after ") {
			out.push_str(&rest[..at + 7]);
			rest = &rest[at + 7..];
			let digits = rest.chars().take_while(char::is_ascii_digit).count();
			if digits > 0 && rest[digits..].starts_with(" ms") {
				out.push('N');
				rest = &rest[digits..];
			}
		}
		out + rest
	};
	for substituters in [
		format!("{refused} {cache}"),
		refused.clone(),
		format!("{failing} {cache}"),
		format!("{empty} {cache}"),
		// Caches are tried by priority, 0 unless set, and then in order.
		format!("{cache} {ranked}"),
		format!("{ranked} {cache}?priority=10"),
		format!("{cache}?priority=20 {ranked}"),
	] {
		let mut results = Vec::new();
		for bin in [
			reference().with_file_name("nix-store"),
			PathBuf::from(env!("CARGO_BIN_EXE_nix-store")),
		] {
			let root = noxi_test_support::tempdir().unwrap();
			let output = Command::new(&bin)
				.env_clear()
				.env("HOME", root.path())
				.env("NIX_CONF_DIR", root.path())
				.env("NIX_USER_CONF_FILES", "")
				.env("NIX_CONFIG", "build-users-group =\n")
				.args(["--store", &format!("local?root={}", root.path().display())])
				.args([
					"--realise",
					&path,
					"--option",
					"substituters",
					&substituters,
				])
				.args(["--option", "require-sigs", "false"])
				.args(["--option", "filetransfer-retry-jitter", "false"])
				.args(["--option", "filetransfer-retry-attempts", "3"])
				.output()
				.unwrap();
			let valid = root.path().join(path.trim_start_matches('/')).exists();
			results.push((
				output.status.code(),
				String::from_utf8_lossy(&output.stdout)
					.replace(&root.path().display().to_string(), "ROOT"),
				normalise(&output.stderr),
				valid,
			));
		}
		assert_eq!(results[1], results[0], "{substituters}");
	}
}
