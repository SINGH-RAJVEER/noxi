//! `nix-collect-garbage` checked against the pinned CppNix 2.35.2 reference.
//!
//! The reference adds four sources to a `local?root=` store and makes two
//! profiles of them, one in a subdirectory, with the first two generations
//! of one profile dated ten days back. Then the reference's
//! `nix-collect-garbage` runs, and the scenario is rebuilt in the same
//! directory for noxi's. Using one directory keeps the names of the
//! indirect roots, which hash their link paths, and the directory order,
//! which both collectors print in. Output, exit status and the remaining
//! files must match.
//!
//! The reference executables come from `NOXI_REFERENCE_BIN` or
//! `target/nix-reference/bin` (`just reference` builds them).

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn reference_bin() -> PathBuf {
	if let Some(path) = std::env::var_os("NOXI_REFERENCE_BIN") {
		return PathBuf::from(path);
	}
	let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin");
	assert!(
		path.join("nix-store").exists(),
		"nix-reference/bin is missing; run `just reference` or set NOXI_REFERENCE_BIN"
	);
	path
}

fn command(dir: &Path, program: &Path) -> Command {
	let mut command = Command::new(program);
	command
		.env_clear()
		.env("HOME", dir.join("home"))
		.env("NIX_STATE_DIR", dir.join("state"))
		.env("NIX_CONF_DIR", dir.join("etc"));
	command
}

/// Set a symlink's modification time to `days` days ago.
fn age(link: &Path, days: i64) {
	let path = CString::new(link.as_os_str().as_bytes()).unwrap();
	let now = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap()
		.as_secs() as i64;
	let time = libc::timespec {
		tv_sec: now - days * 86400,
		tv_nsec: 0,
	};
	// SAFETY: `path` is NUL-terminated and `times` holds two timespecs.
	let status = unsafe {
		libc::utimensat(
			libc::AT_FDCWD,
			path.as_ptr(),
			[time, time].as_ptr(),
			libc::AT_SYMLINK_NOFOLLOW,
		)
	};
	assert_eq!(status, 0);
}

/// Every path below `dir`, relative and sorted, as `find` lists them.
fn listing(dir: &Path) -> Vec<String> {
	let mut paths = Vec::new();
	let mut pending = vec![dir.to_owned()];
	while let Some(path) = pending.pop() {
		for entry in fs::read_dir(&path).unwrap() {
			let path = entry.unwrap().path();
			paths.push(path.strip_prefix(dir).unwrap().display().to_string());
			if fs::symlink_metadata(&path).unwrap().is_dir() {
				pending.push(path);
			}
		}
	}
	paths.sort();
	paths
}

/// Build the scenario in `dir` and run `collector` with `args` on it.
fn run(dir: &Path, collector: &Path, args: &[&str]) -> (String, Vec<String>) {
	let _ = fs::remove_dir_all(dir);
	for sub in ["root", "home", "etc", "state/profiles/sub"] {
		fs::create_dir_all(dir.join(sub)).unwrap();
	}
	let reference = reference_bin();
	let store = format!("local?root={}", dir.join("root").display());
	let mut paths = Vec::new();
	for name in ["a", "b", "c", "d"] {
		let source = dir.join(name);
		fs::write(&source, name).unwrap();
		let output = command(dir, &reference.join("nix-store"))
			.args(["--store", &store, "--add"])
			.arg(&source)
			.output()
			.unwrap();
		assert!(output.status.success());
		paths.push(String::from_utf8(output.stdout).unwrap().trim().to_owned());
	}
	for (profile, generations) in [("test", &paths[..3]), ("sub/other", &paths[..2])] {
		for path in generations {
			let status = command(dir, &reference.join("nix-env"))
				.args(["--store", &store, "-p"])
				.arg(dir.join("state/profiles").join(profile))
				.args(["--set", path])
				.status()
				.unwrap();
			assert!(status.success());
		}
	}
	age(&dir.join("state/profiles/test-1-link"), 10);
	age(&dir.join("state/profiles/test-2-link"), 10);

	let output = command(dir, collector)
		.args(["--store", &store])
		.args(args)
		.output()
		.unwrap();
	let text = format!(
		"{}{}exit {:?}",
		String::from_utf8_lossy(&output.stdout),
		String::from_utf8_lossy(&output.stderr),
		output.status.code()
	);
	// Usage errors name the executable.
	let text = text.replace(&collector.display().to_string(), "nix-collect-garbage");
	(text, listing(dir))
}

#[test]
fn collects_garbage_like_cppnix() {
	let base = tempfile::tempdir().unwrap();
	let dir = base.path().join("scenario");
	let reference = reference_bin().join("nix-collect-garbage");
	let noxi = Path::new(env!("CARGO_BIN_EXE_nix-collect-garbage"));
	for args in [
		&["-d", "--dry-run"][..],
		&["--delete-older-than", "5d"],
		&["-d"],
		&["--max-freed", "1K", "--dry-run"],
		&["--max-freed", "+1K", "--dry-run"],
		&["-d", "--dry-run", "--"],
		&["-d", "--dry-run", "--quiet"],
		&["-d", "--dry-run", "--quiet", "--quiet"],
		&["-j1", "-k", "--delete-older-than", "5d", "--dry-run"],
	] {
		let expected = run(&dir, &reference, args);
		let actual = run(&dir, noxi, args);
		assert_eq!(actual, expected, "nix-collect-garbage {args:?}");
	}
}
