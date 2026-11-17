//! The `auto` store's fallback to a per-user chroot store, compared with
//! CppNix 2.35.2's `resolveStoreConfig`: with no `/nix/var/nix`, no daemon
//! socket and no root privileges, both engines keep the `/nix/store` names
//! of store paths but put the store in `DATA_DIR/root`.
//!
//! The host's `/nix/var` is hidden by a tmpfs inside an unprivileged user
//! and mount namespace, and a nested user namespace maps the caller back to
//! its own uid, so the commands run as a non-root user on a host whose
//! `/nix/store` (holding both engines' dependencies) is still visible. The
//! test returns with a notice when the reference or unprivileged user
//! namespaces are unavailable.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const TO_FILE: &str = "builtins.toFile \"chroot-store-source\" \"kept in a per-user store\"";
const DERIVATION: &str = "derivation { name = \"chroot-store-drv\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo > $out\" ]; }";

fn reference_dir() -> Option<PathBuf> {
	let dir = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		});
	if dir.join("nix-instantiate").exists() {
		Some(dir)
	} else {
		eprintln!("skipping: no reference nix in {}", dir.display());
		None
	}
}

fn candidate_dir() -> PathBuf {
	Path::new(env!("CARGO_BIN_EXE_nix-instantiate"))
		.parent()
		.unwrap()
		.to_owned()
}

/// `program args` as a non-root user on a host without `/nix/var`, with only
/// the given environment.
fn hidden_nix(program: &Path, args: &[&str], env: &[(&str, &Path)]) -> Command {
	// SAFETY: getuid and getgid cannot fail.
	let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
	let mut command = Command::new("unshare");
	command
		.args(["--user", "--map-root-user", "--mount", "--", "sh", "-c"])
		.arg(
			"mount -t tmpfs none /nix/var || exit 99
uid=$1 gid=$2
shift 2
exec unshare --user --map-user=\"$uid\" --map-group=\"$gid\" -- \"$@\"",
		)
		.arg("hidden-nix")
		.arg(uid.to_string())
		.arg(gid.to_string())
		.arg(program)
		.args(args)
		.env_clear()
		.env(
			"PATH",
			std::env::var_os("PATH").unwrap_or_else(|| "/run/current-system/sw/bin:/bin".into()),
		)
		.env("NIX_CONFIG", "substituters =\nsandbox = false\n");
	for (name, value) in env {
		command.env(name, value);
	}
	command
}

fn namespaces_available() -> bool {
	let ok = hidden_nix(Path::new("true"), &[], &[])
		.output()
		.is_ok_and(|output| output.status.success());
	if !ok {
		eprintln!("skipping: unprivileged user and mount namespaces are unavailable");
	}
	ok
}

/// The directories and files below `dir`, relative and sorted.
fn tree(dir: &Path) -> Vec<String> {
	let mut out = Vec::new();
	let mut pending = vec![dir.to_owned()];
	while let Some(path) = pending.pop() {
		let Ok(entries) = fs::read_dir(&path) else {
			continue;
		};
		for entry in entries.flatten() {
			let path = entry.path();
			let relative = path.strip_prefix(dir).unwrap().display().to_string();
			// The database, its journal and the lock files are not compared.
			if relative.ends_with("/nix/var") || relative.contains("/nix/var/") {
				continue;
			}
			out.push(relative);
			if entry.file_type().is_ok_and(|t| t.is_dir()) {
				pending.push(path);
			}
		}
	}
	out.sort();
	out
}

/// Run `steps` with one engine in a fresh `root`, returning each step's
/// status and output, with `root` replaced by `ROOT`, then the files made.
fn session(bin: &Path, root: &Path, steps: &[(&str, &[&str], &[(&str, &str)])]) -> Vec<String> {
	if root.exists() {
		fs::remove_dir_all(root).unwrap();
	}
	fs::create_dir_all(root.join("home")).unwrap();
	fs::create_dir_all(root.join("conf")).unwrap();
	let home = root.join("home");
	let conf = root.join("conf");
	let mut out = Vec::new();
	for (program, args, extra) in steps {
		let extra: Vec<(&str, PathBuf)> = extra
			.iter()
			.map(|(name, value)| (*name, root.join(value)))
			.collect();
		let mut env = vec![("HOME", home.as_path()), ("NIX_CONF_DIR", conf.as_path())];
		env.extend(extra.iter().map(|(name, value)| (*name, value.as_path())));
		let output = hidden_nix(&bin.join(program), args, &env).output().unwrap();
		let mask = |bytes: &[u8]| {
			String::from_utf8_lossy(bytes).replace(&root.display().to_string(), "ROOT")
		};
		out.push(format!(
			"$ {program} {}\nstatus {:?}\nstdout {}\nstderr {}",
			args.join(" "),
			output.status.code(),
			mask(&output.stdout),
			mask(&output.stderr),
		));
	}
	out.extend(tree(root));
	out
}

#[test]
fn auto_store_falls_back_to_a_per_user_chroot_store_like_cppnix() {
	let Some(reference) = reference_dir() else {
		return;
	};
	if !namespaces_available() {
		return;
	}
	let eval: &[&str] = &["--eval", "--read-write-mode", "-E", TO_FILE];
	let steps: &[(&str, &[&str], &[(&str, &str)])] = &[
		// The first use creates the root and warns; later uses stay quiet.
		("nix-instantiate", eval, &[]),
		("nix-instantiate", eval, &[]),
		("nix-instantiate", &["-E", DERIVATION], &[]),
		// The data directory follows XDG_DATA_HOME, then NIX_DATA_HOME.
		("nix-instantiate", eval, &[("XDG_DATA_HOME", "xdg")]),
		(
			"nix-instantiate",
			eval,
			&[("XDG_DATA_HOME", "xdg"), ("NIX_DATA_HOME", "data")],
		),
	];
	let root = tempfile::tempdir().unwrap();
	let root = root.path().join("engine");
	let expected = session(&reference, &root, steps);
	let actual = session(&candidate_dir(), &root, steps);
	assert_eq!(actual, expected);
	assert!(
		expected[0].contains("as a chroot store"),
		"the reference did not fall back: {}",
		expected[0]
	);
	// The reference accepts the store noxi wrote, which is still in place.
	let verify = hidden_nix(
		&reference.join("nix-store"),
		&["--verify", "--check-contents"],
		&[
			("HOME", &root.join("home")),
			("NIX_CONF_DIR", &root.join("conf")),
		],
	)
	.output()
	.unwrap();
	assert!(
		verify.status.success(),
		"{}",
		String::from_utf8_lossy(&verify.stderr)
	);
}

#[test]
fn explicit_state_dir_disables_the_chroot_store_fallback_like_cppnix() {
	let Some(reference) = reference_dir() else {
		return;
	};
	if !namespaces_available() {
		return;
	}
	let root = tempfile::tempdir().unwrap();
	let root = root.path().join("engine");
	let state: &[(&str, &str)] = &[("NIX_STATE_DIR", "state")];
	let steps: &[(&str, &[&str], &[(&str, &str)])] = &[(
		"nix-instantiate",
		&["--eval", "--read-write-mode", "-E", TO_FILE],
		state,
	)];
	// Without the fallback both engines try the unwritable /nix/store. Their
	// messages for that, and the lock files they leave in the state
	// directory, differ, so only the status and what appears in the home
	// directory are compared.
	let summary = |lines: Vec<String>| {
		let status = lines[0]
			.lines()
			.find(|line| line.starts_with("status"))
			.unwrap()
			.to_owned();
		assert!(!lines[0].contains("chroot store"), "{}", lines[0]);
		let home: Vec<String> = lines[1..]
			.iter()
			.filter(|path| path.starts_with("home"))
			.cloned()
			.collect();
		(status, home)
	};
	let expected = summary(session(&reference, &root, steps));
	let actual = summary(session(&candidate_dir(), &root, steps));
	assert_eq!(actual, expected);
	assert!(
		!expected
			.1
			.iter()
			.any(|path| path.contains(".local/share/nix")),
		"{:?}",
		expected.1
	);
}
