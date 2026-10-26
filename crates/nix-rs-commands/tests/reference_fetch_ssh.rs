//! SSH process boundaries use a local adapter; LFS HTTP uses loopback only.
use serde_json::json;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{
	Arc,
	atomic::{AtomicBool, AtomicUsize, Ordering},
};

fn git(path: &Path, args: &[&str]) -> String {
	let output = Command::new("git")
		.arg("-C")
		.arg(path)
		.args(args)
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
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
}
struct Fixture {
	dir: tempfile::TempDir,
	rev: String,
	url: String,
}
impl Fixture {
	fn new(lfs: bool) -> Self {
		let dir = tempfile::tempdir().unwrap();
		let repo = dir.path().join("repo");
		fs::create_dir(&repo).unwrap();
		git(&repo, &["init", "--quiet", "-b", "main"]);
		git(&repo, &["config", "user.name", "SSH fetcher test"]);
		git(&repo, &["config", "user.email", "fetcher@example.invalid"]);
		fs::write(
			repo.join("flake.nix"),
			"{ outputs = { self }: { answer = 42; }; }",
		)
		.unwrap();
		if lfs {
			fs::write(repo.join(".gitattributes"), "object filter=lfs\n").unwrap();
			fs::write(
				repo.join("object"),
				format!(
					"version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
					oid(),
					CONTENT.len()
				),
			)
			.unwrap();
		}
		git(&repo, &["add", "."]);
		git(&repo, &["commit", "--quiet", "-m", "fixture"]);
		let rev = git(&repo, &["rev-parse", "HEAD"]);
		let bare = dir.path().join("remote.git");
		let clone = Command::new("git")
			.args(["clone", "--quiet", "--bare"])
			.arg(&repo)
			.arg(&bare)
			.output()
			.unwrap();
		assert!(clone.status.success());
		let bin = dir.path().join("bin");
		fs::create_dir(&bin).unwrap();
		let adapter = bin.join("ssh");
		fs::write(
			&adapter,
			r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "$NIXRS_SSH_TRACE"
for argument in "$@"; do
	if [ "$argument" = git-lfs-authenticate ]; then
		cat "$NIXRS_LFS_AUTH"
		exit 0
	fi
done
for argument in "$@"; do
	case "$argument" in
		git-upload-pack\ *) exec bash -c "$argument" ;;
	esac
done
exit 0
"#,
		)
		.unwrap();
		fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
		Self {
			url: format!("ssh://fixture.invalid{}", bare.display()),
			dir,
			rev,
		}
	}
	fn run(&self, reference: bool, expression: &str, auth: Option<&str>) -> Output {
		let root = tempfile::tempdir().unwrap();
		let home = tempfile::tempdir().unwrap();
		let cache = tempfile::tempdir().unwrap();
		let executable = if reference {
			std::env::var_os("NIXRS_REFERENCE_BIN")
				.map(PathBuf::from)
				.unwrap_or_else(|| {
					Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
				})
				.join("nix-instantiate")
		} else {
			PathBuf::from(env!("CARGO_BIN_EXE_nix-instantiate"))
		};
		let path = format!(
			"{}:{}",
			self.dir.path().join("bin").display(),
			std::env::var("PATH").unwrap()
		);
		let auth_file = self.dir.path().join("auth.json");
		fs::write(&auth_file, auth.unwrap_or("{}")).unwrap();
		Command::new(executable).env_clear().env("PATH",path).env("HOME",home.path()).env("NIX_CACHE_HOME",cache.path()).env("NIX_CONF_DIR",home.path()).env("NIX_USER_CONF_FILES","").env("NIX_PATH","")
			.env("GIT_SSH_COMMAND",self.dir.path().join("bin/ssh")).env("GIT_SSH_VARIANT","ssh").env("NIXRS_SSH_TRACE",self.dir.path().join("ssh.trace")).env("NIXRS_LFS_AUTH",auth_file)
			.env("NIX_CONFIG","experimental-features = flakes nix-command\nsubstituters =\nflake-registry =\nwarn-dirty = false\n")
			.args(["--store",&format!("local?root={}",root.path().display()),"--eval","--strict","--json","--expr",expression]).output().unwrap()
	}
}
#[test]
fn ssh_fetch_attributes_and_flake_outputs_match_reference() {
	let fixture = Fixture::new(false);
	for suffix in [
		"ref = \"main\";".to_owned(),
		format!("rev = \"{}\"; shallow = true;", fixture.rev),
	] {
		let expression = format!(
			"let tree = builtins.fetchTree {{ type = \"git\"; url = {}; {suffix} }}; in builtins.map (key: {{ inherit key; value = tree.${{key}}; }}) (builtins.attrNames tree)",
			json!(fixture.url)
		);
		let expected = fixture.run(true, &expression, None);
		let actual = fixture.run(false, &expression, None);
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
		assert_eq!(
			serde_json::from_slice::<serde_json::Value>(&actual.stdout).unwrap(),
			serde_json::from_slice::<serde_json::Value>(&expected.stdout).unwrap()
		);
	}
	let expression = format!(
		"let f = builtins.getFlake {}; in {{ inherit (f) answer outPath narHash lastModifiedDate; }}",
		json!(format!("git+{}?rev={}", fixture.url, fixture.rev))
	);
	let expected = fixture.run(true, &expression, None);
	let actual = fixture.run(false, &expression, None);
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
	assert_eq!(actual.stdout, expected.stdout);
	assert!(
		fs::read_to_string(fixture.dir.path().join("ssh.trace"))
			.unwrap()
			.contains("git-upload-pack")
	);
}

const CONTENT: &[u8] = b"LFS through SSH authentication\n";
fn oid() -> String {
	format!("{:x}", nix_compat::nixhash::Sha256::digest_bytes(CONTENT))
}
#[derive(Clone, Copy)]
enum Mode {
	Valid,
	Identity,
	Size,
	Hash,
	MissingObjects,
}
struct Server {
	url: String,
	stop: Arc<AtomicBool>,
	calls: Arc<AtomicUsize>,
	thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
	fn start(mode: Mode) -> Self {
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let url = format!("http://{}", listener.local_addr().unwrap());
		listener.set_nonblocking(true).unwrap();
		let stop = Arc::new(AtomicBool::new(false));
		let stopped = stop.clone();
		let calls = Arc::new(AtomicUsize::new(0));
		let count = calls.clone();
		let base = url.clone();
		let thread = std::thread::spawn(move || {
			while !stopped.load(Ordering::Relaxed) {
				let Ok((mut stream, _)) = listener.accept() else {
					std::thread::sleep(std::time::Duration::from_millis(5));
					continue;
				};
				stream
					.set_read_timeout(Some(std::time::Duration::from_secs(3)))
					.unwrap();
				let mut request = Vec::new();
				let mut buf = [0; 4096];
				let (header_end, length) = loop {
					let n = stream.read(&mut buf).unwrap();
					assert!(n > 0);
					request.extend_from_slice(&buf[..n]);
					if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
						let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
						let length = headers
							.lines()
							.find_map(|line| line.strip_prefix("content-length:"))
							.map(|value| value.trim().parse::<usize>().unwrap())
							.unwrap_or(0);
						break (end + 4, length);
					}
				};
				while request.len() < header_end + length {
					let n = stream.read(&mut buf).unwrap();
					assert!(n > 0);
					request.extend_from_slice(&buf[..n]);
				}
				let headers = String::from_utf8_lossy(&request[..header_end]).to_lowercase();
				count.fetch_add(1, Ordering::Relaxed);
				let body = if headers.starts_with("post ") {
					assert!(headers.contains("authorization: fixture-auth\r\n"));
					assert_eq!(
						serde_json::from_slice::<serde_json::Value>(
							&request[header_end..header_end + length]
						)
						.unwrap(),
						json!({"operation":"download","objects":[{"oid":oid(),"size":CONTENT.len()}]})
					);
					if matches!(mode, Mode::MissingObjects) {
						b"{}".to_vec()
					} else {
						serde_json::to_vec(&json!({"objects":[{"oid":if matches!(mode,Mode::Identity) {"0".repeat(64)} else {oid()},"size":CONTENT.len(),"actions":{"download":{"href":format!("{base}/object"),"header":{"Authorization":"fixture-object"}}}}]})).unwrap()
					}
				} else {
					assert!(headers.starts_with("get /object "));
					assert!(headers.contains("authorization: fixture-object\r\n"));
					if matches!(mode, Mode::Size) {
						b"wrong size".to_vec()
					} else if matches!(mode, Mode::Hash) {
						vec![b'x'; CONTENT.len()]
					} else {
						CONTENT.to_vec()
					}
				};
				write!(
					stream,
					"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
					body.len()
				)
				.unwrap();
				stream.write_all(&body).unwrap();
			}
		});
		Self {
			url,
			stop,
			calls,
			thread: Some(thread),
		}
	}
}
impl Drop for Server {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
		self.thread.take().unwrap().join().unwrap();
	}
}
#[test]
fn ssh_lfs_authentication_and_integrity_messages_match_reference() {
	let fixture = Fixture::new(true);
	for mode in [
		Mode::Valid,
		Mode::Identity,
		Mode::Size,
		Mode::Hash,
		Mode::MissingObjects,
	] {
		let server = Server::start(mode);
		let auth =
			json!({"href":format!("{}/lfs",server.url),"header":{"Authorization":"fixture-auth"}})
				.to_string();
		let expression = format!(
			"let tree = builtins.fetchTree {{ type = \"git\"; url = {}; rev = \"{}\"; lfs = true; }}; in {{ attributes = builtins.map (key: {{ inherit key; value = tree.${{key}}; }}) (builtins.attrNames tree); content = builtins.readFile (tree.outPath + \"/object\"); }}",
			json!(fixture.url),
			fixture.rev
		);
		let expected = fixture.run(true, &expression, Some(&auth));
		let actual = fixture.run(false, &expression, Some(&auth));
		if matches!(mode, Mode::Valid) {
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
			assert_eq!(actual.stdout, expected.stdout);
			assert_eq!(server.calls.load(Ordering::Relaxed), 4);
		} else {
			assert_eq!(actual.status.code(), expected.status.code());
			assert!(!expected.status.success());
			let expected = String::from_utf8(expected.stderr).unwrap();
			let actual = String::from_utf8(actual.stderr).unwrap();
			let actual_message = actual
				.lines()
				.find_map(|line| {
					line.strip_prefix("error[")
						.and_then(|line| line.split_once("]: ").map(|(_, message)| message))
				})
				.or_else(|| {
					actual
						.rsplit_once("error: ")
						.map(|(_, message)| message.trim())
				})
				.expect(&actual);
			assert_eq!(
				actual_message,
				expected.rsplit_once("error: ").unwrap().1.trim()
			);
		}
	}
	assert!(
		fs::read_to_string(fixture.dir.path().join("ssh.trace"))
			.unwrap()
			.contains("git-lfs-authenticate")
	);
}
