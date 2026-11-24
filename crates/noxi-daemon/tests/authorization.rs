//! Daemon authorization through the installed executables: the reference
//! `nix-daemon` and noxi's read the same `nix.conf` settings, listen on
//! their own sockets in isolated stores, and must accept, trust or refuse
//! the user running the tests alike.
//!
//! Every case here runs as one unprivileged user. `trusted-users`,
//! `allowed-users` and `build-users-group` are pointed at that user's own
//! name and groups, or at names it does not have, so both outcomes of each
//! check are reached without a second account. Peers with a different uid,
//! root's default trust and build users connecting are left to the VM
//! tier (docs/verification.md).

use std::io::{BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use noxi_daemon::connection::{PROTOCOL_VERSION, STDERR_LAST, WORKER_MAGIC_1, WORKER_MAGIC_2};
use noxi_daemon::wire::{Reader, Writer};

const BASE: &str = "experimental-features = nix-command daemon-trust-override\nbuild-users-group =\nsubstituters =\n";

fn reference_bin() -> PathBuf {
	std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
}

fn daemons() -> [(&'static str, PathBuf); 2] {
	[
		("reference", reference_bin().join("nix-daemon")),
		("noxi", PathBuf::from(env!("CARGO_BIN_EXE_nix-daemon"))),
	]
}

fn output(program: &str, args: &[&str]) -> String {
	let out = Command::new(program).args(args).output().unwrap();
	assert!(out.status.success(), "{program} {args:?}");
	String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// The user running the tests, its primary group, and a supplementary
/// group whose member list names it, if it has one.
struct Identity {
	user: String,
	primary: String,
	member_of: Option<String>,
}

fn identity() -> Identity {
	let user = output("id", &["-un"]);
	let primary = output("id", &["-gn"]);
	let member_of = output("id", &["-Gn"])
		.split_whitespace()
		.filter(|group| *group != primary)
		.find(|group| {
			Command::new("getent")
				.args(["group", group])
				.output()
				.ok()
				.and_then(|out| String::from_utf8(out.stdout).ok())
				.and_then(|line| line.trim().rsplit(':').next().map(str::to_owned))
				.is_some_and(|members| members.split(',').any(|m| m == user))
		})
		.map(str::to_owned);
	Identity {
		user,
		primary,
		member_of,
	}
}

struct Daemon {
	child: Child,
	socket: PathBuf,
	_dir: noxi_test_support::TempDir,
}

impl Daemon {
	fn start(bin: &Path, config: &str, flags: &[&str]) -> Self {
		let dir = noxi_test_support::tempdir().unwrap();
		std::fs::create_dir(dir.path().join("conf")).unwrap();
		// A socket in directories that do not exist yet.
		let socket = dir.path().join("run/daemon/socket");
		let child = Command::new(bin)
			.args(flags)
			.args([
				"--store",
				&format!("local?root={}", dir.path().join("store").display()),
			])
			.env_clear()
			.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin")
			.env("HOME", dir.path())
			.env("NIX_CONF_DIR", dir.path().join("conf"))
			.env("NIX_CONFIG", format!("{BASE}{config}"))
			.env("NIX_DAEMON_SOCKET_PATH", &socket)
			.stdin(Stdio::null())
			.stdout(Stdio::null())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap();
		let mut daemon = Daemon {
			child,
			socket,
			_dir: dir,
		};
		let deadline = Instant::now() + Duration::from_secs(20);
		while !daemon.socket.exists() {
			if let Some(status) = daemon.child.try_wait().unwrap() {
				panic!(
					"{} exited with {status}: {}",
					bin.display(),
					daemon.stderr()
				);
			}
			assert!(Instant::now() < deadline, "{} did not start", bin.display());
			std::thread::sleep(Duration::from_millis(20));
		}
		daemon
	}

	/// Stop the daemon and return what it printed, without colour.
	fn stderr(&mut self) -> String {
		let _ = self.child.kill();
		let _ = self.child.wait();
		let mut text = Vec::new();
		if let Some(mut stderr) = self.child.stderr.take() {
			stderr.read_to_end(&mut text).unwrap();
		}
		String::from_utf8_lossy(&nix_compat::log::filter_ansi_escapes(&text, true)).into_owned()
	}

	/// Connect and shake hands: the trust the daemon reports, or `None`
	/// if it closed the connection instead.
	fn trust(&self) -> Option<u64> {
		let stream = UnixStream::connect(&self.socket).unwrap();
		stream
			.set_read_timeout(Some(Duration::from_secs(20)))
			.unwrap();
		let mut from = Reader::new(BufReader::new(stream.try_clone().unwrap()));
		let mut to = Writer::new(stream);
		to.u64(WORKER_MAGIC_1);
		// A refused connection may already be closed.
		if to.flush().is_err() {
			return None;
		}
		let magic = from.read_u64().ok()?;
		assert_eq!(magic, WORKER_MAGIC_2);
		assert_eq!(from.read_u64().unwrap(), PROTOCOL_VERSION);
		to.u64(PROTOCOL_VERSION).u64(0).u64(0).u64(0);
		to.flush().unwrap();
		from.read_strings().unwrap();
		assert_eq!(from.read_string().unwrap(), "2.35.2");
		let trust = from.read_u64().unwrap();
		assert_eq!(from.read_u64().unwrap(), STDERR_LAST);
		Some(trust)
	}
}

impl Drop for Daemon {
	fn drop(&mut self) {
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

/// The outcome of one configuration on both daemons; they must agree.
fn outcome(config: &str, flags: &[&str]) -> Option<u64> {
	let mut outcomes = Vec::new();
	for (name, bin) in daemons() {
		let mut daemon = Daemon::start(&bin, config, flags);
		let trust = daemon.trust();
		if trust.is_none() {
			// The reference logs the refusal after closing the connection.
			std::thread::sleep(Duration::from_millis(500));
		}
		let stderr = daemon.stderr();
		if trust.is_none() {
			assert!(
				stderr.contains("is not allowed to connect to the Nix daemon"),
				"{name} with {config:?}: {stderr}"
			);
		}
		outcomes.push((trust, refusal(&stderr)));
	}
	assert_eq!(outcomes[0], outcomes[1], "{config:?} {flags:?}");
	outcomes[0].0
}

/// The reference's log line for a refused connection.
fn refusal(stderr: &str) -> Option<String> {
	stderr
		.lines()
		.find(|line| line.contains("while processing connection"))
		.map(str::to_owned)
}

const TRUSTED: Option<u64> = Some(1);
const UNTRUSTED: Option<u64> = Some(2);
const REFUSED: Option<u64> = None;

#[test]
fn trusted_and_allowed_users_match_the_reference_daemon() {
	let id = identity();
	let user = &id.user;
	let primary = &id.primary;
	let mut cases = vec![
		(format!("trusted-users = {user}\n"), TRUSTED),
		(format!("trusted-users = root {user}\n"), TRUSTED),
		(format!("trusted-users = @{primary}\n"), TRUSTED),
		("trusted-users = *\n".to_owned(), TRUSTED),
		("trusted-users =\n".to_owned(), UNTRUSTED),
		(
			"trusted-users = @noxi-no-such-group noxi-no-such-user\n".to_owned(),
			UNTRUSTED,
		),
		// A trusted user may connect whatever `allowed-users` says.
		(
			format!("trusted-users = {user}\nallowed-users = noxi-no-such-user\n"),
			TRUSTED,
		),
		(
			"trusted-users =\nallowed-users = noxi-no-such-user\n".to_owned(),
			REFUSED,
		),
		("trusted-users =\nallowed-users =\n".to_owned(), REFUSED),
		(
			"trusted-users =\nallowed-users = @noxi-no-such-group\n".to_owned(),
			REFUSED,
		),
		(
			format!("trusted-users =\nallowed-users = {user}\n"),
			UNTRUSTED,
		),
		(
			format!("trusted-users =\nallowed-users = @{primary}\n"),
			UNTRUSTED,
		),
		// Members of the build users group may never connect.
		(
			format!("trusted-users = {user}\nbuild-users-group = {primary}\n"),
			REFUSED,
		),
		(
			format!("trusted-users = *\nallowed-users = *\nbuild-users-group = {primary}\n"),
			REFUSED,
		),
	];
	// The VM harness may run the tests as root, whom this trusts.
	if user != "root" {
		cases.push(("trusted-users = root\n".to_owned(), UNTRUSTED));
	}
	if let Some(group) = &id.member_of {
		cases.push((format!("trusted-users = @{group}\n"), TRUSTED));
		cases.push((
			format!("trusted-users =\nallowed-users = @{group}\n"),
			UNTRUSTED,
		));
		// Only the primary group counts as the build users group.
		cases.push((
			format!("trusted-users =\nbuild-users-group = {group}\n"),
			UNTRUSTED,
		));
	}
	for (config, expected) in cases {
		assert_eq!(outcome(&config, &[]), expected, "{config:?}");
	}
}

#[test]
fn trust_override_flags_on_the_daemon_socket_match_the_reference() {
	let user = identity().user;
	for (config, flags, expected) in [
		(
			format!("trusted-users = {user}\n"),
			vec!["--force-untrusted"],
			UNTRUSTED,
		),
		(
			"trusted-users =\n".to_owned(),
			vec!["--force-trusted"],
			TRUSTED,
		),
		// A forced trust does not look at the peer at all.
		(
			"trusted-users =\nallowed-users =\n".to_owned(),
			vec!["--force-untrusted"],
			UNTRUSTED,
		),
		(
			"trusted-users =\n".to_owned(),
			vec!["--force-trusted", "--default-trust"],
			UNTRUSTED,
		),
		(
			"trusted-users =\nallowed-users =\n".to_owned(),
			vec!["--force-trusted", "--default-trust"],
			REFUSED,
		),
		(
			format!("trusted-users = {user}\n"),
			vec!["--daemon", "--force-untrusted", "--default-trust"],
			TRUSTED,
		),
	] {
		assert_eq!(outcome(&config, &flags), expected, "{config:?} {flags:?}");
	}
}

#[test]
fn daemon_socket_permissions_and_refused_clients_match_the_reference() {
	let mut modes = Vec::new();
	let mut clients = Vec::new();
	for (name, bin) in daemons() {
		for config in ["trusted-users =\n", "trusted-users =\nallowed-users =\n"] {
			let mut daemon = Daemon::start(&bin, config, &[]);
			modes.push(
				std::fs::metadata(&daemon.socket)
					.unwrap()
					.permissions()
					.mode()
					& 0o7777,
			);
			// The reference client against this daemon.
			let dir = noxi_test_support::tempdir().unwrap();
			let out = Command::new(reference_bin().join("nix"))
				.args([
					"store",
					"info",
					"--store",
					&format!("unix://{}", daemon.socket.display()),
				])
				.env_clear()
				.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin")
				.env("HOME", dir.path())
				.env("NIX_CONF_DIR", dir.path())
				.env("NIX_CONFIG", "experimental-features = nix-command\n")
				.output()
				.unwrap();
			// `nix store info` prints its report through the logger.
			let socket = daemon.socket.display().to_string();
			let stdout = format!(
				"{}{}",
				String::from_utf8_lossy(&out.stdout),
				String::from_utf8_lossy(&nix_compat::log::filter_ansi_escapes(&out.stderr, true))
			)
			.replace(&socket, "SOCKET");
			daemon.stderr();
			clients.push((name, out.status.code(), stdout));
		}
	}
	assert!(modes.iter().all(|mode| *mode == 0o666), "{modes:?}");
	let (reference, noxi) = clients.split_at(2);
	for (a, b) in reference.iter().zip(noxi) {
		assert_eq!((a.1, &a.2), (b.1, &b.2), "{a:?} {b:?}");
	}
	assert_eq!(reference[0].1, Some(0));
	assert!(reference[0].2.contains("Trusted: 0"), "{:?}", reference[0]);
	assert_ne!(reference[1].1, Some(0));
}
