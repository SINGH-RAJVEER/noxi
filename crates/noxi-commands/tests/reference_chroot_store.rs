//! Per-user chroot stores compared with CppNix 2.35.2.
//!
//! - `resolveStoreConfig`'s fallback: with no `/nix/var/nix`, no daemon
//!   socket and no root privileges, both engines keep the `/nix/store` names
//!   of store paths but put the store in `DATA_DIR/root`.
//! - Sandboxed builds into that store, and `nix shell` running their
//!   outputs through the store execution helper.
//! - `chrootHelper`'s path for a logical store directory that does not
//!   exist on the host, where the helper runs the program in a temporary
//!   root of bind mounts.
//!
//! The host's `/nix/var` is hidden by a tmpfs inside an unprivileged user
//! and mount namespace, and a nested user namespace maps the caller back to
//! its own uid, so the commands run as a non-root user on a host whose
//! `/nix/store` (holding both engines' dependencies) is still visible. The
//! tests return with a notice when the reference, the static busybox or
//! unprivileged user namespaces are unavailable.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const TO_FILE: &str = "builtins.toFile \"chroot-store-source\" \"kept in a per-user store\"";
const DERIVATION: &str = "derivation { name = \"chroot-store-drv\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo > $out\" ]; }";

/// A program `hello` that reports its working directory and arguments,
/// built in the sandbox with the busybox copied to `ROOT/bb`.
const HELLO: &str = r#"let bb = builtins.path { path = ROOT/bb; name = "busybox"; }; in
derivation {
  name = "hello";
  system = builtins.currentSystem;
  builder = "${bb}/sh";
  args = [ "-c" ''
    ${bb}/mkdir -p $out/bin
    echo '#!${bb}/sh' > $out/bin/hello
    echo 'echo hello from $(${bb}/pwd) "$@"; exit 3' >> $out/bin/hello
    ${bb}/chmod +x $out/bin/hello
    echo built
  '' ];
}
"#;

/// One command: the program in the engine's directory, its arguments
/// (`ROOT` is replaced by the session root and `PREV` by the previous
/// step's trimmed standard output), and extra environment variables whose
/// values are joined to the root.
type Step<'a> = (&'a str, &'a [&'a str], &'a [(&'a str, &'a str)]);

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

fn busybox_bin() -> Option<PathBuf> {
	let dir = std::env::var_os("NOXI_TEST_BUSYBOX")
		.map(PathBuf::from)
		.unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-busybox"))
		.join("bin");
	if dir.join("busybox").exists() {
		Some(dir)
	} else {
		eprintln!("skipping: no static busybox in {}", dir.display());
		None
	}
}

fn candidate_dir() -> PathBuf {
	Path::new(env!("CARGO_BIN_EXE_nix-instantiate"))
		.parent()
		.unwrap()
		.to_owned()
}

/// `program args` with only the given environment and no substituters.
fn plain(program: &Path, args: &[&str], env: &[(&str, &Path)]) -> Command {
	let mut command = Command::new(program);
	command
		.args(args)
		.env_clear()
		.env(
			"PATH",
			std::env::var_os("PATH").unwrap_or_else(|| "/run/current-system/sw/bin:/bin".into()),
		)
		.env("NIX_CONFIG", "substituters =\n");
	for (name, value) in env {
		command.env(name, value);
	}
	command
}

/// `program args` as a non-root user on a host without `/nix/var`, with only
/// the given environment and no substituters.
fn hidden_nix(program: &Path, args: &[&str], env: &[(&str, &Path)]) -> Command {
	// SAFETY: getuid and getgid cannot fail.
	let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
	let mut command = plain(Path::new("unshare"), &[], env);
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
		.args(args);
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

/// How a session runs its commands.
struct Session<'a> {
	/// `nix.conf` for both engines.
	config: &'a str,
	/// Files to create in the fresh root before the first step.
	prepare: &'a dyn Fn(&Path),
	/// Whether the host's `/nix/var` is hidden.
	hide_nix: bool,
}

impl Session<'_> {
	/// Run `steps` with one engine in a fresh `root`, from `root`, returning
	/// each step's status and output with `root` replaced by `ROOT`, then
	/// the files made.
	fn run(&self, bin: &Path, root: &Path, steps: &[Step]) -> Vec<String> {
		noxi_test_support::remove_tree(root).unwrap();
		let home = root.join("home");
		let conf = root.join("conf");
		fs::create_dir_all(&home).unwrap();
		fs::create_dir_all(&conf).unwrap();
		fs::write(conf.join("nix.conf"), self.config).unwrap();
		(self.prepare)(root);
		let root_str = root.display().to_string();
		let mut previous = String::new();
		let mut out = Vec::new();
		for (program, args, extra) in steps {
			let extra: Vec<(&str, PathBuf)> = extra
				.iter()
				.map(|(name, value)| (*name, root.join(value)))
				.collect();
			let mut env = vec![("HOME", home.as_path()), ("NIX_CONF_DIR", conf.as_path())];
			env.extend(extra.iter().map(|(name, value)| (*name, value.as_path())));
			let args: Vec<String> = args
				.iter()
				.map(|arg| arg.replace("ROOT", &root_str).replace("PREV", &previous))
				.collect();
			let args: Vec<&str> = args.iter().map(String::as_str).collect();
			let wrap = if self.hide_nix { hidden_nix } else { plain };
			let output = wrap(&bin.join(program), &args, &env)
				.current_dir(root)
				.output()
				.unwrap();
			previous = String::from_utf8_lossy(&output.stdout).trim().to_owned();
			let mask = |bytes: &[u8]| String::from_utf8_lossy(bytes).replace(&root_str, "ROOT");
			out.push(format!(
				"$ {program} {}\nstatus {:?}\nstdout {}\nstderr {}",
				args.join(" ").replace(&root_str, "ROOT"),
				output.status.code(),
				mask(&output.stdout),
				mask(&output.stderr),
			));
		}
		out.extend(tree(root));
		out
	}
}

const EVAL_ONLY: Session = Session {
	config: "",
	prepare: &|_| {},
	hide_nix: true,
};

/// Copy the static busybox to `ROOT/bb` and write the `hello` expression.
fn prepare_hello(busybox: PathBuf) -> impl Fn(&Path) {
	move |root| {
		let status = Command::new("cp")
			.arg("-a")
			.arg(&busybox)
			.arg(root.join("bb"))
			.status()
			.unwrap();
		assert!(status.success());
		let status = Command::new("chmod")
			.args(["-R", "u+w"])
			.arg(root.join("bb"))
			.status()
			.unwrap();
		assert!(status.success());
		fs::write(
			root.join("default.nix"),
			HELLO.replace("ROOT", &root.display().to_string()),
		)
		.unwrap();
	}
}

/// The reference's `nix-store --verify --check-contents` on the per-user
/// store in `root`, which noxi wrote last.
fn assert_reference_verifies(reference: &Path, root: &Path) {
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
fn auto_store_falls_back_to_a_per_user_chroot_store_like_cppnix() {
	let Some(reference) = reference_dir() else {
		return;
	};
	if !namespaces_available() {
		return;
	}
	let eval: &[&str] = &["--eval", "--read-write-mode", "-E", TO_FILE];
	let steps: &[Step] = &[
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
	let root = noxi_test_support::tempdir().unwrap();
	let root = root.path().join("engine");
	let expected = EVAL_ONLY.run(&reference, &root, steps);
	let actual = EVAL_ONLY.run(&candidate_dir(), &root, steps);
	assert_eq!(actual, expected);
	assert!(
		expected[0].contains("as a chroot store"),
		"the reference did not fall back: {}",
		expected[0]
	);
	assert_reference_verifies(&reference, &root);
	noxi_test_support::remove_tree(&root).unwrap();
}

#[test]
fn explicit_state_dir_disables_the_chroot_store_fallback_like_cppnix() {
	let Some(reference) = reference_dir() else {
		return;
	};
	if !namespaces_available() {
		return;
	}
	let root = noxi_test_support::tempdir().unwrap();
	let root = root.path().join("engine");
	let state: &[(&str, &str)] = &[("NIX_STATE_DIR", "state")];
	let steps: &[Step] = &[(
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
	let expected = summary(EVAL_ONLY.run(&reference, &root, steps));
	let actual = summary(EVAL_ONLY.run(&candidate_dir(), &root, steps));
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

#[test]
fn sandboxed_builds_and_shells_use_the_chroot_store_like_cppnix() {
	let (Some(reference), Some(busybox)) = (reference_dir(), busybox_bin()) else {
		return;
	};
	if !namespaces_available() {
		return;
	}
	// The build sandbox makes its own user namespace inside the one that
	// hides /nix/var, and `nix shell` mounts the per-user store on the
	// existing /nix/store.
	let prepare = prepare_hello(busybox);
	let session = Session {
		config: "experimental-features = nix-command\nsandbox = true\nsandbox-fallback = false\n",
		prepare: &prepare,
		hide_nix: true,
	};
	let steps: &[Step] = &[
		("nix-build", &["--no-out-link", "default.nix"], &[]),
		(
			"nix",
			&["shell", "-f", "default.nix", "--command", "hello", "x"],
			&[],
		),
	];
	let root = noxi_test_support::tempdir().unwrap();
	let root = root.path().join("engine");
	let expected = session.run(&reference, &root, steps);
	let actual = session.run(&candidate_dir(), &root, steps);
	assert_eq!(actual, expected);
	assert!(
		expected[0].contains("\nbuilt\n") && expected[1].contains("hello from ROOT x"),
		"{expected:?}"
	);
	assert_reference_verifies(&reference, &root);
	noxi_test_support::remove_tree(&root).unwrap();
}

#[test]
fn shell_runs_in_a_chroot_when_the_store_dir_is_missing_like_cppnix() {
	let (Some(reference), Some(busybox)) = (reference_dir(), busybox_bin()) else {
		return;
	};
	if !namespaces_available() {
		return;
	}
	// No user can create a top-level directory, so this store directory is
	// missing on every host, as /nix/store is on a host without /nix. The
	// helper must run the program in a temporary root of bind mounts.
	let missing = "/noxi-chroot-helper-store";
	assert!(!Path::new(missing).exists());
	let prepare = prepare_hello(busybox);
	let session = Session {
		config: "experimental-features = nix-command\nsandbox = true\nsandbox-fallback = false\n",
		prepare: &prepare,
		hide_nix: false,
	};
	let store_dir: &[(&str, &str)] = &[("NIX_STORE_DIR", missing)];
	let store = "local?root=ROOT/store";
	let steps: &[Step] = &[
		(
			"nix-build",
			&["--store", store, "--no-out-link", "default.nix"],
			store_dir,
		),
		(
			"nix",
			&["shell", "--store", store, "PREV", "--command", "hello", "y"],
			store_dir,
		),
	];
	let root = noxi_test_support::tempdir().unwrap();
	let root = root.path().join("engine");
	let expected = session.run(&reference, &root, steps);
	let actual = session.run(&candidate_dir(), &root, steps);
	assert_eq!(actual, expected);
	assert!(
		expected[1].contains("status Some(3)") && expected[1].contains("hello from ROOT y"),
		"{expected:?}"
	);
	noxi_test_support::remove_tree(&root).unwrap();
}
