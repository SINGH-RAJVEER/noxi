//! `nix develop` and `nix print-dev-env` checked against the pinned CppNix
//! 2.35.2 reference.
//!
//! The commands need `bash` as the derivation's builder, so each engine's
//! `local?root=` store gets the host's `bash` closure as well as the static
//! busybox. Each engine runs in its own store and working directory at the
//! same depth; standard output, standard error and the exit status must
//! match once the engine directories and random temporary names are
//! normalised. Commands run in the store's mount namespace like the
//! reference's; on a host that refuses one, both engines must report it.
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

/// The store path of the host's `bash` package.
fn bash_package() -> PathBuf {
	let bash = PATH
		.split(':')
		.map(|dir| Path::new(dir).join("bash"))
		.find(|bash| bash.exists())
		.expect("bash on the test PATH");
	let bash = fs::canonicalize(bash).unwrap();
	assert!(
		bash.starts_with("/nix/store"),
		"{} is not in /nix/store",
		bash.display()
	);
	bash.parent().unwrap().parent().unwrap().to_owned()
}

/// Replace the random parts of temporary names: `PREFIX-PID-COUNTER` and
/// `nix-shell.XXXXXX`.
fn normalise_temp_names(text: &str) -> String {
	let mut out = String::new();
	let mut rest = text;
	while let Some(i) = rest.find("nix-") {
		out.push_str(&rest[..i]);
		rest = &rest[i..];
		let word_end = rest
			.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '.'))
			.unwrap_or(rest.len());
		let word = &rest[..word_end];
		if let Some(prefix) = ["nix-shell.", "nix-develop-", "nix-dev-env-", "nix-shell-"]
			.into_iter()
			.find(|prefix| word.starts_with(prefix) && word.len() > prefix.len())
		{
			out.push_str(prefix);
			out.push_str("TMP");
		} else {
			out.push_str(word);
		}
		rest = &rest[word_end..];
	}
	out.push_str(rest);
	out
}

struct Fixture {
	dir: tempfile::TempDir,
	reference: PathBuf,
}

impl Fixture {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		let reference = from_env_or_target("NOXI_REFERENCE_BIN", "nix-reference/bin", "nix");
		let busybox = fs::canonicalize(from_env_or_target(
			"NOXI_TEST_BUSYBOX",
			"test-busybox",
			"bin/busybox",
		))
		.unwrap();
		let bash = bash_package();
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
				.arg(&bash)
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
				fixture.engine(engine).join("work/dev.nix"),
				format!(
					r#"let
	bash = builtins.storePath {bash};
	bb = builtins.storePath {busybox};
	dep = derivation {{ name = "dep"; system = "x86_64-linux"; builder = "${{bb}}/bin/sh"; args = [ "-c" "echo dep > $out" ]; }};
in derivation {{
	name = "dev";
	system = "x86_64-linux";
	builder = "${{bash}}/bin/bash";
	args = [ "-c" "echo > $out" ];
	outputs = [ "out" "dev" ];
	inherit dep;
	foo = "bar baz";
	shellHook = "echo hook-ran $foo";
	allowedReferences = [ ];
}}
"#,
					bash = bash.display(),
					busybox = busybox.display()
				),
			)
			.unwrap();
			fs::write(
				fixture.engine(engine).join("work/pkg.nix"),
				format!(
					r#"let bb = builtins.storePath {busybox}; in derivation {{
	name = "hello";
	system = "x86_64-linux";
	builder = "${{bb}}/bin/sh";
	args = [ "-c" "${{bb}}/bin/mkdir -p $out/bin; printf '#!${{bb}}/bin/sh\necho hello \"$@\"\nexit 5\n' > $out/bin/hello; ${{bb}}/bin/chmod +x $out/bin/hello" ];
}}
"#,
					busybox = busybox.display()
				),
			)
			.unwrap();
			fs::write(
				fixture.engine(engine).join("work/logs.nix"),
				format!(
					r#"{{ name }}: let bb = builtins.storePath {busybox}; in derivation {{
	inherit name;
	system = "x86_64-linux";
	builder = "${{bb}}/bin/sh";
	args = [ "-c" "echo to-stdout; echo to-stderr >&2; echo > $out" ];
}}
"#,
					busybox = busybox.display()
				),
			)
			.unwrap();
			fs::write(
				fixture.engine(engine).join("work/structured.nix"),
				format!(
					r#"let bash = builtins.storePath {bash}; in derivation {{
	name = "structured";
	system = "x86_64-linux";
	builder = "${{bash}}/bin/bash";
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
					bash = bash.display()
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

	fn run(&self, engine: &str, args: &[&str]) -> (Option<i32>, String, String) {
		let program = match engine {
			"reference" => self.reference.join("nix"),
			_ => PathBuf::from(env!("CARGO_BIN_EXE_nix")),
		};
		let output = Command::new(program)
			.arg0("nix")
			.args(["--store", &self.store(engine)])
			.args(args)
			.current_dir(self.engine(engine).join("work"))
			.env_clear()
			.env("HOME", self.dir.path().join("home"))
			.env("NIX_CONF_DIR", self.dir.path().join("conf"))
			.env("TMPDIR", self.engine(engine).join("tmp"))
			.env("PATH", PATH)
			.env(
				"NIX_CONFIG",
				"experimental-features = nix-command\nbuild-users-group =\nsubstituters =\nsandbox = true\nsandbox-fallback = false\ncores = 1\n",
			)
			.output()
			.unwrap();
		let normalise = |bytes: &[u8]| {
			normalise_temp_names(
				&String::from_utf8_lossy(bytes)
					.replace(&*self.engine(engine).to_string_lossy(), "ENGINE"),
			)
		};
		(
			output.status.code(),
			normalise(&output.stdout),
			normalise(&output.stderr),
		)
	}

	fn same(&self, args: &[&str]) -> (Option<i32>, String, String) {
		let expected = self.run("reference", args);
		let actual = self.run("noxi", args);
		assert_eq!(actual, expected, "nix {args:?}");
		expected
	}
}

impl Drop for Fixture {
	/// Store objects are read-only; remove them before the directory goes.
	fn drop(&mut self) {
		for engine in ENGINES {
			let _ = noxi_store::delete_path(&self.engine(engine).join("root"));
		}
	}
}

#[test]
fn print_dev_env_matches_reference() {
	let f = Fixture::new();
	// The first run builds the input and the environment derivation, whose
	// path appears in the build message.
	f.same(&["print-dev-env", "--impure", "--file", "dev.nix"]);
	f.same(&["print-dev-env", "--impure", "--file", "dev.nix", "--json"]);
	f.same(&[
		"print-dev-env",
		"--impure",
		"--expr",
		"(import ./dev.nix).dep",
	]);
}

#[test]
fn develop_runs_commands_and_phases_in_the_environment() {
	let f = Fixture::new();
	for args in [
		vec![
			"develop",
			"--impure",
			"--file",
			"dev.nix",
			"--command",
			"bash",
			"-c",
			"echo foo=$foo out=$out dev=$dev; echo $SHELL $NIX_GCROOT; type -t runHook",
		],
		vec![
			"develop", "--impure", "--file", "dev.nix", "--phase", "build",
		],
		vec![
			"develop",
			"--impure",
			"--file",
			"dev.nix",
			"--command",
			"bash",
			"-c",
			"echo $PATH; declare -p foo; exit 4",
		],
		vec![
			"develop",
			"--impure",
			"--file",
			"dev.nix",
			"--ignore-env",
			"--command",
			"bash",
			"-c",
			"env | sort | grep -v -e ^_= -e ^PWD= -e ^SHLVL= -e TMP -e TEMP -e NIX_BUILD_TOP",
		],
		vec![
			"develop",
			"--impure",
			"--file",
			"dev.nix",
			"--set-env-var",
			"X",
			"1",
			"--unset-env-var",
			"HOME",
			"--command",
			"bash",
			"-c",
			"echo $X ${HOME-unset}",
		],
	] {
		let (status, _, stderr) = f.same(&args);
		if status == Some(1) && stderr.contains("Operation not permitted") {
			eprintln!("execution unavailable: the store's mount namespace was refused");
		}
	}
	// The builder must be bash.
	f.same(&["develop", "--impure", "--expr", "(import ./dev.nix).dep"]);
}

#[test]
fn unsupported_develop_options_fail_explicitly() {
	let f = Fixture::new();
	let (status, _, stderr) = f.run(
		"noxi",
		&[
			"develop",
			"--impure",
			"--file",
			"dev.nix",
			"--redirect",
			"/nix/store/00000000000000000000000000000000-redirect",
			"p",
			"-c",
			"true",
		],
	);
	assert_eq!(status, Some(1));
	assert!(stderr.contains("is not supported by noxi yet"), "{stderr}");
}

#[test]
fn shell_and_run_print_the_summary_of_fresh_builds() {
	let f = Fixture::new();
	for args in [
		vec![
			"shell",
			"--impure",
			"--expr",
			"(import ./dev.nix).dep",
			"--command",
			"true",
		],
		vec!["run", "--impure", "--file", "pkg.nix", "", "a", "b"],
	] {
		let (status, _, stderr) = f.same(&args);
		if status == Some(1) && stderr.contains("Operation not permitted") {
			eprintln!("execution unavailable: the store's mount namespace was refused");
		}
	}
}

/// The environment derivation re-serialises `__json`, so its path checks
/// the structured attributes byte for byte.
#[test]
fn structured_attributes_reach_the_environment() {
	let f = Fixture::new();
	f.same(&["print-dev-env", "--impure", "--file", "structured.nix"]);
	f.same(&[
		"print-dev-env",
		"--impure",
		"--file",
		"structured.nix",
		"--json",
	]);
	let (status, _, stderr) = f.same(&[
		"develop",
		"--impure",
		"--file",
		"structured.nix",
		"-c",
		"bash",
		"-c",
		"cat $NIX_ATTRS_JSON_FILE; declare -p outputs",
	]);
	if status == Some(1) && stderr.contains("Operation not permitted") {
		eprintln!("execution unavailable: the store's mount namespace was refused");
	}
}

/// Build logs are hidden by default and raw with `raw-with-logs`; the
/// reference's `bar` formats prefix each line, which fails explicitly.
#[test]
fn build_logs_follow_the_log_format() {
	let f = Fixture::new();
	let build = |name: &str| {
		vec![
			"build",
			"--impure",
			"--file",
			"logs.nix",
			"--argstr",
			"name",
			name,
			"--no-link",
		]
		.into_iter()
		.map(str::to_owned)
		.collect::<Vec<_>>()
	};
	let args = build("quiet");
	f.same(&args.iter().map(String::as_str).collect::<Vec<_>>());
	let mut args = build("raw");
	args.extend(["--log-format".into(), "raw-with-logs".into()]);
	f.same(&args.iter().map(String::as_str).collect::<Vec<_>>());
	let mut args = build("bar");
	args.push("-L".into());
	let (status, _, stderr) = f.run("noxi", &args.iter().map(String::as_str).collect::<Vec<_>>());
	assert_eq!(status, Some(1));
	assert!(stderr.contains("is not supported by noxi yet"), "{stderr}");
}

#[test]
fn print_dev_env_profile_points_to_matching_environment() {
	let f = Fixture::new();
	f.same(&[
		"print-dev-env",
		"--impure",
		"--file",
		"dev.nix",
		"--profile",
		"dev-profile",
		"--json",
	]);
	for link in ["dev-profile", "dev-profile-1-link"] {
		assert_eq!(
			fs::read_link(f.engine("reference").join("work").join(link)).unwrap(),
			fs::read_link(f.engine("noxi").join("work").join(link)).unwrap()
		);
	}
}
#[test]
fn flake_packages_build_shell_run_and_dev_environment() {
	let f = Fixture::new();
	for engine in ENGINES {
		fs::write(f.engine(engine).join("work/flake.nix"), "{ outputs = { self }: { packages.x86_64-linux.default = import ./pkg.nix; devShells.x86_64-linux.default = import ./dev.nix; }; }").unwrap();
	}
	for args in [
		vec!["build", "--no-link", "--print-out-paths"],
		vec!["shell", "--command", "true"],
		vec!["run", "--", "a", "b"],
		vec!["print-dev-env", "--json"],
		vec!["develop", "--command", "true"],
	] {
		let mut flags = vec![
			"--extra-experimental-features",
			"flakes",
			"--option",
			"flake-registry",
			"",
			args[0],
			"--impure",
			"--no-write-lock-file",
		];
		flags.extend_from_slice(&args[1..]);
		let a = f.run("reference", &flags);
		let b = f.run("noxi", &flags);
		assert_eq!(a.0, b.0, "{flags:?}: {}", b.2);
		assert_eq!(a.1, b.1, "{flags:?}: {}", b.2);
	}
}

#[test]
fn flake_develop_resolves_bash_in_the_selected_store() {
	let f = Fixture::new();
	let busybox = fs::canonicalize(from_env_or_target(
		"NOXI_TEST_BUSYBOX",
		"test-busybox",
		"bin/busybox",
	))
	.unwrap();
	let bash = bash_package();
	let mut results = Vec::new();
	for engine in ENGINES {
		let work = f.engine(engine).join("work");
		fs::write(work.join("flake.nix"), format!(r#"{{ outputs = {{ self }}: {{
	devShells.x86_64-linux.default = import ./dev.nix;
	legacyPackages.x86_64-linux.bashInteractive = let bb = builtins.storePath {busybox}; bash = builtins.storePath {bash}; in derivation {{ name = "interactive-shell"; system = "x86_64-linux"; builder = "${{bb}}/bin/sh"; args = [ "-c" "${{bb}}/bin/mkdir -p $out/bin; ${{bb}}/bin/ln -s ${{bash}}/bin/bash $out/bin/bash" ]; }};
}}; }}"#, busybox = busybox.display(), bash = bash.display())).unwrap();
		let registry = work.join("registry.json");
		fs::write(&registry, serde_json::json!({"version":2,"flakes":[{"from":{"type":"indirect","id":"nixpkgs"},"to":{"type":"path","path":work}}]}).to_string()).unwrap();
		let result = f.run(
			engine,
			&[
				"--extra-experimental-features",
				"flakes",
				"--option",
				"flake-registry",
				registry.to_str().unwrap(),
				"develop",
				"--impure",
				"--no-write-lock-file",
				"--command",
				"true",
			],
		);
		results.push(result);
		assert!(
			fs::read_dir(f.engine(engine).join("root/nix/store"))
				.unwrap()
				.any(|e| e.unwrap().path().join("bin/bash").is_symlink())
		);
	}
	assert_eq!(results[0].0, results[1].0, "{}", results[1].2);
	assert_eq!(results[0].1, results[1].1);
}
