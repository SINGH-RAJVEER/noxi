//! `nix-store` checked against the pinned CppNix 2.35.2 reference.
//!
//! The reference instantiates the same derivations into two temporary
//! `local?root=` stores. The reference's `nix-store` realises and queries
//! them in the first and nix-rs's in the second, from working directories
//! at the same depth, and their output, errors, exit statuses and GC roots
//! must match.
//!
//! The reference executables come from `NIXRS_REFERENCE_BIN` or
//! `target/nix-reference/bin`, and the statically linked busybox used as
//! the builder from `NIXRS_TEST_BUSYBOX` or `target/test-busybox`
//! (`just reference` creates both).

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SYSTEM_FEATURES: &str = "benchmark big-parallel kvm nixos-test uid-range";

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

const ENGINES: [&str; 2] = ["reference", "nix-rs"];

struct Env {
	dir: tempfile::TempDir,
	busybox: String,
}

impl Env {
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
		let env = Env { dir, busybox };
		for engine in ENGINES {
			fs::create_dir_all(env.root(engine)).unwrap();
			fs::create_dir_all(env.cwd(engine)).unwrap();
			let to = env.store(engine);
			let output = env.run(
				"reference",
				"nix",
				&["copy", "--no-check-sigs", "--to", &to, &env.busybox],
				None,
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
		self.dir.path().join(engine).join("root")
	}

	/// Each engine's working directory, at the same depth in its own tree
	/// so that relative GC roots differ only in the engine's name.
	fn cwd(&self, engine: &str) -> PathBuf {
		self.dir.path().join(engine).join("work")
	}

	fn store(&self, engine: &str) -> String {
		format!("local?root={}", self.root(engine).display())
	}

	fn run(&self, engine: &str, program: &str, args: &[&str], cwd: Option<&Path>) -> Output {
		let executable = match engine {
			"reference" => reference_bin().join(program),
			_ => PathBuf::from(match program {
				"nix-build" => env!("CARGO_BIN_EXE_nix-build"),
				"nix-instantiate" => env!("CARGO_BIN_EXE_nix-instantiate"),
				"nix-store" => env!("CARGO_BIN_EXE_nix-store"),
				_ => panic!("no nix-rs executable for {program}"),
			}),
		};
		// CppNix selects the command from the executable name.
		Command::new(executable)
			.arg0(program)
			.args(args)
			.current_dir(cwd.unwrap_or(self.dir.path()))
			.env_clear()
			.env("HOME", self.dir.path().join("home"))
			.env("NIX_CONF_DIR", self.dir.path().join("conf"))
			.env(
				"NIX_CONFIG",
				format!(
					"experimental-features = nix-command\nbuild-users-group =\nsubstituters =\nsandbox = true\nsandbox-fallback = false\nsandbox-paths =\ncores = 1\nsystem-features = {SYSTEM_FEATURES}\n"
				),
			)
			.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin")
			.output()
			.unwrap()
	}

	/// Instantiate `expr` with the reference into both stores.
	fn instantiate(&self, expr: &str) -> String {
		let file = self.dir.path().join("expr.nix");
		fs::write(&file, expr.replace("@busybox@", &self.busybox)).unwrap();
		let mut drv = None;
		for engine in ENGINES {
			let output = self.run(
				"reference",
				"nix-instantiate",
				&["--store", &self.store(engine), file.to_str().unwrap()],
				None,
			);
			assert!(
				output.status.success(),
				"{}",
				String::from_utf8_lossy(&output.stderr)
			);
			let path = String::from_utf8(output.stdout).unwrap().trim().to_owned();
			assert!(drv.as_ref().is_none_or(|d| *d == path));
			drv = Some(path);
		}
		drv.unwrap()
	}

	/// Run `nix-store --store ROOT ARGS` with both engines from their
	/// working directories, and require the same result once each engine's
	/// directory name is replaced. Returns the reference's standard output.
	fn same(&self, args: &[&str]) -> String {
		self.same_with("nix-store", args)
	}

	fn same_with(&self, program: &str, args: &[&str]) -> String {
		let results: Vec<(i32, String, String)> = ENGINES
			.iter()
			.map(|engine| {
				let store = self.store(engine);
				let mut full = vec!["--store", &store];
				full.extend_from_slice(args);
				let output = self.run(engine, program, &full, Some(&self.cwd(engine)));
				let normalise = |bytes: &[u8]| {
					String::from_utf8_lossy(bytes).replace(
						&self.dir.path().join(engine).display().to_string(),
						"<engine>",
					)
				};
				(
					output.status.code().unwrap_or(-1),
					normalise(&output.stdout),
					normalise(&output.stderr),
				)
			})
			.collect();
		assert_eq!(results[1], results[0], "{program} {args:?}");
		results[0].1.clone()
	}
}

const EXPR: &str = r#"
let
  bb = builtins.storePath @busybox@;
  dep = derivation {
    name = "dep";
    system = "x86_64-linux";
    builder = "${bb}/bin/sh";
    args = [ "-c" "echo dep > $out" ];
  };
in
derivation {
  name = "multi";
  system = "x86_64-linux";
  builder = "${bb}/bin/sh";
  outputs = [ "out" "dev" ];
  extra = "a value";
  args = [ "-c" "echo ${dep} > $out; echo $out > $dev" ];
}
"#;

#[test]
fn realises_with_roots_and_queries_like_cppnix() {
	let env = Env::new();
	let drv = env.instantiate(EXPR);

	let realised = env.same(&["--realise", &drv, "--add-root", "result"]);
	assert_eq!(realised, "<engine>/work/result\n<engine>/work/result-dev\n");
	for engine in ENGINES {
		let auto = env.root(engine).join("nix/var/nix/gcroots/auto");
		let roots: Vec<PathBuf> = fs::read_dir(&auto)
			.unwrap()
			.map(|e| fs::read_link(e.unwrap().path()).unwrap())
			.collect();
		assert_eq!(roots.len(), 2, "{engine} registers both roots");
		assert!(roots.contains(&env.cwd(engine).join("result")));
	}

	// Realising again is a no-op that prints the same roots; without
	// `--add-root` the output paths are printed with a warning.
	env.same(&["-r", &drv, "--add-root", "result"]);
	env.same(&["-r", &drv]);
	env.same(&["-r", &format!("{drv}!dev")]);
	env.same(&["-r", "result"]);
	env.same(&["-r", "--dry-run", &drv]);

	for query in [
		vec!["-q", &drv],
		vec!["-q", "--outputs", &drv],
		vec!["-q", "--references", &drv],
		vec!["-q", "--references", "result"],
		vec!["-q", "-R", &drv],
		vec!["-q", "-R", "--include-outputs", &drv],
		vec!["-q", "--requisites", "result"],
		vec!["-q", "--referrers", "result"],
		vec!["-q", "--referrers-closure", &drv],
		vec!["-q", "--deriver", "result"],
		vec!["-q", "-d", "result-dev"],
		vec!["-q", "--valid-derivers", "result"],
		vec!["-q", "--binding", "extra", &drv],
		vec!["-q", "-b", "extra", "result"],
		vec!["-q", "-b", "missing", &drv],
		vec!["-q", "--hash", "result"],
		vec!["-q", "--size", "result", "result-dev"],
		vec!["-q", "--hash", "--use-output", &drv],
		vec!["-q", "-R", "-u", &drv],
		vec!["-q", "--hash", "--size", "result"],
		vec!["-q", "--references", "/tmp"],
		vec!["-q", "--frobnicate", "result"],
	] {
		env.same(&query);
	}
	env.same(&[]);
	env.same(&["-r", "-q", &drv]);

	// The reference accepts the store nix-rs built into.
	let output = env.run(
		"reference",
		"nix-store",
		&[
			"--store",
			&env.store("nix-rs"),
			"--verify",
			"--check-contents",
		],
		None,
	);
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert_eq!(
		String::from_utf8_lossy(&output.stderr),
		"reading the Nix store...\nchecking path existence...\nchecking link hashes...\nchecking store hashes...\n"
	);
}

/// A set of derivations as `nix-build` and `nix-instantiate` collect them:
/// attributes in name order, recursing only into `recurseForDerivations`.
const SET_EXPR: &str = r#"
let
  bb = builtins.storePath @busybox@;
  mk = name: outputs: derivation {
    inherit name outputs;
    system = "x86_64-linux";
    builder = "${bb}/bin/sh";
    args = [ "-c" "for o in $outputs; do eval \"echo ${name} > \\$$o\"; done" ];
  };
  multi = mk "multi" [ "out" "dev" ];
in
{
  b = multi.dev;
  a = multi;
  c = { recurseForDerivations = true; d = mk "dep" [ "out" ]; };
  e = { skipped = mk "skipped" [ "out" ]; };
  "not an attr path" = mk "odd" [ "out" ];
  f = 1;
}
"#;

#[test]
fn builds_and_instantiates_with_roots_like_cppnix() {
	let env = Env::new();
	let file = env.dir.path().join("set.nix");
	fs::write(&file, SET_EXPR.replace("@busybox@", &env.busybox)).unwrap();
	let file = file.to_str().unwrap();

	let instantiated = env.same_with("nix-instantiate", &[file, "--add-root", "drv"]);
	assert_eq!(
		instantiated,
		"<engine>/work/drv\n<engine>/work/drv-2!dev\n<engine>/work/drv-3\n"
	);
	env.same_with("nix-instantiate", &[file]);
	env.same_with("nix-instantiate", &[file, "-A", "c"]);

	env.same_with("nix-build", &[file, "--dry-run"]);
	let built = env.same_with("nix-build", &[file]);
	assert_eq!(built.lines().count(), 3);
	for link in ["result", "result-dev", "result-2"] {
		for engine in ENGINES {
			assert!(
				env.cwd(engine).join(link).is_symlink(),
				"{engine} makes {link}"
			);
		}
	}
	env.same_with("nix-build", &[file, "-A", "b", "-o", "custom"]);
	env.same_with("nix-build", &[file, "-A", "a", "--no-out-link"]);
	env.same_with("nix-build", &[file, "-A", "c.d"]);
	env.same_with("nix-store", &["-q", "--requisites", "custom-dev"]);
}

const FAILING_EXPR: &str = r#"
let
  bb = builtins.storePath @busybox@;
  mk = name: cmd: extra: derivation ({
    inherit name;
    system = "x86_64-linux";
    builder = "${bb}/bin/sh";
    args = [ "-c" cmd ];
  } // extra);
  fails = mk "fails" "echo line1; echo line2 >&2; exit 3" { };
in
{
  inherit fails;
  dependent = mk "dependent" "echo ${fails} > $out" { };
  noOutput = mk "no-output" "true" { };
}
"#;

#[test]
fn reports_failed_builds_like_cppnix() {
	let env = Env::new();
	let file = env.dir.path().join("failing.nix");
	fs::write(&file, FAILING_EXPR.replace("@busybox@", &env.busybox)).unwrap();
	let file = file.to_str().unwrap();

	env.same_with("nix-build", &[file, "-A", "fails"]);
	env.same_with("nix-build", &[file, "-A", "noOutput"]);
	env.same_with("nix-build", &[file, "-A", "dependent"]);
	env.same_with("nix-build", &[file, "-A", "fails", "-A", "dependent"]);
	env.same_with("nix-build", &[file, "-A", "fails", "-A", "noOutput"]);
	let drv = env.same_with("nix-instantiate", &[file, "-A", "fails", "--no-gc-warning"]);
	env.same(&["-r", drv.trim()]);
}
