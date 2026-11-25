//! `builtin:fetchurl` in the sandbox compared with CppNix 2.35.2: what it can
//! read and send, and which process it runs in. Every build runs unprivileged
//! in a scratch store with a local HTTP or HTTPS server.
//!
//! The reference comes from `NOXI_REFERENCE_BIN`, else
//! `target/nix-reference/bin`; without it only noxi is checked.
use std::ffi::CString;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fetchurl-tls/ca.pem");
const OTHER_CA: &str = concat!(
	env!("CARGO_MANIFEST_DIR"),
	"/tests/fetchurl-tls/other-ca.pem"
);

fn engines() -> Vec<PathBuf> {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		});
	let mut engines = vec![
		Path::new(env!("CARGO_BIN_EXE_nix"))
			.parent()
			.unwrap()
			.to_owned(),
	];
	if reference.join("nix").exists() {
		engines.push(reference);
	} else {
		eprintln!("no reference: checking noxi alone");
	}
	engines
}

/// A server on `127.0.0.1` that records each request's path and
/// `Authorization` header.
struct Server {
	port: u16,
	log: Arc<Mutex<Vec<String>>>,
}

fn serve(stream: &mut (impl Read + Write), log: &Mutex<Vec<String>>) -> std::io::Result<()> {
	let mut path = String::new();
	let mut authorization = String::from("-");
	let mut host = String::new();
	{
		let mut reader = BufReader::new(&mut *stream);
		let mut line = String::new();
		while reader.read_line(&mut line)? > 2 {
			if path.is_empty() {
				path = line.split(' ').nth(1).unwrap_or("").to_owned();
			}
			if let Some((name, value)) = line.split_once(':') {
				match name.to_ascii_lowercase().as_str() {
					"authorization" => authorization = value.trim().to_owned(),
					"host" => host = value.trim().to_owned(),
					_ => {}
				}
			}
			line.clear();
		}
	}
	let mut log = log.lock().unwrap();
	let seen = log.iter().filter(|l| l.starts_with(&path)).count();
	log.push(format!(
		"{path} {} {authorization}",
		host.split(':').next().unwrap_or("")
	));
	drop(log);
	let port = host.rsplit(':').next().unwrap_or("").to_owned();
	let (status, headers, body) = match path.as_str() {
		"/auth" if authorization == "Basic dXNlcjpwYXNz" => ("200 OK", String::new(), "one\n"),
		"/auth" => ("401 Unauthorized", String::new(), "who?"),
		"/missing" => ("404 Not Found", String::new(), "nothing here\n"),
		"/broken" => ("500 Oops", String::new(), "later"),
		"/flaky" if seen % 2 == 0 => ("500 Oops", String::new(), "later"),
		"/to-relative" => ("302 Found", "Location: data\r\n".to_owned(), ""),
		"/to-localhost" => (
			"302 Found",
			format!("Location: http://localhost:{port}/data\r\n"),
			"",
		),
		"/to-file" => (
			"302 Found",
			"Location: file:///etc/passwd\r\n".to_owned(),
			"",
		),
		"/loop" => ("302 Found", "Location: /loop\r\n".to_owned(), ""),
		"/hang" => {
			stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\no")?;
			stream.flush()?;
			std::thread::sleep(Duration::from_secs(20));
			return Ok(());
		}
		_ => ("200 OK", String::new(), "one\n"),
	};
	write!(
		stream,
		"HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
		body.len()
	)?;
	stream.flush()
}

impl Server {
	fn http() -> Self {
		Self::start(None)
	}

	fn https() -> Self {
		use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
		let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fetchurl-tls");
		let config = rustls::ServerConfig::builder_with_provider(Arc::new(
			rustls::crypto::ring::default_provider(),
		))
		.with_safe_default_protocol_versions()
		.unwrap()
		.with_no_client_auth()
		.with_single_cert(
			vec![CertificateDer::from_pem_file(dir.join("server.pem")).unwrap()],
			PrivateKeyDer::from_pem_file(dir.join("server.key")).unwrap(),
		)
		.unwrap();
		Self::start(Some(Arc::new(config)))
	}

	fn start(tls: Option<Arc<rustls::ServerConfig>>) -> Self {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let port = listener.local_addr().unwrap().port();
		let log = Arc::new(Mutex::new(Vec::new()));
		let shared = log.clone();
		// The thread ends with the test process.
		std::thread::spawn(move || {
			for stream in listener.incoming() {
				let (Ok(mut stream), log, tls) = (stream, shared.clone(), tls.clone()) else {
					return;
				};
				std::thread::spawn(move || {
					let _: std::io::Result<()> = match tls {
						None => serve(&mut stream, &log),
						Some(config) => {
							let connection = rustls::ServerConnection::new(config).unwrap();
							let mut stream: rustls::StreamOwned<_, TcpStream> =
								rustls::StreamOwned::new(connection, stream);
							let result = serve(&mut stream, &log);
							stream.conn.send_close_notify();
							let _ = stream.flush();
							result
						}
					};
				});
			}
		});
		Self { port, log }
	}

	fn requests(&self) -> Vec<String> {
		std::mem::take(&mut *self.log.lock().unwrap())
	}
}

const IMPURE: &str = "__impure = true;";
const HASH: &str = r#"outputHash = builtins.hashString "sha256" "one\n";"#;

/// One build of a `builtin:fetchurl` derivation.
#[derive(Default)]
struct Case<'a> {
	/// Attributes besides the defaults of a flat output with contents `one`.
	attrs: &'a str,
	config: &'a str,
	env: &'a [(&'a str, &'a str)],
}

/// What a build did: its exit status, the output's contents and mode, and the
/// errors.
#[derive(Debug, PartialEq)]
struct Outcome {
	status: Option<i32>,
	output: Option<String>,
	errors: String,
}

fn build(bin: &Path, dir: &Path, url: &str, case: &Case<'_>) -> Outcome {
	use std::os::unix::fs::PermissionsExt;
	let root = dir.join("root");
	noxi_test_support::remove_tree(&root).unwrap();
	let expr = format!(
		r#"derivation ({{
			name = "fetched";
			system = "builtin";
			builder = "builtin:fetchurl";
			url = "{url}";
			outputHashMode = "flat";
			outputHashAlgo = "sha256";
			{}
		}} // {{ {} }})"#,
		if case.attrs.starts_with(IMPURE) {
			""
		} else {
			HASH
		},
		case.attrs
	);
	let impure = case.attrs.starts_with(IMPURE);
	// The reference's `nix-build` crashes on impure outputs.
	let mut command = Command::new(bin.join(if impure { "nix" } else { "nix-build" }));
	command.args(if impure {
		[
			"build",
			"--impure",
			"--no-link",
			"--print-out-paths",
			"--expr",
		]
		.as_slice()
	} else {
		&["--no-out-link", "-E"]
	});
	let output = command
		.arg(&expr)
		.env_clear()
		.env("HOME", dir.join("home"))
		.env("NIX_CONF_DIR", dir.join("etc"))
		.envs(case.env.iter().copied())
		.env(
			"NIX_CONFIG",
			format!(
				"experimental-features = nix-command ca-derivations impure-derivations\nsandbox = true\nsandbox-paths =\nbuild-users-group =\nsubstituters =\nfiletransfer-retry-attempts = 1\nfiletransfer-retry-delay = 1\nfiletransfer-retry-jitter = false\n{}\n",
				case.config
			),
		)
		.args(["--store", &format!("local?root={}", root.display())])
		.output()
		.unwrap();
	let path = String::from_utf8(output.stdout).unwrap().trim().to_owned();
	let contents = (!path.is_empty()).then(|| {
		let real = root.join(path.trim_start_matches('/'));
		let mode = fs::symlink_metadata(&real).unwrap().permissions().mode();
		let text = if real.is_dir() {
			fs::read_dir(&real)
				.unwrap()
				.map(|e| e.unwrap().file_name().into_string().unwrap())
				.collect::<String>()
		} else {
			String::from_utf8_lossy(&fs::read(&real).unwrap()).into_owned()
		};
		format!("{mode:o} {text}")
	});
	let errors = String::from_utf8_lossy(&output.stderr)
		.lines()
		.filter(|line| !line.starts_with("warning: you don't have Internet access"))
		.map(|line| format!("{line}\n"))
		.collect();
	Outcome {
		status: output.status.code(),
		output: contents,
		errors,
	}
}

struct Scene {
	dir: noxi_test_support::TempDir,
	engines: Vec<PathBuf>,
}

impl Scene {
	fn new() -> Self {
		let dir = noxi_test_support::tempdir().unwrap();
		fs::create_dir_all(dir.path().join("etc")).unwrap();
		Self {
			dir,
			engines: engines(),
		}
	}

	fn path(&self, name: &str) -> PathBuf {
		self.dir.path().join(name)
	}

	/// Build with every engine. Their exit statuses and outputs must be equal;
	/// messages are compared where the caller does. Returns noxi's outcome,
	/// and each engine's.
	fn build(&self, url: &str, case: &Case<'_>) -> Vec<Outcome> {
		let outcomes: Vec<Outcome> = self
			.engines
			.iter()
			.map(|bin| build(bin, self.dir.path(), url, case))
			.collect();
		for outcome in &outcomes[1..] {
			assert_eq!(
				(&outcomes[0].status, &outcomes[0].output),
				(&outcome.status, &outcome.output),
				"{url}: noxi, then the reference: {outcomes:#?}"
			);
		}
		outcomes
	}

	fn succeeds(&self, url: &str, case: &Case<'_>) -> String {
		let mut outcomes = self.build(url, case);
		assert_eq!(outcomes[0].status, Some(0), "{url}: {outcomes:#?}");
		outcomes.remove(0).output.unwrap()
	}

	/// The build fails, with `message` from every engine.
	fn fails(&self, url: &str, case: &Case<'_>, status: i32, message: &str) {
		for outcome in self.build(url, case) {
			assert_eq!(outcome.status, Some(status), "{url}: {outcome:#?}");
			assert!(outcome.errors.contains(message), "{url}: {outcome:#?}");
		}
	}
}

#[test]
fn reads_only_the_sandboxs_files() {
	let scene = Scene::new();
	let secret = scene.path("secret");
	fs::write(&secret, "one\n").unwrap();
	fs::create_dir(scene.path("shown")).unwrap();
	fs::write(scene.path("shown/a b%"), "one\n").unwrap();
	let shown = format!("sandbox-paths = /shown={}", scene.path("shown").display());
	let case = Case {
		config: &shown,
		..Default::default()
	};
	for url in [
		"file:///shown/a%20b%25",
		"file://localhost/shown/a%20b%25",
		"file://127.0.0.1/shown/a%20b%25",
		"file:///shown/a%20b%25?query",
		"file:///shown/a%20b%25#fragment",
		"file:///shown/../shown/a%20b%25",
	] {
		assert_eq!(scene.succeeds(url, &case), "100444 one\n");
	}
	let hidden = [
		format!("file://{}", secret.display()),
		"file:///shown/../../../../shown/absent".to_owned(),
		format!("file:///proc/1/root{}", secret.display()),
		format!("file:///proc/self/cwd/../..{}", secret.display()),
		"file:///nix/var/nix/db/db.sqlite".to_owned(),
		"file:///root/.netrc".to_owned(),
	];
	for url in &hidden {
		scene.fails(url, &case, 1, "Could not read a file:// file (37)");
	}
	// The reference's process has descriptors of its downloader open.
	for fd in 3..64 {
		let url = format!("file:///proc/self/fd/{fd}");
		let outcome = build(&scene.engines[0], scene.dir.path(), &url, &case);
		assert!(outcome.errors.contains("Couldn't open file"), "{outcome:?}");
	}
	for url in [
		"file://example.org/shown/a",
		"file://shown/a",
		"nosuch://localhost/",
	] {
		scene.fails(url, &case, 1, "unable to download '");
	}
	scene.fails("/shown/a%20b%25", &case, 1, "doesn't have a scheme");
	// The sandbox's own `/etc/passwd`, not the host's.
	let impure = Case {
		attrs: IMPURE,
		..Default::default()
	};
	let passwd = scene.succeeds("file:///etc/passwd", &impure);
	assert!(passwd.contains("root:x:0:0:Nix build user:/build:/noshell"));
}

/// The process has the build user's identity, no capabilities and nothing of
/// the environment of the process that builds.
#[test]
fn does_not_inherit_privileges_or_the_environment() {
	let scene = Scene::new();
	let impure = Case {
		attrs: IMPURE,
		env: &[("NOXI_FETCHURL_SECRET", "leaked")],
		..Default::default()
	};
	let noxi = &scene.engines[0];
	let status = build(noxi, scene.dir.path(), "file:///proc/self/status", &impure);
	let status = status.output.unwrap_or_else(|| panic!("{}", status.errors));
	let field = |name: &str| {
		status
			.lines()
			.find_map(|line| line.strip_prefix(name))
			.unwrap()
			.split_whitespace()
			.next()
			.unwrap()
			.to_owned()
	};
	for set in ["CapInh:", "CapPrm:", "CapEff:", "CapAmb:"] {
		assert_eq!(field(set), "0000000000000000", "{status}");
	}
	assert_eq!(field("Uid:"), "1000");
	assert_eq!(field("Gid:"), "100");
	assert_eq!(field("NoNewPrivs:"), "1");
	assert_eq!(field("Seccomp:"), "2");
	assert_eq!(field("Pid:"), "1");
	let environ = build(noxi, scene.dir.path(), "file:///proc/self/environ", &impure);
	assert!(
		!format!("{environ:?}").contains("leaked"),
		"the environment is readable: {environ:?}"
	);
}

/// The reference leaves the `netrc` contents in the build directory, where a
/// `file:` URL can read them. noxi deliberately does not, so this checks noxi
/// alone.
#[test]
fn netrc_contents_stay_unreadable() {
	let scene = Scene::new();
	fs::write(
		scene.path("etc/netrc"),
		"machine localhost login user password hunter2\n",
	)
	.unwrap();
	let case = Case {
		attrs: IMPURE,
		..Default::default()
	};
	for url in ["file:///build/netrc", "file:///proc/self/cwd/netrc"] {
		let outcome = build(&scene.engines[0], scene.dir.path(), url, &case);
		assert_eq!(outcome.status, Some(1), "{outcome:?}");
		assert!(!format!("{outcome:?}").contains("hunter2"), "{outcome:?}");
	}
}

#[test]
fn timeouts_stop_downloads() {
	let scene = Scene::new();
	let server = Server::http();
	fs::create_dir(scene.path("shown")).unwrap();
	let fifo = CString::new(scene.path("shown/fifo").to_str().unwrap()).unwrap();
	// SAFETY: a NUL-terminated path.
	assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
	let shown = format!("sandbox-paths = /shown={}", scene.path("shown").display());
	let hang = format!("http://127.0.0.1:{}/hang", server.port);
	let fifo = "file:///shown/fifo".to_owned();
	for (setting, url, status, message) in [
		("timeout = 2", &hang, 101, "timed out after 2 seconds"),
		("timeout = 2", &fifo, 101, "timed out after 2 seconds"),
		(
			"max-silent-time = 2",
			&fifo,
			101,
			"timed out after 2 seconds",
		),
		// A download reports its progress while it waits, so it is not silent;
		// a stalled one has its own timeout.
		(
			"max-silent-time = 2\nstalled-download-timeout = 4",
			&hang,
			1,
			"unable to download '",
		),
	] {
		let config = format!("{shown}\n{setting}");
		let case = Case {
			config: &config,
			..Default::default()
		};
		let start = Instant::now();
		scene.fails(url, &case, status, message);
		assert!(start.elapsed() < Duration::from_secs(18), "{setting} {url}");
	}
}

#[test]
fn http_downloads_match_the_reference() {
	let scene = Scene::new();
	let server = Server::http();
	let url = |path: &str| format!("http://127.0.0.1:{}{path}", server.port);
	let default = Case::default();
	for path in ["/data", "/to-relative", "/to-localhost"] {
		assert_eq!(scene.succeeds(&url(path), &default), "100444 one\n");
	}
	let executable = Case {
		attrs: r#"executable = "1"; outputHashMode = "recursive"; outputHash = "sha256-rEKnFaLZTB0lKB2m8Y6Nz30va8sTd8tED5Mqe9nV98I=";"#,
		..Default::default()
	};
	assert_eq!(scene.succeeds(&url("/data"), &executable), "100555 one\n");
	server.requests();
	for (path, message) in [
		("/missing", "HTTP error 404"),
		("/missing", "nothing here"),
		("/auth", "HTTP error 401"),
		("/to-file", "unable to download '"),
		("/loop", "Number of redirects hit maximum amount"),
		("/broken", "HTTP error 500"),
	] {
		server.requests();
		scene.fails(&url(path), &default, 1, message);
	}
	server.requests();
	let retries = Case {
		config: "filetransfer-retry-attempts = 2",
		..Default::default()
	};
	let outcomes = scene.build(&url("/flaky"), &retries);
	for outcome in &outcomes {
		assert_eq!(outcome.status, Some(0), "{outcome:#?}");
		assert!(
			outcome.errors.contains("; retrying in 1 ms (attempt 1/2)"),
			"{outcome:#?}"
		);
	}
	// Nothing listens there.
	let closed = TcpListener::bind("127.0.0.1:0")
		.unwrap()
		.local_addr()
		.unwrap()
		.port();
	scene.fails(
		&format!("http://127.0.0.1:{closed}/x"),
		&default,
		1,
		"Could not connect to server (7)",
	);
	// The result must still have the expected hash.
	scene.fails(
		&url("/data"),
		&Case {
			attrs: r#"outputHash = builtins.hashString "sha256" "two\n";"#,
			..Default::default()
		},
		102,
		"hash mismatch in fixed-output derivation",
	);
}

#[test]
fn netrc_credentials_match_the_reference() {
	let scene = Scene::new();
	let server = Server::http();
	let port = server.port;
	let netrc = scene.path("netrc");
	let setting = format!("netrc-file = {}", netrc.display());
	let case = Case {
		config: &setting,
		..Default::default()
	};
	let check = |contents: &str, url: &str, expected: &[&str]| {
		fs::write(&netrc, contents).unwrap();
		for bin in &scene.engines {
			server.requests();
			build(bin, scene.dir.path(), url, &case);
			assert_eq!(server.requests(), expected, "{}: {url}", bin.display());
		}
	};
	let on = |host: &str, path: &str| format!("http://{host}:{port}{path}");
	const USER: &str = "Basic dXNlcjpwYXNz";
	let auth = format!("/auth localhost {USER}");
	check(
		"machine localhost login user password pass\n",
		&on("localhost", "/auth"),
		&[&auth],
	);
	check(
		"machine other login a password b\ndefault login user password pass\n",
		&on("localhost", "/auth"),
		&[&auth],
	);
	check(
		"machine localhost\n\tlogin user\n\tpassword \"pass\"\n",
		&on("localhost", "/auth"),
		&[&auth],
	);
	check(
		"machine LOCALHOST login user password pass\n",
		&on("localhost", "/auth"),
		&[&auth],
	);
	check(
		"machine 127.0.0.1 login user password pass\n",
		&on("localhost", "/data"),
		&["/data localhost -"],
	);
	check("", &on("localhost", "/data"), &["/data localhost -"]);
	// The URL's own credentials win, and a user without a password gets the
	// file's.
	check(
		"machine localhost login a password b\n",
		&format!("http://user:pass@localhost:{port}/auth"),
		&[&auth],
	);
	check(
		"machine localhost login other password no\nmachine localhost login user password pass\n",
		&format!("http://user@localhost:{port}/auth"),
		&[&auth],
	);
	// Credentials do not follow a redirect to another host, which has its own.
	check(
		"machine 127.0.0.1 login user password pass\n",
		&on("127.0.0.1", "/to-localhost"),
		&[
			&format!("/to-localhost 127.0.0.1 {USER}"),
			"/data localhost -",
		],
	);
	check(
		"machine localhost login user password pass\n",
		&on("127.0.0.1", "/to-localhost"),
		&[
			"/to-localhost 127.0.0.1 -",
			&format!("/data localhost {USER}"),
		],
	);
	check(
		"machine 127.0.0.1 login user password pass\n",
		&on("127.0.0.1", "/to-relative"),
		&[
			&format!("/to-relative 127.0.0.1 {USER}"),
			&format!("/data 127.0.0.1 {USER}"),
		],
	);
	// The default file is the configuration directory's.
	fs::remove_file(&netrc).unwrap();
	fs::write(
		scene.path("etc/netrc"),
		"machine localhost login user password pass\n",
	)
	.unwrap();
	let outcome = scene.succeeds(&on("localhost", "/auth"), &Case::default());
	assert_eq!(outcome, "100444 one\n");
}

#[test]
fn https_roots_match_the_reference() {
	let scene = Scene::new();
	let server = Server::https();
	let url = format!("https://localhost:{}/data", server.port);
	let with = |file: &str| format!("ssl-cert-file = {file}");
	let trusted = with(CA);
	let case = Case {
		config: &trusted,
		..Default::default()
	};
	assert_eq!(scene.succeeds(&url, &case), "100444 one\n");
	// The file also comes from the environment.
	for variable in ["NIX_SSL_CERT_FILE", "SSL_CERT_FILE"] {
		let case = Case {
			env: &[(variable, CA)],
			..Default::default()
		};
		assert_eq!(scene.succeeds(&url, &case), "100444 one\n");
	}
	assert!(!server.requests().is_empty());
	let untrusted = [
		with(OTHER_CA),
		with(scene.path("absent").to_str().unwrap()),
		with(""),
	];
	for config in &untrusted {
		scene.fails(
			&url,
			&Case {
				config,
				..Default::default()
			},
			1,
			"unable to download '",
		);
	}
	// A certificate for another name.
	scene.fails(
		&format!("https://127.0.0.2:{}/data", server.port),
		&case,
		1,
		"unable to download '",
	);
	assert_eq!(server.requests(), Vec::<String>::new());
	// The certificates are in the build directory, as in the reference.
	let impure = Case {
		attrs: IMPURE,
		config: &trusted,
		..Default::default()
	};
	let copy = scene.succeeds("file:///build/ca-certificates.crt", &impure);
	assert_eq!(copy, format!("100444 {}", fs::read_to_string(CA).unwrap()));
}

#[test]
fn mirrors_and_unpacking_match_the_reference() {
	let scene = Scene::new();
	let server = Server::http();
	let shown = scene.path("shown");
	let digest = "2c8b08da5ce60398e1f19af0e5dccc744df274b826abe585eaba68c525434806";
	fs::create_dir_all(shown.join("sha256")).unwrap();
	fs::write(shown.join("sha256").join(digest), "one\n").unwrap();
	// A mirror is read through the sandbox as any URL is.
	for (mirrors, sources) in [
		("file:///shown", "/shown"),
		("file:///shown/", "/shown"),
		("file:///absent file:///shown", "/shown"),
	] {
		let config = format!(
			"hashed-mirrors = {mirrors}\nsandbox-paths = {sources}={}",
			shown.display()
		);
		let case = Case {
			config: &config,
			..Default::default()
		};
		assert_eq!(scene.succeeds("file:///nowhere", &case), "100444 one\n");
	}
	let outside = format!("hashed-mirrors = file://{}", shown.display());
	let case = Case {
		config: &outside,
		..Default::default()
	};
	scene.fails("file:///nowhere", &case, 1, "file:///nowhere");
	let mirror = format!("hashed-mirrors = http://127.0.0.1:{}", server.port);
	let case = Case {
		config: &mirror,
		..Default::default()
	};
	assert_eq!(scene.succeeds("file:///nowhere", &case), "100444 one\n");
	for request in server.requests() {
		assert_eq!(request, format!("/sha256/{digest} 127.0.0.1 -"));
	}

	fs::create_dir(scene.path("tree")).unwrap();
	fs::write(scene.path("tree/file"), "x").unwrap();
	let nix = |args: &[&str]| {
		let output = Command::new(scene.engines[0].join("nix-store"))
			.args(args)
			.output()
			.unwrap();
		assert!(output.status.success());
		output.stdout
	};
	let nar = nix(&["--dump", scene.path("tree").to_str().unwrap()]);
	fs::write(shown.join("tree.nar"), &nar).unwrap();
	let mut compressed =
		lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(1)).unwrap();
	compressed.write_all(&nar).unwrap();
	fs::write(shown.join("tree.nar.xz"), compressed.finish().unwrap()).unwrap();
	let config = format!("sandbox-paths = /shown={}", shown.display());
	let case = Case {
		attrs: r#"__impure = true; unpack = true; outputHashMode = "recursive";"#,
		config: &config,
		..Default::default()
	};
	for name in ["tree.nar", "tree.nar.xz"] {
		let tree = scene.succeeds(&format!("file:///shown/{name}"), &case);
		assert_eq!(tree, "40555 file");
	}
	scene.fails(
		&format!("file:///shown/sha256/{digest}"),
		&case,
		1,
		"builder failed with exit code 1",
	);
}

/// The other builtin builders run in the sandbox too.
#[test]
fn other_builtins_read_only_the_sandboxs_files() {
	let scene = Scene::new();
	fs::create_dir_all(scene.path("tree/secret-name")).unwrap();
	fs::write(scene.path("tree/secret-name/file"), "x").unwrap();
	let status = Command::new("tar")
		.arg("-cf")
		.arg(scene.path("channel.tar"))
		.arg("-C")
		.arg(scene.path("tree"))
		.arg("secret-name")
		.status()
		.unwrap();
	assert!(status.success());
	let fixed = r#"outputHashMode = "recursive";"#;
	let shown = format!("sandbox-paths = /shown={}", scene.dir.path().display());
	for (attrs, config, status) in [
		(
			format!(
				r#"{fixed} builder = "builtin:unpack-channel"; channelName = "c"; src = "{}";"#,
				scene.path("channel.tar").display()
			),
			"",
			1,
		),
		(
			format!(
				r#"{fixed} builder = "builtin:buildenv"; manifest = "/m"; derivations = "true 5 1 {}";"#,
				scene.path("tree").display()
			),
			"",
			1,
		),
		// With the files visible the builders get to the hash check.
		(
			format!(
				r#"{fixed} builder = "builtin:unpack-channel"; channelName = "c"; src = "/shown/channel.tar";"#
			),
			&shown[..],
			102,
		),
		(
			format!(
				r#"{fixed} builder = "builtin:buildenv"; manifest = "/m"; derivations = "true 5 1 /shown/tree";"#
			),
			&shown,
			102,
		),
	] {
		let case = Case {
			attrs: &attrs,
			config,
			..Default::default()
		};
		for outcome in scene.build("unused:", &case) {
			assert_eq!(outcome.status, Some(status), "{attrs}: {outcome:#?}");
		}
	}
}
