//! Derivation builds checked against the pinned CppNix 2.35.2 reference.
//!
//! The reference writes each derivation into two temporary `--store ROOT`
//! stores. It builds the derivation in the first; `nix-rs-build` builds it
//! in the second. The outputs, their registered metadata, and the
//! reference's verification of the store nix-rs wrote must match.
//!
//! The reference executables come from `NIXRS_REFERENCE_BIN` or
//! `target/nix-reference/bin`, and the statically linked busybox used as
//! the builder from `NIXRS_TEST_BUSYBOX` or `target/test-busybox`
//! (`just reference` creates both).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nix_compat::store_path::StorePath;
use nix_rs_build::FailureStatus;
use nix_rs_build::local::LocalBuilder;
use nix_rs_build::settings::BuildSettings;
use nix_rs_store::{LocalStore, LocalStoreConfig};
use serde_json::Value;

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

fn busybox() -> String {
	let path = from_env_or_target("NIXRS_TEST_BUSYBOX", "test-busybox", "bin/busybox");
	fs::canonicalize(path)
		.unwrap()
		.to_string_lossy()
		.into_owned()
}

struct Env {
	dir: tempfile::TempDir,
	busybox: String,
}

impl Env {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		fs::create_dir(dir.path().join("conf")).unwrap();
		fs::create_dir(dir.path().join("home")).unwrap();
		let env = Env {
			dir,
			busybox: busybox(),
		};
		for store in ["reference", "nix-rs"] {
			let to = format!("local?root={}", env.root(store).display());
			env.reference_ok(
				"nix",
				&["copy", "--no-check-sigs", "--to", &to, &env.busybox],
			);
		}
		env
	}

	fn root(&self, store: &str) -> PathBuf {
		self.dir.path().join(store)
	}

	fn reference(&self, program: &str, args: &[&str]) -> Output {
		Command::new(reference_bin().join(program))
			.args(args)
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

	fn reference_ok(&self, program: &str, args: &[&str]) -> String {
		let output = self.reference(program, args);
		assert!(
			output.status.success(),
			"{program} {args:?} failed:\n{}",
			String::from_utf8_lossy(&output.stderr)
		);
		String::from_utf8(output.stdout).unwrap()
	}

	/// Write `expr` to a file and instantiate it in both stores. Returns
	/// the derivation path.
	fn instantiate(&self, expr: &str) -> StorePath {
		let file = self.dir.path().join("expr.nix");
		fs::write(&file, expr.replace("@busybox@", &self.busybox)).unwrap();
		let mut drv = None;
		for store in ["reference", "nix-rs"] {
			let root = self.root(store);
			let out = self.reference_ok(
				"nix-instantiate",
				&["--store", root.to_str().unwrap(), file.to_str().unwrap()],
			);
			let path = StorePath::from_absolute_path(out.trim().as_bytes()).unwrap();
			assert!(
				drv.is_none_or(|d| d == path),
				"both stores get the same derivation"
			);
			drv = Some(path);
		}
		drv.unwrap()
	}

	fn reference_realise(&self, drv: &StorePath) -> Output {
		let root = self.root("reference");
		self.reference(
			"nix-store",
			&[
				"--store",
				root.to_str().unwrap(),
				"--realise",
				&drv.to_absolute_path(),
			],
		)
	}

	fn nix_rs_realise(
		&self,
		drv: &StorePath,
	) -> nix_rs_build::Result<std::collections::BTreeMap<String, StorePath>> {
		self.nix_rs_realise_with(drv, |_| {})
	}

	fn nix_rs_realise_with(
		&self,
		drv: &StorePath,
		configure: impl FnOnce(&mut BuildSettings),
	) -> nix_rs_build::Result<std::collections::BTreeMap<String, StorePath>> {
		let mut config = LocalStoreConfig::with_root(&self.root("nix-rs"));
		config.require_sigs = false;
		let store = LocalStore::open(config).unwrap();
		let mut settings = BuildSettings {
			sandbox_paths: Vec::new(),
			build_cores: 1,
			system_features: SYSTEM_FEATURES.split(' ').map(String::from).collect(),
			nix_command: true,
			..BuildSettings::default()
		};
		configure(&mut settings);
		LocalBuilder::new(&store, &settings).realise(drv, &mut |_| {})
	}

	/// `nix path-info --json` for `paths` in `store`, without the
	/// registration time.
	fn path_info(&self, store: &str, paths: &[&StorePath]) -> Value {
		let root = self.root(store);
		let mut args = vec!["path-info", "--json", "--store", root.to_str().unwrap()];
		let paths: Vec<String> = paths.iter().map(|p| p.to_absolute_path()).collect();
		args.extend(paths.iter().map(String::as_str));
		let mut json: Value = serde_json::from_str(&self.reference_ok("nix", &args)).unwrap();
		for info in json.as_object_mut().unwrap().values_mut() {
			info.as_object_mut().unwrap().remove("registrationTime");
		}
		json
	}

	fn verify(&self, store: &str) {
		let root = self.root(store);
		let output = self.reference(
			"nix-store",
			&[
				"--store",
				root.to_str().unwrap(),
				"--verify",
				"--check-contents",
			],
		);
		let stderr = String::from_utf8_lossy(&output.stderr);
		assert!(
			output.status.success(),
			"verifying the {store} store failed:\n{stderr}"
		);
		for bad in [
			"error",
			"warning",
			"modified",
			"disappeared",
			"fixing",
			"updating",
		] {
			assert!(
				!stderr.contains(bad),
				"verifying the {store} store reported problems:\n{stderr}"
			);
		}
	}

	fn read(&self, store: &str, path: &StorePath) -> Vec<u8> {
		fs::read(self.root(store).join("nix/store").join(path.to_string())).unwrap()
	}

	/// The contents of a regular-file output, or the listing of a directory.
	fn read_tree(&self, store: &str, path: &StorePath) -> Vec<u8> {
		let real = self.root(store).join("nix/store").join(path.to_string());
		if real.is_dir() {
			let mut names: Vec<String> = fs::read_dir(&real)
				.unwrap()
				.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
				.collect();
			names.sort();
			names.join("\n").into_bytes()
		} else {
			fs::read(real).unwrap()
		}
	}

	/// Build `drv` with both engines and check that they register the same
	/// outputs. Returns the output paths.
	fn build_both(&self, drv: &StorePath) -> Vec<StorePath> {
		let output = self.reference_realise(drv);
		assert!(
			output.status.success(),
			"the reference failed:\n{}",
			String::from_utf8_lossy(&output.stderr)
		);
		let reference: Vec<StorePath> = String::from_utf8(output.stdout)
			.unwrap()
			.lines()
			.map(|l| StorePath::from_absolute_path(l.as_bytes()).unwrap())
			.collect();
		let nix_rs = self
			.nix_rs_realise(drv)
			.unwrap_or_else(|e| panic!("nix-rs failed: {e}"));
		let mut nix_rs: Vec<StorePath> = nix_rs.into_values().collect();
		nix_rs.sort();
		let mut sorted = reference.clone();
		sorted.sort();
		assert_eq!(nix_rs, sorted);
		let paths: Vec<&StorePath> = reference.iter().collect();
		assert_eq!(
			self.path_info("nix-rs", &paths),
			self.path_info("reference", &paths)
		);
		self.verify("nix-rs");
		reference
	}
}

#[test]
fn simple_derivation_matches_the_reference() {
	let env = Env::new();
	let drv = env.instantiate(
		r#"derivation {
			name = "hello";
			system = "x86_64-linux";
			builder = "${builtins.storePath @busybox@}/bin/sh";
			args = [ "-c" "echo hello > $out" ];
		}"#,
	);
	let outputs = env.build_both(&drv);
	assert_eq!(env.read("nix-rs", &outputs[0]), b"hello\n");
}

#[test]
fn rust_build_command_evaluates_instantiates_and_realises_without_reference_derivations() {
	for output in ["out", "dev"] {
		rust_build_command_matches_reference_output(output);
	}
}

fn rust_build_command_matches_reference_output(output: &str) {
	let env = Env::new();
	let expression = format!(
		r#"(derivation {{ name = "rust-command"; system = "x86_64-linux"; builder = "${{builtins.storePath {}}}/bin/sh"; outputs = [ "out" "dev" ]; args = [ "-c" "echo $NIX_BUILD_CORES > $out; echo dev > $dev" ]; }}).{output}"#,
		env.busybox
	);
	let reference = env.reference_ok(
		"nix-build",
		&[
			"--store",
			env.root("reference").to_str().unwrap(),
			"--no-out-link",
			"--expr",
			&expression,
		],
	);
	let mut settings = nix_compat::nixcpp::settings::Settings::default();
	for (name, value) in [("substituters", ""), ("sandbox-paths", ""), ("cores", "1")] {
		settings.set(name, value).unwrap();
	}
	let mut options = nix_rs_commands::build::parse_args(
		[
			"--store".to_owned(),
			format!("local?root={}", env.root("nix-rs").display()),
			"--no-out-link".to_owned(),
			"--expr".to_owned(),
			expression,
		],
		settings,
	)
	.unwrap();
	options.out_link = Some("result".into());
	let mut stdout = Vec::new();
	let mut stderr = Vec::new();
	assert_eq!(
		nix_rs_commands::build::run(&options, env.dir.path(), &mut stdout, &mut stderr),
		std::process::ExitCode::SUCCESS,
		"{}",
		String::from_utf8_lossy(&stderr)
	);
	assert_eq!(String::from_utf8(stdout).unwrap(), reference);
	assert_eq!(
		fs::read_link(env.dir.path().join("result")).unwrap(),
		Path::new(reference.trim())
	);
	assert!(
		env.reference_ok(
			"nix-store",
			&[
				"--store",
				env.root("nix-rs").to_str().unwrap(),
				"--gc",
				"--print-live"
			]
		)
		.lines()
		.any(|path| path == reference.trim())
	);
	let path = StorePath::from_absolute_path(reference.trim().as_bytes()).unwrap();
	assert_eq!(
		env.read_tree("reference", &path),
		env.read_tree("nix-rs", &path)
	);
	assert_eq!(
		env.path_info("reference", &[&path]),
		env.path_info("nix-rs", &[&path])
	);
	for query in [
		"--hash",
		"--size",
		"--references",
		"--referrers",
		"--deriver",
		"--requisites",
	] {
		let expected = env.reference_ok(
			"nix-store",
			&[
				"--store",
				env.root("reference").to_str().unwrap(),
				"--query",
				query,
				reference.trim(),
			],
		);
		let options = nix_rs_commands::store::parse_args(
			[
				"--store".to_owned(),
				format!("local?root={}", env.root("nix-rs").display()),
				"--query".to_owned(),
				query.to_owned(),
				reference.trim().to_owned(),
			],
			nix_compat::nixcpp::settings::Settings::default(),
		)
		.unwrap();
		let mut actual = Vec::new();
		let mut stderr = Vec::new();
		assert_eq!(
			nix_rs_commands::store::run(&options, &mut actual, &mut stderr),
			std::process::ExitCode::SUCCESS,
			"{query}: {}",
			String::from_utf8_lossy(&stderr)
		);
		assert_eq!(String::from_utf8(actual).unwrap(), expected, "{query}");
	}
	env.verify("nix-rs");
}

/// The builder records what it can observe of its sandbox: environment,
/// identity, generated files, and the directory layout.
#[test]
fn sandbox_environment_matches_the_reference() {
	let env = Env::new();
	let drv = env.instantiate(
		r#"let bb = builtins.storePath @busybox@; in derivation {
			name = "probe";
			system = "x86_64-linux";
			builder = "${bb}/bin/sh";
			PATH = "${bb}/bin";
			args = [ "-c" ''
				{
					env | sort
					echo ---; id; hostname; cat /etc/passwd /etc/group /etc/hosts; pwd
					echo ---; ls -a / /dev /build /etc /tmp
					stat -c '%A %U %G %n' / /build /tmp /nix/store /etc /nix/store/*
					echo ---; cat /proc/self/uid_map /proc/self/gid_map; umask; ulimit -c
				} > $out
			'' ];
		}"#,
	);
	let outputs = env.build_both(&drv);
	let probe = env.read("nix-rs", &outputs[0]);
	assert_eq!(
		probe,
		env.read("reference", &outputs[0]),
		"{}",
		String::from_utf8_lossy(&probe)
	);
}

#[test]
fn input_derivations_and_multiple_outputs_match_the_reference() {
	let env = Env::new();
	let drv = env.instantiate(
		r#"let
			bb = builtins.storePath @busybox@;
			dep = derivation {
				name = "dep";
				system = "x86_64-linux";
				builder = "${bb}/bin/sh";
				outputs = [ "out" "dev" ];
				args = [ "-c" "echo $dev > $out; echo dep-dev > $dev" ];
			};
		in derivation {
			name = "top";
			system = "x86_64-linux";
			builder = "${bb}/bin/sh";
			outputs = [ "out" "bin" ];
			args = [ "-c" ''
				${bb}/bin/mkdir $out $bin
				echo ${dep} $bin > $out/refs
				echo $bin > $bin/self
				${bb}/bin/cat ${dep.dev} > $bin/dev
			'' ];
		}"#,
	);
	env.build_both(&drv);
}

#[test]
fn placeholders_and_pass_as_file_match_the_reference() {
	let env = Env::new();
	let drv = env.instantiate(
		r#"let bb = builtins.storePath @busybox@; in derivation {
			name = "files";
			system = "x86_64-linux";
			builder = "${bb}/bin/sh";
			outputs = [ "out" "doc" ];
			doc_path = builtins.placeholder "doc";
			big = "contents for ${builtins.placeholder "out"}";
			passAsFile = [ "big" ];
			args = [ "-c" ''
				${bb}/bin/mkdir $doc
				{ echo ${builtins.placeholder "out"} $doc_path; echo $bigPath; ${bb}/bin/cat $bigPath; } > $out
			'' ];
		}"#,
	);
	env.build_both(&drv);
}

#[test]
fn export_references_graph_matches_the_reference() {
	let env = Env::new();
	let drv = env.instantiate(
		r#"let bb = builtins.storePath @busybox@; in derivation {
			name = "graph";
			system = "x86_64-linux";
			builder = "${bb}/bin/sh";
			exportReferencesGraph = [ "closure" bb ];
			args = [ "-c" "${bb}/bin/cat closure > $out" ];
		}"#,
	);
	env.build_both(&drv);
}

#[test]
fn fixed_output_derivations_match_the_reference() {
	let env = Env::new();
	// The SHA-256 of "fixed\n".
	let drv = env.instantiate(
		r#"derivation {
			name = "flat";
			system = "x86_64-linux";
			builder = "${builtins.storePath @busybox@}/bin/sh";
			args = [ "-c" "echo fixed > $out" ];
			outputHashMode = "flat";
			outputHashAlgo = "sha256";
			outputHash = "sha256-DDBxQY5jVuYUiYyE7QZMqV6IVRvAgRtTS98ZUuza5TQ=";
		}"#,
	);
	env.build_both(&drv);
}

#[test]
fn fixed_output_hash_mismatch_fails_like_the_reference() {
	let env = Env::new();
	let drv = env.instantiate(
		r#"let bb = builtins.storePath @busybox@; in derivation {
			name = "wrong";
			system = "x86_64-linux";
			builder = "${bb}/bin/sh";
			args = [ "-c" "echo wrong > $out" ];
			outputHashMode = "recursive";
			outputHashAlgo = "sha256";
			outputHash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
		}"#,
	);
	let reference = env.reference_realise(&drv);
	assert!(!reference.status.success());
	let stderr = String::from_utf8_lossy(&reference.stderr);
	let error = env.nix_rs_realise(&drv).unwrap_err();
	assert_eq!(error.status(), FailureStatus::HashMismatch);
	for line in error.to_string().lines() {
		assert!(
			stderr.contains(line.trim()),
			"missing {line:?} in:\n{stderr}"
		);
	}
	// Both keep the wrongly hashed output.
	let got = error
		.to_string()
		.lines()
		.last()
		.unwrap()
		.trim()
		.trim_start_matches("got:")
		.trim()
		.to_owned();
	assert!(got.starts_with("sha256-"));
	env.verify("nix-rs");
}

/// The reference reads the build log nix-rs wrote back as the one it wrote
/// itself.
#[test]
fn build_logs_match_the_reference() {
	let env = Env::new();
	let drv = env.instantiate(
		r#"derivation {
			name = "logs";
			system = "x86_64-linux";
			builder = "${builtins.storePath @busybox@}/bin/sh";
			args = [ "-c" "echo to stdout; echo to stderr >&2; printf 'no newline'; echo x > $out" ];
		}"#,
	);
	env.build_both(&drv);
	let log = |store: &str| {
		let root = env.root(store);
		env.reference_ok(
			"nix-store",
			&[
				"--store",
				root.to_str().unwrap(),
				"-l",
				&drv.to_absolute_path(),
			],
		)
	};
	assert_eq!(log("nix-rs"), log("reference"));
	assert_eq!(log("nix-rs"), "to stdout\nto stderr\nno newline");
}

#[test]
fn builder_failures_fail_like_the_reference() {
	let env = Env::new();
	let drv = env.instantiate(
		r#"derivation {
			name = "fails";
			system = "x86_64-linux";
			builder = "${builtins.storePath @busybox@}/bin/sh";
			args = [ "-c" ''echo one; echo two >&2; printf 'three\rTHREE\n@nix {"action":"msg","level":0,"msg":"hidden"}\nfour'; exit 3'' ];
		}"#,
	);
	let reference = env.reference_realise(&drv);
	assert_eq!(reference.status.code(), Some(100));
	let error = env.nix_rs_realise(&drv).unwrap_err();
	assert_eq!(error.status(), FailureStatus::PermanentFailure);
	let stderr = String::from_utf8_lossy(&reference.stderr);
	for line in error.to_string().lines() {
		assert!(
			stderr.contains(line.trim()),
			"missing {line:?} in:\n{stderr}"
		);
	}
	// `nix build` without `--print-build-logs` ends the error with the log
	// tail instead, leaving out `@nix` messages and overwritten text.
	let error = env
		.nix_rs_realise_with(&drv, |s| s.show_log_tail = true)
		.unwrap_err();
	let message = error.to_string();
	let root = env.root("reference");
	let output = env.reference(
		"nix",
		&[
			"build",
			"--no-link",
			"--store",
			root.to_str().unwrap(),
			&format!("{}^*", drv.to_absolute_path()),
		],
	);
	let stderr = String::from_utf8_lossy(&output.stderr);
	for line in message.lines() {
		assert!(
			stderr.contains(line.trim()),
			"missing {line:?} in:\n{stderr}"
		);
	}
	let tail = message.split_once("Last 4 log lines:\n").unwrap().1;
	assert_eq!(
		tail,
		format!(
			"> one\n> two\n> THREE\n> four\nFor full logs, run:\n  nix log {}",
			drv.to_absolute_path()
		)
	);
}

#[test]
fn missing_outputs_and_reference_checks_fail_like_the_reference() {
	let env = Env::new();
	for (expr, status) in [
		(
			r#"derivation {
				name = "no-output";
				system = "x86_64-linux";
				builder = "${builtins.storePath @busybox@}/bin/sh";
				args = [ "-c" "true" ];
			}"#,
			FailureStatus::OutputRejected,
		),
		(
			r#"let bb = builtins.storePath @busybox@; in derivation {
				name = "cycle";
				system = "x86_64-linux";
				builder = "${bb}/bin/sh";
				outputs = [ "out" "bin" ];
				args = [ "-c" "echo $bin > $out; echo $out > $bin" ];
			}"#,
			FailureStatus::OutputRejected,
		),
		(
			r#"let bb = builtins.storePath @busybox@; in derivation {
				name = "disallowed";
				system = "x86_64-linux";
				builder = "${bb}/bin/sh";
				disallowedReferences = [ bb ];
				args = [ "-c" "echo ${bb} > $out" ];
			}"#,
			FailureStatus::OutputRejected,
		),
	] {
		let drv = env.instantiate(expr);
		let reference = env.reference_realise(&drv);
		assert!(!reference.status.success());
		let stderr = String::from_utf8_lossy(&reference.stderr)
			.replace(&env.root("reference").display().to_string(), "ROOT");
		let error = env.nix_rs_realise(&drv).unwrap_err();
		assert_eq!(error.status(), status);
		let message = error
			.to_string()
			.replace(&env.root("nix-rs").display().to_string(), "ROOT");
		for line in message.lines() {
			assert!(
				stderr.contains(line.trim()),
				"missing {line:?} in:\n{stderr}"
			);
		}
	}
}

#[test]
fn structured_attributes_match_the_reference() {
	let env = Env::new();
	let drv = env.instantiate(
		r#"let bb = builtins.storePath @busybox@; in derivation {
			name = "structured";
			system = "x86_64-linux";
			builder = "${bb}/bin/sh";
			__structuredAttrs = true;
			outputs = [ "out" "dev" ];
			string = "it's ${builtins.placeholder "dev"}";
			int = 3000000000;
			float = 2.0;
			fraction = 0.00001;
			fixed = 1.0e14;
			exponent = 1.0e15;
			list = [ 1 "x" null true false ];
			attrs = { k = 1; "with space" = "v"; };
			nested = [ [ 1 ] ];
			"not a var" = 1;
			exportReferencesGraph.closure = [ bb ];
			passAsFile = [ "string" ];
			outputChecks.out.maxSize = 100000;
			args = [ "-c" ''
				# ash cannot source .attrs.sh, so take the paths from the JSON.
				get() { ${bb}/bin/grep -o "\"$1\":\"[^\"]*\"" .attrs.json | ${bb}/bin/head -1 | ${bb}/bin/cut -d '"' -f 4; }
				${bb}/bin/mkdir $(get dev)
				{ ${bb}/bin/env | ${bb}/bin/sort; ${bb}/bin/cat .attrs.sh .attrs.json; } > $(get out)
			'' ];
		}"#,
	);
	let outputs = env.build_both(&drv);
	for output in &outputs {
		assert_eq!(
			env.read_tree("nix-rs", output),
			env.read_tree("reference", output)
		);
	}
}

/// A Nixpkgs package that misses the binary cache, built on top of a
/// substituted stdenv. Set `NIXRS_NIXPKGS` to the root flake's Nixpkgs
/// source (`just nixpkgs-paths` shows how); it downloads from
/// cache.nixos.org, so it only runs with `--run-ignored`.
#[test]
#[ignore = "needs NIXRS_NIXPKGS and cache.nixos.org"]
fn nixpkgs_hello_matches_the_reference() {
	const CACHE: &str = "https://cache.nixos.org";
	const KEY: &str = "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=";
	let nixpkgs = std::env::var("NIXRS_NIXPKGS").expect("NIXRS_NIXPKGS");
	let env = Env::new();
	// The reference's compiled-in sandbox shell, which stdenv's
	// `#!/bin/sh` scripts need.
	let sandbox_paths = String::from_utf8(
		Command::new(reference_bin().join("nix"))
			.args(["config", "show", "sandbox-paths"])
			.env_clear()
			.env("HOME", env.dir.path().join("home"))
			.env("NIX_CONF_DIR", env.dir.path().join("conf"))
			.env("NIX_CONFIG", "experimental-features = nix-command")
			.output()
			.unwrap()
			.stdout,
	)
	.unwrap()
	.trim()
	.to_owned();
	let drv = env.instantiate(&format!(
		r#"(import {nixpkgs} {{ system = "x86_64-linux"; config = {{ }}; overlays = [ ]; }}).hello.overrideAttrs (_: {{
			NIXRS_CACHE_MISS = "1";
		}})"#
	));

	let root = env.root("reference");
	let output = env.reference(
		"nix-store",
		&[
			"--store",
			root.to_str().unwrap(),
			"--realise",
			&drv.to_absolute_path(),
			"--option",
			"substituters",
			CACHE,
			"--option",
			"trusted-public-keys",
			KEY,
			"--option",
			"sandbox-paths",
			&sandbox_paths,
		],
	);
	assert!(
		output.status.success(),
		"the reference failed:\n{}",
		String::from_utf8_lossy(&output.stderr)
	);
	let mut reference: Vec<StorePath> = String::from_utf8(output.stdout)
		.unwrap()
		.lines()
		.map(|l| StorePath::from_absolute_path(l.as_bytes()).unwrap())
		.collect();
	reference.sort();

	let mut config = LocalStoreConfig::with_root(&env.root("nix-rs"));
	config.trusted_public_keys = vec![KEY.to_owned()];
	let store = LocalStore::open(config).unwrap();
	let settings = BuildSettings {
		sandbox_paths: sandbox_paths
			.split_whitespace()
			.map(nix_rs_build::settings::SandboxPath::parse)
			.collect(),
		build_cores: 1,
		system_features: SYSTEM_FEATURES.split(' ').map(String::from).collect(),
		nix_command: true,
		..BuildSettings::default()
	};
	let caches = [nix_rs_store::binary_cache::BinaryCache::open(CACHE).unwrap()];
	let mut log = Vec::new();
	let outputs = LocalBuilder::new(&store, &settings)
		.with_substituters(&caches)
		.realise(&drv, &mut |data| log.extend_from_slice(data))
		.unwrap_or_else(|e| {
			panic!(
				"nix-rs failed: {e}\nlog tail:\n{}",
				String::from_utf8_lossy(&log[log.len().saturating_sub(4000)..])
			)
		});
	let mut nix_rs: Vec<StorePath> = outputs.into_values().collect();
	nix_rs.sort();
	assert_eq!(nix_rs, reference);
	let paths: Vec<&StorePath> = reference.iter().collect();
	assert_eq!(
		env.path_info("nix-rs", &paths),
		env.path_info("reference", &paths)
	);
	env.verify("nix-rs");
}

/// Only the outputs a derivation uses are realised: an input's deleted
/// `dev` output stays missing when the top derivation is rebuilt.
#[test]
fn only_used_input_outputs_are_realised() {
	let env = Env::new();
	let drv = env.instantiate(
		r#"let
			bb = builtins.storePath @busybox@;
			dep = derivation {
				name = "dep";
				system = "x86_64-linux";
				builder = "${bb}/bin/sh";
				outputs = [ "out" "dev" ];
				args = [ "-c" "echo out > $out; echo dev > $dev" ];
			};
		in derivation {
			name = "uses-out";
			system = "x86_64-linux";
			builder = "${bb}/bin/sh";
			args = [ "-c" "echo ${dep.out} > $out" ];
		}"#,
	);
	let top = env.build_both(&drv);
	let query = |store: &str, args: &[&str]| -> String {
		let root = env.root(store);
		let mut full = vec!["--store", root.to_str().unwrap()];
		full.extend_from_slice(args);
		env.reference_ok("nix-store", &full)
	};
	let dep_drv = query(
		"reference",
		&["-q", "--references", &drv.to_absolute_path()],
	)
	.lines()
	.find(|l| l.ends_with("-dep.drv"))
	.unwrap()
	.to_owned();
	let dev = query("reference", &["-q", "--outputs", &dep_drv])
		.lines()
		.find(|l| l.ends_with("-dep-dev"))
		.unwrap()
		.to_owned();
	for store in ["reference", "nix-rs"] {
		query(store, &["--delete", &top[0].to_absolute_path(), &dev]);
	}
	env.build_both(&drv);
	for store in ["reference", "nix-rs"] {
		let output = env.reference(
			"nix-store",
			&[
				"--store",
				env.root(store).to_str().unwrap(),
				"--check-validity",
				&dev,
			],
		);
		assert!(!output.status.success(), "{store} rebuilt {dev}");
	}
}
