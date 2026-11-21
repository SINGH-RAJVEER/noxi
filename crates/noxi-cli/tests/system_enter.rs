//! `noxi system enter` against a fixture root in a temporary directory,
//! without root privileges: the commands run in a user namespace, so these
//! tests are skipped where unprivileged user namespaces are disabled.
//!
//! The fixture's activation script, `systemd-tmpfiles` and shell are shell
//! scripts using the host's `/bin/sh`, whose store closure the first test
//! binds into the root from a `unshare -r -m` wrapper. What a real NixOS
//! root's activation does is covered by the VM tests only.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn userns_available() -> bool {
	Command::new("unshare")
		.args(["-r", "-m", "true"])
		.status()
		.is_ok_and(|status| status.success())
}

fn script(path: &Path, shell: &Path, body: &str) {
	fs::create_dir_all(path.parent().unwrap()).unwrap();
	fs::write(path, format!("#!{}\n{body}\n", shell.display())).unwrap();
	fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A root with `etc/NIXOS`, an empty `nix/store` to bind the host store
/// onto, and a system at `/system` whose programs log what they ran.
fn fixture(shell: &Path) -> tempfile::TempDir {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path().join("root");
	fs::create_dir_all(root.join("etc")).unwrap();
	fs::create_dir_all(root.join("nix/store")).unwrap();
	fs::write(root.join("etc/NIXOS"), "").unwrap();
	let system = root.join("system");
	script(
		&system.join("activate"),
		shell,
		"echo \"activate IN_NIXOS_ENTER=$IN_NIXOS_ENTER LOCALE_ARCHIVE=$LOCALE_ARCHIVE\" > /log\necho activation-output",
	);
	script(
		&system.join("sw/bin/systemd-tmpfiles"),
		shell,
		"echo \"tmpfiles $*\" >> /log",
	);
	std::os::unix::fs::symlink(shell, system.join("sw/bin/bash")).unwrap();
	dir
}

fn host_shell() -> Option<PathBuf> {
	fs::canonicalize("/bin/sh")
		.ok()
		.filter(|shell| shell.starts_with("/nix/store"))
}

#[test]
fn activates_and_runs_the_command_in_the_chroot() {
	let Some(shell) = host_shell() else {
		eprintln!("skipped: /bin/sh is not a store path");
		return;
	};
	if !userns_available() {
		eprintln!("skipped: unprivileged user namespaces are unavailable");
		return;
	}
	let dir = fixture(&shell);
	let root = dir.path().join("root");
	let output = Command::new("unshare")
		.args(["-r", "-m", "--"])
		.arg(&shell)
		.args([
			"-c",
			"mount --rbind /nix/store \"$1/nix/store\" && exec \"$2\" system enter --root \"$1\" --system /system -c 'echo \"pwd=$(pwd) tmpdir=${TMPDIR-unset}\"; test -e /proc/self/status && echo proc'",
			"sh",
		])
		.arg(&root)
		.arg(env!("CARGO_BIN_EXE_noxi"))
		.env("TMPDIR", "/tmp")
		.output()
		.unwrap();
	let stdout = String::from_utf8_lossy(&output.stdout);
	let stderr = String::from_utf8_lossy(&output.stderr);
	assert!(output.status.success(), "{stdout}\n{stderr}");
	assert_eq!(stdout, "pwd=/ tmpdir=unset\nproc\n", "{stderr}");
	// The activation output goes to standard error.
	assert!(stderr.contains("activation-output"), "{stderr}");
	assert_eq!(
		fs::read_to_string(root.join("log")).unwrap(),
		"activate IN_NIXOS_ENTER=1 LOCALE_ARCHIVE=/system/sw/lib/locale/locale-archive\ntmpfiles --create --remove -E\n"
	);
	for name in ["dev", "sys", "proc"] {
		let mode = fs::metadata(root.join(name)).unwrap().permissions().mode();
		assert_eq!(mode & 0o777, 0o755, "{name}");
	}
}

#[test]
fn creates_its_own_user_namespace_without_root() {
	// SAFETY: `geteuid` has no preconditions.
	if unsafe { libc_geteuid() } == 0 || !userns_available() {
		eprintln!("skipped: needs an unprivileged caller with user namespaces");
		return;
	}
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path();
	fs::create_dir_all(root.join("etc")).unwrap();
	fs::write(root.join("etc/NIXOS"), "").unwrap();
	let output = Command::new(env!("CARGO_BIN_EXE_noxi"))
		.args(["system", "enter", "--silent", "--root"])
		.arg(root)
		.args(["--", "/missing"])
		.output()
		.unwrap();
	let stderr = String::from_utf8_lossy(&output.stderr);
	// The namespaces and bind mounts succeeded; only the program is missing.
	assert!(!output.status.success());
	assert!(stderr.contains("cannot run '/missing'"), "{stderr}");
	assert!(root.join("proc").is_dir());
}

#[test]
fn refuses_a_root_that_is_not_nixos() {
	let dir = tempfile::tempdir().unwrap();
	let output = Command::new(env!("CARGO_BIN_EXE_noxi"))
		.args(["system", "enter", "--root"])
		.arg(dir.path())
		.output()
		.unwrap();
	assert_eq!(output.status.code(), Some(126));
}

unsafe extern "C" {
	#[link_name = "geteuid"]
	fn libc_geteuid() -> u32;
}
