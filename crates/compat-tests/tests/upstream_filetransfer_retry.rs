//! Runs CppNix 2.35.2's `tests/filetransfer-retry-backoff` check.
//!
//! `default.nix` is a `runCommand`, not a VM test: it writes a `nix.conf`
//! with `filetransfer-retry-jitter = false`, diverts the store, state, log
//! and configuration directories into the build's temporary directory, and
//! runs `test_retry_backoff.py`. That script serves 503 and 429 responses
//! with `Retry-After` from a local HTTP server and checks the retry timing
//! and counts `nix-prefetch-url` reports. This test reproduces the
//! derivation's environment with `PATH` holding only the engine's commands
//! and Python, so no installed CppNix is reachable.
//!
//! noxi's executables come from `NOXI_CANDIDATE_BIN` (default: this test's
//! Cargo profile directory; build them with `cargo build -p noxi-commands
//! --bins`). The pinned reference runs only when `NOXI_REFERENCE_BIN` is
//! set. The script binds only to 127.0.0.1.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SCRIPT: &str = concat!(
	env!("CARGO_MANIFEST_DIR"),
	"/../../tests/nix/tests/filetransfer-retry-backoff/test_retry_backoff.py"
);
const DEFAULT_NIX: &str = concat!(
	env!("CARGO_MANIFEST_DIR"),
	"/../../tests/nix/tests/filetransfer-retry-backoff/default.nix"
);

/// The observed outcome under noxi: `nix-prefetch-url` does not retry a
/// 503 and fails on the first response.
const NOXI_PASSES: bool = false;

fn candidate_dir() -> PathBuf {
	if let Some(dir) = env::var_os("NOXI_CANDIDATE_BIN") {
		return PathBuf::from(dir);
	}
	let exe = env::current_exe().unwrap();
	exe.ancestors()
		.skip(1)
		.find(|dir| dir.join("nix").is_file())
		.expect("build noxi-commands' binaries first or set NOXI_CANDIDATE_BIN")
		.to_path_buf()
}

fn python_dir() -> PathBuf {
	let path = env::var_os("PATH").expect("PATH");
	let python = env::split_paths(&path)
		.map(|dir| dir.join("python3"))
		.find(|path| path.is_file())
		.expect("python3 on PATH");
	let dir = fs::canonicalize(python)
		.unwrap()
		.parent()
		.unwrap()
		.to_path_buf();
	assert!(
		!fs::read_dir(&dir).unwrap().any(|entry| entry
			.unwrap()
			.file_name()
			.to_string_lossy()
			.starts_with("nix")),
		"{} holds a nix executable",
		dir.display()
	);
	dir
}

/// Runs the script with `bin` first on `PATH`, as `default.nix` does.
fn run(bin: &Path) -> (bool, String) {
	let prefetch = bin.join("nix-prefetch-url");
	assert!(prefetch.exists(), "{} is missing", prefetch.display());
	let scratch = tempfile::tempdir().unwrap();
	let tmp = scratch.path();
	for dir in ["nix-state", "nix-log", "nix-store", "nix-conf"] {
		fs::create_dir_all(tmp.join(dir)).unwrap();
	}
	// The `nixConf` of default.nix.
	let default_nix = fs::read_to_string(DEFAULT_NIX).unwrap();
	assert!(default_nix.contains("filetransfer-retry-jitter = false"));
	fs::write(
		tmp.join("nix-conf/nix.conf"),
		"experimental-features = nix-command\nfiletransfer-retry-jitter = false\nsubstituters =\n",
	)
	.unwrap();
	let path = env::join_paths([bin.to_path_buf(), python_dir()]).unwrap();
	let output = Command::new("python3")
		.arg(SCRIPT)
		.env_clear()
		.env("PATH", path)
		.env("HOME", tmp)
		.env("TMPDIR", tmp)
		.env("NIX_STATE_DIR", tmp.join("nix-state"))
		.env("NIX_LOG_DIR", tmp.join("nix-log"))
		.env("NIX_STORE_DIR", tmp.join("nix-store"))
		.env("NIX_CONF_DIR", tmp.join("nix-conf"))
		.output()
		.unwrap();
	let log = String::from_utf8_lossy(&output.stdout).into_owned()
		+ &String::from_utf8_lossy(&output.stderr);
	(output.status.success(), log)
}

/// noxi's command, laid out by name like the replacement package.
fn noxi_bin(into: &Path) -> PathBuf {
	let source = candidate_dir();
	let own = source.join("nix-prefetch-url");
	let target = if own.is_file() {
		own
	} else {
		source.join("nix")
	};
	std::os::unix::fs::symlink(
		fs::canonicalize(target).unwrap(),
		into.join("nix-prefetch-url"),
	)
	.unwrap();
	into.to_path_buf()
}

#[test]
fn noxi_retry_backoff_matches_recorded_outcome() {
	let bin = tempfile::tempdir().unwrap();
	let (passed, log) = run(&noxi_bin(bin.path()));
	assert_eq!(
		passed, NOXI_PASSES,
		"noxi's outcome changed; update NOXI_PASSES and expectations/fixtures.txt:\n{log}"
	);
}

#[test]
fn reference_retry_backoff_passes() {
	let Some(reference) = env::var_os("NOXI_REFERENCE_BIN") else {
		eprintln!("skipped: NOXI_REFERENCE_BIN is unset");
		return;
	};
	let (passed, log) = run(Path::new(&reference));
	assert!(passed, "the reference failed:\n{log}");
}
