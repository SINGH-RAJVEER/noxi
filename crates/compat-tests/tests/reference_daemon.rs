//! The nix-rs daemon checked against the pinned CppNix 2.35.2 reference.
//!
//! The reference builds a small store, which is copied twice. The reference
//! `nix-daemon` serves one copy and the nix-rs daemon, started in this
//! process, serves the other. Unchanged reference clients then run the same
//! commands against each socket, and their exit status, standard output and
//! standard error must match.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
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

	/// The store URL a client uses for daemon `name`: its socket, with the
	/// root the client reads store files from.
	fn store_url(&self, name: &str) -> String {
		format!("{}?root={}", self.socket(name), self.root(name).display())
	}

	fn new() -> Self {
		Self::with_trust(false)
	}

	/// With `trusted`, both daemons trust the user running the tests.
	fn with_trust(trusted: bool) -> Self {
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

		let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
			.unwrap()
			.trim()
			.to_owned();
		let mut daemon_config = CONFIG.to_owned();
		if trusted {
			daemon_config.push_str(&format!("trusted-users = {user}\n"));
		}
		let reference = env
			.command(
				"nix-daemon",
				&["--store", env.root("reference").to_str().unwrap()],
			)
			.env("NIX_CONFIG", daemon_config)
			.env("NIX_DAEMON_SOCKET_PATH", env.root("reference.sock"))
			.stderr(std::process::Stdio::null())
			.spawn()
			.unwrap();
		env.reference_daemon = Some(reference);

		let listener = UnixListener::bind(env.root("nix-rs.sock")).unwrap();
		let mut settings = DaemonSettings::new(LocalStoreConfig::with_root(&env.root("nix-rs")));
		settings.build.sandbox_paths = Vec::new();
		settings.build.build_cores = 1;
		// The reference daemon runs with `nix-command` enabled.
		settings.build.nix_command = true;
		if trusted {
			settings.trusted_users.push(user);
		}
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
		self.compare_with_input(program, args, None)
	}

	/// [`Env::compare`] with `stdin` as the client's standard input. In
	/// `args`, `{daemon}` is replaced by the daemon's name and `{store}` by
	/// its store URL.
	fn compare_with_input(&self, program: &str, args: &[&str], stdin: Option<&[u8]>) -> Output {
		self.compare_filtered(program, args, stdin, None)
	}

	fn compare_filtered(
		&self,
		program: &str,
		args: &[&str],
		stdin: Option<&[u8]>,
		root: Option<&str>,
	) -> Output {
		let mut outputs = Vec::new();
		for daemon in ["reference", "nix-rs"] {
			let url = self.store_url(daemon);
			let args: Vec<String> = args
				.iter()
				.map(|a| a.replace("{daemon}", daemon).replace("{store}", &url))
				.collect();
			let mut full = vec!["--store", url.as_str()];
			full.extend(args.iter().map(String::as_str));
			let mut command = self.command(program, &full);
			command
				.stdin(std::process::Stdio::piped())
				.stdout(std::process::Stdio::piped())
				.stderr(std::process::Stdio::piped());
			let mut child = command.spawn().unwrap();
			let mut input = child.stdin.take().unwrap();
			if let Some(stdin) = stdin {
				input.write_all(stdin).unwrap();
			}
			drop(input);
			let mut output = child.wait_with_output().unwrap();
			if let Some(root) = root {
				output.stdout = String::from_utf8(output.stdout)
					.unwrap()
					.lines()
					.filter(|line| line.ends_with(&format!(" -> {root}")))
					.map(|line| format!("{line}\n"))
					.collect::<String>()
					.into_bytes();
			}
			outputs.push(output);
		}
		let normalise = |bytes: &[u8], name: &str| {
			let text = String::from_utf8_lossy(bytes)
				.replace(&self.root(name).display().to_string(), "ROOT")
				.replace(&self.store_url(name), "SOCKET")
				.replace(&self.socket(name), "SOCKET")
				.replace(&format!("link-{name}"), "link-DAEMON");
			// Paths built by each daemon may be registered in different
			// seconds; the times are compared with a tolerance below.
			let text = mask_registration_times(&text);
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
		let times = |output: &Output| registration_times(&String::from_utf8_lossy(&output.stdout));
		let (reference_times, nix_rs_times) = (times(reference), times(nix_rs));
		assert_eq!(nix_rs_times.len(), reference_times.len());
		for (a, b) in nix_rs_times.iter().zip(&reference_times) {
			assert!(
				*a != 0 && a.abs_diff(*b) <= 2,
				"{program} {args:?}: registration time {a}, reference {b}"
			);
		}
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

const REGISTRATION_TIME: &str = "\"registrationTime\":";

#[test]
fn client_build_cores_are_applied_without_leaking_between_connections() {
	let env = Env::new();
	for cores in [2, 3] {
		let expression = format!(
			r#"derivation {{ name = "client-cores-{cores}"; system = "x86_64-linux"; builder = "${{builtins.storePath {}}}/bin/sh"; args = [ "-c" "echo $NIX_BUILD_CORES > $out" ]; }}"#,
			env.busybox
		);
		let output = env.compare(
			"nix-build",
			&[
				"--store",
				"{store}",
				"--no-out-link",
				"--cores",
				&cores.to_string(),
				"--expr",
				&expression,
			],
		);
		assert!(output.status.success());
		let path = String::from_utf8(output.stdout).unwrap();
		let physical = env.root("nix-rs").join(path.trim().trim_start_matches('/'));
		assert_eq!(fs::read_to_string(physical).unwrap(), format!("{cores}\n"));
	}
}

#[test]
fn store_optimisation_through_daemon_matches_reference() {
	let env = Env::new();
	env.compare("nix-store", &["--store", "{store}", "--optimise"]);
	for daemon in ["reference", "nix-rs"] {
		let links = env.root(daemon).join("nix/store/.links");
		assert!(fs::read_dir(links).unwrap().next().is_some());
	}
}

#[test]
fn build_log_upload_through_daemon_matches_reference() {
	let env = Env::with_trust(true);
	for daemon in ["reference", "nix-rs"] {
		let logs = env.root(daemon).join("nix/var/log/nix/drvs");
		if logs.exists() {
			fs::remove_dir_all(logs).unwrap();
		}
	}
	env.compare(
		"nix",
		&[
			"store",
			"copy-log",
			"--from",
			env.root("src").to_str().unwrap(),
			"--to",
			"{store}",
			&env.drv,
		],
	);
	env.compare("nix", &["log", "--store", "{store}", &env.drv]);
}

/// The values of `"registrationTime"` JSON fields.
fn registration_times(text: &str) -> Vec<u64> {
	text.split(REGISTRATION_TIME)
		.skip(1)
		.map(|rest| {
			let end = rest
				.find(|c: char| !c.is_ascii_digit())
				.unwrap_or(rest.len());
			rest[..end].parse().unwrap()
		})
		.collect()
}

/// Replace the values of `"registrationTime"` JSON fields with 0.
fn mask_registration_times(text: &str) -> String {
	const FIELD: &str = REGISTRATION_TIME;
	let mut out = String::new();
	let mut rest = text;
	while let Some(i) = rest.find(FIELD) {
		let (head, tail) = rest.split_at(i + FIELD.len());
		out.push_str(head);
		out.push('0');
		rest = tail.trim_start_matches(|c: char| c.is_ascii_digit());
	}
	out.push_str(rest);
	out
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
	// Runtime roots scan the shared /proc. Other tests can start or stop the
	// same BusyBox between our sequential daemon queries. Pin a unique path
	// with temporary roots owned by this test, and compare only its roots.
	let input = env.dir.path().join(format!(
		"censored-{}",
		env.dir.path().file_name().unwrap().to_string_lossy()
	));
	fs::write(&input, b"root owned by this test\n").unwrap();
	let added = env.compare("nix-store", &["--add", input.to_str().unwrap()]);
	assert!(added.status.success());
	let path = String::from_utf8(added.stdout).unwrap().trim().to_owned();
	let store_path =
		nix_compat::store_path::StorePath::from_absolute_path(path.as_bytes()).unwrap();
	let stores: Vec<_> = ["reference", "nix-rs"]
		.iter()
		.map(|name| {
			let store =
				nix_rs_store::LocalStore::open(LocalStoreConfig::with_root(&env.root(name)))
					.unwrap();
			store.add_temp_root(&store_path).unwrap();
			store
		})
		.collect();
	let roots = env.compare_filtered("nix-store", &["--gc", "--print-roots"], None, Some(&path));
	assert_eq!(
		String::from_utf8(roots.stdout).unwrap(),
		format!("\"{{censored}}\" -> {path}\n")
	);
	drop(stores);
}

fn write_expr(env: &Env, name: &str, script: &str) -> String {
	let file = env.dir.path().join(format!("{name}.nix"));
	fs::write(
		&file,
		format!(
			r#"derivation {{ name = "{name}"; system = "x86_64-linux"; builder = "${{builtins.storePath {}}}/bin/sh"; args = [ "-c" "{script}" ]; }}"#,
			env.busybox
		),
	)
	.unwrap();
	file.to_string_lossy().into_owned()
}

#[test]
fn builds_through_the_daemon_match_the_reference() {
	let env = Env::new();
	let expr = write_expr(
		&env,
		"daemon-built",
		"echo line one; echo line two >&2; printf partial; echo $builder > $out",
	);
	let output = env.compare("nix-build", &[&expr, "--no-out-link"]);
	let out = String::from_utf8(output.stdout).unwrap().trim().to_owned();
	assert!(out.ends_with("-daemon-built"), "{out}");
	assert!(String::from_utf8_lossy(&output.stderr).contains("line two"));
	env.compare("nix", &["path-info", "--json", &out]);
	env.compare("nix-store", &["-q", "--references", &out]);

	// Building again finds the output valid.
	env.compare("nix-build", &[&expr, "--no-out-link"]);

	// A failing build reports the same error.
	let failing = write_expr(&env, "daemon-failed", "echo before failing; exit 7");
	env.compare("nix-build", &[&failing, "--no-out-link"]);
}

#[test]
fn indirect_roots_through_the_daemon_match_the_reference() {
	let env = Env::new();
	let expr = write_expr(&env, "daemon-rooted", "echo rooted > $out");
	let link = env.dir.path().join("link-{daemon}");
	env.compare("nix-build", &[&expr, "-o", link.to_str().unwrap()]);
	env.compare("nix-store", &["--gc", "--print-roots"]);
}

#[test]
fn garbage_collection_through_the_daemon_matches_the_reference() {
	let env = Env::new();
	let out = env.out.clone();
	let busybox = env.busybox.clone();
	let expr = write_expr(&env, "daemon-kept", "echo kept > $out");
	let link = env.dir.path().join("kept-{daemon}");
	env.compare("nix-build", &[&expr, "-o", link.to_str().unwrap()]);
	for args in [
		vec!["--gc", "--print-live"],
		vec!["--gc", "--print-dead"],
		vec!["--delete", &busybox],
		vec!["--delete", &out],
		vec!["--gc", "--max-freed", "1"],
		vec!["--gc"],
		vec!["--gc", "--print-dead"],
	] {
		env.compare("nix-store", &args);
	}
	env.compare("nix-collect-garbage", &[]);
	env.compare("nix", &["store", "gc", "--dry-run"]);
	env.compare("nix", &["path-info", "--all"]);
}

/// Add two files to a third store, for copying into the daemons' stores.
/// Returns the store's URL and the paths.
fn other_store_paths(env: &Env) -> (String, Vec<String>) {
	let other = env.root("other").to_string_lossy().into_owned();
	let mut paths = Vec::new();
	for name in ["first", "second"] {
		let file = env.dir.path().join(name);
		fs::write(&file, format!("{name} file\n")).unwrap();
		let path = env.run_ok(
			"nix-store",
			&["--store", &other, "--add", file.to_str().unwrap()],
		);
		paths.push(path.trim().to_owned());
	}
	(format!("local?root={other}"), paths)
}

fn export(env: &Env, store: &str, path: &str) -> Vec<u8> {
	let output = env.run("nix-store", &["--store", store, "--export", path]);
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	output.stdout
}

#[test]
fn untrusted_copies_and_imports_need_signatures_like_the_reference() {
	let env = Env::new();
	let (other, paths) = other_store_paths(&env);
	// Content-addressed paths need no signatures.
	env.compare(
		"nix",
		&[
			"copy",
			"--no-check-sigs",
			"--from",
			&other,
			"--to",
			"{store}",
			&paths[0],
		],
	);
	// Imported paths are input-addressed, and do.
	env.compare_with_input(
		"nix-store",
		&["--import"],
		Some(&export(&env, &other, &paths[0])),
	);
	let imported = env.root("imported").to_string_lossy().into_owned();
	let mut import = env.command("nix-store", &["--store", &imported, "--import"]);
	import.stdin(std::process::Stdio::piped());
	let mut child = import.spawn().unwrap();
	child
		.stdin
		.take()
		.unwrap()
		.write_all(&export(&env, &other, &paths[1]))
		.unwrap();
	assert!(child.wait().unwrap().success());
	let output = env.compare(
		"nix",
		&[
			"copy",
			"--no-check-sigs",
			"--from",
			&format!("local?root={imported}"),
			"--to",
			"{store}",
			&paths[1],
		],
	);
	assert!(
		String::from_utf8_lossy(&output.stderr).contains("lacks a signature by a trusted key"),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	env.compare("nix", &["path-info", &paths[0], &paths[1]]);
}

#[test]
fn copies_imports_and_signatures_through_the_daemon_match_the_reference() {
	let env = Env::with_trust(true);
	let (other, paths) = other_store_paths(&env);
	let (first, second) = (paths[0].as_str(), paths[1].as_str());
	env.compare(
		"nix",
		&[
			"copy",
			"--no-check-sigs",
			"--from",
			&other,
			"--to",
			"{store}",
			first,
		],
	);
	// Copies keep the source's registration time.
	let source_time = |path: &str| {
		registration_times(&env.run_ok("nix", &["path-info", "--json", "--store", &other, path]))
	};
	let output = env.compare("nix", &["path-info", "--json", first]);
	assert_eq!(
		registration_times(&String::from_utf8_lossy(&output.stdout)),
		source_time(first)
	);

	// `nix-store --import` sends paths one at a time.
	env.compare_with_input(
		"nix-store",
		&["--import"],
		Some(&export(&env, &other, second)),
	);
	env.compare("nix", &["path-info", "--json", second]);

	// Copying valid paths again does nothing.
	env.compare(
		"nix",
		&[
			"copy",
			"--no-check-sigs",
			"--from",
			&other,
			"--to",
			"{store}",
			first,
			second,
		],
	);

	let key = env.dir.path().join("key");
	let secret = env.run_ok("nix", &["key", "generate-secret", "--key-name", "test-1"]);
	fs::write(&key, secret).unwrap();
	env.compare(
		"nix",
		&["store", "sign", "--key-file", key.to_str().unwrap(), first],
	);
	env.compare("nix", &["path-info", "--sigs", first, second]);
}

#[test]
fn verification_through_the_daemon_matches_the_reference() {
	let env = Env::new();
	env.compare("nix-store", &["--verify"]);
	env.compare("nix-store", &["--verify", "--check-contents"]);
	env.compare("nix-store", &["--verify", "--repair"]);

	let modified = write_expr(&env, "daemon-modified", "echo original > $out");
	let modified = env.compare("nix-build", &[&modified, "--no-out-link"]);
	let modified = String::from_utf8(modified.stdout)
		.unwrap()
		.trim()
		.to_owned();
	let vanished = write_expr(&env, "daemon-vanished", "echo vanished > $out");
	let vanished = env.compare("nix-build", &[&vanished, "--no-out-link"]);
	let vanished = String::from_utf8(vanished.stdout)
		.unwrap()
		.trim()
		.to_owned();
	for daemon in ["reference", "nix-rs"] {
		let root = env.root(daemon);
		let real = |path: &str| root.join(path.trim_start_matches('/'));
		let store_dir = real("nix/store");
		fs::set_permissions(&store_dir, fs::Permissions::from_mode(0o755)).unwrap();
		fs::set_permissions(real(&modified), fs::Permissions::from_mode(0o644)).unwrap();
		fs::write(real(&modified), "changed\n").unwrap();
		fs::set_permissions(real(&vanished), fs::Permissions::from_mode(0o644)).unwrap();
		fs::remove_file(real(&vanished)).unwrap();
	}
	env.compare("nix-store", &["--verify"]);
	env.compare("nix-store", &["--verify", "--check-contents", "--quiet"]);
	env.compare("nix-store", &["--verify", "--check-contents"]);
	env.compare("nix", &["path-info", &vanished]);
}
