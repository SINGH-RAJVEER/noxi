//! The nix-rs daemon checked against the pinned CppNix 2.35.2 reference.
//!
//! The reference builds a small store, which is copied twice. The reference
//! `nix-daemon` serves one copy and the nix-rs daemon, started in this
//! process, serves the other. Unchanged reference clients then run the same
//! commands against each socket, and their exit status, standard output and
//! standard error must match.

use std::fs;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use nix_rs_daemon::DaemonSettings;
use nix_rs_daemon::server::serve;
use nix_rs_store::LocalStoreConfig;

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

const CONFIG: &str = "experimental-features = nix-command\nbuild-users-group =\nsubstituters =\nsandbox = true\nsandbox-fallback = false\nsandbox-paths =\ncores = 1\n";

struct Env {
	dir: tempfile::TempDir,
	busybox: String,
	out: String,
	drv: String,
	reference_daemon: Option<Child>,
}

impl Drop for Env {
	fn drop(&mut self) {
		if let Some(mut child) = self.reference_daemon.take() {
			let _ = child.kill();
			let _ = child.wait();
		}
	}
}

impl Env {
	fn command(&self, program: &str, args: &[&str]) -> Command {
		let bin = from_env_or_target("NIXRS_REFERENCE_BIN", "nix-reference/bin", "nix-store");
		let mut command = Command::new(bin.join(program));
		command
			.args(args)
			.env_clear()
			.env("HOME", self.dir.path().join("home"))
			.env("NIX_CONF_DIR", self.dir.path().join("conf"))
			.env("NIX_CONFIG", CONFIG)
			.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin");
		command
	}

	fn run(&self, program: &str, args: &[&str]) -> Output {
		self.command(program, args).output().unwrap()
	}

	fn run_ok(&self, program: &str, args: &[&str]) -> String {
		let output = self.run(program, args);
		assert!(
			output.status.success(),
			"{program} {args:?} failed:\n{}",
			String::from_utf8_lossy(&output.stderr)
		);
		String::from_utf8(output.stdout).unwrap()
	}

	fn root(&self, name: &str) -> PathBuf {
		self.dir.path().join(name)
	}

	fn socket(&self, name: &str) -> String {
		format!("unix://{}", self.root(&format!("{name}.sock")).display())
	}

	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		for sub in ["conf", "home"] {
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
			out: String::new(),
			drv: String::new(),
			reference_daemon: None,
		};
		let src = env.root("src").to_string_lossy().into_owned();
		env.run_ok(
			"nix",
			&[
				"copy",
				"--no-check-sigs",
				"--to",
				&format!("local?root={src}"),
				&env.busybox,
			],
		);
		let expr = env.dir.path().join("expr.nix");
		fs::write(
			&expr,
			format!(
				r#"derivation {{ name = "served"; system = "x86_64-linux"; builder = "${{builtins.storePath {}}}/bin/sh"; args = [ "-c" "echo $builder > $out" ]; }}"#,
				env.busybox
			),
		)
		.unwrap();
		env.drv = env
			.run_ok(
				"nix-instantiate",
				&["--store", &src, expr.to_str().unwrap()],
			)
			.trim()
			.to_owned();
		env.out = env
			.run_ok("nix-store", &["--store", &src, "--realise", &env.drv])
			.trim()
			.to_owned();
		for copy in ["reference", "nix-rs"] {
			let status = Command::new("cp")
				.args(["-a", &src, env.root(copy).to_str().unwrap()])
				.status()
				.unwrap();
			assert!(status.success());
		}

		let reference = env
			.command(
				"nix-daemon",
				&["--store", env.root("reference").to_str().unwrap()],
			)
			.env("NIX_DAEMON_SOCKET_PATH", env.root("reference.sock"))
			.stderr(std::process::Stdio::null())
			.spawn()
			.unwrap();
		env.reference_daemon = Some(reference);

		let listener = UnixListener::bind(env.root("nix-rs.sock")).unwrap();
		let settings = DaemonSettings::new(LocalStoreConfig::with_root(&env.root("nix-rs")));
		std::thread::spawn(move || serve(listener, &settings));

		let deadline = Instant::now() + Duration::from_secs(20);
		while !env.root("reference.sock").exists() {
			assert!(
				Instant::now() < deadline,
				"the reference daemon did not start"
			);
			std::thread::sleep(Duration::from_millis(20));
		}
		env
	}

	/// Run a client against both daemons and check they behave the same.
	/// Returns the output of the run against nix-rs.
	fn compare(&self, program: &str, args: &[&str]) -> Output {
		let mut outputs = Vec::new();
		for daemon in ["reference", "nix-rs"] {
			let socket = self.socket(daemon);
			let mut full = vec!["--store", socket.as_str()];
			full.extend_from_slice(args);
			let output = self.run(program, &full);
			outputs.push(output);
		}
		let normalise = |bytes: &[u8], name: &str| {
			let text = String::from_utf8_lossy(bytes)
				.replace(&self.root(name).display().to_string(), "ROOT")
				.replace(&self.socket(name), "SOCKET");
			// Process IDs in temporary-root file names differ.
			text.lines()
				.map(|line| match line.split_once("/temproots/") {
					Some((head, _)) => format!("{head}/temproots/PID\""),
					None => line.to_owned(),
				})
				.collect::<Vec<_>>()
				.join("\n")
		};
		let (reference, nix_rs) = (&outputs[0], &outputs[1]);
		assert_eq!(
			nix_rs.status.code(),
			reference.status.code(),
			"{program} {args:?}: exit status\nreference stderr:\n{}\nnix-rs stderr:\n{}",
			String::from_utf8_lossy(&reference.stderr),
			String::from_utf8_lossy(&nix_rs.stderr)
		);
		assert_eq!(
			normalise(&nix_rs.stdout, "nix-rs"),
			normalise(&reference.stdout, "reference"),
			"{program} {args:?}: stdout"
		);
		assert_eq!(
			normalise(&nix_rs.stderr, "nix-rs"),
			normalise(&reference.stderr, "reference"),
			"{program} {args:?}: stderr"
		);
		outputs.pop().unwrap()
	}
}

#[test]
fn queries_match_the_reference_daemon() {
	let env = Env::new();
	let out = env.out.clone();
	let drv = env.drv.clone();
	let busybox = env.busybox.clone();
	let missing = "/nix/store/00000000000000000000000000000000-missing";
	for args in [
		vec!["-q", "--references", &out],
		vec!["-q", "--referrers", &busybox],
		vec!["-q", "--deriver", &out],
		vec!["-q", "--outputs", &drv],
		vec!["-q", "--hash", &out],
		vec!["-q", "--size", &out],
		vec!["-q", "--requisites", &out],
		vec!["-q", "--valid-derivers", &out],
		vec!["--check-validity", &out],
		vec!["--check-validity", missing],
		vec!["-q", "--references", missing],
		vec!["-q", "--references", "/not/in/store"],
	] {
		env.compare("nix-store", &args);
	}
	for args in [
		vec!["path-info", "--json", &out],
		vec!["path-info", "--json", "--all"],
		vec!["path-info", "-r", &out],
		vec!["path-info", missing],
		vec!["store", "info", "--json"],
	] {
		env.compare("nix", &args);
	}
}

#[test]
fn untrusted_clients_get_the_reference_warnings_and_censored_roots() {
	let env = Env::new();
	let output = env.compare("nix-store", &["-q", "--references", &env.out.clone()]);
	assert!(
		String::from_utf8_lossy(&output.stderr).contains("because it is a restricted setting"),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	env.compare("nix-store", &["--gc", "--print-roots"]);
}
