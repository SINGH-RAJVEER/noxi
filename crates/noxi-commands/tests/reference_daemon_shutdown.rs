//! Stopping a daemon with a signal, checked against the pinned CppNix 2.35.2
//! reference. `nix daemon` and `nix-daemon` both return from `daemonLoop`
//! as soon as SIGINT, SIGTERM or SIGHUP arrives and exit with status 0;
//! `nix daemon`'s progress bar prints `\rshutting down\e[K` on the way out.
//! The worker serving a client that is already connected runs in its own
//! session and keeps answering it. SIGINT is ignored when the daemon starts,
//! as it is for a background job of a non-interactive shell such as the
//! functional tests' `startDaemon`, and must still stop the daemon.
//!
//! The reference executables come from `NOXI_REFERENCE_BIN` or
//! `target/nix-reference/bin` (`just reference` creates them).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CONFIG: &str =
	"experimental-features = nix-command\nbuild-users-group =\nsubstituters =\nallowed-users = *\n";
const WORKER_MAGIC_1: u64 = 0x6e69_7863;
const WORKER_MAGIC_2: u64 = 0x6478_696f;
const STDERR_LAST: u64 = 0x616c_7473;
/// Protocol 1.21: no feature negotiation, version string or trust flag.
const CLIENT_VERSION: u64 = 0x115;
const IS_VALID_PATH: u64 = 1;

fn reference_bin() -> PathBuf {
	if let Some(path) = std::env::var_os("NOXI_REFERENCE_BIN") {
		return PathBuf::from(path);
	}
	let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin");
	assert!(
		path.join("nix-daemon").exists(),
		"the reference is missing; run `just reference` or set NOXI_REFERENCE_BIN"
	);
	path
}

fn read_u64(stream: &mut UnixStream) -> u64 {
	let mut bytes = [0u8; 8];
	stream.read_exact(&mut bytes).unwrap();
	u64::from_le_bytes(bytes)
}

fn write_string(out: &mut Vec<u8>, value: &str) {
	out.extend_from_slice(&(value.len() as u64).to_le_bytes());
	out.extend_from_slice(value.as_bytes());
	out.resize(out.len().next_multiple_of(8), 0);
}

/// Connect and finish the handshake, so a worker is serving the client.
fn connect(socket: &Path) -> UnixStream {
	let mut stream = UnixStream::connect(socket).unwrap();
	stream
		.set_read_timeout(Some(Duration::from_secs(20)))
		.unwrap();
	let mut out = Vec::new();
	for word in [WORKER_MAGIC_1, CLIENT_VERSION, 0, 0] {
		out.extend_from_slice(&word.to_le_bytes());
	}
	stream.write_all(&out).unwrap();
	assert_eq!(read_u64(&mut stream), WORKER_MAGIC_2);
	read_u64(&mut stream);
	assert_eq!(read_u64(&mut stream), STDERR_LAST);
	stream
}

/// Ask the worker whether a path is valid; it is not.
fn query(stream: &mut UnixStream) -> u64 {
	let mut out = IS_VALID_PATH.to_le_bytes().to_vec();
	write_string(
		&mut out,
		"/nix/store/00000000000000000000000000000000-shutdown",
	);
	stream.write_all(&out).unwrap();
	assert_eq!(read_u64(stream), STDERR_LAST);
	read_u64(stream)
}

struct Stopped {
	status: Option<i32>,
	signal: Option<i32>,
	stderr: Vec<u8>,
	seconds: f64,
	socket_kept: bool,
	worker_answered: u64,
}

fn stop(program: &Path, args: &[&str], signal: libc::c_int) -> Stopped {
	use std::os::unix::process::ExitStatusExt;
	let dir = noxi_test_support::tempdir().unwrap();
	let socket = dir.path().join("socket");
	let stderr = dir.path().join("stderr");
	let root = dir.path().join("root");
	let mut command = Command::new(program);
	command
		.args(args)
		.args(["--store", &format!("local?root={}", root.display())])
		.env_clear()
		.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin")
		.env("HOME", dir.path())
		.env("NIX_CONF_DIR", dir.path().join("conf"))
		.env("NIX_CONFIG", CONFIG)
		.env("NIX_DAEMON_SOCKET_PATH", &socket)
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(std::fs::File::create(&stderr).unwrap());
	// SAFETY: only async-signal-safe calls between fork and exec.
	unsafe {
		command.pre_exec(|| {
			libc::signal(libc::SIGINT, libc::SIG_IGN);
			Ok(())
		});
	}
	let mut child = command.spawn().unwrap();
	let started = Instant::now();
	while !socket.exists() {
		assert!(
			child.try_wait().unwrap().is_none(),
			"{} exited before listening",
			program.display()
		);
		assert!(started.elapsed() < Duration::from_secs(30));
		std::thread::sleep(Duration::from_millis(20));
	}
	let mut client = connect(&socket);
	assert_eq!(query(&mut client), 0);
	// SAFETY: signalling the child this test started.
	unsafe { libc::kill(child.id() as libc::pid_t, signal) };
	let signalled = Instant::now();
	let status = loop {
		if let Some(status) = child.try_wait().unwrap() {
			break status;
		}
		if signalled.elapsed() > Duration::from_secs(20) {
			let _ = child.kill();
			break child.wait().unwrap();
		}
		std::thread::sleep(Duration::from_millis(5));
	};
	let seconds = signalled.elapsed().as_secs_f64();
	let worker_answered = query(&mut client);
	drop(client);
	Stopped {
		status: status.code(),
		signal: status.signal(),
		stderr: std::fs::read(&stderr).unwrap(),
		seconds,
		socket_kept: socket.exists(),
		worker_answered,
	}
}

#[test]
fn signalled_daemons_exit_at_once_like_cppnix() {
	let links = noxi_test_support::tempdir().unwrap();
	// The `nix` executable is `nix-daemon` when it is called that.
	let noxi_daemon = links.path().join("nix-daemon");
	std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_nix"), &noxi_daemon).unwrap();
	let reference = reference_bin();
	let forms: [(&str, PathBuf, PathBuf, &[&str]); 2] = [
		(
			"nix daemon",
			reference.join("nix"),
			PathBuf::from(env!("CARGO_BIN_EXE_nix")),
			&["--extra-experimental-features", "nix-command", "daemon"],
		),
		("nix-daemon", reference.join("nix-daemon"), noxi_daemon, &[]),
	];
	for (form, reference, noxi, args) in &forms {
		for (name, signal) in [
			("SIGTERM", libc::SIGTERM),
			("SIGINT", libc::SIGINT),
			("SIGHUP", libc::SIGHUP),
		] {
			let expected = stop(reference, args, signal);
			let actual = stop(noxi, args, signal);
			let case = format!("{form} {name}");
			assert_eq!(
				(expected.status, expected.signal),
				(Some(0), None),
				"{case}: reference"
			);
			assert_eq!(
				(actual.status, actual.signal),
				(expected.status, expected.signal),
				"{case}"
			);
			assert_eq!(
				String::from_utf8_lossy(&actual.stderr),
				String::from_utf8_lossy(&expected.stderr),
				"{case}"
			);
			assert_eq!(actual.socket_kept, expected.socket_kept, "{case}");
			assert_eq!(actual.worker_answered, expected.worker_answered, "{case}");
			// The reference exits within milliseconds; noxi used to wait for
			// its five-second interrupt deadline.
			assert!(
				expected.seconds < 2.0,
				"{case}: reference took {}s",
				expected.seconds
			);
			assert!(
				actual.seconds < 2.0,
				"{case}: noxi took {}s",
				actual.seconds
			);
		}
	}
}
