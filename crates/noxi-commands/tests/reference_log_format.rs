//! `--log-format` checked against the pinned CppNix 2.35.2 reference.
//!
//! Each engine realises the same derivations in its own `local?root=`
//! store, and the standard error of the two runs is compared:
//!
//! - `internal-json`: every `@nix` line, for local builds, cached outputs,
//!   a failed build, a dependency failure, `nix-build`, substitution
//!   from a `file://` cache and a `file://` download. Activity ids embed
//!   the process id, so ids and parents are renumbered in order of first
//!   appearance; the tree is still compared. Repeated progress results of
//!   a file transfer come from curl's callbacks and vary with its chunking,
//!   so consecutive identical lines are collapsed, and the transfer's
//!   `stop`, which curl's thread races with the next activity, is placed
//!   after the transfer's last result. Error messages are
//!   compared without the `\e[35;1m` hint colouring CppNix puts around
//!   interpolated values, which noxi's error strings do not carry.
//! - `raw`, `raw-with-logs`, `bar`, `bar-with-logs` and the absent
//!   `multiline` formats, redirected: standard error and exit status,
//!   byte for byte.
//! - `bar` and `bar-with-logs` on a terminal, through `script` with an
//!   80-column pty: the committed lines and the cursor handling. The
//!   status line is redrawn by a timer thread in both engines, so its
//!   intermediate states are not compared.
//!
//! The reference executables come from `NOXI_REFERENCE_BIN` or
//! `target/nix-reference/bin`, and busybox from `NOXI_TEST_BUSYBOX` or
//! `target/test-busybox` (`just reference` creates both).

use std::collections::HashMap;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SYSTEM_FEATURES: &str = "benchmark big-parallel kvm nixos-test uid-range";
const ENGINES: [&str; 2] = ["reference", "noxi"];

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

const EXPR: &str = r#"
let
  bb = builtins.storePath @busybox@;
  mk = name: deps: script: derivation {
    inherit name deps;
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
}

impl Env {
	fn new() -> Self {
		let dir = noxi_test_support::tempdir().unwrap();
		for sub in ["conf", "home"] {
			fs::create_dir(dir.path().join(sub)).unwrap();
		}
		let busybox = fs::canonicalize(from_env_or_target(
			"NOXI_TEST_BUSYBOX",
			"test-busybox",
			"bin/busybox",
		))
		.unwrap()
		.to_string_lossy()
		.into_owned();
		let env = Env { dir, busybox };
		fs::write(
			env.dir.path().join("expr.nix"),
			EXPR.replace("@busybox@", &env.busybox),
		)
		.unwrap();
		for engine in ENGINES {
			let to = env.store(engine);
			let output = env.run(
				"reference",
				"nix",
				&["copy", "--no-check-sigs", "--to", &to, &env.busybox],
			);
			assert!(
				output.status.success(),
				"{}",
				String::from_utf8_lossy(&output.stderr)
			);
		}
		env
	}

	fn root(&self, engine: &str) -> PathBuf {
		self.dir.path().join(engine)
	}

	fn store(&self, engine: &str) -> String {
		format!("local?root={}", self.root(engine).display())
	}

	fn command(&self, engine: &str, program: &str) -> Command {
		let executable = match engine {
			"reference" => reference_bin().join(program),
			_ => PathBuf::from(match program {
				"nix-build" => env!("CARGO_BIN_EXE_nix-build"),
				"nix-store" => env!("CARGO_BIN_EXE_nix-store"),
				"nix" => env!("CARGO_BIN_EXE_nix"),
				_ => panic!("no noxi executable for {program}"),
			}),
		};
		let mut command = Command::new(executable);
		command
			.arg0(program)
			.current_dir(self.dir.path())
			.env_clear()
			.env("HOME", self.dir.path().join("home"))
			.env("NIX_CONF_DIR", self.dir.path().join("conf"))
			.env(
				"NIX_CONFIG",
				format!(
					"experimental-features = nix-command\nbuild-users-group =\nsubstituters =\nsandbox = true\nsandbox-fallback = false\nsandbox-paths =\ncores = 1\nsystem-features = {SYSTEM_FEATURES}\n"
				),
			)
			.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin");
		command
	}

	fn run(&self, engine: &str, program: &str, args: &[&str]) -> Output {
		self.command(engine, program).args(args).output().unwrap()
	}

	/// Instantiate attribute `attr` with the reference into `engine`'s store.
	fn instantiate(&self, engine: &str, attr: &str) -> String {
		let output = self.run(
			"reference",
			"nix-instantiate",
			&["--store", &self.store(engine), "expr.nix", "-A", attr],
		);
		assert!(
			output.status.success(),
			"{}",
			String::from_utf8_lossy(&output.stderr)
		);
		String::from_utf8(output.stdout).unwrap().trim().to_owned()
	}

	/// Run `program` with `args` in each engine's store; `{drv}` is replaced
	/// by `attr`'s derivation there. Returns each engine's stderr and status.
	fn both(&self, program: &str, attr: Option<&str>, args: &[&str]) -> Vec<(String, Option<i32>)> {
		ENGINES
			.iter()
			.map(|engine| {
				let drv = attr.map(|attr| self.instantiate(engine, attr));
				let store = self.store(engine);
				let mut full = vec!["--store", store.as_str()];
				for arg in args {
					full.push(match (*arg, &drv) {
						("{drv}", Some(drv)) => drv.as_str(),
						_ => arg,
					});
				}
				let output = self.run(engine, program, &full);
				(
					String::from_utf8_lossy(&output.stderr).into_owned(),
					output.status.code(),
				)
			})
			.collect()
	}

	/// Remove both stores' outputs by recreating them.
	fn reset(&self) {
		for engine in ENGINES {
			let root = self.root(engine);
			noxi_test_support::remove_tree(&root).unwrap();
			let output = self.run(
				"reference",
				"nix",
				&[
					"copy",
					"--no-check-sigs",
					"--to",
					&self.store(engine),
					&self.busybox,
				],
			);
			assert!(output.status.success());
		}
	}
}

/// The `@nix` lines of `stderr` with ids renumbered in order of first
/// appearance, hint colouring removed from messages and consecutive
/// identical lines collapsed.
fn normalise_json(stderr: &str, temp: &Path) -> Vec<String> {
	let mut ids: HashMap<String, usize> = HashMap::new();
	let mut lines: Vec<String> = Vec::new();
	let temp = temp.to_string_lossy();
	for line in stderr.lines() {
		assert!(
			line.starts_with("@nix {"),
			"not an internal-json line: {line}\n{stderr}"
		);
		let mut out = String::new();
		let mut rest = line;
		// Renumber `"id":N` and nonzero `"parent":N`.
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
		let mut out = out.replace(&*temp, "<tmp>");
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

fn assert_json_equal(env: &Env, case: &str, runs: &[(String, Option<i32>)]) {
	let reference = normalise_json(&runs[0].0, env.dir.path());
	let noxi = normalise_json(&runs[1].0, env.dir.path());
	assert_eq!(
		reference.join("\n"),
		noxi.join("\n"),
		"{case}: internal-json differs"
	);
	assert_eq!(runs[0].1, runs[1].1, "{case}: exit status differs");
	println!("{case}: {} lines match", reference.len());
}

#[test]
fn internal_json_builds_like_cppnix() {
	let env = Env::new();
	let json = ["--log-format", "internal-json"];
	let realise = |attr: &str, extra: &[&str]| {
		let mut args = vec!["-r", "{drv}"];
		args.extend_from_slice(&json);
		args.extend_from_slice(extra);
		env.both("nix-store", Some(attr), &args)
	};
	// A chain built from scratch, then again with valid outputs.
	assert_json_equal(&env, "fresh chain", &realise("b", &[]));
	assert_json_equal(&env, "valid chain", &realise("b", &[]));
	// The input already built, then build logs, phases and partial lines.
	env.reset();
	assert_json_equal(&env, "input built", &{
		realise("a", &[]);
		realise("b", &[])
	});
	assert_json_equal(&env, "log lines", &realise("logs", &[]));
	assert_json_equal(&env, "failure", &realise("fail", &[]));
	assert_json_equal(&env, "dependency failure", &realise("dependent", &[]));
	env.reset();
	// `-Q` after `--log-format` selects the raw logger again.
	let runs = realise("b", &["-Q"]);
	assert_eq!(runs[0], runs[1], "-Q after internal-json");
	env.reset();
	let runs = env.both(
		"nix-build",
		None,
		&[
			"expr.nix",
			"-A",
			"logs",
			"--no-out-link",
			"--log-format",
			"internal-json",
		],
	);
	assert_json_equal(&env, "nix-build", &runs);
}

#[test]
fn internal_json_substitutes_and_downloads_like_cppnix() {
	let env = Env::new();
	let drv = env.instantiate("reference", "b");
	let built = env.run(
		"reference",
		"nix-store",
		&["--store", &env.store("reference"), "-r", &drv],
	);
	assert!(built.status.success());
	let out = String::from_utf8(built.stdout).unwrap().trim().to_owned();
	let cache = format!("file://{}/cache?compression=none", env.dir.path().display());
	let copied = env.run(
		"reference",
		"nix",
		&[
			"copy",
			"--store",
			&env.store("reference"),
			"--to",
			&cache,
			&out,
		],
	);
	assert!(
		copied.status.success(),
		"{}",
		String::from_utf8_lossy(&copied.stderr)
	);
	env.reset();
	let runs = env.both(
		"nix-store",
		None,
		&[
			"-r",
			&out,
			"--option",
			"substituters",
			&cache,
			"--option",
			"require-sigs",
			"false",
			"--log-format",
			"internal-json",
		],
	);
	assert_json_equal(&env, "substitution", &runs);

	let data = env.dir.path().join("data.txt");
	fs::write(&data, "hello-data\n").unwrap();
	let url = format!("file://{}", data.display());
	let runs = env.both(
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
	assert_json_equal(&env, "download", &runs);
}

#[test]
fn redirected_log_formats_match_cppnix() {
	let env = Env::new();
	let formats = [
		None,
		Some("raw"),
		Some("raw-with-logs"),
		Some("bar"),
		Some("bar-with-logs"),
		Some("multiline"),
		Some("multiline-with-logs"),
	];
	for format in formats {
		for attr in ["logs", "fail"] {
			env.reset();
			let mut args = vec!["-r", "{drv}"];
			if let Some(format) = format {
				args.extend(["--log-format", format]);
			}
			let runs = env.both("nix-store", Some(attr), &args);
			assert_eq!(runs[0], runs[1], "nix-store {format:?} {attr}");
		}
	}
	// `nix` defaults to the bar; `-L` adds build logs.
	for logs in [false, true] {
		env.reset();
		let mut args = vec!["build", "-f", "expr.nix", "logs", "--no-link"];
		if logs {
			args.push("-L");
		}
		let runs = env.both("nix", None, &args);
		assert_eq!(runs[0], runs[1], "nix build -L={logs}");
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
fn terminal_bar_matches_cppnix() {
	if Command::new("script").arg("--version").output().is_err() {
		println!("SKIP: util-linux `script` is not available for a pty");
		return;
	}
	let env = Env::new();
	for format in ["bar", "bar-with-logs"] {
		for attr in ["logs", "fail"] {
			env.reset();
			let mut results = Vec::new();
			for engine in ENGINES {
				let drv = env.instantiate(engine, attr);
				let program = match engine {
					"reference" => reference_bin().join("nix-store"),
					_ => PathBuf::from(env!("CARGO_BIN_EXE_nix-store")),
				};
				let inner = format!(
					"stty cols 80 rows 24; {} --store '{}' -r {drv} --log-format {format} > /dev/null",
					program.display(),
					env.store(engine)
				);
				let command = env.command(engine, "nix-store");
				let output = Command::new("script")
					.args(["-qec", &inner, "/dev/null"])
					.current_dir(env.dir.path())
					.env_clear()
					.envs(command.get_envs().filter_map(|(k, v)| Some((k, v?))))
					.env("TERM", "xterm")
					.env("SHELL", "/bin/sh")
					.output()
					.unwrap();
				results.push(committed_lines(&String::from_utf8_lossy(&output.stdout)));
			}
			assert_eq!(results[0], results[1], "terminal {format} {attr}");
			assert!(
				results[1].1 && results[1].2,
				"cursor hidden and shown again"
			);
		}
	}
}
