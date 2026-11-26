//! A builder that closes its log compared with CppNix 2.35.2: the build
//! ends at once, the builder and the processes it started are killed, and
//! the failure is reported as the reference reports it.
//!
//! The reference comes from `NOXI_REFERENCE_BIN`, else
//! `target/nix-reference/bin`; without it the test prints a notice and
//! returns.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn reference_dir() -> Option<PathBuf> {
	let dir = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		});
	if dir.join("nix").exists() {
		Some(dir)
	} else {
		eprintln!("skipping: no reference nix in {}", dir.display());
		None
	}
}

fn candidate_dir() -> &'static Path {
	Path::new(env!("CARGO_BIN_EXE_nix")).parent().unwrap()
}

/// `timeout.nix`'s `closeLog`, which also records the PID of a process it
/// leaves running.
const EXPR: &str = r#"
derivation {
	name = "close-log";
	system = "x86_64-linux";
	builder = "/bin/sh";
	PATH = "/run/current-system/sw/bin:/usr/bin:/bin";
	pidFile = builtins.getEnv "PID_FILE";
	args = [ "-c" "touch $out; sleep 1000 > /dev/null 2>&1 & echo $! > $pidFile; exec > /dev/null 2>&1; sleep 1000" ];
}
"#;

/// Build the derivation with one engine and return the exit status, the
/// error lines, and whether either process it started is still alive.
fn build(bin: &Path, root: &Path) -> (Option<i32>, Vec<String>, bool) {
	noxi_test_support::remove_tree(root).unwrap();
	fs::create_dir_all(root.join("etc")).unwrap();
	fs::write(
		root.join("etc/nix.conf"),
		"sandbox = false\nbuild-users-group =\nsubstituters =\n",
	)
	.unwrap();
	fs::write(root.join("x.nix"), EXPR).unwrap();
	let pid_file = root.join("sleep.pid");
	let output = Command::new("timeout")
		.args(["120", "nix-build", "x.nix", "--no-out-link"])
		.current_dir(root)
		.env_clear()
		.env("HOME", root.join("home"))
		.env("PID_FILE", &pid_file)
		.env(
			"PATH",
			format!("{}:/run/current-system/sw/bin:/usr/bin:/bin", bin.display()),
		)
		.env("NIX_STORE_DIR", root.join("store"))
		.env("NIX_STATE_DIR", root.join("var"))
		.env("NIX_LOG_DIR", root.join("log"))
		.env("NIX_CONF_DIR", root.join("etc"))
		.env("NIX_REMOTE", "")
		.output()
		.unwrap();
	let errors = String::from_utf8_lossy(&output.stderr)
		.lines()
		.filter(|line| line.starts_with("error:"))
		.map(str::to_owned)
		.collect();
	let pid: libc::pid_t = fs::read_to_string(&pid_file)
		.unwrap()
		.trim()
		.parse()
		.unwrap();
	// Give a killed process a moment to be reaped by its new parent.
	let mut alive = true;
	for _ in 0..50 {
		// SAFETY: signal 0 only checks that the process exists.
		alive = unsafe { libc::kill(pid, 0) } == 0
			&& fs::read_to_string(format!("/proc/{pid}/stat"))
				.is_ok_and(|stat| !stat.contains(") Z "));
		if !alive {
			break;
		}
		std::thread::sleep(std::time::Duration::from_millis(100));
	}
	if alive {
		// SAFETY: killing the process this test's build left behind.
		unsafe { libc::kill(pid, libc::SIGKILL) };
	}
	(output.status.code(), errors, alive)
}

#[test]
fn builders_that_close_their_log_are_killed_like_the_reference() {
	let Some(reference) = reference_dir() else {
		return;
	};
	let dir = noxi_test_support::tempdir().unwrap();
	let root = dir.path().join("engine");
	let expected = build(&reference, &root);
	let actual = build(candidate_dir(), &root);
	let normalize = |(code, errors, alive): (Option<i32>, Vec<String>, bool)| {
		let errors: Vec<String> = errors
			.iter()
			.map(|line| line.replace(&root.display().to_string(), "ROOT"))
			.collect();
		(code, errors, alive)
	};
	let (expected, actual) = (normalize(expected), normalize(actual));
	assert_eq!(actual, expected);
	assert_eq!(expected.0, Some(100), "{expected:?}");
	assert!(!expected.2, "the reference left a process running");
}
