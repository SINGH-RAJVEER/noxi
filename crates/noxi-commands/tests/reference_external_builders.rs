//! `external-builders` compared with CppNix 2.35.2. A program configured
//! for a derivation's system builds it instead of the local builder, before
//! the platform is checked: outside the sandbox, with the caller's
//! environment, in the build directory, with the build described in
//! `build.json`. Every other build keeps the sandbox, the program never runs
//! without the `external-builders` feature, and an untrusted daemon client
//! cannot set it. Every store is a scratch store under a private temporary
//! directory, and builds run unprivileged.
//!
//! The reference comes from `NOXI_REFERENCE_BIN`, else
//! `target/nix-reference/bin`, and busybox from `NOXI_TEST_BUSYBOX`, else
//! `target/test-busybox` (`just reference` creates both).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const SYSTEM: &str = "x68_46-xunil";
const CONFIG: &str = "sandbox = true\nsandbox-paths =\nbuild-users-group =\nsubstituters =\n";

fn reference_bin() -> PathBuf {
	if let Some(path) = std::env::var_os("NOXI_REFERENCE_BIN") {
		return PathBuf::from(path);
	}
	let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin");
	assert!(
		path.join("nix-build").exists(),
		"the reference is missing; run `just reference` or set NOXI_REFERENCE_BIN"
	);
	path
}

fn busybox() -> PathBuf {
	fs::canonicalize(
		std::env::var_os("NOXI_TEST_BUSYBOX")
			.map(PathBuf::from)
			.unwrap_or_else(|| {
				Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-busybox")
			}),
	)
	.unwrap()
}

fn program(engine: &str, name: &str) -> PathBuf {
	match engine {
		"reference" => reference_bin().join(name),
		_ => PathBuf::from(match name {
			"nix-build" => env!("CARGO_BIN_EXE_nix-build"),
			"nix" => env!("CARGO_BIN_EXE_nix"),
			"nix-store" => env!("CARGO_BIN_EXE_nix-store"),
			_ => panic!("no noxi executable for {name}"),
		}),
	}
}

/// One engine's scratch directory below `outer`, which both engines share:
/// a store, the external builder and what it leaves behind.
struct Scene {
	outer: PathBuf,
	dir: PathBuf,
	engine: &'static str,
}

impl Scene {
	fn new(engine: &'static str, outer: &Path) -> Self {
		let dir = outer.join(engine);
		fs::create_dir(&dir).unwrap();
		let busybox = busybox().join("bin/busybox");
		let marks = dir.join("marks");
		fs::create_dir(&marks).unwrap();
		// The program records how it was run, reports that it started, and
		// writes the output at its real location.
		let script = format!(
			r#"#! {bb} sh
set -e
{bb} cp "$2" {marks}/build.json
echo "argv0=$0 args=$* pwd=$(pwd) FOO=${{FOO-unset}} " > {marks}/info
printf '\2\n'
echo "external builder output"
real=$({bb} sed -e 's/.*"realStoreDir":"\([^"]*\)".*/\1/' "$2")
out=$({bb} sed -e 's/.*"outputs":{{"out":"\([^"]*\)".*/\1/' "$2")
printf foobar > "$real/${{out##*/}}"
"#,
			bb = busybox.display(),
			marks = marks.display(),
		);
		let builder = dir.join("external-builder");
		fs::write(&builder, script).unwrap();
		fs::set_permissions(&builder, fs::Permissions::from_mode(0o755)).unwrap();
		Self {
			outer: outer.to_owned(),
			dir,
			engine,
		}
	}

	fn path(&self, name: &str) -> PathBuf {
		self.dir.join(name)
	}

	/// `external-builders` naming this scene's program for `system`.
	fn builders(&self, system: &str) -> String {
		format!(
			r#"[{{"systems": ["{system}"], "args": ["bla"], "program": "{}"}}]"#,
			self.path("external-builder").display()
		)
	}

	/// A derivation for `system` whose builder, when it runs, reports where
	/// it runs and whether the scenes' directory is visible; it is the same
	/// derivation for both engines.
	fn expression(&self, system: &str) -> String {
		format!(
			r#"let bb = builtins.path {{ path = "{}"; name = "bb"; }}; in derivation {{
				name = "external";
				system = "{system}";
				builder = "${{bb}}/bin/busybox";
				args = [ "sh" "-c" ''
					{{ pwd; if [ -e {} ]; then echo visible; else echo hidden; fi; }} > $out
				'' ];
				extra = "a  b";
				passAsFile = [ "extra" ];
			}}"#,
			busybox().display(),
			self.outer.display(),
		)
	}

	fn nix_build(&self, store: &str, system: &str, features: &str, builders: &str) -> Output {
		Command::new(program(self.engine, "nix-build"))
			.env_clear()
			.env("HOME", self.path("home"))
			.env("NIX_CONF_DIR", self.path("none"))
			.env("NIX_CONFIG", CONFIG)
			.env("FOO", "inherited")
			.args(["--store", store, "--no-out-link"])
			.args(["--option", "experimental-features", features])
			.args(["--option", "external-builders", builders])
			.args(["-E", &self.expression(system)])
			.output()
			.unwrap()
	}

	fn local_store(&self) -> String {
		format!("local?root={}", self.path("root").display())
	}

	/// A file the program left, with this scene's paths replaced.
	fn mark(&self, name: &str) -> Option<String> {
		fs::read_to_string(self.path("marks").join(name))
			.ok()
			.map(|text| self.normalise(&text))
	}

	/// The contents of the output `stdout` names, read from the store.
	fn output(&self, stdout: &[u8]) -> String {
		let path = String::from_utf8_lossy(stdout).trim().to_owned();
		fs::read_to_string(self.path("root").join(path.trim_start_matches('/'))).unwrap()
	}

	fn normalise(&self, text: &str) -> String {
		let mut text = text.replace(&self.dir.display().to_string(), "DIR");
		text = text.replace(&reference_bin().display().to_string(), "BIN");
		// Temporary build directories, `nix-PID-RANDOM`.
		let mut out = String::new();
		let mut rest = text.as_str();
		while let Some(at) = rest.find("/nix-") {
			out.push_str(&rest[..at + 5]);
			rest = &rest[at + 5..];
			let end = rest
				.find(|c: char| !(c.is_ascii_digit() || c == '-'))
				.unwrap_or(rest.len());
			if end > 0 {
				out.push_str("TMP");
			}
			rest = &rest[end..];
		}
		out.push_str(rest);
		out
	}

	fn messages(&self, output: &Output) -> String {
		self.normalise(&String::from_utf8_lossy(&output.stderr))
			.lines()
			.filter(|line| !line.starts_with("warning: you don't have Internet access"))
			.collect::<Vec<_>>()
			.join("\n")
	}
}

#[test]
fn external_builders_build_their_systems_like_cppnix() {
	let outer = noxi_test_support::tempdir().unwrap();
	let mut runs = Vec::new();
	for engine in ["reference", "noxi"] {
		let scene = Scene::new(engine, outer.path());
		let output = scene.nix_build(
			&scene.local_store(),
			SYSTEM,
			"nix-command external-builders",
			&scene.builders(SYSTEM),
		);
		assert!(
			output.status.success(),
			"{engine}: {}",
			scene.messages(&output)
		);
		assert_eq!(scene.output(&output.stdout), "foobar", "{engine}");
		runs.push((
			scene.messages(&output),
			scene.mark("info"),
			scene.mark("build.json"),
		));
	}
	let (messages, info, json) = &runs[1];
	assert!(
		messages
			.lines()
			.any(|line| line == "external builder output"),
		"{messages}"
	);
	let info = info.as_deref().unwrap();
	// Run outside the sandbox, in the build directory on the host, with
	// the caller's environment.
	assert!(
		info.starts_with("argv0=DIR/external-builder args=bla DIR/root/nix/var/nix/builds/nix-TMP/build.json pwd=DIR/root/nix/var/nix/builds/nix-TMP/build FOO=inherited "),
		"{info}"
	);
	let json = json.as_deref().unwrap();
	assert!(json.contains(r#""tmpDirInSandbox":"/build""#), "{json}");
	assert!(json.contains(r#""NIX_BUILD_TOP":"/build""#), "{json}");
	assert_eq!(runs[0], runs[1]);
}

#[test]
fn external_builders_need_their_feature_like_cppnix() {
	let outer = noxi_test_support::tempdir().unwrap();
	let mut runs = Vec::new();
	for engine in ["reference", "noxi"] {
		let scene = Scene::new(engine, outer.path());
		let output = scene.nix_build(
			&scene.local_store(),
			SYSTEM,
			"nix-command",
			&scene.builders(SYSTEM),
		);
		assert!(!output.status.success(), "{engine}");
		// The program never ran.
		assert_eq!(scene.mark("info"), None, "{engine}");
		runs.push((output.status.code(), scene.messages(&output)));
	}
	assert!(
		runs[1]
			.1
			.contains("experimental Nix feature 'external-builders' is disabled"),
		"{}",
		runs[1].1
	);
	assert_eq!(runs[0], runs[1]);
}

#[test]
fn builds_for_other_systems_stay_sandboxed_like_cppnix() {
	let outer = noxi_test_support::tempdir().unwrap();
	let mut runs = Vec::new();
	for engine in ["reference", "noxi"] {
		let scene = Scene::new(engine, outer.path());
		let output = scene.nix_build(
			&scene.local_store(),
			"x86_64-linux",
			"nix-command external-builders",
			&scene.builders(SYSTEM),
		);
		assert!(
			output.status.success(),
			"{engine}: {}",
			scene.messages(&output)
		);
		assert_eq!(scene.mark("info"), None, "{engine}");
		runs.push(scene.output(&output.stdout));
	}
	// In the sandbox's build directory, without the host's files.
	assert_eq!(runs[1], "/build\nhidden\n");
	assert_eq!(runs[0], runs[1]);
}

struct Daemon(Child);

impl Drop for Daemon {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

#[test]
fn untrusted_daemon_clients_cannot_set_external_builders() {
	let outer = noxi_test_support::tempdir().unwrap();
	let mut runs = Vec::new();
	for engine in ["reference", "noxi"] {
		let scene = Scene::new(engine, outer.path());
		let socket = scene.path("socket");
		// The daemon serves a store that exists.
		let init = Command::new(program(engine, "nix-store"))
			.env_clear()
			.env("HOME", scene.path("home"))
			.env("NIX_CONF_DIR", scene.path("none"))
			.args(["--store", &scene.local_store(), "--init"])
			.output()
			.unwrap();
		assert!(init.status.success(), "{engine}: {init:?}");
		let daemon = Command::new(program(engine, "nix"))
			.env_clear()
			.env("HOME", scene.path("home"))
			.env("NIX_CONF_DIR", scene.path("none"))
			.env("NIX_CONFIG", CONFIG)
			.env("NIX_DAEMON_SOCKET_PATH", &socket)
			.args([
				"--extra-experimental-features",
				"nix-command daemon-trust-override external-builders",
				"daemon",
				"--force-untrusted",
				"--store",
				&scene.local_store(),
			])
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()
			.unwrap();
		let _daemon = Daemon(daemon);
		let started = Instant::now();
		while !socket.exists() {
			assert!(started.elapsed() < Duration::from_secs(30), "{engine}");
			std::thread::sleep(Duration::from_millis(20));
		}
		let output = scene.nix_build(
			&format!(
				"unix://{}?root={}",
				socket.display(),
				scene.path("root").display()
			),
			SYSTEM,
			"nix-command external-builders",
			&scene.builders(SYSTEM),
		);
		assert!(!output.status.success(), "{engine}");
		// The daemon ignored the setting, so its program never ran.
		assert_eq!(scene.mark("info"), None, "{engine}");
		runs.push((output.status.code(), scene.messages(&output)));
	}
	assert!(
		runs[1]
			.1
			.contains("ignoring the client-specified setting 'external-builders', because it is a restricted setting and you are not a trusted user"),
		"{}",
		runs[1].1
	);
	assert_eq!(runs[0], runs[1]);
}
