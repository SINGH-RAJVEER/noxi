//! noxi's commands as clients of the pinned CppNix 2.35.2 daemon.
//!
//! The reference makes a store holding the test busybox, which is copied
//! three times. A reference `nix-daemon` serves two copies and the noxi
//! daemon, started in this process, the third. The reference's commands
//! run against the first reference daemon, and noxi's against the
//! second and against the noxi daemon, through
//! `unix://SOCKET?root=ROOT`, as an untrusted user: queries, an
//! instantiation, builds that succeed and fail, a realisation without a
//! substituter, and a profile switch. Output, exit statuses and the daemon
//! messages relayed to the client, including the warnings about the
//! settings it forwards, must match.
//!
//! The reference executables come from `NOXI_REFERENCE_BIN` or
//! `target/nix-reference/bin`, and the busybox from `NOXI_TEST_BUSYBOX`
//! or `target/test-busybox` (`just reference` creates both).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const CONFIG: &str = "experimental-features = nix-command\nbuild-users-group =\nsubstituters =\nsandbox = true\nsandbox-fallback = false\nsandbox-paths =\ncores = 1\n";

fn from_env_or_target(var: &str, target: &str, probe: &str) -> PathBuf {
	if let Some(path) = std::env::var_os(var) {
		return PathBuf::from(path);
	}
	let path = Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../target")
		.join(target);
	assert!(
		path.join(probe).exists(),
		"{target} is missing; run `just reference` or set {var}"
	);
	path
}

fn reference_bin() -> PathBuf {
	from_env_or_target("NOXI_REFERENCE_BIN", "nix-reference/bin", "nix-store")
}

fn noxi_bin(program: &str) -> &'static str {
	match program {
		"nix-build" => env!("CARGO_BIN_EXE_nix-build"),
		"nix-env" => env!("CARGO_BIN_EXE_nix-env"),
		"nix-instantiate" => env!("CARGO_BIN_EXE_nix-instantiate"),
		"nix-store" => env!("CARGO_BIN_EXE_nix-store"),
		"nix-collect-garbage" => env!("CARGO_BIN_EXE_nix-collect-garbage"),
		"nix" => env!("CARGO_BIN_EXE_nix"),
		_ => unreachable!(),
	}
}

struct Env {
	dir: tempfile::TempDir,
	/// Where the stores, work directories and sockets live.
	base: PathBuf,
	busybox: String,
	daemons: Vec<Child>,
}

impl Drop for Env {
	fn drop(&mut self) {
		for daemon in &mut self.daemons {
			let _ = daemon.kill();
			let _ = daemon.wait();
		}
		// Store paths are read-only.
		let _ = Command::new("chmod")
			.args(["-R", "u+w"])
			.arg(self.dir.path())
			.status();
	}
}

impl Env {
	fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
		let mut command = Command::new(program);
		command
			.env_clear()
			.env("HOME", self.dir.path().join("home"))
			.env("NIX_CONF_DIR", self.dir.path().join("conf"))
			.env("NIX_CONFIG", CONFIG)
			.env("NIX_PATH", "")
			.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin");
		command
	}

	fn new() -> Self {
		Self::new_in("")
	}

	/// An environment whose stores, work directories and sockets are in
	/// `subdir` of its temporary directory.
	fn new_in(subdir: &str) -> Self {
		let dir = tempfile::tempdir().unwrap();
		let base = dir.path().join(subdir);
		for sub in ["conf", "home"] {
			fs::create_dir(dir.path().join(sub)).unwrap();
		}
		for sub in ["work-reference", "work-noxi", "work-pair"] {
			fs::create_dir_all(base.join(sub)).unwrap();
		}
		let busybox = fs::canonicalize(from_env_or_target(
			"NOXI_TEST_BUSYBOX",
			"test-busybox",
			"bin/busybox",
		))
		.unwrap()
		.to_string_lossy()
		.into_owned();
		let mut env = Env {
			dir,
			base,
			busybox,
			daemons: Vec::new(),
		};
		let src = env.base.join("src");
		let status = env
			.command(reference_bin().join("nix"))
			.args(["copy", "--no-check-sigs", "--to"])
			.arg(format!("local?root={}", src.display()))
			.arg(&env.busybox)
			.stderr(std::process::Stdio::null())
			.status()
			.unwrap();
		assert!(status.success());
		let expr = format!(
			r#"let bb = builtins.storePath {}; in {{
  ok = derivation {{ name = "ok"; system = "x86_64-linux"; builder = "${{bb}}/bin/sh"; args = [ "-c" "echo building ok; printf 'tab\there\n'; echo $builder > $out" ]; }};
  fail = derivation {{ name = "fail"; system = "x86_64-linux"; builder = "${{bb}}/bin/sh"; args = [ "-c" "echo about to fail; exit 3" ]; }};
  text = builtins.toFile "note" "hello";
	text2 = builtins.toFile "note2" "hello";
}}"#,
			env.busybox
		);
		for engine in ["reference", "noxi", "pair"] {
			fs::write(env.base.join(format!("work-{engine}/expr.nix")), &expr).unwrap();
			let status = Command::new("cp")
				.arg("-a")
				.arg(&src)
				.arg(env.root(engine))
				.status()
				.unwrap();
			assert!(status.success());
			if engine == "pair" {
				let listener = noxi_store::unix_socket::bind(&env.socket(engine)).unwrap();
				let mut settings = noxi_daemon::DaemonSettings::new(
					noxi_store::LocalStoreConfig::with_root(&env.root(engine)),
				);
				settings.build.sandbox_paths = Vec::new();
				settings.build.build_cores = 1;
				// The reference daemons run with `nix-command` enabled.
				settings.build.nix_command = true;
				std::thread::spawn(move || noxi_daemon::server::serve(listener, &settings));
				continue;
			}
			let daemon = env
				.command(reference_bin().join("nix-daemon"))
				.arg("--store")
				.arg(env.root(engine))
				.env("NIX_DAEMON_SOCKET_PATH", env.socket(engine))
				.stderr(std::process::Stdio::null())
				.spawn()
				.unwrap();
			env.daemons.push(daemon);
		}
		let deadline = Instant::now() + Duration::from_secs(20);
		while !(env.socket("reference").exists() && env.socket("noxi").exists()) {
			assert!(
				Instant::now() < deadline,
				"the reference daemons did not start"
			);
			std::thread::sleep(Duration::from_millis(20));
		}
		env
	}

	fn root(&self, engine: &str) -> PathBuf {
		self.base.join(format!("root-{engine}"))
	}

	fn socket(&self, engine: &str) -> PathBuf {
		self.base.join(format!("{engine}.sock"))
	}

	/// Run `program` from `engine` against that engine's daemon: the
	/// reference's for `reference` and `noxi`, noxi's for `pair`.
	fn run(&self, engine: &str, program: &str, args: &[&str]) -> String {
		let executable = match engine {
			"reference" => reference_bin().join(program),
			_ => PathBuf::from(noxi_bin(program)),
		};
		let work = self.base.join(format!("work-{engine}"));
		let store = format!(
			"unix://{}?root={}",
			self.socket(engine).display(),
			self.root(engine).display()
		);
		let args: Vec<String> = args
			.iter()
			.map(|arg| {
				arg.replace("{work}", &work.display().to_string()).replace(
					"{state}",
					&self.root(engine).join("nix/var/nix").display().to_string(),
				)
			})
			.collect();
		// Indirect root names hash absolute link paths, so separate copied
		// stores have different names. Preserve each link's identity while
		// normalizing its generated name, including links removed by GC.
		let auto_roots: Vec<_> = fs::read_dir(self.root(engine).join("nix/var/nix/gcroots/auto"))
			.into_iter()
			.flatten()
			.map(|entry| {
				let path = entry.unwrap().path();
				let target = fs::read_link(&path).unwrap();
				(
					path.display().to_string(),
					format!("AUTO({})", target.display()),
				)
			})
			.collect();
		let output = self
			.command(&executable)
			.current_dir(&work)
			.env("NIX_STATE_DIR", self.root(engine).join("nix/var/nix"))
			.arg("--store")
			.arg(&store)
			.args(&args)
			.output()
			.unwrap();
		let mut result = format!(
			"stdout:\n{}stderr:\n{}exit {:?}",
			String::from_utf8_lossy(&output.stdout),
			String::from_utf8_lossy(&output.stderr),
			output.status.code()
		);
		for (path, target) in auto_roots {
			result = result.replace(&path, &target);
		}
		result
			.replace(&self.socket(engine).display().to_string(), "SOCKET")
			.replace(&self.root(engine).display().to_string(), "ROOT")
			.replace(&work.display().to_string(), "WORK")
			.replace(&executable.display().to_string(), program)
	}
}

#[test]
fn commands_work_through_the_reference_daemon() {
	let env = Env::new();
	let busybox = env.busybox.as_str();
	let missing = "/nix/store/00000000000000000000000000000000-x";
	let cases: Vec<(&str, Vec<&str>)> = vec![
		("nix-store", vec!["-q", "--references", busybox]),
		("nix-store", vec!["-q", "--hash", "--size", busybox]),
		("nix-store", vec!["-q", "--size", busybox]),
		("nix-store", vec!["-q", "--references", missing]),
		("nix-instantiate", vec!["expr.nix", "-A", "ok"]),
		(
			"nix-instantiate",
			vec!["expr.nix", "-A", "text", "--eval", "--read-write-mode"],
		),
		("nix-build", vec!["expr.nix", "-A", "ok", "--no-out-link"]),
		("nix-build", vec!["expr.nix", "-A", "ok", "-o", "result-ok"]),
		("nix-build", vec!["expr.nix", "-A", "fail", "--no-out-link"]),
		(
			"nix-build",
			vec!["expr.nix", "-A", "fail", "--no-out-link", "-Q"],
		),
		(
			"nix-build",
			vec![
				"expr.nix",
				"-A",
				"fail",
				"--no-out-link",
				"--quiet",
				"--quiet",
			],
		),
		("nix-store", vec!["-qR", busybox]),
		("nix-store", vec!["--realise", missing]),
		("nix-env", vec!["-p", "{work}/profile", "--set", busybox]),
		(
			"nix-store",
			vec![
				"--realise",
				busybox,
				"--add-root",
				"{work}/gcroots/new/root",
			],
		),
		("nix-store", vec!["-q", "--referrers", busybox]),
	];
	for (program, args) in cases {
		let expected = env.run("reference", program, &args);
		let actual = env.run("noxi", program, &args);
		assert_eq!(actual, expected, "{program} {args:?}");
		let actual = env.run("pair", program, &args);
		assert_eq!(actual, expected, "{program} {args:?} with the noxi daemon");
	}
	for engine in ["reference", "noxi", "pair"] {
		let work = env.base.join(format!("work-{engine}"));
		assert!(fs::read_link(work.join("result-ok")).is_ok());
		assert!(fs::read_link(work.join("profile-1-link")).is_ok());
	}
}

/// CppNix clients forward only overrides: settings from user files,
/// `NIX_CONFIG` and flags, not the system nix.conf, which `loadConfFile`
/// resets. A NixOS system nix.conf full of restricted settings therefore
/// produces no daemon warnings for an untrusted client, as Home Manager's
/// standalone activation relies on.
#[test]
fn system_configuration_is_not_forwarded_to_the_daemon() {
	let env = Env::new();
	let system = "require-sigs = true\nauto-optimise-store = false\nsandbox = true\n\
		system-features = nixos-test benchmark big-parallel kvm\n\
		trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=\n\
		trusted-substituters = \nextra-sandbox-paths = \nmax-silent-time = 1K\n";
	fs::write(env.dir.path().join("conf/nix.conf"), system).unwrap();
	let user = env.dir.path().join("home/.config/nix");
	fs::create_dir_all(&user).unwrap();
	for (user_conf, args) in [
		("", vec!["-q", "--references", env.busybox.as_str()]),
		(
			"require-sigs = false\nextra-trusted-public-keys = a:b\n",
			vec!["-q", "--references", env.busybox.as_str()],
		),
		(
			"",
			vec![
				"-q",
				"--references",
				env.busybox.as_str(),
				"--option",
				"system-features",
				"kvm",
			],
		),
		// Settings given before their experimental feature is enabled are
		// ignored, yet forwarded with their default once it is.
		(
			"auto-allocate-uids = true\nimpure-env = A=b\n",
			vec![
				"-q",
				"--references",
				env.busybox.as_str(),
				"--extra-experimental-features",
				"auto-allocate-uids configurable-impure-env",
			],
		),
		// Not forwarded while the feature stays disabled.
		(
			"auto-allocate-uids = true\n",
			vec!["-q", "--references", env.busybox.as_str()],
		),
		// Forwarded with the given value when the feature is enabled first.
		(
			"extra-experimental-features = auto-allocate-uids\nauto-allocate-uids = true\n",
			vec!["-q", "--references", env.busybox.as_str()],
		),
	] {
		fs::write(user.join("nix.conf"), user_conf).unwrap();
		let expected = env.run("reference", "nix-store", &args);
		let actual = env.run("noxi", "nix-store", &args);
		assert_eq!(actual, expected, "{user_conf}{args:?}");
		let actual = env.run("pair", "nix-store", &args);
		assert_eq!(actual, expected, "{user_conf}{args:?} with the noxi daemon");
	}
}

#[test]
fn garbage_collection_works_through_both_daemons() {
	let env = Env::new();
	let busybox = env.busybox.as_str();
	let cases: Vec<(&str, Vec<&str>)> = vec![
		("nix-build", vec!["expr.nix", "-A", "ok", "-o", "result-ok"]),
		(
			"nix-instantiate",
			vec!["expr.nix", "-A", "text", "--eval", "--read-write-mode"],
		),
		("nix-store", vec!["--gc", "--print-roots"]),
		("nix-store", vec!["-q", "--roots", busybox]),
		("nix-store", vec!["-q", "--roots", "{work}/result-ok"]),
		("nix-store", vec!["--gc", "--print-live"]),
		("nix-store", vec!["--gc", "--print-dead"]),
		(
			"nix-store",
			vec!["--gc", "--print-dead", "--max-freed", "1K"],
		),
		(
			"nix-store",
			vec!["--gc", "--print-roots", "--max-freed", "1"],
		),
		("nix-store", vec!["--gc", busybox]),
		("nix-store", vec!["--gc", "--unknown"]),
		("nix-store", vec!["--delete", busybox]),
		("nix-store", vec!["--delete", "--ignore-liveness", busybox]),
		("nix-store", vec!["--delete"]),
		(
			"nix-env",
			vec!["-p", "{state}/profiles/gc", "--set", busybox],
		),
		(
			"nix-env",
			vec!["-p", "{state}/profiles/gc", "--set", "{work}/result-ok"],
		),
		("nix-collect-garbage", vec!["--dry-run"]),
		("nix-collect-garbage", vec!["-d", "--dry-run"]),
		("nix-collect-garbage", vec!["-d"]),
		("nix-store", vec!["--gc", "--max-freed", "1"]),
		("nix-collect-garbage", vec!["--max-freed", "1"]),
		("nix-collect-garbage", vec![]),
		("nix-store", vec!["--gc", "--print-dead"]),
	];
	for (program, args) in cases {
		let expected = env.run("reference", program, &args);
		for engine in ["noxi", "pair"] {
			assert_eq!(
				env.run(engine, program, &args),
				expected,
				"{program} {args:?} through {engine}"
			);
		}
	}
	for engine in ["reference", "noxi", "pair"] {
		assert!(
			fs::symlink_metadata(env.root(engine).join("nix/var/nix/profiles/gc-1-link")).is_err()
		);
		assert!(
			fs::symlink_metadata(env.root(engine).join("nix/var/nix/profiles/gc-2-link")).is_ok()
		);
		let store = noxi_store::LocalStore::open(noxi_store::LocalStoreConfig::with_root(
			&env.root(engine),
		))
		.unwrap();
		assert!(
			store
				.is_valid_path(
					&nix_compat::store_path::StorePath::from_absolute_path(busybox.as_bytes())
						.unwrap()
				)
				.unwrap()
		);
		let reference = env.run("reference", "nix-store", &["--gc", "--print-live"]);
		assert_eq!(
			env.run(engine, "nix-store", &["--gc", "--print-live"]),
			reference
		);
	}
}

#[test]
fn optimisation_works_through_both_daemons() {
	use std::os::unix::fs::MetadataExt;
	let env = Env::new();
	let cases: Vec<(&str, Vec<&str>)> = vec![
		(
			"nix-instantiate",
			vec!["expr.nix", "-A", "text", "--eval", "--read-write-mode"],
		),
		(
			"nix-instantiate",
			vec!["expr.nix", "-A", "text2", "--eval", "--read-write-mode"],
		),
		("nix-store", vec!["--optimise"]),
		("nix-store", vec!["--optimize"]),
		("nix-store", vec!["--optimise", "--quiet"]),
		("nix-store", vec!["--optimise", "--unknown"]),
		("nix-store", vec!["--optimise", "unexpected"]),
	];
	for (program, args) in cases {
		let expected = env.run("reference", program, &args);
		for engine in ["noxi", "pair"] {
			assert_eq!(
				env.run(engine, program, &args),
				expected,
				"{program} {args:?} through {engine}"
			);
		}
	}
	for engine in ["reference", "noxi", "pair"] {
		let paths: Vec<_> = fs::read_dir(env.root(engine).join("nix/store"))
			.unwrap()
			.map(|entry| entry.unwrap().path())
			.filter(|path| {
				let name = path.file_name().unwrap().to_string_lossy();
				name.ends_with("-note") || name.ends_with("-note2")
			})
			.collect();
		assert_eq!(paths.len(), 2);
		let first = fs::metadata(&paths[0]).unwrap();
		let second = fs::metadata(&paths[1]).unwrap();
		assert_eq!((first.dev(), first.ino()), (second.dev(), second.ino()));
		assert_eq!(fs::read(&paths[0]).unwrap(), b"hello");
	}
}

#[test]
fn imports_and_realizes_through_both_daemons() {
	let env = Env::new();
	let expression = format!(
		r#"let
		bb = builtins.storePath {};
		d = derivation {{ name = "daemon-ifd"; system = "x86_64-linux";
			builder = "${{bb}}/bin/sh"; args = [ "-c" "echo '{{ answer = 42; }}' > $out" ];
		}};
	in import d"#,
		env.busybox
	);
	for flags in [
		vec![
			"--eval",
			"--read-write-mode",
			"--strict",
			"--json",
			"-E",
			&expression,
		],
		vec!["--eval", "--strict", "--json", "-E", &expression],
	] {
		let expected = env.run("reference", "nix-instantiate", &flags);
		assert!(expected.contains("exit Some(0)"), "{expected}");
		for engine in ["noxi", "pair"] {
			assert_eq!(
				env.run(engine, "nix-instantiate", &flags),
				expected,
				"{engine}"
			);
		}
	}
	let fails = expression
		.replace("daemon-ifd", "daemon-failed-ifd")
		.replace(
			"echo '{ answer = 42; }' > $out",
			"echo failed IFD >&2; exit 3",
		);
	let args = ["--eval", "--read-write-mode", "--strict", "-E", &fails];
	let expected = env.run("reference", "nix-instantiate", &args);
	assert!(expected.ends_with("exit Some(1)"), "{expected}");
	for engine in ["noxi", "pair"] {
		let actual = env.run(engine, "nix-instantiate", &args);
		assert!(actual.ends_with("exit Some(1)"), "{engine}: {actual}");
		assert!(actual.contains("daemon-failed-ifd"));
	}
}

#[test]
fn verification_and_copies_work_through_both_daemons() {
	use std::os::unix::fs::PermissionsExt;
	let env = Env::new();
	// A local store with two paths no daemon has, one referring to the other.
	let source = env.dir.path().join("copy-source");
	let source_uri = format!("local?root={}", source.display());
	let output = env
		.command(reference_bin().join("nix-instantiate"))
		.args([
			"--store",
			&source_uri,
			"--eval",
			"--strict",
			"--read-write-mode",
		])
		.args([
			"--expr",
			r#"builtins.toFile "copied" "${builtins.toFile "copied-dep" "x"}""#,
		])
		.output()
		.unwrap();
	assert!(output.status.success());
	let copied = String::from_utf8(output.stdout).unwrap();
	let copied = copied.trim().trim_matches('"').to_owned();
	let cases: Vec<(&str, Vec<&str>)> = vec![
		("nix-store", vec!["--verify"]),
		("nix-store", vec!["--verify", "--check-contents"]),
		("nix-store", vec!["--verify", "--quiet"]),
		("nix-store", vec!["--verify", "unexpected"]),
		("nix-store", vec!["--verify", "--unknown"]),
		(
			"nix",
			vec!["copy", "--no-check-sigs", "--from", &source_uri, &copied],
		),
		(
			"nix",
			vec!["copy", "--no-check-sigs", "--from", &source_uri, &copied],
		),
		("nix-store", vec!["--query", "--requisites", &copied]),
	];
	let compare = |program: &str, args: &[&str]| {
		let expected = env.run("reference", program, args);
		for engine in ["noxi", "pair"] {
			assert_eq!(
				env.run(engine, program, args),
				expected,
				"{program} {args:?} through {engine}"
			);
		}
	};
	for (program, args) in &cases {
		compare(program, args);
	}
	// Remove the copied dependency behind every daemon's back.
	let dependency = fs::read_dir(source.join("nix/store"))
		.unwrap()
		.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
		.find(|name| name.ends_with("-copied-dep"))
		.unwrap();
	for engine in ["reference", "noxi", "pair"] {
		let store = env.root(engine).join("nix/store");
		fs::set_permissions(&store, fs::Permissions::from_mode(0o755)).unwrap();
		fs::remove_file(store.join(&dependency)).unwrap();
	}
	compare("nix-store", &["--verify"]);
	compare("nix-store", &["--verify", "--check-contents"]);
}

/// Socket and state paths longer than `sockaddr_un` allows. CppNix binds
/// and connects them from a child in the socket's directory; the daemon
/// socket, the garbage collector's roots socket under the state directory
/// and the clients of both must work as with short paths, and a missing
/// long socket must fail the same way.
#[test]
fn long_socket_and_state_paths_work_like_cppnix() {
	let env = Env::new_in(&"l".repeat(100));
	assert!(env.socket("noxi").as_os_str().len() >= 108);
	assert!(
		env.root("noxi")
			.join("nix/var/nix/gc-socket/socket")
			.as_os_str()
			.len()
			>= 108
	);
	let busybox = env.busybox.as_str();
	let cases: Vec<(&str, Vec<&str>)> = vec![
		("nix-store", vec!["-q", "--references", busybox]),
		("nix-build", vec!["expr.nix", "-A", "ok", "-o", "result-ok"]),
		(
			"nix-store",
			vec![
				"--realise",
				busybox,
				"--add-root",
				"{work}/gcroots/new/root",
			],
		),
		("nix-store", vec!["--gc", "--print-dead"]),
		("nix-store", vec!["--gc"]),
		("nix-store", vec!["--gc", "--print-live"]),
	];
	for (program, args) in cases {
		let expected = env.run("reference", program, &args);
		for engine in ["noxi", "pair"] {
			assert_eq!(
				env.run(engine, program, &args),
				expected,
				"{program} {args:?} through {engine}"
			);
		}
	}
	let missing = env.base.join("missing").join("daemon.sock");
	let outputs: Vec<_> = [
		reference_bin().join("nix-store"),
		PathBuf::from(noxi_bin("nix-store")),
	]
	.iter()
	.map(|bin| {
		let output = env
			.command(bin)
			.arg("--store")
			.arg(format!("unix://{}", missing.display()))
			.args(["-q", "--references", busybox])
			.output()
			.unwrap();
		(
			output.status.code(),
			String::from_utf8_lossy(&output.stderr).into_owned(),
		)
	})
	.collect();
	assert_eq!(outputs[1], outputs[0]);
}

/// A daemon whose `allowed-users` excludes the client closes the
/// connection; the client reports the failed read as CppNix does, for
/// both daemons.
#[test]
fn refused_clients_fail_like_cppnix() {
	let env = Env::new();
	let refused = |engine: &str| env.base.join(format!("refused-{engine}.sock"));
	let mut settings = noxi_daemon::DaemonSettings::new(noxi_store::LocalStoreConfig::with_root(
		&env.root("pair"),
	));
	settings.allowed_users = vec!["root".to_owned()];
	settings.trusted_users = Vec::new();
	let listener = noxi_store::unix_socket::bind(&refused("noxi")).unwrap();
	std::thread::spawn(move || noxi_daemon::server::serve(listener, &settings));
	let mut daemon = env
		.command(reference_bin().join("nix-daemon"))
		.arg("--store")
		.arg(env.root("reference"))
		.env("NIX_DAEMON_SOCKET_PATH", refused("reference"))
		.env(
			"NIX_CONFIG",
			format!("{CONFIG}allowed-users = root\ntrusted-users = root\n"),
		)
		.stderr(std::process::Stdio::null())
		.spawn()
		.unwrap();
	let deadline = Instant::now() + Duration::from_secs(20);
	while !refused("reference").exists() {
		assert!(
			Instant::now() < deadline,
			"the reference daemon did not start"
		);
		std::thread::sleep(Duration::from_millis(20));
	}
	let mut outputs = Vec::new();
	for daemon_engine in ["reference", "noxi"] {
		for bin in [
			reference_bin().join("nix-store"),
			PathBuf::from(noxi_bin("nix-store")),
		] {
			let output = env
				.command(&bin)
				.arg("--store")
				.arg(format!("unix://{}", refused(daemon_engine).display()))
				.args(["-q", "--references", env.busybox.as_str()])
				.output()
				.unwrap();
			outputs.push((
				output.status.code(),
				String::from_utf8_lossy(&output.stderr)
					.replace(&refused(daemon_engine).display().to_string(), "SOCKET"),
			));
		}
	}
	let _ = daemon.kill();
	let _ = daemon.wait();
	assert_eq!(outputs[0].0, Some(1), "{outputs:?}");
	for output in &outputs[1..] {
		assert_eq!(output, &outputs[0]);
	}
}
