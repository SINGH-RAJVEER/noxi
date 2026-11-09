//! Native channel subscriptions and lifecycle in private homes and stores.
use std::{
	fs,
	path::{Path, PathBuf},
	process::Command,
};
#[test]
fn subscription_files_and_xdg_precedence() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-channel");
	for xdg in [false, true] {
		let homes = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
		for args in [
			vec!["--add", "https://example.org/foo/"],
			vec!["--add", "file:///tmp/source", "local"],
			vec!["--add", "https://example.org/replacement", "foo"],
			vec!["--list"],
			vec!["--remove", "foo"],
			vec!["--remove", "missing"],
			vec!["--list"],
		] {
			let results: Vec<_> = [
				reference.as_path(),
				Path::new(env!("CARGO_BIN_EXE_nix-channel")),
			]
			.iter()
			.zip(&homes)
			.map(|(bin, home)| {
				Command::new(bin)
					.env_clear()
					.env("HOME", home.path())
					.env("NIX_CONF_DIR", home.path())
					.env("NIX_USER_CONF_FILES", "")
					.env("NIX_CONFIG", format!("use-xdg-base-directories = {xdg}\n"))
					.args(&args)
					.output()
					.unwrap()
			})
			.collect();
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
			assert_eq!(results[0].stdout, results[1].stdout, "{args:?}");
			let file = if xdg {
				".local/state/nix/channels"
			} else {
				".nix-channels"
			};
			assert_eq!(
				fs::read(homes[0].path().join(file)).unwrap(),
				fs::read(homes[1].path().join(file)).unwrap(),
				"{args:?}"
			);
		}
	}
}

#[test]
fn channel_updates_preserve_reference_paths_contents_and_generations() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		});
	let fixture = tempfile::tempdir().unwrap();
	let source = fixture.path().join("nixexprs");
	fs::create_dir(&source).unwrap();
	let archive = fixture.path().join("channel.tar.gz");
	let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
	let binaries = [
		reference.join("nix-channel"),
		PathBuf::from(env!("CARGO_BIN_EXE_nix-channel")),
	];
	let run = |engine: usize, args: &[&str]| {
		let root = roots[engine].path();
		Command::new(&binaries[engine])
			.env_clear()
			.env("PATH", std::env::var_os("PATH").unwrap())
			.env("HOME", root)
			.env("NIX_CONF_DIR", root.join("conf"))
			.env("NIX_USER_CONF_FILES", "")
			.env("NIX_STATE_DIR", root.join("state"))
			.env(
				"NIX_CONFIG",
				format!(
					"store = local?root={}/store\nsubstituters =\nbuild-users-group =\nsandbox = false\ntarball-ttl = 0\n",
					root.display()
				),
			)
			.args(args)
			.output()
			.unwrap()
	};
	let same = |args: &[&str]| {
		let outputs = [run(0, args), run(1, args)];
		for output in &outputs {
			assert!(
				output.status.success(),
				"{args:?}: {}",
				String::from_utf8_lossy(&output.stderr)
			);
		}
		assert_eq!(outputs[0].stdout, outputs[1].stdout, "{args:?}");
		assert_eq!(outputs[0].stderr, outputs[1].stderr, "{args:?}");
	};
	let url = format!("file://{}", archive.display());
	same(&["--add", &url, "demo"]);
	for answer in [42, 43] {
		fs::write(
			source.join("default.nix"),
			format!("{{ answer = {answer}; }}"),
		)
		.unwrap();
		assert!(
			Command::new("tar")
				.args([
					"-czf",
					archive.to_str().unwrap(),
					"-C",
					fixture.path().to_str().unwrap(),
					"nixexprs"
				])
				.status()
				.unwrap()
				.success()
		);
		same(&["--update"]);
		let profiles: Vec<_> = roots
			.iter()
			.map(|root| root.path().join(".local/state/nix/profiles/channels"))
			.collect();
		assert_eq!(
			fs::read_link(&profiles[0]).unwrap(),
			fs::read_link(&profiles[1]).unwrap()
		);
		let generations: Vec<_> = profiles
			.iter()
			.map(|p| p.parent().unwrap().join(fs::read_link(p).unwrap()))
			.collect();
		let targets: Vec<_> = generations
			.iter()
			.map(|p| fs::read_link(p).unwrap())
			.collect();
		assert_eq!(targets[0], targets[1]);
		// Exact NAR bytes include the manifest and channel links, independent of timestamps.
		let dumps: Vec<_> = roots
			.iter()
			.zip(&targets)
			.map(|(root, target)| {
				let physical = root
					.path()
					.join("store")
					.join(target.strip_prefix("/").unwrap());
				let mut bytes = Vec::new();
				noxi_store::nar::dump_path(&mut bytes, &physical).unwrap();
				bytes
			})
			.collect();
		assert_eq!(dumps[0], dumps[1]);
	}
	same(&["--rollback"]);
	same(&["--rollback", "2"]);
}

#[test]
fn loopback_download_cache_and_selective_update() {
	use std::{
		io::{Read, Write},
		net::TcpListener,
		sync::{
			Arc, Mutex,
			atomic::{AtomicBool, Ordering},
		},
		time::Duration,
	};
	let reference = PathBuf::from(std::env::var_os("NOXI_REFERENCE_BIN").unwrap());
	let fixture = tempfile::tempdir().unwrap();
	let source = fixture.path().join("nixexprs");
	fs::create_dir(&source).unwrap();
	let make_archive = |answer: u8| {
		fs::write(source.join("default.nix"), answer.to_string()).unwrap();
		let out = Command::new("tar")
			.args(["-cJf", "-", "-C"])
			.arg(fixture.path())
			.arg("nixexprs")
			.output()
			.unwrap();
		assert!(out.status.success());
		out.stdout
	};
	let payload = Arc::new(Mutex::new(make_archive(42)));
	let requests = Arc::new(Mutex::new(Vec::new()));
	let stop = Arc::new(AtomicBool::new(false));
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	listener.set_nonblocking(true).unwrap();
	let addr = listener.local_addr().unwrap();
	let (body, seen, done) = (payload.clone(), requests.clone(), stop.clone());
	let server = std::thread::spawn(move || {
		let deadline = std::time::Instant::now() + Duration::from_secs(40);
		while !done.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
			let (mut stream, _) = match listener.accept() {
				Ok(v) => v,
				Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
					std::thread::sleep(Duration::from_millis(5));
					continue;
				}
				Err(e) => panic!("{e}"),
			};
			stream
				.set_read_timeout(Some(Duration::from_secs(3)))
				.unwrap();
			let mut request = Vec::new();
			while !request.ends_with(b"\r\n\r\n") {
				let mut byte = [0];
				if stream.read(&mut byte).unwrap() == 0 {
					break;
				}
				request.push(byte[0]);
				assert!(request.len() < 65536, "bounded loopback request headers");
			}
			if request.is_empty() {
				continue;
			}
			let request = String::from_utf8_lossy(&request);
			let path = request.split_whitespace().nth(1).unwrap().to_owned();
			seen.lock().unwrap().push(path.clone());
			let (status, extra, bytes) = if path == "/redirect" {
				("302 Found", "Location: /channel\r\n", Vec::new())
			} else if path.ends_with("nixexprs.tar.xz") {
				("200 OK", "", body.lock().unwrap().clone())
			} else {
				("200 OK", "", b"channel index".to_vec())
			};
			let mut response = format!(
				"HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
				bytes.len()
			)
			.into_bytes();
			if !request.starts_with("HEAD ") {
				response.extend(bytes);
			}
			if let Err(error) = stream.write_all(&response) {
				assert!(
					matches!(
						error.kind(),
						std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
					),
					"{error}"
				);
			}
		}
	});
	let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
	let bins = [
		reference.join("nix-channel"),
		PathBuf::from(env!("CARGO_BIN_EXE_nix-channel")),
	];
	// Only the oracle needs logical /nix/store paths for selective profile traversal.
	let package = fs::canonicalize(&reference)
		.unwrap()
		.parent()
		.unwrap()
		.to_path_buf();
	let copied = Command::new(reference.join("nix"))
		.env_clear()
		.env("HOME", roots[0].path())
		.env("NIX_CONF_DIR", roots[0].path())
		.env("NIX_USER_CONF_FILES", "")
		.env("NIX_CONFIG", "experimental-features = nix-command")
		.args([
			"copy",
			"--no-check-sigs",
			"--to",
			&format!("local?root={}/store", roots[0].path().display()),
		])
		.arg(package)
		.output()
		.unwrap();
	assert!(
		copied.status.success(),
		"{}",
		String::from_utf8_lossy(&copied.stderr)
	);
	let run = |engine: usize, args: &[&str]| {
		let root = roots[engine].path();
		let mut command = if engine == 0 && args.contains(&"--update") {
			let mut c = Command::new("unshare");
			c.args([
				"-Ur",
				"-m",
				"--",
				"bash",
				"-c",
				"mount --bind \"$1\" /nix/store; shift; exec \"$@\"",
				"channel-test",
			])
			.arg(root.join("store/nix/store"))
			.arg(&bins[engine]);
			c
		} else {
			Command::new(&bins[engine])
		};
		command.env_clear().env("PATH", std::env::var_os("PATH").unwrap()).env("HOME", root).env("XDG_CACHE_HOME", root.join("cache")).env("NIX_CONF_DIR", root.join("conf")).env("NIX_USER_CONF_FILES", "").env("NIX_STATE_DIR", root.join("state")).env("NIX_CONFIG", format!("store = local?root={}/store\nsubstituters =\nbuild-users-group =\nsandbox = false\nrequire-drop-supplementary-groups = false\ntarball-ttl = 3600\n", root.display())).args(args).output().unwrap()
	};
	let compare = |args: &[&str]| {
		let results = [run(0, args), run(1, args)];
		for r in &results {
			assert!(
				r.status.success(),
				"{args:?}: {}",
				String::from_utf8_lossy(&r.stderr)
			);
		}
		assert_eq!(results[0].stdout, results[1].stdout);
		assert_eq!(
			String::from_utf8_lossy(&results[0].stderr),
			String::from_utf8_lossy(&results[1].stderr),
			"{args:?}"
		);
	};
	compare(&["--add", &format!("http://{addr}/redirect"), "demo"]);
	compare(&["--add", &format!("http://{addr}/other"), "other"]);
	compare(&["--update"]);
	let initial = requests.lock().unwrap().clone();
	compare(&["--update", "demo"]);
	assert_eq!(
		*requests.lock().unwrap(),
		initial,
		"fresh cached downloads must not hit loopback"
	);
	*payload.lock().unwrap() = make_archive(43);
	// CppNix timestamps cache entries in whole seconds, including TTL zero.
	std::thread::sleep(Duration::from_millis(1100));
	compare(&["--option", "tarball-ttl", "0", "--update", "demo"]);
	let mut targets = Vec::new();
	for (engine, root) in roots.iter().enumerate() {
		let profile = root.path().join(if engine == 0 {
			"state/profiles/per-user/root/channels"
		} else {
			".local/state/nix/profiles/channels"
		});
		let generation = profile
			.parent()
			.unwrap()
			.join(fs::read_link(&profile).unwrap());
		let output = fs::read_link(generation).unwrap();
		let physical = root
			.path()
			.join("store")
			.join(output.strip_prefix("/").unwrap());
		for (name, expected) in [("demo", "43"), ("other", "42")] {
			let target = fs::read_link(physical.join(name)).unwrap();
			let source = root
				.path()
				.join("store")
				.join(target.strip_prefix("/").unwrap());
			assert_eq!(
				fs::read_to_string(source.join("default.nix")).unwrap(),
				expected,
				"engine {engine}, channel {name}, requests {:?}",
				requests.lock().unwrap()
			);
		}
		let mut dump = Vec::new();
		noxi_store::nar::dump_path(&mut dump, &physical).unwrap();
		targets.push((output, dump));
	}
	assert_eq!(targets[0], targets[1]);
	stop.store(true, Ordering::Relaxed);
	server.join().unwrap();
}

#[test]
fn subscription_defaults_comments_and_errors_match() {
	use std::os::unix::process::CommandExt;
	let reference =
		PathBuf::from(std::env::var_os("NOXI_REFERENCE_BIN").unwrap()).join("nix-channel");
	let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
	for root in &roots {
		fs::write(
			root.path().join(".nix-channels"),
			"# comment\nhttps://example.org/old legacy\nhttps://example.org/implicit\n",
		)
		.unwrap();
	}
	for args in [
		vec!["--list"],
		vec!["--add", "https://example.org/nixos-unstable"],
		vec!["--list"],
		vec!["--add", "https://example.org/stable-stable"],
		vec!["--add", "https://example.org/name-unstable-unstable"],
		vec!["--add", "https://example.org"],
		vec!["--list"],
		vec![],
		vec!["--add"],
		vec!["--remove"],
		vec!["--list", "extra"],
		vec!["--rollback", "1", "2"],
		vec!["--add", "https://example.org/a", "-invalid"],
		vec!["--add", "ftp://example.org/a"],
	] {
		let results: Vec<_> = [
			reference.as_path(),
			Path::new(env!("CARGO_BIN_EXE_nix-channel")),
		]
		.iter()
		.zip(&roots)
		.map(|(bin, root)| {
			Command::new(bin)
				.arg0("nix-channel")
				.env_clear()
				.env("HOME", root.path())
				.env("NIX_CONF_DIR", root.path())
				.env("NIX_USER_CONF_FILES", "")
				.args(&args)
				.output()
				.unwrap()
		})
		.collect();
		assert_eq!(
			results[0].status.code(),
			results[1].status.code(),
			"{args:?}"
		);
		assert_eq!(
			String::from_utf8_lossy(&results[0].stdout),
			String::from_utf8_lossy(&results[1].stdout),
			"{args:?}"
		);
		assert_eq!(
			String::from_utf8_lossy(&results[0].stderr),
			String::from_utf8_lossy(&results[1].stderr),
			"{args:?}"
		);
		assert_eq!(
			fs::read(roots[0].path().join(".nix-channels")).unwrap(),
			fs::read(roots[1].path().join(".nix-channels")).unwrap()
		);
	}
}
