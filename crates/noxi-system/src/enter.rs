//! `noxi system enter`: the native `nixos-enter`.
//!
//! It follows nixpkgs' `pkgs/by-name/ni/nixos-enter/nixos-enter.sh` step by
//! step, with system calls in place of the util-linux programs:
//!
//! 1. Move into a private mount and UTS namespace, as the script's
//!    `unshare --fork --mount --uts` re-execution does, so every mount below
//!    disappears with the process. Without root it also creates a user
//!    namespace mapping the caller to root, as `unshare -r` does. The script's
//!    `--mount-proc` only remounts `/proc` for a PID namespace it never
//!    creates, so it is not reproduced. Then `mount --make-rprivate /`.
//! 2. Refuse a root without `etc/NIXOS` with exit status 126.
//! 3. Create `dev`, `sys` and `proc` with mode 0755 and recursively bind the
//!    host's `/dev`, `/sys` and `/proc` onto them.
//! 4. Bind the host's `/etc/resolv.conf` over the root's, following one
//!    level of symbolic link and creating an empty file to bind onto, as
//!    arch-chroot does. A failure here is a warning.
//! 5. Run `SYSTEM/activate` in the chroot with `LOCALE_ARCHIVE` and
//!    `IN_NIXOS_ENTER=1` and its output sent to standard error, then
//!    `SYSTEM/sw/bin/systemd-tmpfiles --create --remove -E`, ignoring both
//!    results. This is what populates `/etc` and points `/run/current-system`
//!    at the system. `--silent` discards the activation output.
//! 6. Unset `TMPDIR` and replace this process with the command in the
//!    chroot: `SYSTEM/sw/bin/bash --login` by default, `bash -c COMMAND`
//!    with `--command`, or the arguments after `--`.
//!
//! The namespace calls need a single-threaded process, which `noxi` is when
//! it dispatches here. Usage errors exit with status 2 instead of the
//! script's 1, as every `noxi` command does.

use std::ffi::{CString, OsStr};
use std::fs;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use noxi_events::Styling;

use crate::snapshot::DEFAULT_PROFILE;

pub const USAGE: &str = "usage: noxi system enter [--root ROOT] [--system SYSTEM] [--command COMMAND | -c COMMAND] [--silent] [-- PROGRAM ARGS...]";

/// The root entered when none is given.
pub const DEFAULT_ROOT: &str = "/mnt";
/// The exit status for a root that is not a NixOS installation.
pub const NOT_NIXOS: u8 = 126;

/// What to run in the chroot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Program {
	/// `SYSTEM/sw/bin/bash --login`.
	LoginShell,
	/// `SYSTEM/sw/bin/bash -c COMMAND`.
	Shell(String),
	/// The arguments after `--`, run without a shell.
	Exec(Vec<String>),
}

/// A parsed `noxi system enter` command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
	pub root: PathBuf,
	/// The system inside the root, normally the system profile.
	pub system: PathBuf,
	pub program: Program,
	pub silent: bool,
}

impl Default for Options {
	fn default() -> Self {
		Options {
			root: PathBuf::from(DEFAULT_ROOT),
			system: PathBuf::from(DEFAULT_PROFILE),
			program: Program::LoginShell,
			silent: false,
		}
	}
}

impl Options {
	/// The argument vector run in the chroot.
	pub fn argv(&self) -> Vec<String> {
		let bash = self
			.system
			.join("sw/bin/bash")
			.to_string_lossy()
			.into_owned();
		match &self.program {
			Program::LoginShell => vec![bash, "--login".to_owned()],
			Program::Shell(command) => vec![bash, "-c".to_owned(), command.clone()],
			Program::Exec(argv) => argv.clone(),
		}
	}
}

/// Parse the arguments after `noxi system enter`; `None` for `--help`.
pub fn parse(args: Vec<String>) -> Result<Option<Options>, String> {
	let mut options = Options::default();
	let mut args = args.into_iter();
	let value = |flag: &str, args: &mut dyn Iterator<Item = String>| {
		args.next()
			.ok_or_else(|| format!("flag '{flag}' requires an argument"))
	};
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--root" => options.root = PathBuf::from(value(&arg, &mut args)?),
			"--system" => options.system = PathBuf::from(value(&arg, &mut args)?),
			"--command" | "-c" => options.program = Program::Shell(value(&arg, &mut args)?),
			"--silent" => options.silent = true,
			"-h" | "--help" => return Ok(None),
			"--" => {
				let argv: Vec<String> = args.by_ref().collect();
				if argv.is_empty() {
					return Err("'--' must be followed by a program".to_owned());
				}
				options.program = Program::Exec(argv);
			}
			other => return Err(format!("unknown option '{other}'")),
		}
	}
	Ok(Some(options))
}

/// Whether `root` holds a NixOS installation: it has `etc/NIXOS`.
pub fn is_nixos(root: &Path) -> bool {
	root.join("etc/NIXOS").exists()
}

/// The file in `root` to bind the host's resolv.conf onto: the root's
/// `etc/resolv.conf`, or the target of that link resolved inside the root
/// when it is a symbolic link. Nested links and `..` are not followed,
/// as in the script.
pub fn resolv_conf_target(root: &Path) -> PathBuf {
	let resolv = root.join("etc/resolv.conf");
	match fs::read_link(&resolv) {
		Ok(target) if target.is_absolute() => {
			root.join(target.strip_prefix("/").unwrap_or(&target))
		}
		Ok(target) => root.join("etc").join(target),
		Err(_) => resolv,
	}
}

fn c_path(path: &Path) -> io::Result<CString> {
	CString::new(path.as_os_str().as_bytes())
		.map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

fn check(result: libc::c_int, what: impl FnOnce() -> String) -> Result<(), String> {
	if result == 0 {
		Ok(())
	} else {
		Err(format!("{}: {}", what(), io::Error::last_os_error()))
	}
}

fn mount(
	source: Option<&Path>,
	target: &Path,
	flags: libc::c_ulong,
	what: impl FnOnce() -> String,
) -> Result<(), String> {
	let source = source.map(c_path).transpose().map_err(|e| e.to_string())?;
	let target = c_path(target).map_err(|e| e.to_string())?;
	// SAFETY: the strings are NUL-terminated and outlive the call.
	let result = unsafe {
		libc::mount(
			source.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
			target.as_ptr(),
			std::ptr::null(),
			flags,
			std::ptr::null(),
		)
	};
	check(result, what)
}

/// Step 1: a private mount and UTS namespace, and a user namespace mapping
/// the caller to root when the caller is not root.
pub fn unshare_namespaces() -> Result<(), String> {
	// SAFETY: these calls have no memory preconditions.
	let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
	let mut flags = libc::CLONE_NEWNS | libc::CLONE_NEWUTS;
	if uid != 0 {
		flags |= libc::CLONE_NEWUSER;
	}
	// SAFETY: as above; the kernel checks the flags.
	check(unsafe { libc::unshare(flags) }, || {
		"cannot create a private mount namespace".to_owned()
	})?;
	if uid != 0 {
		let write = |file: &str, text: String| {
			fs::write(file, text).map_err(|e| format!("cannot write {file}: {e}"))
		};
		write("/proc/self/setgroups", "deny".to_owned())?;
		write("/proc/self/uid_map", format!("0 {uid} 1"))?;
		write("/proc/self/gid_map", format!("0 {gid} 1"))?;
	}
	mount(
		None,
		Path::new("/"),
		libc::MS_REC | libc::MS_PRIVATE,
		|| "cannot make the mounts of / private".to_owned(),
	)
}

/// Steps 3 and 4: the `dev`, `sys` and `proc` bind mounts, and resolv.conf.
/// The resolv.conf failure is returned as a warning.
pub fn bind_mounts(root: &Path) -> Result<Option<String>, String> {
	for name in ["dev", "sys", "proc"] {
		let target = root.join(name);
		fs::create_dir_all(&target)
			.and_then(|()| fs::set_permissions(&target, fs::Permissions::from_mode(0o755)))
			.map_err(|e| format!("cannot create \"{}\": {e}", target.display()))?;
		let source = Path::new("/").join(name);
		mount(Some(&source), &target, libc::MS_BIND | libc::MS_REC, || {
			format!(
				"cannot bind {} onto \"{}\"",
				source.display(),
				target.display()
			)
		})?;
	}
	Ok(bind_resolv_conf(root).err())
}

fn bind_resolv_conf(root: &Path) -> Result<(), String> {
	let host = Path::new("/etc/resolv.conf");
	if !host.exists() {
		return Ok(());
	}
	let target = resolv_conf_target(root);
	if !target.is_file() {
		if let Some(parent) = target.parent() {
			fs::create_dir_all(parent).map_err(|e| e.to_string())?;
		}
		fs::write(&target, "").map_err(|e| e.to_string())?;
		fs::set_permissions(&target, fs::Permissions::from_mode(0o644))
			.map_err(|e| e.to_string())?;
	}
	mount(Some(host), &target, libc::MS_BIND, || {
		format!("cannot bind /etc/resolv.conf onto \"{}\"", target.display())
	})
}

/// A command run with `root` as its root directory and working directory,
/// as chroot(1) runs it.
pub fn chrooted(root: &Path, program: impl AsRef<OsStr>) -> Command {
	let mut command = Command::new(program);
	let root = c_path(root).ok();
	// SAFETY: the closure only makes async-signal-safe system calls and
	// does not allocate.
	unsafe {
		command.pre_exec(move || {
			let Some(root) = &root else {
				return Err(io::Error::from_raw_os_error(libc::EINVAL));
			};
			if libc::chroot(root.as_ptr()) != 0 || libc::chdir(c"/".as_ptr()) != 0 {
				return Err(io::Error::last_os_error());
			}
			Ok(())
		});
	}
	command
}

fn stderr_stdio() -> Stdio {
	io::stderr()
		.as_fd()
		.try_clone_to_owned()
		.map_or_else(|_| Stdio::inherit(), Stdio::from)
}

/// Step 5: run the system's activation script and `systemd-tmpfiles` in the
/// chroot, ignoring their exit statuses as the script does.
pub fn activate(options: &Options) {
	let (stdout, stderr) = if options.silent {
		(Stdio::null(), Stdio::null())
	} else {
		(stderr_stdio(), stderr_stdio())
	};
	let _ = chrooted(&options.root, options.system.join("activate"))
		.env(
			"LOCALE_ARCHIVE",
			options.system.join("sw/lib/locale/locale-archive"),
		)
		.env("IN_NIXOS_ENTER", "1")
		.stdout(stdout)
		.stderr(stderr)
		.status();
	// Hide the "failed to replace specifiers" errors a missing
	// /etc/machine-id causes.
	let _ = chrooted(
		&options.root,
		options.system.join("sw/bin/systemd-tmpfiles"),
	)
	.args(["--create", "--remove", "-E"])
	.stderr(Stdio::null())
	.status();
}

/// Run every step and replace this process with the command. Returns only
/// on failure.
pub fn enter(options: &Options, err: &mut dyn Write, styling: &Styling) -> ExitCode {
	if let Err(message) = unshare_namespaces() {
		return crate::fail(err, styling, None, &message);
	}
	if !is_nixos(&options.root) {
		return not_nixos(&options.root, err, styling);
	}
	match bind_mounts(&options.root) {
		Ok(None) => {}
		Ok(Some(warning)) => {
			let _ = writeln!(
				err,
				"{}: failed to set up resolv.conf: {warning}",
				styling.paint(noxi_events::style::WARNING, "warning")
			);
		}
		Err(message) => return crate::fail(err, styling, None, &message),
	}
	activate(options);
	let argv = options.argv();
	let error = chrooted(&options.root, &argv[0])
		.args(&argv[1..])
		.env_remove("TMPDIR")
		.exec();
	crate::fail(
		err,
		styling,
		None,
		&format!(
			"cannot run '{}' in \"{}\": {error}",
			argv[0],
			options.root.display()
		),
	)
}

fn not_nixos(root: &Path, err: &mut dyn Write, styling: &Styling) -> ExitCode {
	let _ = crate::fail(
		err,
		styling,
		None,
		&format!("'{}' is not a NixOS installation", root.display()),
	);
	ExitCode::from(NOT_NIXOS)
}

pub fn main(args: Vec<String>, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
	match parse(args) {
		Ok(Some(options)) => {
			let styling = Styling::for_stderr();
			// Checked before the namespace too, so the common mistake fails
			// without needing namespace privileges.
			if !is_nixos(&options.root) {
				return not_nixos(&options.root, err, &styling);
			}
			enter(&options, err, &styling)
		}
		Ok(None) => {
			let _ = writeln!(out, "{USAGE}");
			ExitCode::SUCCESS
		}
		Err(message) => {
			let _ = writeln!(err, "error: {message}\n{USAGE}");
			ExitCode::from(2)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn args(list: &[&str]) -> Vec<String> {
		list.iter().map(|arg| (*arg).to_owned()).collect()
	}

	#[test]
	fn defaults_match_nixos_enter() {
		let options = parse(Vec::new()).unwrap().unwrap();
		assert_eq!(options.root, Path::new("/mnt"));
		assert_eq!(
			options.argv(),
			args(&["/nix/var/nix/profiles/system/sw/bin/bash", "--login"])
		);
	}

	#[test]
	fn command_forms() {
		let options = parse(args(&["--system", "/s", "-c", "ls -l /"]))
			.unwrap()
			.unwrap();
		assert_eq!(options.argv(), args(&["/s/sw/bin/bash", "-c", "ls -l /"]));
		let options = parse(args(&["--root", "/r", "--silent", "--", "true", "--root"]))
			.unwrap()
			.unwrap();
		assert_eq!(options.root, Path::new("/r"));
		assert!(options.silent);
		assert_eq!(options.argv(), args(&["true", "--root"]));
		assert!(parse(args(&["--"])).is_err());
		assert!(parse(args(&["--bogus"])).is_err());
		assert!(parse(args(&["--root"])).is_err());
		assert_eq!(parse(args(&["--help"])).unwrap(), None);
	}

	#[test]
	fn resolv_conf_follows_one_link_inside_the_root() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path();
		fs::create_dir(root.join("etc")).unwrap();
		assert_eq!(resolv_conf_target(root), root.join("etc/resolv.conf"));
		std::os::unix::fs::symlink(
			"/run/systemd/resolve/stub-resolv.conf",
			root.join("etc/resolv.conf"),
		)
		.unwrap();
		assert_eq!(
			resolv_conf_target(root),
			root.join("run/systemd/resolve/stub-resolv.conf")
		);
		fs::remove_file(root.join("etc/resolv.conf")).unwrap();
		std::os::unix::fs::symlink("static/resolv.conf", root.join("etc/resolv.conf")).unwrap();
		assert_eq!(
			resolv_conf_target(root),
			root.join("etc/static/resolv.conf")
		);
	}

	#[test]
	fn a_root_without_etc_nixos_is_refused_with_126() {
		let dir = tempfile::tempdir().unwrap();
		let (mut out, mut err) = (Vec::new(), Vec::new());
		let code = main(
			args(&["--root", dir.path().to_str().unwrap()]),
			&mut out,
			&mut err,
		);
		assert_eq!(code, ExitCode::from(NOT_NIXOS));
		let err = String::from_utf8(err).unwrap();
		assert!(err.contains("is not a NixOS installation"), "{err}");
	}
}
