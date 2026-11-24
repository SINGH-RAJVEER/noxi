//! Fetched lookup path entries (`flake:`, pseudo-URLs) against pinned CppNix
//! 2.35.2, with a local registry, local sources and a loopback server.

use std::{
	fs,
	io::{Read, Write},
	net::TcpListener,
	path::{Path, PathBuf},
	process::Command,
	sync::{Arc, Mutex},
	time::Duration,
};

const FLAKES: &str = "experimental-features = nix-command flakes";

/// Serve `archive` for every path, recording requests. `/missing` is a 404,
/// `/redirect` redirects, and a proxy `CONNECT` is refused.
fn serve(archive: Vec<u8>) -> (String, Arc<Mutex<Vec<String>>>) {
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	let url = format!("http://{}", listener.local_addr().unwrap());
	let requests = Arc::new(Mutex::new(Vec::new()));
	let seen = requests.clone();
	// The process's exit ends the accept loop.
	std::thread::spawn(move || {
		for stream in listener.incoming() {
			let mut stream = stream.unwrap();
			stream
				.set_read_timeout(Some(Duration::from_secs(5)))
				.unwrap();
			let mut request = Vec::new();
			let mut byte = [0];
			while !request.ends_with(b"\r\n\r\n")
				&& request.len() < 65536
				&& matches!(stream.read(&mut byte), Ok(1))
			{
				request.push(byte[0]);
			}
			let request = String::from_utf8_lossy(&request);
			let mut words = request.split_whitespace();
			let (Some(method), Some(path)) = (words.next(), words.next()) else {
				continue;
			};
			seen.lock().unwrap().push(format!("{method} {path}"));
			let (status, extra, body): (_, _, &[u8]) = match (method, path) {
				("CONNECT", _) => ("403 Forbidden", "", b""),
				(_, "/missing") => ("404 Not Found", "", b""),
				(_, "/redirect") => ("302 Found", "Location: /target.tar.gz\r\n", b""),
				(_, "/bad.tar.gz") => ("200 OK", "", b"not an archive"),
				_ => ("200 OK", "", &archive),
			};
			let head = format!(
				"HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
				body.len()
			);
			let _ = stream.write_all(head.as_bytes());
			let _ = stream.write_all(body);
		}
	});
	(url, requests)
}

struct Case<'a> {
	command: &'a str,
	args: &'a [&'a str],
	/// `NIX_PATH`, whose colons split URLs with a port.
	nix_path: Option<&'a str>,
	/// Extra `nix.conf` lines.
	config: &'a str,
	env: &'a [(&'a str, &'a str)],
}

impl Default for Case<'_> {
	fn default() -> Self {
		Case {
			command: "nix-instantiate",
			args: &[],
			nix_path: Some(""),
			config: FLAKES,
			env: &[],
		}
	}
}

struct Fixture {
	dir: noxi_test_support::TempDir,
	url: String,
	requests: Arc<Mutex<Vec<String>>>,
	bins: [PathBuf; 2],
}

impl Fixture {
	fn new() -> Self {
		let reference = std::env::var_os("NOXI_REFERENCE_BIN")
			.map(PathBuf::from)
			.unwrap_or_else(|| {
				Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
			});
		assert!(
			reference.join("nix").exists(),
			"the pinned reference is needed; run `just reference` or set NOXI_REFERENCE_BIN"
		);
		let noxi = Path::new(env!("CARGO_BIN_EXE_nix"))
			.parent()
			.unwrap()
			.to_owned();
		let dir = noxi_test_support::tempdir().unwrap();
		let root = dir.path();
		for tree in ["tree", "flake", "git"] {
			fs::create_dir_all(root.join(tree).join("sub")).unwrap();
			fs::write(root.join(tree).join("default.nix"), "{ }: 42").unwrap();
			fs::write(root.join(tree).join("sub/x.nix"), "7").unwrap();
		}
		fs::write(root.join("flake/flake.nix"), "{ outputs = _: { }; }").unwrap();
		for args in [
			&["init", "-q"][..],
			&["add", "."],
			&[
				"-c",
				"user.name=test",
				"-c",
				"user.email=test@localhost",
				"commit",
				"-qm",
				"x",
			],
		] {
			let status = Command::new("git")
				.env("GIT_CONFIG_GLOBAL", "/dev/null")
				.args(args)
				.current_dir(root.join("git"))
				.status()
				.unwrap();
			assert!(status.success());
		}
		let status = Command::new("tar")
			.args(["-czf", "a.tar.gz", "tree"])
			.current_dir(root)
			.status()
			.unwrap();
		assert!(status.success());
		// A pseudo-URL is unpacked whatever its name is.
		fs::copy(root.join("a.tar.gz"), root.join("archive")).unwrap();
		let entry = |id: &str, to: serde_json::Value| serde_json::json!({ "from": { "type": "indirect", "id": id }, "to": to });
		let path = |name: &str| root.join(name).display().to_string();
		let registry = serde_json::json!({ "version": 2, "flakes": [
			entry("nixpkgs", serde_json::json!({ "type": "path", "path": path("tree") })),
			entry("git", serde_json::json!({ "type": "git", "url": format!("file://{}", path("git")) })),
			entry("sub", serde_json::json!({ "type": "path", "path": path("tree"), "dir": "sub" })),
			entry("chain", serde_json::json!({ "type": "indirect", "id": "nixpkgs" })),
			entry("gone", serde_json::json!({ "type": "path", "path": path("gone") })),
		] });
		fs::write(root.join("registry.json"), registry.to_string()).unwrap();
		let (url, requests) = serve(fs::read(root.join("a.tar.gz")).unwrap());
		Fixture {
			dir,
			url,
			requests,
			bins: [reference, noxi],
		}
	}

	fn path(&self, name: &str) -> String {
		self.dir.path().join(name).display().to_string()
	}

	/// Exit status, stdout, stderr and the requests made.
	fn run(
		&self,
		engine: usize,
		state: &Path,
		case: &Case,
	) -> (Option<i32>, String, String, Vec<String>) {
		self.requests.lock().unwrap().clear();
		let mut command = Command::new(self.bins[engine].join(case.command));
		command
			.env_clear()
			.env("PATH", std::env::var_os("PATH").unwrap())
			.env("HOME", state)
			.env("XDG_CACHE_HOME", state.join("cache"))
			.env("NIX_CONF_DIR", state.join("conf"))
			.env("NIX_USER_CONF_FILES", "")
			.env("NIX_STATE_DIR", state.join("state"))
			.env(
				"NIX_CONFIG",
				format!(
					"download-attempts = 1\nstore = local?root={}/root\nsubstituters =\nflake-registry = {}\n{}\n",
					state.display(),
					self.path("registry.json"),
					case.config
				),
			)
			.envs(case.env.iter().copied())
			.current_dir(self.dir.path())
			.args(case.args);
		if let Some(nix_path) = case.nix_path {
			command.env("NIX_PATH", nix_path);
		}
		let output = command.output().unwrap();
		let text = |bytes| {
			String::from_utf8(bytes)
				.unwrap()
				.replace(state.to_str().unwrap(), "STATE")
		};
		(
			output.status.code(),
			text(output.stdout),
			text(output.stderr),
			self.requests.lock().unwrap().clone(),
		)
	}

	/// Run both engines with fresh state, and return their common result.
	fn same(&self, case: &Case) -> (Option<i32>, String, String, Vec<String>) {
		let [reference, noxi] = [0, 1].map(|engine| {
			let state = noxi_test_support::tempdir().unwrap();
			self.run(engine, state.path(), case)
		});
		assert_eq!(
			reference, noxi,
			"{} {:?} NIX_PATH={:?} {:?}",
			case.command, case.args, case.nix_path, case.config
		);
		noxi
	}
}

#[test]
fn flake_entries_match_the_reference() {
	let fixture = Fixture::new();
	for reference in [
		"nixpkgs".to_owned(),
		"git".into(),
		"sub".into(),
		"chain".into(),
		"gone".into(),
		"nope".into(),
		"nixpkgs?dir=sub".into(),
		"nixpkgs#x".into(),
		"./flake".into(),
		".".into(),
		"".into(),
		"%%".into(),
		fixture.path("flake"),
		format!("path:{}", fixture.path("flake")),
		format!("git+file://{}", fixture.path("git")),
		format!("{}/x.tar.gz", fixture.url),
	] {
		let entry = format!("nixpkgs=flake:{reference}");
		for args in [
			&["--find-file", "nixpkgs", "nixpkgs/sub/x.nix"][..],
			&["--eval", "--strict", "-E", "toString <nixpkgs>"],
			&["--eval", "<nixpkgs>"],
			&["--eval", "-E", "builtins.nixPath", "--strict"],
			&[
				"--eval",
				"-E",
				"(builtins.tryEval <nixpkgs/missing>).success",
			],
			&[
				"--eval",
				"--restrict-eval",
				"--strict",
				"-E",
				"builtins.readDir <nixpkgs>",
			],
		] {
			let args = [&["-I", &entry], args].concat();
			fixture.same(&Case {
				args: &args,
				..Case::default()
			});
		}
	}
	// As flake-built NixOS sets it, and without a prefix.
	for (nix_path, name) in [
		("nixpkgs=flake:nixpkgs", "nixpkgs"),
		("/nonexistent:flake:nixpkgs", "sub/x.nix"),
	] {
		for config in [FLAKES, ""] {
			let (status, ..) = fixture.same(&Case {
				args: &["--find-file", name],
				nix_path: Some(nix_path),
				config,
				..Case::default()
			});
			assert_eq!(status, Some(i32::from(config.is_empty())));
			fixture.same(&Case {
				args: &["--eval", "-E", &format!("<{name}>")],
				nix_path: Some(nix_path),
				config,
				..Case::default()
			});
		}
	}
}

#[test]
fn pseudo_url_entries_match_the_reference() {
	let fixture = Fixture::new();
	let url = &fixture.url;
	for entry in [
		format!("{url}/x.tar.gz"),
		format!("{url}/no-extension"),
		format!("{url}/redirect"),
		format!("{url}/missing"),
		format!("{url}/bad.tar.gz"),
		format!("{url}/x.tar.gz?narHash=sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="),
		format!("file://{}", fixture.path("a.tar.gz")),
		format!("file://{}", fixture.path("archive")),
		format!("file://{}", fixture.path("missing.tar.gz")),
		format!("file://{}", fixture.path("tree")),
		"file://relative".into(),
		"git://127.0.0.1:1/x".into(),
		"ssh://localhost/x".into(),
	] {
		let nixpkgs = format!("nixpkgs={entry}");
		for args in [
			&["--find-file", "nixpkgs", "nixpkgs/sub/x.nix"][..],
			&["--eval", "--strict", "-E", "import <nixpkgs> { }"],
			&["--eval", "-E", "<nixpkgs>"],
			&["--eval", "<nixpkgs>"],
			&[
				"--eval",
				"--restrict-eval",
				"-E",
				"builtins.length builtins.nixPath",
			],
			&["--eval", "--restrict-eval", "-E", "<nixpkgs>"],
			&["--restrict-eval", "--find-file", "nixpkgs"],
			&["--eval", "--pure-eval", "-E", "<nixpkgs>"],
		] {
			let args = [&["-I", &nixpkgs], args].concat();
			fixture.same(&Case {
				args: &args,
				..Case::default()
			});
		}
		let find = format!(
			"builtins.findFile [ {{ prefix = \"p\"; path = \"{entry}\"; }} ] \"p/sub/x.nix\""
		);
		let caught = format!("(builtins.tryEval ({find})).success");
		for expression in [&find, &caught] {
			for flags in [&[][..], &["--pure-eval"], &["--restrict-eval"]] {
				let args = [&["--eval", "-E", expression], flags].concat();
				fixture.same(&Case {
					args: &args,
					..Case::default()
				});
			}
		}
	}
	// `NIX_PATH` ends an entry at the colon before a port.
	let nix_path = format!("{url}/x.tar.gz");
	fixture.same(&Case {
		args: &["--eval", "--strict", "-E", "builtins.nixPath"],
		nix_path: Some(&nix_path),
		..Case::default()
	});
}

#[test]
fn channel_entries_request_the_reference_url() {
	let fixture = Fixture::new();
	// A refusing proxy keeps the test offline and shows the host.
	let proxy = [("https_proxy", fixture.url.as_str())];
	for (args, nix_path) in [
		(
			&["--find-file", "nixpkgs"][..],
			"nixpkgs=channel:nixos-26.05",
		),
		(
			&["--eval", "-E", "<nixpkgs>"],
			"nixpkgs=channel:nixos-26.05",
		),
		(
			&["--eval", "-E", "<nixpkgs>", "--restrict-eval"],
			"channel:a:channel:b",
		),
	] {
		let (status, _, stderr, requests) = fixture.same(&Case {
			args,
			nix_path: Some(nix_path),
			env: &proxy,
			..Case::default()
		});
		assert_eq!(status, Some(1));
		assert!(
			stderr.contains("unpacking 'https://channels.nixos.org/"),
			"{stderr}"
		);
		assert_eq!(requests[0], "CONNECT channels.nixos.org:443");
	}
}

#[test]
fn entry_precedence_matches_the_reference() {
	let fixture = Fixture::new();
	let url = format!("nixpkgs={}/x.tar.gz", fixture.url);
	let include = format!("nixpkgs={}", fixture.path("flake"));
	let option = format!("nixpkgs=flake:git o={}/o.tar.gz", fixture.url);
	for flags in [
		&[][..],
		&["-I", &include],
		&["-I", &url],
		&["-I", "flake:nixpkgs", "--extra-nix-path", "x=flake:git"],
		&["--nix-path", &option],
		&["--option", "extra-nix-path", "x=channel:unused"],
	] {
		for nix_path in [None, Some(""), Some("nixpkgs=flake:nixpkgs:a=channel:b:/c")] {
			for config in [
				"",
				"nix-path = nixpkgs=flake:git",
				"extra-nix-path = nixpkgs=flake:git",
			] {
				let config = format!("{FLAKES}\n{config}");
				for args in [
					&["--eval", "--strict", "-E", "builtins.nixPath"][..],
					&["--find-file", "nixpkgs"],
				] {
					let args = [flags, args].concat();
					fixture.same(&Case {
						args: &args,
						nix_path,
						config: &config,
						..Case::default()
					});
				}
			}
		}
	}
}

#[test]
fn modern_commands_match_the_reference() {
	let fixture = Fixture::new();
	for entry in [
		"p=flake:nixpkgs".to_owned(),
		"p=flake:nope".into(),
		format!("p={}/x.tar.gz", fixture.url),
		format!("p={}/missing", fixture.url),
	] {
		for args in [
			&["eval", "--impure", "--expr", "import <p/sub/x.nix>"][..],
			&["eval", "--impure", "--expr", "<p>"],
			&["eval", "--impure", "-f", "<p>", "--apply", "f: f { }"],
			&["eval", "--expr", "<p>"],
		] {
			let args = [args, &["-I", &entry]].concat();
			fixture.same(&Case {
				command: "nix",
				args: &args,
				..Case::default()
			});
		}
	}
}

#[test]
fn tarball_ttl_governs_downloads() {
	let fixture = Fixture::new();
	let entry = format!("nixpkgs={}/x.tar.gz", fixture.url);
	let states = [
		noxi_test_support::tempdir().unwrap(),
		noxi_test_support::tempdir().unwrap(),
	];
	let run = |ttl| {
		let [reference, noxi] = [0, 1].map(|engine| {
			fixture.run(
				engine,
				states[engine].path(),
				&Case {
					args: &[
						"-I",
						&entry,
						"--option",
						"tarball-ttl",
						ttl,
						"--eval",
						"-E",
						"import <nixpkgs/sub/x.nix> + import <nixpkgs/sub/x.nix>",
					],
					..Case::default()
				},
			)
		});
		assert_eq!(reference, noxi, "tarball-ttl = {ttl}");
		noxi
	};
	let fetched = run("3600");
	assert_eq!(fetched.1, "14\n");
	assert_eq!(fetched.3, ["GET /x.tar.gz"]);
	assert_eq!(run("3600").3, [""; 0]);
	// CppNix timestamps cache entries in whole seconds.
	std::thread::sleep(Duration::from_millis(1100));
	assert_eq!(run("0").3, ["GET /x.tar.gz"]);
}
