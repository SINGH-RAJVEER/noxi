//! Cross-engine cache exchange, with no external network.
use bstr::ByteSlice;
use noxi_eval::{EvalMode, Evaluation};
use noxi_glue::fetch::FetchConfig;
use noxi_glue::{EvaluatorStore, configure};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

fn archive(path: &Path, text: &[u8]) {
	let mut tar = tar::Builder::new(fs::File::create(path).unwrap());
	let mut header = tar::Header::new_gnu();
	header.set_size(text.len() as u64);
	header.set_mode(0o755);
	header.set_mtime(123);
	header.set_cksum();
	tar.append_data(&mut header, "root/file", text).unwrap();
	let mut header = tar::Header::new_gnu();
	header.set_entry_type(tar::EntryType::Directory);
	header.set_size(0);
	header.set_mode(0o755);
	header.set_mtime(123);
	header.set_cksum();
	tar.append_data(&mut header, "root/empty", &b""[..])
		.unwrap();
	tar.finish().unwrap();
}

fn candidate(cache: &Path, expression: &str, offline: bool) -> String {
	let store = EvaluatorStore::new();
	store.configure_fetching(FetchConfig {
		cache_dir: Some(cache.to_owned()),
		offline,
		..Default::default()
	});
	let result = configure(
		Evaluation::builder_pure().experimental_features(["flakes", "fetch-tree"]),
		store,
	)
	.mode(EvalMode::Strict)
	.build()
	.evaluate(expression, None);
	assert!(result.errors.is_empty(), "{:?}", result.errors);
	result
		.value
		.unwrap()
		.to_contextful_str()
		.unwrap()
		.to_str()
		.unwrap()
		.to_owned()
}

fn reference(cache: &Path, expression: &str) -> String {
	let executable = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate");
	let home = tempfile::tempdir().unwrap();
	let root = tempfile::tempdir().unwrap();
	let output = Command::new(executable)
		.env_clear()
		.env("PATH", std::env::var_os("PATH").unwrap())
		.env("HOME", home.path())
		.env("NIX_CACHE_HOME", cache)
		.env("NIX_CONF_DIR", home.path())
		.env(
			"NIX_CONFIG",
			"experimental-features = nix-command flakes\nflake-registry =\nsubstituters =\n",
		)
		.args([
			"--store",
			&format!("local?root={}", root.path().display()),
			"--eval",
			"--strict",
			"--json",
			"--expr",
			expression,
		])
		.output()
		.unwrap();
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	serde_json::from_slice::<String>(&output.stdout).unwrap()
}

#[test]
fn tarball_cache_is_shared_bidirectionally_with_cppnix() {
	let fixture = tempfile::tempdir().unwrap();
	let archive_path = fixture.path().join("source.tar");
	let expression = format!(
		"builtins.readFile ((builtins.fetchTarball {}) + \"/file\")",
		serde_json::json!(format!("file://{}", archive_path.display()))
	);
	for candidate_first in [true, false] {
		let cache = tempfile::tempdir().unwrap();
		archive(&archive_path, b"first");
		let first = if candidate_first {
			candidate(cache.path(), &expression, false)
		} else {
			reference(cache.path(), &expression)
		};
		assert_eq!(first, "first");
		archive(&archive_path, b"changed");
		let second = if candidate_first {
			reference(cache.path(), &expression)
		} else {
			candidate(cache.path(), &expression, true)
		};
		assert_eq!(second, "first");
		let db = noxi_flakeref::cache::Cache::open(cache.path()).unwrap();
		let key = [(
			"url".into(),
			format!("file://{}", archive_path.display()).into(),
		)]
		.into();
		assert!(
			db.lookup("tarball", &key)
				.unwrap()
				.unwrap()
				.contains_key("treeHash")
		);
	}
}

struct FileServer {
	url: String,
	stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
	thread: Option<std::thread::JoinHandle<()>>,
}
impl FileServer {
	fn start(root: PathBuf) -> Self {
		use std::io::{Read, Write};
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		listener.set_nonblocking(true).unwrap();
		let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
		let stopped = stop.clone();
		let thread = std::thread::spawn(move || {
			while !stopped.load(std::sync::atomic::Ordering::Relaxed) {
				let Ok((mut stream, _)) = listener.accept() else {
					std::thread::sleep(std::time::Duration::from_millis(5));
					continue;
				};
				stream
					.set_read_timeout(Some(std::time::Duration::from_secs(3)))
					.unwrap();
				let mut buf = [0; 8192];
				let count = stream.read(&mut buf).unwrap_or(0);
				let request = String::from_utf8_lossy(&buf[..count]);
				let path = request
					.split_whitespace()
					.nth(1)
					.unwrap_or("/")
					.split('?')
					.next()
					.unwrap();
				let body = fs::read(root.join(path.trim_start_matches('/')));
				let (status, body) = match body {
					Ok(body) => ("200 OK", body),
					Err(_) => ("404 Not Found", Vec::new()),
				};
				let _ = write!(
					stream,
					"HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
					body.len()
				);
				let _ = stream.write_all(&body);
			}
		});
		Self {
			url,
			stop,
			thread: Some(thread),
		}
	}
}
impl Drop for FileServer {
	fn drop(&mut self) {
		self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
		self.thread.take().unwrap().join().unwrap();
	}
}

#[test]
fn gitv3_cache_is_shared_bidirectionally_with_cppnix() {
	let fixture = tempfile::tempdir().unwrap();
	let repo = fixture.path().join("repo");
	fs::create_dir(&repo).unwrap();
	let git = |args: &[&str]| {
		let result = Command::new("git")
			.arg("-C")
			.arg(&repo)
			.args(args)
			.output()
			.unwrap();
		assert!(
			result.status.success(),
			"{}",
			String::from_utf8_lossy(&result.stderr)
		);
	};
	git(&["init", "--quiet", "-b", "main"]);
	fs::write(repo.join("file"), "cached git").unwrap();
	git(&["add", "."]);
	git(&[
		"-c",
		"user.name=Fetcher test",
		"-c",
		"user.email=fetcher@example.invalid",
		"commit",
		"--quiet",
		"-m",
		"initial",
	]);
	let remote = fixture.path().join("remote.git");
	assert!(
		Command::new("git")
			.args(["clone", "--quiet", "--bare"])
			.arg(&repo)
			.arg(&remote)
			.status()
			.unwrap()
			.success()
	);
	assert!(
		Command::new("git")
			.arg("-C")
			.arg(&remote)
			.arg("update-server-info")
			.status()
			.unwrap()
			.success()
	);
	for candidate_first in [true, false] {
		let cache = tempfile::tempdir().unwrap();
		let server = FileServer::start(fixture.path().to_owned());
		let expression = format!(
			"builtins.toJSON (let tree = builtins.fetchGit {{ url = {}; ref = \"main\"; }}; in {{ rev = tree.rev; narHash = tree.narHash; count = tree.revCount; }})",
			serde_json::json!(format!("{}/remote.git", server.url))
		);
		let first = if candidate_first {
			candidate(cache.path(), &expression, false)
		} else {
			reference(cache.path(), &expression)
		};
		drop(server);
		let second = if candidate_first {
			reference(cache.path(), &expression)
		} else {
			candidate(cache.path(), &expression, true)
		};
		assert_eq!(first, second);
	}
}

#[derive(Debug)]
struct EtagClient {
	calls: std::cell::Cell<usize>,
}
impl noxi_glue::fetch::http::Client for EtagClient {
	fn request(
		&self,
		request: &noxi_glue::fetch::http::Request,
	) -> noxi_flakeref::error::Result<noxi_glue::fetch::http::Response> {
		let count = self.calls.get();
		self.calls.set(count + 1);
		if count > 0 {
			assert!(
				request
					.headers
					.iter()
					.any(|(k, v)| k == "If-None-Match" && v == "version-1")
			);
		}
		Ok(noxi_glue::fetch::http::Response {
			status: if count == 0 { 200 } else { 304 },
			headers: [("etag".into(), "version-1".into())].into(),
			effective_url: request.url.clone(),
			body: b"cached".to_vec(),
		})
	}
}

#[test]
fn file_cache_revalidates_etags_and_reopens_offline() {
	let cache = tempfile::tempdir().unwrap();
	let client = Rc::new(EtagClient {
		calls: std::cell::Cell::new(0),
	});
	let expression = "builtins.readFile (builtins.fetchurl \"https://example.invalid/file\")";
	for offline in [false, false, true] {
		let store = EvaluatorStore::new();
		let mut config = FetchConfig {
			cache_dir: Some(cache.path().to_owned()),
			http: Some(client.clone()),
			offline,
			..Default::default()
		};
		config.settings.tarball_ttl = 0;
		store.configure_fetching(config);
		let result = configure(Evaluation::builder_pure(), store)
			.build()
			.evaluate(expression, None);
		assert!(result.errors.is_empty(), "{:?}", result.errors);
		assert_eq!(
			result
				.value
				.unwrap()
				.to_contextful_str()
				.unwrap()
				.to_str()
				.unwrap(),
			"cached"
		);
	}
	assert_eq!(client.calls.get(), 2);
}

#[test]
fn remote_relative_submodules_match_reference_and_reopen_offline() {
	let fixture = tempfile::tempdir().unwrap();
	let child = fixture.path().join("child");
	let parent = fixture.path().join("parent");
	let git = |path: &Path, args: &[&str]| -> String {
		let output = Command::new("git")
			.arg("-C")
			.arg(path)
			.args(args)
			.env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
			.env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
			.output()
			.unwrap();
		assert!(
			output.status.success(),
			"{}",
			String::from_utf8_lossy(&output.stderr)
		);
		String::from_utf8(output.stdout).unwrap().trim().into()
	};
	for repo in [&child, &parent] {
		fs::create_dir(repo).unwrap();
		git(repo, &["init", "--quiet", "-b", "main"]);
		git(repo, &["config", "user.name", "Fetcher test"]);
		git(repo, &["config", "user.email", "fetcher@example.invalid"]);
		fs::write(repo.join("file"), "committed source\n").unwrap();
		git(repo, &["add", "."]);
		git(repo, &["commit", "--quiet", "-m", "initial"]);
	}
	git(
		&parent,
		&[
			"-c",
			"protocol.file.allow=always",
			"submodule",
			"add",
			"../child",
			"nested",
		],
	);
	git(
		&parent,
		&[
			"config",
			"--file",
			".gitmodules",
			"submodule.nested.url",
			"../child.git",
		],
	);
	git(&parent, &["add", "."]);
	git(&parent, &["commit", "--quiet", "-m", "relative submodule"]);
	let rev = git(&parent, &["rev-parse", "HEAD"]);
	for repo in [&child, &parent] {
		let bare = fixture.path().join(format!(
			"{}.git",
			repo.file_name().unwrap().to_str().unwrap()
		));
		let output = Command::new("git")
			.args(["clone", "--quiet", "--bare"])
			.arg(repo)
			.arg(&bare)
			.output()
			.unwrap();
		assert!(output.status.success());
		git(&bare, &["update-server-info"]);
	}
	let server = GitServer::start(fixture.path());
	let expression = format!(
		"builtins.toJSON (let tree = builtins.fetchTree {{ type = \"git\"; url = {}; rev = \"{rev}\"; submodules = true; }}; in {{ attributes = builtins.map (key: {{ inherit key; value = tree.${{key}}; }}) (builtins.attrNames tree); content = builtins.readFile (tree.outPath + \"/nested/file\"); }})",
		serde_json::json!(format!("{}/parent.git", server.url))
	);
	let reference_cache = tempfile::tempdir().unwrap();
	let candidate_cache = tempfile::tempdir().unwrap();
	let expected = reference(reference_cache.path(), &expression);
	assert_eq!(
		candidate(candidate_cache.path(), &expression, false),
		expected
	);
	drop(server);
	assert_eq!(
		candidate(candidate_cache.path(), &expression, true),
		expected
	);
}

struct GitServer {
	url: String,
	child: std::process::Child,
}
impl GitServer {
	fn start(root: &Path) -> Self {
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let port = listener.local_addr().unwrap().port();
		drop(listener);
		let mut child = Command::new("git")
			.args([
				"daemon",
				"--reuseaddr",
				"--export-all",
				"--listen=127.0.0.1",
				&format!("--port={port}"),
				&format!("--base-path={}", root.display()),
			])
			.arg(root)
			.stdout(std::process::Stdio::null())
			.stderr(std::process::Stdio::piped())
			.spawn()
			.unwrap();
		let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
		loop {
			if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
				break;
			}
			assert!(
				child.try_wait().unwrap().is_none(),
				"Git fixture exited before listening"
			);
			assert!(
				std::time::Instant::now() < deadline,
				"Git fixture did not listen"
			);
			std::thread::sleep(std::time::Duration::from_millis(10));
		}
		Self {
			url: format!("git://127.0.0.1:{port}"),
			child,
		}
	}
}
impl Drop for GitServer {
	fn drop(&mut self) {
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}
