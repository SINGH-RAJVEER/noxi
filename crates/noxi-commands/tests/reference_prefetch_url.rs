//! Fixed-output download differential tests. Every invocation uses a private store.
use std::{
	fs,
	path::{Path, PathBuf},
	process::{Command, Output},
};

fn reference(name: &str) -> PathBuf {
	std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join(name)
}
fn run(bin: &Path, root: &Path, flags: &[&str]) -> Output {
	Command::new(bin)
		.env_clear()
		.env("HOME", root)
		.env("NIX_CONF_DIR", root)
		.env("NIX_USER_CONF_FILES", "")
		.env("NIX_CONFIG", "substituters =\nbuild-users-group =\n")
		.args(["--store", &format!("local?root={}", root.display())])
		.args(flags)
		.output()
		.unwrap()
}
#[test]
fn files_hashes_paths_executable_and_unpacked_trees() {
	let fixture = tempfile::tempdir().unwrap();
	let source = fixture.path().join("hello");
	fs::write(&source, b"hello world\n").unwrap();
	let tree = fixture.path().join("tree");
	fs::create_dir(&tree).unwrap();
	fs::write(tree.join("default.nix"), "42\n").unwrap();
	let archive = fixture.path().join("tree.tar");
	assert!(
		Command::new("tar")
			.args(["cf"])
			.arg(&archive)
			.arg("-C")
			.arg(fixture.path())
			.arg("tree")
			.status()
			.unwrap()
			.success()
	);
	for (file, flags) in [
		(&source, vec![]),
		(&source, vec!["--executable"]),
		(&archive, vec!["--unpack"]),
	] {
		for algo in ["md5", "sha1", "sha256", "sha512"] {
			let a = tempfile::tempdir().unwrap();
			let b = tempfile::tempdir().unwrap();
			let url = format!("file://{}", file.display());
			let mut args = vec!["--print-path", "--name", "download", "--type", algo];
			args.extend(flags.iter().copied());
			args.push(&url);
			let expected = run(&reference("nix-prefetch-url"), a.path(), &args);
			let actual = run(
				Path::new(env!("CARGO_BIN_EXE_nix-prefetch-url")),
				b.path(),
				&args,
			);
			assert!(
				expected.status.success(),
				"{}",
				String::from_utf8_lossy(&expected.stderr)
			);
			assert!(
				actual.status.success(),
				"{}",
				String::from_utf8_lossy(&actual.stderr)
			);
			assert_eq!(actual.stdout, expected.stdout, "{args:?}");
			assert_eq!(actual.stderr, expected.stderr, "{args:?}");
			let stdout = String::from_utf8(actual.stdout).unwrap();
			let mut lines = stdout.lines();
			let hash = lines.next().unwrap();
			let path = lines.next().unwrap().trim_start_matches('/');
			let dump = |root: &Path| {
				Command::new(reference("nix-store"))
					.args(["--dump"])
					.arg(root.join(path))
					.output()
					.unwrap()
					.stdout
			};
			assert_eq!(dump(a.path()), dump(b.path()));
			let mut with_hash = args.clone();
			with_hash.push(hash);
			assert!(
				run(
					Path::new(env!("CARGO_BIN_EXE_nix-prefetch-url")),
					b.path(),
					&with_hash
				)
				.status
				.success()
			);
		}
	}
}
#[test]
fn mismatch_does_not_register_download() {
	let fixture = tempfile::tempdir().unwrap();
	let source = fixture.path().join("hello");
	fs::write(&source, "hello").unwrap();
	let root = tempfile::tempdir().unwrap();
	let output = run(
		Path::new(env!("CARGO_BIN_EXE_nix-prefetch-url")),
		root.path(),
		&[
			&format!("file://{}", source.display()),
			"0000000000000000000000000000000000000000000000000000",
		],
	);
	assert!(!output.status.success());
	assert!(String::from_utf8_lossy(&output.stderr).contains("hash mismatch"));
	assert!(output.stdout.is_empty());
	assert!(
		!fs::read_dir(root.path().join("nix/store"))
			.unwrap()
			.any(|e| e.unwrap().file_name().to_string_lossy().ends_with("-hello"))
	);
}

#[test]
fn http_redirects_and_expected_hash_reuse() {
	use std::{
		io::{Read, Write},
		net::TcpListener,
		time::{Duration, Instant},
	};
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	listener.set_nonblocking(true).unwrap();
	let addr = listener.local_addr().unwrap();
	let server = std::thread::spawn(move || {
		let deadline = Instant::now() + Duration::from_secs(10);
		let mut count = 0;
		while count < 4 && Instant::now() < deadline {
			let (mut stream, _) = match listener.accept() {
				Ok(pair) => pair,
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
					std::thread::sleep(Duration::from_millis(5));
					continue;
				}
				Err(e) => panic!("{e}"),
			};
			stream
				.set_read_timeout(Some(Duration::from_secs(2)))
				.unwrap();
			let mut request = [0; 4096];
			let n = stream.read(&mut request).unwrap();
			let response = if request[..n].starts_with(b"GET /redirect ") {
				"HTTP/1.1 302 Found\r\nLocation: /payload\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
			} else {
				"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
			};
			stream.write_all(response.as_bytes()).unwrap();
			count += 1;
		}
		count
	});
	let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
	let url = format!("http://{addr}/redirect");
	let args = ["--print-path", "--name", "download", &url];
	let a = run(&reference("nix-prefetch-url"), roots[0].path(), &args);
	let b = run(
		Path::new(env!("CARGO_BIN_EXE_nix-prefetch-url")),
		roots[1].path(),
		&args,
	);
	assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
	assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
	assert_eq!(a.stdout, b.stdout);
	assert_eq!(server.join().unwrap(), 4);
	let stdout = String::from_utf8(a.stdout).unwrap();
	let hash = stdout.lines().next().unwrap();
	// The server is gone: the expected fixed-output path must be reused without downloading.
	for (bin, root) in [
		reference("nix-prefetch-url"),
		PathBuf::from(env!("CARGO_BIN_EXE_nix-prefetch-url")),
	]
	.iter()
	.zip(&roots)
	{
		let mut args = args.to_vec();
		args.push(hash);
		let output = run(bin, root.path(), &args);
		assert!(
			output.status.success(),
			"{}",
			String::from_utf8_lossy(&output.stderr)
		);
		assert_eq!(output.stdout, stdout.as_bytes());
	}
}
