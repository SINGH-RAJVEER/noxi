//! `nix-shell` checked against the pinned CppNix 2.35.2 reference.
//!
//! Each engine gets its own `local?root=` store seeded with the static
//! busybox, and the same `shell.nix` in a working directory at the same
//! depth. Both run with `NIX_BUILD_SHELL` set to the host's `bash`, so no
//! `<nixpkgs>` is needed, and standard output, standard error and the exit
//! status must match once the engine directories and the random temporary
//! directory names are normalised.
//!
//! The reference executables come from `NOXI_REFERENCE_BIN` or
//! `target/nix-reference/bin`, and busybox from `NOXI_TEST_BUSYBOX` or
//! `target/test-busybox` (`just reference` creates both).

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const PATH: &str = "/run/current-system/sw/bin:/usr/bin:/bin";
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

fn host_bash() -> PathBuf {
	PATH.split(':')
		.map(|dir| Path::new(dir).join("bash"))
		.find(|bash| bash.exists())
		.expect("bash on the test PATH")
}

struct Fixture {
	dir: tempfile::TempDir,
	reference: PathBuf,
}

impl Fixture {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let reference = from_env_or_target("NOXI_REFERENCE_BIN", "nix-reference/bin", "nix-shell");
		let busybox = fs::canonicalize(from_env_or_target(
			"NOXI_TEST_BUSYBOX",
			"test-busybox",
			"bin/busybox",
		))
		.unwrap();
		for sub in ["conf", "home"] {
			fs::create_dir(dir.path().join(sub)).unwrap();
		}
		let fixture = Fixture { dir, reference };
		for engine in ENGINES {
			for sub in ["work", "tmp"] {
				fs::create_dir_all(fixture.engine(engine).join(sub)).unwrap();
			}
			let copy = Command::new(fixture.reference.join("nix"))
				.args(["copy", "--no-check-sigs", "--to", &fixture.store(engine)])
				.arg(&busybox)
				.env_clear()
				.env("HOME", fixture.dir.path().join("home"))
				.env("NIX_CONF_DIR", fixture.dir.path().join("conf"))
				.env("NIX_CONFIG", "experimental-features = nix-command")
				.env("PATH", PATH)
				.output()
				.unwrap();
			assert!(
				copy.status.success(),
				"{}",
				String::from_utf8_lossy(&copy.stderr)
			);
			fs::write(
				fixture.engine(engine).join("work/shell.nix"),
				format!(
					r#"let
	bb = builtins.storePath {busybox};
	dep = derivation {{ name = "dep"; system = "x86_64-linux"; builder = "${{bb}}/bin/sh"; args = [ "-c" "echo dep > $out" ]; }};
in {{ inNixShell ? false, extra ? "none" }}: derivation {{
	name = "shell-test";
	system = "x86_64-linux";
	builder = "${{bb}}/bin/sh";
	args = [ "-c" "echo > $out" ];
	inherit dep extra;
	foo = "bar baz";
	passAsFile = [ "big" ];
	big = "contents";
	isShell = if inNixShell then "yes" else "no";
}}
"#,
					busybox = busybox.display()
				),
			)
			.unwrap();
			fs::write(
				fixture.engine(engine).join("work/structured.nix"),
				format!(
					r#"let bb = builtins.storePath {busybox}; in derivation {{
	name = "structured";
	system = "x86_64-linux";
	builder = "${{bb}}/bin/sh";
	args = [ "-c" "echo > $out" ];
	__structuredAttrs = true;
	outputs = [ "out" "doc" ];
	foo = "bar";
	num = 3;
	list = [ "a" "b" ];
	attrs = {{ x = "y"; }};
	outputChecks.out.allowedReferences = [ ];
}}
"#,
					busybox = busybox.display()
				),
			)
			.unwrap();
		}
		fixture
	}

	fn engine(&self, engine: &str) -> PathBuf {
		self.dir.path().join(engine)
	}

	fn store(&self, engine: &str) -> String {
		format!("local?root={}", self.engine(engine).join("root").display())
	}

	fn run(&self, engine: &str, args: &[&str], cwd: &str) -> (Option<i32>, String, String) {
		let program = match engine {
			"reference" => self.reference.join("nix-shell"),
			_ => PathBuf::from(env!("CARGO_BIN_EXE_nix-shell")),
		};
		let output = Command::new(program)
			.arg0("nix-shell")
			.args(["--store", &self.store(engine)])
			.args(args)
			.current_dir(self.engine(engine).join(cwd))
			.env_clear()
			.env("HOME", self.dir.path().join("home"))
			.env("NIX_CONF_DIR", self.dir.path().join("conf"))
			.env("NIX_BUILD_SHELL", host_bash())
			.env("TMPDIR", self.engine(engine).join("tmp"))
			.env("PATH", PATH)
			.env(
				"NIX_CONFIG",
				"experimental-features = nix-command\nbuild-users-group =\nsubstituters =\nsandbox = true\nsandbox-fallback = false\ncores = 1\n",
			)
			.output()
			.unwrap();
		let normalise = |bytes: &[u8]| {
			let text = String::from_utf8_lossy(bytes)
				.replace(&*self.engine(engine).to_string_lossy(), "ENGINE");
			normalise_temp_names(&text)
		};
		(
			output.status.code(),
			normalise(&output.stdout),
			normalise(&output.stderr),
		)
	}

	/// Run `args` with each engine, from a fresh copy of the stores' state
	/// for each, and compare.
	fn same(&self, args: &[&str]) {
		self.same_in(args, "work");
	}

	fn same_in(&self, args: &[&str], cwd: &str) {
		let expected = self.run("reference", args, cwd);
		let actual = self.run("noxi", args, cwd);
		assert_eq!(actual, expected, "nix-shell {args:?}");
	}
}

/// Replace `nix-shell-PID-COUNTER` temporary directory names.
fn normalise_temp_names(text: &str) -> String {
	let mut out = String::new();
	let mut rest = text;
	while let Some(i) = rest.find("nix-shell-") {
		out.push_str(&rest[..i + "nix-shell-".len()]);
		rest = &rest[i + "nix-shell-".len()..];
		let digits = rest
			.char_indices()
			.find(|(_, c)| !c.is_ascii_digit() && *c != '-')
			.map_or(rest.len(), |(i, _)| i);
		if digits > 0 {
			out.push_str("TMP");
			rest = &rest[digits..];
		}
	}
	out.push_str(rest);
	out
}

#[test]
fn builds_inputs_and_runs_commands_in_the_derivation_environment() {
	let f = Fixture::new();
	// The first run builds `dep` and prints the missing-path summary.
	f.same(&[
		"--run",
		"echo foo=$foo isShell=$isShell extra=$extra dep=$dep out=$out; cat $bigPath; echo; echo $IN_NIX_SHELL $NIX_BUILD_CORES $NIX_STORE $TMPDIR $NIX_BUILD_TOP",
	]);
	f.same(&[
		"--run",
		"env | sort | grep -v -e ^_= -e ^SHLVL= -e ^PWD= -e ^OLDPWD=",
	]);
	f.same(&[
		"--pure",
		"--run",
		"env | sort | grep -v -e ^_= -e ^PWD= -e ^OLDPWD=",
	]);
	f.same(&["--pure", "--keep", "FOO", "--run", "echo $HOME"]);
	f.same(&["--argstr", "extra", "hello", "--run", "echo $extra"]);
	f.same(&["--arg", "extra", "1 + 2", "--run", "echo $extra"]);
	f.same(&["--run", "echo $SHELL $BASH; echo \"$PATH\""]);
	f.same(&["--run", "exit 7"]);
	f.same(&["--command", "echo cmd; exit 3"]);
	f.same(&["--exclude", "dep", "--run", "echo $foo"]);
	f.same(&["--dry-run"]);
	f.same(&[".", "--run", "echo $isShell"]);
	f.same(&[
		"-E",
		r#"derivation { name = "x"; system = "x86_64-linux"; builder = "/bin/sh"; v = 1; }"#,
		"--run",
		"echo $v $name",
	]);
}

#[test]
fn reports_argument_errors_like_cppnix() {
	let f = Fixture::new();
	f.same(&["-p", "hello", "-E", "x"]);
	f.same(&["-E", "{}", "--run", "true"]);
	f.same(&["shell.nix", "shell.nix", "--run", "true"]);
	f.same(&["/nonexistent", "--run", "true"]);
	f.same(&["--version"]);
	// No argument and neither shell.nix nor default.nix.
	f.same_in(&["--run", "true"], "tmp");
}

#[test]
fn unsupported_modes_fail_explicitly() {
	let f = Fixture::new();
	let (status, _, stderr) = f.run("noxi", &["--check"], "work");
	assert_eq!(status, Some(1));
	assert!(stderr.contains("is not supported by noxi yet"), "{stderr}");
	// The default shell would come from a `flake:` `<nixpkgs>`, which noxi
	// skips and the reference resolves.
	let output = Command::new(env!("CARGO_BIN_EXE_nix-shell"))
		.arg0("nix-shell")
		.args(["--store", &f.store("noxi"), "--run", "true"])
		.current_dir(f.engine("noxi").join("work"))
		.env_clear()
		.env("HOME", f.dir.path().join("home"))
		.env("NIX_CONF_DIR", f.dir.path().join("conf"))
		.env("NIX_PATH", "nixpkgs=flake:nixpkgs")
		.env("PATH", PATH)
		.env(
			"NIX_CONFIG",
			"experimental-features = nix-command\nbuild-users-group =\nsubstituters =\nsandbox = true\nsandbox-fallback = false\n",
		)
		.output()
		.unwrap();
	assert_eq!(output.status.code(), Some(1));
	assert!(
		String::from_utf8_lossy(&output.stderr).contains("is not supported by noxi yet"),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
}

#[test]
fn passes_structured_attributes_as_files() {
	let f = Fixture::new();
	f.same(&[
		"structured.nix",
		"--run",
		"cat $NIX_ATTRS_JSON_FILE; echo; cat $NIX_ATTRS_SH_FILE; echo; declare -p outputs list attrs foo num; echo \"${__json-unset}\"",
	]);
	f.same(&[
		"structured.nix",
		"--pure",
		"--run",
		"env | sort | grep -v -e ^_= -e ^PWD= -e ^OLDPWD=",
	]);
}
