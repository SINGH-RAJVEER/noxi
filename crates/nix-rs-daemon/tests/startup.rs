//! Exercise the installed daemon invocation without opening the host store.

use std::io::{BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix_rs_daemon::connection::{
	PROTOCOL_VERSION, STDERR_LAST, WORKER_MAGIC_1, WORKER_MAGIC_2, op,
};
use nix_rs_daemon::wire::{Reader, Writer};

const CONFIG: &str = "experimental-features = nix-command daemon-trust-override mounted-ssh-store\nbuild-users-group =\nsubstituters =\ntrusted-users =\nallowed-users = *\n";

struct Process(Child);

impl Drop for Process {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

fn reference() -> PathBuf {
	std::env::var_os("NIXRS_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
}

fn command(bin: &Path, dir: &Path) -> Command {
	let mut command = Command::new(bin);
	command
		.env_clear()
		.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin")
		.env("HOME", dir)
		.env("NIX_CONF_DIR", dir.join("conf"))
		.env("NIX_CONFIG", CONFIG);
	command
}

fn handshake<R: Read, W: Write>(from: &mut Reader<R>, to: &mut Writer<W>) -> u64 {
	to.u64(WORKER_MAGIC_1)
		.u64(PROTOCOL_VERSION)
		.u64(0)
		.u64(0)
		.u64(0);
	to.flush().unwrap();
	assert_eq!(from.read_u64().unwrap(), WORKER_MAGIC_2);
	assert_eq!(from.read_u64().unwrap(), PROTOCOL_VERSION);
	assert_eq!(
		from.read_strings().unwrap(),
		[
			"delete-dead-specific-referrers",
			"realisation-with-path-not-hash"
		]
	);
	assert_eq!(from.read_string().unwrap(), "2.35.2");
	let trust = from.read_u64().unwrap();
	assert_eq!(from.read_u64().unwrap(), STDERR_LAST);
	trust
}

fn register_text<R: Read, W: Write>(from: &mut Reader<R>, to: &mut Writer<W>) -> String {
	to.u64(op::ADD_TEXT_TO_STORE)
		.string("startup-source")
		.string("daemon source registration\n")
		.u64(0);
	to.flush().unwrap();
	assert_eq!(from.read_u64().unwrap(), STDERR_LAST);
	let path = from.read_string().unwrap();
	to.u64(op::IS_VALID_PATH).string(&path);
	to.flush().unwrap();
	assert_eq!(from.read_u64().unwrap(), STDERR_LAST);
	assert_eq!(from.read_u64().unwrap(), 1);
	path
}

#[test]
fn stdio_flags_trust_overrides_and_registration_match_reference() {
	for (flags, trust) in [
		(vec!["--stdio"], 1),
		(
			vec!["--daemon", "--stdio", "--process-ops", "--force-trusted"],
			1,
		),
		(vec!["--stdio", "--force-untrusted"], 2),
		(vec!["--stdio", "--force-untrusted", "--default-trust"], 1),
	] {
		let mut paths = Vec::new();
		for bin in [
			reference().join("nix-daemon"),
			PathBuf::from(env!("CARGO_BIN_EXE_nix-daemon")),
		] {
			let dir = tempfile::tempdir().unwrap();
			let mut child = command(&bin, dir.path())
				.args(&flags)
				.args(["--store", &format!("local?root={}", dir.path().display())])
				.stdin(Stdio::piped())
				.stdout(Stdio::piped())
				.stderr(Stdio::piped())
				.spawn()
				.unwrap();
			let mut from = Reader::new(BufReader::new(child.stdout.take().unwrap()));
			let mut to = Writer::new(child.stdin.take().unwrap());
			let mut process = Process(child);
			assert_eq!(handshake(&mut from, &mut to), trust, "{flags:?}");
			paths.push(register_text(&mut from, &mut to));
			drop(to);
			assert!(process.0.wait().unwrap().success());
		}
		assert_eq!(paths[0], paths[1]);
	}
}

#[test]
fn experimental_flag_gates_match_reference() {
	for (flag, feature) in [
		("--force-trusted", "daemon-trust-override"),
		("--force-untrusted", "daemon-trust-override"),
		("--default-trust", "daemon-trust-override"),
		("--process-ops", "mounted-ssh-store"),
	] {
		let dir = tempfile::tempdir().unwrap();
		let mut outputs = Vec::new();
		for bin in [
			reference().join("nix-daemon"),
			PathBuf::from(env!("CARGO_BIN_EXE_nix-daemon")),
		] {
			let out = command(&bin, dir.path())
				.env("NIX_CONFIG", "")
				.arg(flag)
				.output()
				.unwrap();
			assert!(!out.status.success());
			assert!(String::from_utf8_lossy(&out.stderr).contains(feature));
			outputs.push(out);
		}
		assert_eq!(outputs[0].status.code(), outputs[1].status.code());
		// The reference adds ANSI highlighting even on this pipe.
		let clean = |out: &[u8]| nix_compat::log::filter_ansi_escapes(out, true);
		assert_eq!(clean(&outputs[0].stderr), clean(&outputs[1].stderr));
	}
}

#[test]
fn daemon_service_default_socket_and_evaluator_registration() {
	let dir = tempfile::tempdir().unwrap();
	// UDSRemoteStore opens its local view of the store before connecting.
	std::fs::create_dir_all(dir.path().join("nix/store")).unwrap();
	let socket = dir.path().join("nix/var/nix/daemon-socket/socket");
	// A configured diverted store exercises the no --store service invocation.
	let config = format!("{CONFIG}store = local?root={}\n", dir.path().display());
	let child = command(Path::new(env!("CARGO_BIN_EXE_nix-daemon")), dir.path())
		.env("NIX_CONFIG", config)
		.arg("--daemon")
		.spawn()
		.unwrap();
	let mut process = Process(child);
	wait_for_socket(&socket, &mut process);
	let uri = format!("unix://{}?root={}", socket.display(), dir.path().display());
	let output = command(&reference().join("nix-instantiate"), dir.path())
		.args([
			"--store",
			&uri,
			"--eval",
			"--read-write-mode",
			"-E",
			"builtins.toFile \"startup-evaluator-source\" \"registered by the evaluator\"",
		])
		.output()
		.unwrap();
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	let path = String::from_utf8(output.stdout).unwrap();
	let path = path.trim().trim_matches('"');
	assert_eq!(
		std::fs::read_to_string(dir.path().join(path.trim_start_matches('/'))).unwrap(),
		"registered by the evaluator"
	);
	// Restart must replace the stale socket and continue to open the same store.
	process.0.kill().unwrap();
	process.0.wait().unwrap();
	process.0 = command(Path::new(env!("CARGO_BIN_EXE_nix-daemon")), dir.path())
		.args([
			"--daemon",
			"--store",
			&format!("local?root={}", dir.path().display()),
		])
		.spawn()
		.unwrap();
	let mut output = command(&reference().join("nix-store"), dir.path());
	output.args(["--store", &uri, "--query", "--hash", path]);
	let deadline = Instant::now() + Duration::from_secs(10);
	loop {
		let result = output.output().unwrap();
		if result.status.success() {
			break;
		}
		assert!(
			Instant::now() < deadline,
			"{}",
			String::from_utf8_lossy(&result.stderr)
		);
		std::thread::sleep(Duration::from_millis(20));
	}
}

fn wait_for_socket(socket: &Path, process: &mut Process) {
	let deadline = Instant::now() + Duration::from_secs(10);
	while !socket.exists() {
		assert!(
			process.0.try_wait().unwrap().is_none(),
			"daemon exited before binding"
		);
		assert!(Instant::now() < deadline, "daemon did not bind");
		std::thread::sleep(Duration::from_millis(20));
	}
}

#[test]
fn systemd_socket_activation_serves_inherited_descriptors() {
	for named in [false, true] {
		let dir = tempfile::tempdir().unwrap();
		let socket = dir.path().join("activated.sock");
		let listener = UnixListener::bind(&socket).unwrap();
		let fd = listener.as_raw_fd();
		// The shell supplies its own PID before exec, just as a service manager does.
		let mut cmd = command(Path::new("/bin/sh"), dir.path());
		cmd.args([
			"-c",
			"export LISTEN_PID=$$; exec \"$@\"",
			"socket-activation",
		])
		.arg(env!("CARGO_BIN_EXE_nix-daemon"))
		.args([
			"--daemon",
			"--store",
			&format!("local?root={}", dir.path().display()),
		])
		.env("LISTEN_FDS", "1");
		if named {
			cmd.env("LISTEN_FDNAMES", "nix-daemon.socket");
		}
		// SAFETY: only descriptor syscalls run between fork and exec.
		unsafe {
			cmd.pre_exec(move || {
				if libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
					return Err(std::io::Error::last_os_error());
				}
				Ok(())
			});
		}
		let _process = Process(cmd.spawn().unwrap());
		drop(listener);
		let stream = UnixStream::connect(&socket).unwrap();
		stream
			.set_read_timeout(Some(Duration::from_secs(10)))
			.unwrap();
		let mut from = Reader::new(BufReader::new(stream.try_clone().unwrap()));
		let mut to = Writer::new(stream);
		assert_eq!(handshake(&mut from, &mut to), 2);
		let path = register_text(&mut from, &mut to);
		assert!(dir.path().join(path.trim_start_matches('/')).exists());
		assert!(!dir.path().join("nix/var/nix/daemon-socket/socket").exists());
	}
}
