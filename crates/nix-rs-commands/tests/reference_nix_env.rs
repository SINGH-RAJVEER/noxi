//! `nix-env`'s profile operations checked against the pinned CppNix 2.35.2
//! reference.
//!
//! The reference adds three sources to a `local?root=` store. Each engine's
//! `nix-env` then runs the same sequence of `--set`, `--list-generations`,
//! `--rollback`, `--switch-generation`, `--delete-generations`,
//! `--switch-profile` and failing invocations on a fresh copy of the
//! scenario in one directory, so the names of the indirect roots, which
//! hash their link paths, agree. Output, exit statuses and the resulting
//! files must match, apart from the substituter cache CppNix creates under
//! `~/.cache`.
//!
//! The reference executables come from `NIXRS_REFERENCE_BIN` or
//! `target/nix-reference/bin` (`just reference` builds them).

use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn reference_bin() -> PathBuf {
	if let Some(path) = std::env::var_os("NIXRS_REFERENCE_BIN") {
		return PathBuf::from(path);
	}
	let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin");
	assert!(
		path.join("nix-store").exists(),
		"nix-reference/bin is missing; run `just reference` or set NIXRS_REFERENCE_BIN"
	);
	path
}

fn command(dir: &Path, program: &Path) -> Command {
	let mut command = Command::new(program);
	command
		.current_dir(dir)
		.env_clear()
		.env("HOME", dir.join("home"))
		.env("NIX_STATE_DIR", dir.join("state"))
		.env("NIX_CONF_DIR", dir.join("etc"))
		.env("TZ", "UTC");
	command
}

/// Date every generation link of `profile` at a fixed time, so that
/// `--list-generations` prints the same times for both engines.
fn fix_times(profile: &Path) {
	let dir = profile.parent().unwrap();
	let prefix = format!("{}-", profile.file_name().unwrap().to_string_lossy());
	for entry in fs::read_dir(dir).unwrap() {
		let path = entry.unwrap().path();
		if !path
			.file_name()
			.unwrap()
			.to_string_lossy()
			.starts_with(&prefix)
		{
			continue;
		}
		let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
		let time = libc::timespec {
			tv_sec: 1_767_323_045,
			tv_nsec: 0,
		};
		// SAFETY: `c_path` is NUL-terminated and `times` holds two timespecs.
		let status = unsafe {
			libc::utimensat(
				libc::AT_FDCWD,
				c_path.as_ptr(),
				[time, time].as_ptr(),
				libc::AT_SYMLINK_NOFOLLOW,
			)
		};
		assert_eq!(status, 0);
	}
}

/// Every path below `dir` except the substituter cache, relative and
/// sorted, with symlink targets.
fn listing(dir: &Path) -> Vec<String> {
	let mut paths = Vec::new();
	let mut pending = vec![dir.to_owned()];
	while let Some(path) = pending.pop() {
		for entry in fs::read_dir(&path).unwrap() {
			let path = entry.unwrap().path();
			let relative = path.strip_prefix(dir).unwrap().display().to_string();
			if relative == "home/.cache" {
				continue;
			}
			let metadata = fs::symlink_metadata(&path).unwrap();
			if metadata.is_symlink() {
				let target = fs::read_link(&path).unwrap();
				paths.push(format!("{relative} -> {}", target.display()));
			} else {
				paths.push(relative);
			}
			if metadata.is_dir() {
				pending.push(path);
			}
		}
	}
	paths.sort();
	paths
}

/// Build the scenario in `dir` and run the sequence with `nix_env`.
fn run(dir: &Path, nix_env: &Path) -> (String, Vec<String>) {
	let _ = fs::remove_dir_all(dir);
	for sub in ["root", "home", "etc", "state/profiles"] {
		fs::create_dir_all(dir.join(sub)).unwrap();
	}
	let store = format!("local?root={}", dir.join("root").display());
	let mut paths = Vec::new();
	for name in ["a", "b", "c"] {
		let source = dir.join(name);
		fs::write(&source, name).unwrap();
		let output = command(dir, &reference_bin().join("nix-store"))
			.args(["--store", &store, "--add"])
			.arg(&source)
			.output()
			.unwrap();
		assert!(output.status.success());
		paths.push(String::from_utf8(output.stdout).unwrap().trim().to_owned());
	}
	let profile = dir.join("state/profiles/test");
	let profile = profile.to_str().unwrap();
	let (a, b, c) = (paths[0].as_str(), paths[1].as_str(), paths[2].as_str());
	let missing = "/nix/store/00000000000000000000000000000000-x";
	let not_in_store = dir.join("a").display().to_string();
	let sequence: Vec<Vec<&str>> = vec![
		vec!["-p", profile, "--set", a],
		vec!["-p", profile, "--set", b],
		vec!["-p", profile, "--set", b],
		vec!["-p", profile, "--set", c, "--dry-run"],
		vec!["-p", profile, "--set", c],
		vec!["-p", profile, "--list-generations"],
		vec!["-p", profile, "--rollback"],
		vec!["-p", profile, "--rollback", "--dry-run"],
		vec!["-p", profile, "--switch-generation", "3"],
		vec!["-p", profile, "-G", "9"],
		vec!["-p", profile, "--switch-generation", "x"],
		vec!["-p", profile, "--rollback", "foo"],
		vec!["-p", profile, "--rollback", "--bogus"],
		vec!["-p", profile, "--set", a, b],
		vec!["-p", profile, "--set", &not_in_store],
		vec!["-p", profile, "--set", missing],
		vec!["-p", profile],
		vec!["-p", profile, "--set", a, "--rollback"],
		vec!["-p", profile, "--delete-generations", "3"],
		vec!["-p", profile, "--delete-generations", "+1", "--dry-run"],
		vec!["-p", profile, "--delete-generations", "old", "--dry-run"],
		vec!["-p", profile, "--delete-generations", "+"],
		vec!["-p", profile, "--delete-generations", "1", "2"],
		vec!["-p", profile, "--list-generations"],
		vec!["--version"],
		vec!["-S", profile],
	];
	let mut text = String::new();
	for args in sequence {
		if args.contains(&"--list-generations") {
			fix_times(Path::new(profile));
		}
		let output = command(dir, nix_env)
			.args(["--store", &store])
			.args(&args)
			.output()
			.unwrap();
		text.push_str(&format!(
			"--- {args:?}\n{}{}exit {:?}\n",
			String::from_utf8_lossy(&output.stdout),
			String::from_utf8_lossy(&output.stderr),
			output.status.code()
		));
	}
	// Usage errors name the executable.
	let text = text.replace(&nix_env.display().to_string(), "nix-env");
	(text, listing(dir))
}

#[test]
fn manages_profiles_like_cppnix() {
	let base = tempfile::tempdir().unwrap();
	let dir = base.path().join("scenario");
	let expected = run(&dir, &reference_bin().join("nix-env"));
	let actual = run(&dir, Path::new(env!("CARGO_BIN_EXE_nix-env")));
	assert_eq!(actual.0, expected.0);
	assert_eq!(actual.1, expected.1);
}
