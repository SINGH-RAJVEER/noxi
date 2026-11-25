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
	dir: noxi_test_support::TempDir,
	reference: PathBuf,
}

impl Fixture {
	fn new() -> Self {
		let dir = noxi_test_support::tempdir().unwrap();
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
		self.run_with(engine, args, cwd, None, false)
	}

	/// With `nix_path`, use the default shell instead of `NIX_BUILD_SHELL`.
	fn run_with(
		&self,
		engine: &str,
		args: &[&str],
		cwd: &str,
		nix_path: Option<&str>,
		script: bool,
	) -> (Option<i32>, String, String) {
		let program = match engine {
			"reference" => self.reference.join("nix-shell"),
			_ => PathBuf::from(env!("CARGO_BIN_EXE_nix-shell")),
		};
		let mut command = Command::new(program);
		command.arg0("nix-shell");
		// A script must be the first argument.
		if !script {
			command.args(["--store", &self.store(engine)]);
		}
		command
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
			);
		if script {
			command
				.env("NIX_REMOTE", self.store(engine))
				.env("KEPT", "k");
		}
		if let Some(nix_path) = nix_path {
			command
				.env_remove("NIX_BUILD_SHELL")
				.env("NIX_PATH", nix_path);
		}
		let output = command.output().unwrap();
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

	/// Compare a script's run; the store comes from the environment.
	fn same_script(&self, args: &[&str], cwd: &str) {
		let expected = self.run_with("reference", args, cwd, None, true);
		let actual = self.run_with("noxi", args, cwd, None, true);
		assert_eq!(actual, expected, "nix-shell {args:?} in {cwd}");
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
}

/// Without `NIX_BUILD_SHELL` the shell is `<nixpkgs>`'s `bashInteractive`,
/// and `<nixpkgs>` may come from the flake registry.
#[test]
fn finds_the_default_shell_through_a_flake_entry() {
	let f = Fixture::new();
	let busybox = fs::canonicalize(from_env_or_target(
		"NOXI_TEST_BUSYBOX",
		"test-busybox",
		"bin/busybox",
	))
	.unwrap();
	let nixpkgs = f.dir.path().join("nixpkgs");
	fs::create_dir(&nixpkgs).unwrap();
	fs::write(
		nixpkgs.join("default.nix"),
		format!(
			r#"{{ }}: let bb = builtins.storePath {}; in {{
	bashInteractive = derivation {{
		name = "test-bash";
		system = "x86_64-linux";
		builder = "${{bb}}/bin/sh";
		args = [ "-c" "${{bb}}/bin/mkdir -p $out/bin; echo 'exec sh' > $out/bin/bash" ];
	}};
}}
"#,
			busybox.display()
		),
	)
	.unwrap();
	let registry = f.dir.path().join("registry.json");
	fs::write(
		&registry,
		serde_json::json!({ "version": 2, "flakes": [{
			"from": { "type": "indirect", "id": "nixpkgs" },
			"to": { "type": "path", "path": nixpkgs },
		}] })
		.to_string(),
	)
	.unwrap();
	let registry = registry.to_str().unwrap();
	for nix_path in ["nixpkgs=flake:nixpkgs", "nixpkgs=flake:unknown"] {
		let args = [
			"--option",
			"extra-experimental-features",
			"flakes",
			"--option",
			"flake-registry",
			registry,
			"--run",
			"true",
		];
		let [expected, actual] =
			ENGINES.map(|engine| f.run_with(engine, &args, "work", Some(nix_path), false));
		assert_eq!(actual, expected, "{nix_path}");
	}
	// The shell that the registry's `<nixpkgs>` names was built.
	let store = f.engine("noxi").join("root/nix/store");
	assert!(fs::read_dir(store).unwrap().any(|entry| {
		entry
			.unwrap()
			.file_name()
			.to_string_lossy()
			.ends_with("-test-bash")
	}));
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

/// Scripts run through `nix-shell SCRIPT ARGS`, as the kernel does for a
/// script whose interpreter is `nix-shell`.
#[test]
fn runs_shebang_scripts() {
	let f = Fixture::new();
	// Prints its name, arguments and the derivation's environment.
	let body = "echo \"$(basename \"$0\") $# $* isShell=$isShell extra=$extra\"; cat </dev/null\n";
	let scripts = [
		("plain", "#! /usr/bin/env nix-shell\n#! nix-shell -i bash\n"),
		(
			"args",
			"#!/usr/bin/env nix-shell\n#!nix-shell shell.nix --argstr extra 'a \"b\" c'\n#!  \tnix-shell   -i 'bash -eu'  \n",
		),
		(
			"late",
			"#! /usr/bin/env nix-shell\n# a comment\n#! nix-shell-not --bad\n#!nix-shell\necho between\n#! nix-shell -i bash\n",
		),
		(
			"expr",
			"#! /usr/bin/env nix-shell\n#! nix-shell -i bash --expr \"import ./shell.nix { inNixShell = true; extra = toString ./x; }\"\n",
		),
		(
			"arg",
			"#! /usr/bin/env nix-shell\n#! nix-shell -i bash --arg extra \"toString ./x\"\n",
		),
		(
			"legacy",
			"#! /usr/bin/env nix-shell\n#! nix-shell -i bash --arg extra \"toString ./x\" --option nix-shell-shebang-arguments-relative-to-script false\n",
		),
		(
			"perl",
			"#! /usr/bin/env nix-shell\n#! nix-shell -i ../bin/myperl\n",
		),
		(
			"ruby",
			"#! /usr/bin/env nix-shell\n#! nix-shell -i ../bin/myruby\n",
		),
		(
			"run",
			"#! /usr/bin/env nix-shell\n#! nix-shell --run 'echo ran'\n",
		),
		(
			"pure",
			"#! /usr/bin/env nix-shell\n#! nix-shell --pure --keep KEPT -i bash\n",
		),
		(
			"missing",
			"#! /usr/bin/env nix-shell\n#! nix-shell absent.nix -i bash\n",
		),
		(
			"unterminated",
			"#! /usr/bin/env nix-shell\n#! nix-shell -i 'bash\n",
		),
		(
			"unknown",
			"#! /usr/bin/env nix-shell\n#! nix-shell --no-such-flag\n",
		),
		(
			"interpreter",
			"#! /usr/bin/env nix-shell\n#! nix-shell -i\n",
		),
		(
			"two",
			"#! /usr/bin/env nix-shell\n#! nix-shell shell.nix shell.nix -i bash\n",
		),
		(
			"it's \"odd\" \\$x",
			"#! /usr/bin/env nix-shell\n#! nix-shell -i bash\n",
		),
		(
			"blank",
			"\n\n#! /usr/bin/env nix-shell\n#! nix-shell -i bash\n",
		),
		(
			"crlf",
			"#! /usr/bin/env nix-shell\r\n#! nix-shell -i bash\r\n",
		),
	];
	for engine in ENGINES {
		let bin = f.engine(engine).join("bin");
		fs::create_dir(&bin).unwrap();
		for name in ["myperl", "myruby"] {
			let program = bin.join(name);
			fs::write(
				&program,
				format!("#!{}\necho \"$0 $*\"\n", host_bash().display()),
			)
			.unwrap();
			let chmod = Command::new("chmod").arg("+x").arg(&program).status();
			assert!(chmod.unwrap().success());
		}
		for (name, text) in scripts {
			fs::write(
				f.engine(engine).join("work").join(name),
				format!("{text}{body}"),
			)
			.unwrap();
		}
		fs::create_dir(f.engine(engine).join("work/dir")).unwrap();
	}
	for (name, _) in scripts {
		for (cwd, path) in [
			("work", name.to_owned()),
			("tmp", format!("../work/{name}")),
		] {
			// Without `shell.nix` in the working directory the default file
			// name is refused before the script's directory is considered.
			for args in [&[][..], &["one", "two words", "it's", ""]] {
				let mut command = vec![path.as_str()];
				command.extend(args);
				f.same_script(&command, cwd);
			}
		}
	}
	// A directory is not a script, and `-i` is only for scripts.
	f.same(&["dir"]);
	f.same(&["shell.nix", "-i", "bash"]);
	f.same_script(&["absent"], "work");
}

/// `nix-shell-always-looks-for-shell-nix`, and lookup paths of directories.
#[test]
fn looks_for_shell_nix_as_configured() {
	let f = Fixture::new();
	for engine in ENGINES {
		let work = f.engine(engine).join("work");
		fs::create_dir_all(work.join("both")).unwrap();
		fs::create_dir_all(work.join("empty")).unwrap();
		fs::copy(work.join("shell.nix"), work.join("both/shell.nix")).unwrap();
		fs::write(work.join("both/default.nix"), "abort \"default.nix\"").unwrap();
	}
	let off = ["--option", "nix-shell-always-looks-for-shell-nix", "false"];
	for target in ["both", "<here/both>", "<here/empty>", "empty", "."] {
		let args = ["-I", "here=.", target, "--run", "echo $isShell"];
		f.same(&args);
		f.same(&[&args[..], &off].concat());
	}
	f.same(&[&["--run", "echo $isShell"][..], &off].concat());
}
