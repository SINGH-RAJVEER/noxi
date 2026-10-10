//! nix-rs's commands as clients of the pinned CppNix 2.35.2 daemon.
//!
//! The reference makes a store holding the test busybox, which is copied
//! three times. A reference `nix-daemon` serves two copies and the nix-rs
//! daemon, started in this process, the third. The reference's commands
//! run against the first reference daemon, and nix-rs's against the
//! second and against the nix-rs daemon, through
//! `unix://SOCKET?root=ROOT`, as an untrusted user: queries, an
//! instantiation, builds that succeed and fail, a realisation without a
//! substituter, and a profile switch. Output, exit statuses and the daemon
//! messages relayed to the client, including the warnings about the
//! settings it forwards, must match.
//!
//! The reference executables come from `NIXRS_REFERENCE_BIN` or
//! `target/nix-reference/bin`, and the busybox from `NIXRS_TEST_BUSYBOX`
//! or `target/test-busybox` (`just reference` creates both).

use std::fs;
use std::os::unix::net::UnixListener;
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
	from_env_or_target("NIXRS_REFERENCE_BIN", "nix-reference/bin", "nix-store")
}

fn nix_rs_bin(program: &str) -> &'static str {
	match program {
		"nix-build" => env!("CARGO_BIN_EXE_nix-build"),
		"nix-env" => env!("CARGO_BIN_EXE_nix-env"),
		"nix-instantiate" => env!("CARGO_BIN_EXE_nix-instantiate"),
		"nix-store" => env!("CARGO_BIN_EXE_nix-store"),
		_ => unreachable!(),
	}
}

struct Env {
	dir: tempfile::TempDir,
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
		let dir = tempfile::tempdir().unwrap();
		for sub in ["conf", "home", "work-reference", "work-nix-rs", "work-pair"] {
			fs::create_dir(dir.path().join(sub)).unwrap();
		}
		let busybox = fs::canonicalize(from_env_or_target(
			"NIXRS_TEST_BUSYBOX",
			"test-busybox",
			"bin/busybox",
		))
		.unwrap()
		.to_string_lossy()
		.into_owned();
		let mut env = Env {
			dir,
			busybox,
			daemons: Vec::new(),
		};
		let src = env.dir.path().join("src");
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
}}"#,
			env.busybox
		);
		for engine in ["reference", "nix-rs", "pair"] {
			fs::write(
				env.dir.path().join(format!("work-{engine}/expr.nix")),
				&expr,
			)
			.unwrap();
			let status = Command::new("cp")
				.arg("-a")
				.arg(&src)
				.arg(env.root(engine))
				.status()
				.unwrap();
			assert!(status.success());
			if engine == "pair" {
				let listener = UnixListener::bind(env.socket(engine)).unwrap();
				let mut settings = nix_rs_daemon::DaemonSettings::new(
					nix_rs_store::LocalStoreConfig::with_root(&env.root(engine)),
				);
				settings.build.sandbox_paths = Vec::new();
				settings.build.build_cores = 1;
				// The reference daemons run with `nix-command` enabled.
				settings.build.nix_command = true;
				std::thread::spawn(move || nix_rs_daemon::server::serve(listener, &settings));
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
		while !(env.socket("reference").exists() && env.socket("nix-rs").exists()) {
			assert!(
				Instant::now() < deadline,
				"the reference daemons did not start"
			);
			std::thread::sleep(Duration::from_millis(20));
		}
		env
	}

	fn root(&self, engine: &str) -> PathBuf {
		self.dir.path().join(format!("root-{engine}"))
	}

	fn socket(&self, engine: &str) -> PathBuf {
		self.dir.path().join(format!("{engine}.sock"))
	}

	/// Run `program` from `engine` against that engine's daemon: the
	/// reference's for `reference` and `nix-rs`, nix-rs's for `pair`.
	fn run(&self, engine: &str, program: &str, args: &[&str]) -> String {
		let executable = match engine {
			"reference" => reference_bin().join(program),
			_ => PathBuf::from(nix_rs_bin(program)),
		};
		let work = self.dir.path().join(format!("work-{engine}"));
		let store = format!(
			"unix://{}?root={}",
			self.socket(engine).display(),
			self.root(engine).display()
		);
		let args: Vec<String> = args
			.iter()
			.map(|arg| arg.replace("{work}", &work.display().to_string()))
			.collect();
		let output = self
			.command(&executable)
			.current_dir(&work)
			.arg("--store")
			.arg(&store)
			.args(&args)
			.output()
			.unwrap();
		format!(
			"{}{}exit {:?}",
			String::from_utf8_lossy(&output.stdout),
			String::from_utf8_lossy(&output.stderr),
			output.status.code()
		)
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
		("nix-store", vec!["-q", "--referrers", busybox]),
	];
	for (program, args) in cases {
		let expected = env.run("reference", program, &args);
		let actual = env.run("nix-rs", program, &args);
		assert_eq!(actual, expected, "{program} {args:?}");
		let actual = env.run("pair", program, &args);
		assert_eq!(
			actual, expected,
			"{program} {args:?} with the nix-rs daemon"
		);
	}
	for engine in ["reference", "nix-rs", "pair"] {
		let work = env.dir.path().join(format!("work-{engine}"));
		assert!(fs::read_link(work.join("result-ok")).is_ok());
		assert!(fs::read_link(work.join("profile-1-link")).is_ok());
	}
}
