//! `--log-format` through a daemon, checked against the pinned CppNix
//! 2.35.2 reference in all four pairings: the reference client with the
//! reference daemon, the reference client with the noxi daemon, the noxi
//! client with the reference daemon and the noxi client with the noxi
//! daemon. Each pairing has its own copy of a store holding the test
//! busybox and its own daemon; the clients connect as an untrusted user
//! through `unix://SOCKET?root=ROOT`.
//!
//! The daemon sends the activities and results of the builds and
//! substitutions it runs (the worker's `realise`, `builds` and
//! `copy-paths` progress, each `build`, `substitute`, `copy-path` and
//! `query-path-info`, build log lines and phases), and the client renders
//! them with its own logger, keeping the daemon's ids, parents and types.
//! Every pairing must print what the reference client prints with the
//! reference daemon:
//!
//! - `internal-json`, normalised as in `reference_log_format`: activity
//!   ids and parents renumbered by first appearance, consecutive identical
//!   lines collapsed, a file transfer's `stop` placed after its last
//!   result, and error messages compared without colour. Store roots and
//!   sockets are replaced by placeholders.
//! - `bar` and `bar-with-logs` with standard error redirected, byte for
//!   byte, and on an 80-column pty through util-linux `script`: the
//!   committed lines and the cursor handling.
//!
//! The reference executables come from `NOXI_REFERENCE_BIN` or
//! `target/nix-reference/bin`, and busybox from `NOXI_TEST_BUSYBOX` or
//! `target/test-busybox` (`just reference` creates both).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// The client sets no `substituters`: an untrusted client's setting would
/// replace the daemon's.
const CLIENT_CONFIG: &str = "experimental-features = nix-command\n";
const DAEMON_CONFIG: &str = "experimental-features = nix-command\nbuild-users-group =\nsandbox = true\nsandbox-fallback = false\nsandbox-paths =\ncores = 1\nrequire-sigs = false\n";
/// Client and daemon, in that order.
const PAIRINGS: [(&str, &str); 4] = [
	("reference", "reference"),
	("reference", "noxi"),
	("noxi", "reference"),
	("noxi", "noxi"),
];

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

fn executable(engine: &str, program: &str) -> PathBuf {
	match engine {
		"reference" => reference_bin().join(program),
		_ => PathBuf::from(match program {
			"nix-build" => env!("CARGO_BIN_EXE_nix-build"),
			"nix-store" => env!("CARGO_BIN_EXE_nix-store"),
			"nix" => env!("CARGO_BIN_EXE_nix"),
			_ => panic!("no noxi executable for {program}"),
		}),
	}
}

const EXPR: &str = r#"
{ tag ? "" }:
let
  bb = builtins.storePath @busybox@;
  mk = name: deps: script: derivation {
    inherit name deps tag;
    system = "x86_64-linux";
    builder = "${bb}/bin/busybox";
    args = [ "sh" "-c" script ];
  };
  a = mk "a-1.0" [ ] "echo building a; echo x > $out";
in {
  inherit a;
  b = mk "b" [ a ] "echo building b; echo y > $out";
  logs = mk "logs-2.1" [ ] ''
    echo one
    echo '@nix {"action":"setPhase","phase":"buildPhase"}'
    printf 'abc\rX\n'
    printf 'red\ttab\n'
    echo two >&2
    printf partial
    echo x > $out
  '';
  fail = mk "fail" [ ] "echo l1; echo l2 >&2; exit 3";
  dependent = mk "dependent" [ (mk "broken" [ ] "echo oops; exit 2") ] "echo z > $out";
}
"#;

struct Env {
	dir: noxi_test_support::TempDir,
	busybox: String,
	daemons: Vec<Child>,
}

impl Drop for Env {
	fn drop(&mut self) {
		for daemon in &mut self.daemons {
			let _ = daemon.kill();
			let _ = daemon.wait();
		}
	}
}

/// A pairing's name: `CLIENT-DAEMON`.
fn name(client: &str, daemon: &str) -> String {
	format!("{client}-{daemon}")
}

impl Env {
	/// Four stores holding the busybox, each served by its pairing's
	/// daemon with `daemon_config` added to its configuration.
	fn new(daemon_config: &str) -> Self {
		let dir = noxi_test_support::tempdir().unwrap();
		for sub in ["conf", "home", "daemon-conf"] {
			fs::create_dir(dir.path().join(sub)).unwrap();
		}
		fs::write(
			dir.path().join("daemon-conf/nix.conf"),
			format!("{DAEMON_CONFIG}{daemon_config}"),
		)
		.unwrap();
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
			busybox,
			daemons: Vec::new(),
		};
		fs::write(
			env.dir.path().join("expr.nix"),
			EXPR.replace("@busybox@", &env.busybox),
		)
		.unwrap();
		for (client, daemon) in PAIRINGS {
			let pairing = name(client, daemon);
			let output = env.reference(&[
				"nix",
				"copy",
				"--no-check-sigs",
				"--to",
				&format!("local?root={}", env.root(&pairing).display()),
				&env.busybox,
			]);
			assert!(
				output.status.success(),
				"{}",
				String::from_utf8_lossy(&output.stderr)
			);
			let mut command = Command::new(match daemon {
				"reference" => reference_bin().join("nix-daemon"),
				_ => executable("noxi", "nix"),
			});
			if daemon == "noxi" {
				command.arg("daemon");
			}
			let child = command
				.arg("--store")
				.arg(env.root(&pairing))
				.env_clear()
				.env("HOME", env.dir.path().join("home"))
				.env("NIX_CONF_DIR", env.dir.path().join("daemon-conf"))
				.env("NIX_USER_CONF_FILES", "")
				.env("NIX_DAEMON_SOCKET_PATH", env.socket(&pairing))
				.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin")
				.stdout(Stdio::null())
				.stderr(Stdio::null())
				.spawn()
				.unwrap();
			env.daemons.push(child);
		}
		let deadline = Instant::now() + Duration::from_secs(20);
		while !PAIRINGS
			.iter()
			.all(|(client, daemon)| env.socket(&name(client, daemon)).exists())
		{
			assert!(Instant::now() < deadline, "the daemons did not start");
			std::thread::sleep(Duration::from_millis(20));
		}
		env
	}

	fn root(&self, pairing: &str) -> PathBuf {
		self.dir.path().join(format!("root-{pairing}"))
	}

	fn socket(&self, pairing: &str) -> PathBuf {
		self.dir.path().join(format!("{pairing}.sock"))
	}

	fn store(&self, pairing: &str) -> String {
		format!(
			"unix://{}?root={}",
			self.socket(pairing).display(),
			self.root(pairing).display()
		)
	}

	fn command(&self, program: &Path) -> Command {
		let mut command = Command::new(program);
		command
			.current_dir(self.dir.path())
			.env_clear()
			.env("HOME", self.dir.path().join("home"))
			.env("NIX_CONF_DIR", self.dir.path().join("conf"))
			.env("NIX_USER_CONF_FILES", "")
			.env("NIX_CONFIG", CLIENT_CONFIG)
			.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin");
		command
	}

	/// Run a reference command (`program` and its arguments) directly.
	fn reference(&self, args: &[&str]) -> Output {
		self.command(&reference_bin().join(args[0]))
			.args(&args[1..])
			.output()
			.unwrap()
	}

	/// Instantiate attribute `attr` with `tag` into `pairing`'s store.
	fn instantiate(&self, pairing: &str, attr: &str, tag: &str) -> String {
		let output = self.reference(&[
			"nix-instantiate",
			"--store",
			&format!("local?root={}", self.root(pairing).display()),
			"expr.nix",
			"-A",
			attr,
			"--argstr",
			"tag",
			tag,
		]);
		assert!(
			output.status.success(),
			"{}",
			String::from_utf8_lossy(&output.stderr)
		);
		String::from_utf8(output.stdout).unwrap().trim().to_owned()
	}

	/// Run `program` with `args` in every pairing; `{drv}` is replaced by
	/// `attr`'s derivation with an empty tag. Returns each pairing's standard error, with its
	/// store root and socket replaced, and exit status.
	fn all(&self, program: &str, attr: Option<&str>, args: &[&str]) -> Vec<(String, Option<i32>)> {
		PAIRINGS
			.iter()
			.map(|(client, daemon)| {
				let pairing = name(client, daemon);
				let drv = attr.map(|attr| self.instantiate(&pairing, attr, ""));
				let store = self.store(&pairing);
				let mut full = vec!["--store", store.as_str()];
				for arg in args {
					full.push(match (*arg, &drv) {
						("{drv}", Some(drv)) => drv.as_str(),
						_ => arg,
					});
				}
				let output = self
					.command(&executable(client, program))
					.args(&full)
					.output()
					.unwrap();
				(
					self.placeholders(&pairing, &output.stderr),
					output.status.code(),
				)
			})
			.collect()
	}

	fn placeholders(&self, pairing: &str, text: &[u8]) -> String {
		String::from_utf8_lossy(text)
			.replace(&self.socket(pairing).display().to_string(), "SOCKET")
			.replace(&self.root(pairing).display().to_string(), "ROOT")
			.replace(&self.dir.path().display().to_string(), "TMP")
	}
}

/// The `@nix` lines of `stderr` with ids renumbered in order of first
/// appearance, hint colouring removed from messages and consecutive
/// identical lines collapsed, as `reference_log_format` normalises them.
fn normalise_json(stderr: &str) -> Vec<String> {
	let mut ids: HashMap<String, usize> = HashMap::new();
	let mut lines: Vec<String> = Vec::new();
	for line in stderr.lines() {
		assert!(
			line.starts_with("@nix {"),
			"not an internal-json line: {line}\n{stderr}"
		);
		let mut out = String::new();
		let mut rest = line;
		while let Some(pos) = rest.find(['"']) {
			out.push_str(&rest[..pos]);
			rest = &rest[pos..];
			let key = ["\"id\":", "\"parent\":"]
				.into_iter()
				.find(|k| rest.starts_with(k));
			match key {
				Some(key) => {
					out.push_str(key);
					rest = &rest[key.len()..];
					let end = rest
						.find(|c: char| !c.is_ascii_digit())
						.unwrap_or(rest.len());
					let number = &rest[..end];
					if number == "0" {
						out.push('0');
					} else {
						let next = ids.len();
						let n = *ids.entry(number.to_owned()).or_insert(next);
						out.push_str(&format!("#{n}"));
					}
					rest = &rest[end..];
				}
				None => {
					out.push('"');
					rest = &rest[1..];
				}
			}
		}
		out.push_str(rest);
		if out.contains("\"action\":\"msg\"") {
			for code in ["\\u001b[35;1m", "\\u001b[31;1m", "\\u001b[1m", "\\u001b[0m"] {
				out = out.replace(code, "");
			}
		}
		if lines.last() != Some(&out) {
			lines.push(out);
		}
	}
	// curl finishes a transfer on its own thread, so the transfer's `stop`
	// races the caller's next activity; place it after the transfer's last
	// result.
	let transfers: Vec<String> = lines
		.iter()
		.filter(|l| l.contains("\"action\":\"start\"") && l.contains("\"type\":101}"))
		.filter_map(|l| {
			l.split("\"id\":")
				.nth(1)?
				.split(',')
				.next()
				.map(str::to_owned)
		})
		.collect();
	for id in transfers {
		let stop = format!("@nix {{\"action\":\"stop\",\"id\":{id}}}");
		let marker = format!("\"id\":{id},");
		if let Some(pos) = lines.iter().position(|l| *l == stop) {
			lines.remove(pos);
			let last = lines.iter().rposition(|l| l.contains(&marker)).unwrap_or(0);
			lines.insert(last + 1, stop);
		}
	}
	lines
}

fn assert_json_equal(case: &str, runs: &[(String, Option<i32>)]) {
	let reference = normalise_json(&runs[0].0);
	assert!(!reference.is_empty(), "{case}: no output");
	for (i, (client, daemon)) in PAIRINGS.iter().enumerate().skip(1) {
		let other = normalise_json(&runs[i].0);
		assert_eq!(
			reference.join("\n"),
			other.join("\n"),
			"{case}: internal-json of the {client} client with the {daemon} daemon differs"
		);
		assert_eq!(
			runs[0].1, runs[i].1,
			"{case}: exit status of the {client} client with the {daemon} daemon differs"
		);
	}
	println!("{case}: {} lines match", reference.len());
}

fn assert_all_equal(case: &str, runs: &[(String, Option<i32>)]) {
	for (i, (client, daemon)) in PAIRINGS.iter().enumerate().skip(1) {
		assert_eq!(
			runs[0], runs[i],
			"{case}: the {client} client with the {daemon} daemon differs"
		);
	}
}

#[test]
fn internal_json_through_a_daemon_matches_cppnix() {
	let env = Env::new("substituters =\n");
	let realise = |attr: &str| {
		env.all(
			"nix-store",
			Some(attr),
			&["-r", "{drv}", "--log-format", "internal-json"],
		)
	};
	let runs = realise("b");
	assert_eq!(runs[0].1, Some(0), "{}", runs[0].0);
	assert!(runs[0].0.contains("\"text\":\"building '"), "{}", runs[0].0);
	assert_json_equal("fresh chain", &runs);
	assert_json_equal("valid chain", &realise("b"));
	assert_json_equal("log lines", &realise("logs"));
	assert_json_equal("failure", &realise("fail"));
	assert_json_equal("dependency failure", &realise("dependent"));
	let runs = env.all(
		"nix-build",
		None,
		&[
			"expr.nix",
			"-A",
			"a",
			"--no-out-link",
			"--log-format",
			"internal-json",
		],
	);
	assert_json_equal("nix-build", &runs);
	// `nix build` asks for `BuildPathsWithResults`.
	let runs = env.all(
		"nix",
		None,
		&[
			"build",
			"-f",
			"expr.nix",
			"b",
			"--argstr",
			"tag",
			"nix-build",
			"--no-link",
			"--log-format",
			"internal-json",
		],
	);
	assert_json_equal("nix build", &runs);

	// The download is the client's; the import goes through the daemon.
	let data = env.dir.path().join("data.txt");
	fs::write(&data, "hello-data\n").unwrap();
	let url = format!("file://{}", data.display());
	let runs = env.all(
		"nix",
		None,
		&[
			"store",
			"prefetch-file",
			&url,
			"--log-format",
			"internal-json",
		],
	);
	assert_json_equal("download", &runs);
}

#[test]
fn substitution_through_a_daemon_matches_cppnix() {
	// The daemons substitute from a `file://` cache; a substituter they
	// cannot open is reported to the client when they first need them.
	let probe = noxi_test_support::tempdir().unwrap();
	let cache = probe.path().join("cache");
	let env = Env::new(&format!(
		"substituters = foo://bar file://{}?trusted=1\n",
		cache.display()
	));
	let src = env.dir.path().join("src");
	let output = env.reference(&[
		"nix",
		"copy",
		"--no-check-sigs",
		"--to",
		&format!("local?root={}", src.display()),
		&env.busybox,
	]);
	assert!(output.status.success());
	let drv = env.reference(&[
		"nix-instantiate",
		"--store",
		&format!("local?root={}", src.display()),
		"expr.nix",
		"-A",
		"b",
	]);
	let drv = String::from_utf8(drv.stdout).unwrap().trim().to_owned();
	let built = env.reference(&[
		"nix-store",
		"--store",
		&format!("local?root={}", src.display()),
		"-r",
		&drv,
	]);
	assert!(built.status.success());
	let out = String::from_utf8(built.stdout).unwrap().trim().to_owned();
	let copied = env.reference(&[
		"nix",
		"copy",
		"--store",
		&format!("local?root={}", src.display()),
		"--to",
		&format!("file://{}?compression=none", cache.display()),
		&out,
	]);
	assert!(
		copied.status.success(),
		"{}",
		String::from_utf8_lossy(&copied.stderr)
	);
	let runs = env.all(
		"nix-store",
		None,
		&["-r", &out, "--log-format", "internal-json"],
	);
	assert_eq!(runs[0].1, Some(0), "{}", runs[0].0);
	for expected in [
		"don't know how to open Nix store with scheme",
		"\"type\":108",
		"copying path",
	] {
		assert!(runs[0].0.contains(expected), "{expected}:\n{}", runs[0].0);
	}
	assert_json_equal("substitution", &runs);
	// Already valid: the warning is printed once per connection.
	let runs = env.all("nix-store", None, &["-r", &out]);
	assert_all_equal("valid substitute, raw", &runs);
}

/// Realise `attr` with `tag` in every pairing with `args` after the
/// derivation, returning each pairing's standard error and exit status.
fn realise_tagged(env: &Env, attr: &str, tag: &str, args: &[&str]) -> Vec<(String, Option<i32>)> {
	PAIRINGS
		.iter()
		.map(|(client, daemon)| {
			let pairing = name(client, daemon);
			let drv = env.instantiate(&pairing, attr, tag);
			let output = env
				.command(&executable(client, "nix-store"))
				.args(["--store", &env.store(&pairing), "-r", &drv])
				.args(args)
				.output()
				.unwrap();
			(
				env.placeholders(&pairing, &output.stderr),
				output.status.code(),
			)
		})
		.collect()
}

#[test]
fn redirected_bar_through_a_daemon_matches_cppnix() {
	let env = Env::new("substituters =\n");
	for format in ["raw", "raw-with-logs", "bar", "bar-with-logs"] {
		for attr in ["logs", "fail"] {
			let runs = realise_tagged(&env, attr, format, &["--log-format", format]);
			// A failure the daemon reports exits with status 1.
			let expected = if attr == "logs" { Some(0) } else { Some(1) };
			assert_eq!(runs[0].1, expected, "{format} {attr}: {}", runs[0].0);
			if format.ends_with("-with-logs") {
				let line = if attr == "logs" { "one" } else { "l1" };
				assert!(runs[0].0.contains(line), "{format} {attr}: {}", runs[0].0);
			}
			assert_all_equal(&format!("nix-store {format} {attr}"), &runs);
		}
	}
	// `nix` defaults to the bar; `-L` adds build logs.
	for logs in [false, true] {
		let tag = format!("nix-build-{logs}");
		let mut args = vec![
			"build",
			"-f",
			"expr.nix",
			"logs",
			"--argstr",
			"tag",
			&tag,
			"--no-link",
		];
		if logs {
			args.push("-L");
		}
		let runs = env.all("nix", None, &args);
		assert_all_equal(&format!("nix build -L={logs}"), &runs);
	}
}

/// The lines a terminal keeps: each line's text after the last line clear
/// (or carriage return), with the cursor visibility codes removed. The
/// lines of an error message are compared without colour, as noxi's error
/// strings lack CppNix's hint colouring. Also returns whether the cursor
/// was hidden first and shown again.
fn committed_lines(tty: &str) -> (Vec<String>, bool, bool) {
	let hidden = tty.starts_with("\x1b[?25l");
	let shown = tty.contains("\x1b[?25h");
	let tty = tty.replace("\x1b[?25l", "").replace("\x1b[?25h", "");
	let mut lines = Vec::new();
	let mut parts: Vec<&str> = tty.split("\r\n").collect();
	parts.pop();
	let mut in_error = false;
	for part in parts {
		let text = match part.rfind("\r\x1b[K") {
			Some(i) => &part[i + 4..],
			None => part.rsplit('\r').next().unwrap(),
		};
		in_error =
			text.starts_with("\x1b[31;1merror:") || (in_error && text.starts_with("       "));
		if in_error {
			let mut plain = String::new();
			let mut rest = text;
			while let Some(i) = rest.find("\x1b[") {
				plain.push_str(&rest[..i]);
				rest = &rest[i..];
				let end = rest.find('m').map_or(rest.len(), |m| m + 1);
				rest = &rest[end..];
			}
			plain.push_str(rest);
			lines.push(plain);
		} else {
			lines.push(text.to_owned());
		}
	}
	(lines, hidden, shown)
}

#[test]
fn terminal_bar_through_a_daemon_matches_cppnix() {
	if Command::new("script").arg("--version").output().is_err() {
		println!("SKIP: util-linux `script` is not available for a pty");
		return;
	}
	let env = Env::new("substituters =\n");
	for format in ["bar", "bar-with-logs"] {
		for attr in ["logs", "fail"] {
			let tag = format!("pty-{format}");
			let mut results = Vec::new();
			for (client, daemon) in PAIRINGS {
				let pairing = name(client, daemon);
				let drv = env.instantiate(&pairing, attr, &tag);
				let inner = format!(
					"stty cols 80 rows 24; {} --store '{}' -r {drv} --log-format {format} > /dev/null",
					executable(client, "nix-store").display(),
					env.store(&pairing)
				);
				let command = env.command(Path::new("script"));
				let output = Command::new("script")
					.args(["-qec", &inner, "/dev/null"])
					.current_dir(env.dir.path())
					.env_clear()
					.envs(command.get_envs().filter_map(|(k, v)| Some((k, v?))))
					.env("TERM", "xterm")
					.env("SHELL", "/bin/sh")
					.output()
					.unwrap();
				let tty = env.placeholders(&pairing, &output.stdout);
				results.push(committed_lines(&tty));
			}
			for (i, (client, daemon)) in PAIRINGS.iter().enumerate().skip(1) {
				assert_eq!(
					results[0], results[i],
					"terminal {format} {attr}: the {client} client with the {daemon} daemon differs"
				);
			}
			assert!(
				!results[0].0.is_empty(),
				"terminal {format} {attr}: no output"
			);
			assert!(
				results[0].1 && results[0].2,
				"cursor hidden and shown again"
			);
		}
	}
}
