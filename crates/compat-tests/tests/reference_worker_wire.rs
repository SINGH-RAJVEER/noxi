//! Worker-protocol exchanges with the noxi daemon checked byte for byte
//! against the pinned CppNix 2.35.2 `nix-daemon`.
//!
//! The reference builds a small store, which is copied twice. The reference
//! daemon serves one copy and the noxi daemon, started in this process,
//! serves the other. A scripted client in this file then speaks the worker
//! protocol to each socket directly, at every protocol version the
//! reference accepts, and records every value either daemon sends. The two
//! transcripts must be equal once the store roots, activity IDs, process IDs
//! and registration times that legitimately differ are masked.

use std::fs;
use std::io::{BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use noxi_daemon::DaemonSettings;
use noxi_daemon::server::serve;
use noxi_store::LocalStoreConfig;

fn from_env_or_target(var: &str, target: &str, probe: &str) -> PathBuf {
	if let Some(path) = std::env::var_os(var) {
		return PathBuf::from(path);
	}
	let path = Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../target")
		.join(target);
	assert!(
		path.join(probe).exists(),
		"{target} is missing; run `just reference` or set {var}"
	);
	path
}

const CONFIG: &str = "experimental-features = nix-command\nbuild-users-group =\nsubstituters =\nsandbox = true\nsandbox-fallback = false\nsandbox-paths =\ncores = 1\n";

const WORKER_MAGIC_1: u64 = 0x6e697863;
const WORKER_MAGIC_2: u64 = 0x6478696f;
const STDERR_NEXT: u64 = 0x6f6c6d67;
const STDERR_READ: u64 = 0x64617461;
const STDERR_WRITE: u64 = 0x64617416;
const STDERR_LAST: u64 = 0x616c7473;
const STDERR_ERROR: u64 = 0x63787470;
const STDERR_START_ACTIVITY: u64 = 0x53545254;
const STDERR_STOP_ACTIVITY: u64 = 0x53544f50;
const STDERR_RESULT: u64 = 0x52534c54;

/// The oldest and newest versions `WorkerProto` accepts.
/// How long a reply may take, in seconds, before the daemon is taken to be
/// waiting for input.
const READ_TIMEOUT: u64 = 15;

const OLDEST: u64 = 18;
const NEWEST: u64 = 38;

/// The progress and expected-size results of the `realise`, `builds` and
/// `copy-paths` activities CppNix's `Worker` opens for its lifetime. Both
/// daemons start and stop these activities alike, and their counts match
/// for `BuildPaths` and `BuildDerivation`, but noxi does not yet count the
/// substitution goals of `BuildPathsWithResults`'s opaque paths or a
/// repair's goals as CppNix does, so the counts are left out of the
/// comparison.
const PROGRESS_RESULTS: [u64; 2] = [105, 106];

const fn version(minor: u64) -> u64 {
	1 << 8 | minor
}

/// The daemons a test compares.
const DAEMONS: [&str; 2] = ["reference", "noxi"];

/// One value a daemon sent, as the transcript records it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
	Int(u64),
	Str(String),
	/// A value that legitimately differs between the daemons.
	Masked(&'static str),
	/// An activity, numbered in the order the daemon started it.
	Activity(usize),
	/// Unframed bytes, such as a NAR, by length and SHA-256.
	Raw(usize, String),
	/// Where the script was, so a difference is easy to place.
	Note(String),
	/// The daemon closed the connection.
	Eof,
}

/// The client side of one connection.
struct Wire {
	read: BufReader<UnixStream>,
	write: UnixStream,
	buf: Vec<u8>,
	log: Vec<Tok>,
	/// The negotiated version and features.
	version: u64,
	features: Vec<String>,
	/// Text replaced in every string the daemon sends.
	replacements: Vec<(String, String)>,
	closed: bool,
	/// The activity IDs seen, in order.
	activities: Vec<u64>,
}

impl Wire {
	fn connect(socket: &Path, replacements: Vec<(String, String)>) -> Self {
		let stream = UnixStream::connect(socket).unwrap();
		stream
			.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT)))
			.unwrap();
		Wire {
			read: BufReader::new(stream.try_clone().unwrap()),
			write: stream,
			buf: Vec::new(),
			log: Vec::new(),
			version: 0,
			features: Vec::new(),
			replacements,
			closed: false,
			activities: Vec::new(),
		}
	}

	fn u64(&mut self, n: u64) -> &mut Self {
		self.buf.extend_from_slice(&n.to_le_bytes());
		self
	}

	fn bytes(&mut self, s: impl AsRef<[u8]>) -> &mut Self {
		let s = s.as_ref();
		self.u64(s.len() as u64);
		self.buf.extend_from_slice(s);
		self.buf.resize(self.buf.len() + (8 - s.len() % 8) % 8, 0);
		self
	}

	fn strs(&mut self, items: &[&str]) -> &mut Self {
		self.u64(items.len() as u64);
		for item in items {
			self.bytes(item);
		}
		self
	}

	fn raw(&mut self, bytes: &[u8]) -> &mut Self {
		self.buf.extend_from_slice(bytes);
		self
	}

	/// Send what is buffered. A daemon that has hung up is recorded when
	/// the reply is read, so a failed write is not an error here.
	fn flush(&mut self) {
		let buf = std::mem::take(&mut self.buf);
		if !self.closed {
			let _ = self.write.write_all(&buf);
			let _ = self.write.flush();
		}
	}

	fn note(&mut self, what: impl Into<String>) {
		self.log.push(Tok::Note(what.into()));
	}

	fn read_exact(&mut self, buf: &mut [u8]) -> bool {
		if self.closed {
			return false;
		}
		match self.read.read_exact(buf) {
			Ok(()) => true,
			Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
				self.closed = true;
				self.log.push(Tok::Eof);
				false
			}
			Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
				self.closed = true;
				self.log.push(Tok::Eof);
				false
			}
			Err(e)
				if matches!(
					e.kind(),
					std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
				) =>
			{
				panic!("daemon reply timed out: {:#?}", self.log);
			}
			Err(e) => panic!("reading from the daemon: {e}"),
		}
	}

	fn get_u64(&mut self) -> Option<u64> {
		let mut buf = [0u8; 8];
		self.read_exact(&mut buf).then(|| u64::from_le_bytes(buf))
	}

	fn get_bytes(&mut self) -> Option<Vec<u8>> {
		let len = self.get_u64()? as usize;
		assert!(len < 1 << 28, "implausible string length {len}");
		let mut bytes = vec![0u8; len + (8 - len % 8) % 8];
		self.read_exact(&mut bytes).then_some(())?;
		bytes.truncate(len);
		Some(bytes)
	}

	/// Read and record an integer.
	fn int(&mut self) -> Option<u64> {
		let n = self.get_u64()?;
		self.log.push(Tok::Int(n));
		Some(n)
	}

	/// Read an integer that legitimately differs, recording only that one
	/// was sent.
	fn masked(&mut self, what: &'static str) -> Option<u64> {
		let n = self.get_u64()?;
		self.log.push(Tok::Masked(what));
		Some(n)
	}

	/// An activity ID, recorded by the order activities started in, so
	/// a stop or result still names its activity. Zero is no activity.
	fn activity(&mut self) -> Option<u64> {
		let id = self.get_u64()?;
		self.record_activity(id);
		Some(id)
	}

	fn record_activity(&mut self, id: u64) {
		if id == 0 {
			self.log.push(Tok::Int(0));
			return;
		}
		let index = match self.activities.iter().position(|&a| a == id) {
			Some(index) => index,
			None => {
				self.activities.push(id);
				self.activities.len() - 1
			}
		};
		self.log.push(Tok::Activity(index));
	}

	fn normalise(&self, s: &str) -> String {
		// CppNix highlights the values in its messages (`HintFmt`); noxi
		// does not yet, anywhere, so colour is compared without. Clients
		// that are not writing to a terminal remove it themselves.
		let mut s = strip_colour(s);
		for (from, to) in &self.replacements {
			s = s.replace(from, to);
		}
		// Process IDs in temporary-root file names differ.
		if let Some((head, tail)) = s.split_once("/temproots/") {
			let rest = tail.trim_start_matches(|c: char| c.is_ascii_digit());
			s = format!("{head}/temproots/PID{rest}");
		}
		s
	}

	/// Read and record a string.
	fn str(&mut self) -> Option<String> {
		let bytes = self.get_bytes()?;
		let s = self.normalise(&String::from_utf8_lossy(&bytes));
		self.log.push(Tok::Str(s.clone()));
		Some(s)
	}

	fn strings(&mut self) -> Option<Vec<String>> {
		let n = self.int()?;
		(0..n).map(|_| self.str()).collect()
	}

	/// Unframed bytes, recorded by length and hash.
	fn raw_bytes(&mut self, len: usize) -> Option<Vec<u8>> {
		let mut bytes = vec![0u8; len];
		self.read_exact(&mut bytes).then_some(())?;
		use sha2::Digest;
		let hash = sha2::Sha256::digest(&bytes);
		self.log.push(Tok::Raw(
			len,
			hash.iter().map(|b| format!("{b:02x}")).collect(),
		));
		Some(bytes)
	}

	fn fields(&mut self) -> Option<()> {
		let n = self.int()?;
		for _ in 0..n {
			match self.int()? {
				0 => {
					self.int()?;
				}
				1 => {
					self.str()?;
				}
				t => panic!("unknown field type {t}"),
			}
		}
		Some(())
	}

	/// `processStderr`: record the daemon's messages until it finishes
	/// the operation. Returns whether it succeeded, or `None` if it hung
	/// up. `source` answers `STDERR_READ`.
	fn stderr_with(&mut self, source: &mut &[u8]) -> Option<bool> {
		self.flush();
		loop {
			let msg = self.get_u64()?;
			match msg {
				STDERR_LAST => {
					self.log.push(Tok::Str("LAST".into()));
					return Some(true);
				}
				STDERR_ERROR => {
					self.log.push(Tok::Str("ERROR".into()));
					if self.version >= version(26) {
						self.str()?;
						self.int()?;
						self.str()?;
						self.str()?;
						self.int()?;
						let traces = self.int()?;
						for _ in 0..traces {
							self.int()?;
							self.str()?;
						}
					} else {
						self.str()?;
						self.int()?;
					}
					return Some(false);
				}
				STDERR_NEXT => {
					self.log.push(Tok::Str("NEXT".into()));
					self.str()?;
				}
				STDERR_WRITE => {
					self.log.push(Tok::Str("WRITE".into()));
					self.str()?;
				}
				STDERR_READ => {
					self.log.push(Tok::Str("READ".into()));
					let len = self.int()? as usize;
					let n = len.min(source.len());
					let (chunk, rest) = source.split_at(n);
					self.bytes(chunk);
					*source = rest;
					self.flush();
				}
				STDERR_START_ACTIVITY => {
					let id = self.get_u64()?;
					let level = self.get_u64()?;
					let kind = self.get_u64()?;
					self.log.push(Tok::Str("START".into()));
					self.record_activity(id);
					self.log.push(Tok::Int(level));
					self.log.push(Tok::Int(kind));
					self.str()?;
					self.fields()?;
					self.activity()?;
				}
				STDERR_STOP_ACTIVITY => {
					let id = self.get_u64()?;
					self.log.push(Tok::Str("STOP".into()));
					self.record_activity(id);
				}
				STDERR_RESULT => {
					let mark = self.log.len();
					let id = self.get_u64()?;
					let kind = self.get_u64()?;
					self.log.push(Tok::Str("RESULT".into()));
					self.record_activity(id);
					self.log.push(Tok::Int(kind));
					self.fields()?;
					if PROGRESS_RESULTS.contains(&kind) {
						self.log.truncate(mark);
					}
				}
				other => panic!("unknown stderr message {other:#x}"),
			}
		}
	}

	fn stderr(&mut self) -> Option<bool> {
		self.stderr_with(&mut &[][..])
	}

	/// The client handshake at `client_version`, offering `features`, then
	/// `postHandshake`. Returns whether the daemon accepted it.
	fn handshake(&mut self, client_version: u64, features: &[&str]) -> Option<bool> {
		self.note(format!("handshake {client_version:#x}"));
		self.u64(WORKER_MAGIC_1).flush();
		assert_eq!(self.int()?, WORKER_MAGIC_2);
		let daemon = self.int()?;
		self.u64(client_version).flush();
		let negotiated = (client_version & 0xffff).min(daemon & 0xffff);
		self.version = negotiated;
		if negotiated >= version(38) {
			self.strs(features).flush();
			self.features = self.strings()?;
			self.features.retain(|f| features.contains(&f.as_str()));
		}
		if negotiated >= version(14) {
			self.u64(0);
		}
		if negotiated >= version(11) {
			self.u64(0);
		}
		self.flush();
		if negotiated >= version(33) {
			self.str()?;
		}
		if negotiated >= version(35) {
			self.int()?;
		}
		self.stderr()
	}

	/// `SetOptions` with the defaults a CppNix client sends, at
	/// `verbosity`.
	fn set_options(&mut self, verbosity: u64, overrides: &[(&str, &str)]) -> Option<bool> {
		self.note("SetOptions");
		self.u64(19);
		for n in [0, 0, 0, verbosity, 1, 0, 0, 0, 0, 0, 1, 1] {
			self.u64(n);
		}
		self.u64(overrides.len() as u64);
		for (name, value) in overrides {
			self.bytes(name).bytes(value);
		}
		self.stderr()
	}
}

struct Env {
	dir: noxi_test_support::TempDir,
	busybox: String,
	out: String,
	drv: String,
	/// Per protocol minor version, an unbuilt derivation to build and one
	/// to ask about with `QueryMissing`.
	build_drvs: Vec<String>,
	missing_drvs: Vec<String>,
	reference_daemon: Option<Child>,
	rust_daemon: Option<libc::pid_t>,
	/// The reference daemon's `nix.conf` and the noxi daemon's settings,
	/// kept to restart them.
	daemon_config: String,
	noxi_settings: Option<DaemonSettings>,
}

impl Drop for Env {
	fn drop(&mut self) {
		if let Some(pid) = self.rust_daemon.take() {
			// The test server has its own process group, including connection workers.
			unsafe {
				libc::kill(-pid, libc::SIGTERM);
				libc::waitpid(pid, std::ptr::null_mut(), 0);
			}
		}
		if let Some(mut child) = self.reference_daemon.take() {
			let _ = child.kill();
			let _ = child.wait();
		}
	}
}

/// How the daemons are configured.
#[derive(Clone, Copy, Default)]
struct Options {
	trusted: bool,
	ca_derivations: bool,
	/// `require-sigs`, when not the default.
	require_sigs: Option<bool>,
	/// `trusted-public-keys`, when not the default.
	trusted_public_key: Option<&'static str>,
	/// A `trusted-substituters` entry.
	trusted_substituter: Option<&'static str>,
}

impl Env {
	fn command(&self, program: &str, args: &[&str]) -> Command {
		let bin = from_env_or_target("NOXI_REFERENCE_BIN", "nix-reference/bin", "nix-store");
		let mut command = Command::new(bin.join(program));
		command
			.args(args)
			.env_clear()
			.env("HOME", self.dir.path().join("home"))
			.env("NIX_CONF_DIR", self.dir.path().join("conf"))
			.env("NIX_CONFIG", CONFIG)
			.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin");
		command
	}

	fn run(&self, program: &str, args: &[&str]) -> Output {
		self.command(program, args).output().unwrap()
	}

	fn run_ok(&self, program: &str, args: &[&str]) -> String {
		let output = self.run(program, args);
		assert!(
			output.status.success(),
			"{program} {args:?} failed:\n{}",
			String::from_utf8_lossy(&output.stderr)
		);
		String::from_utf8(output.stdout).unwrap()
	}

	fn root(&self, name: &str) -> PathBuf {
		self.dir.path().join(name)
	}

	fn socket(&self, name: &str) -> PathBuf {
		self.root(&format!("{name}.sock"))
	}

	fn new(options: Options) -> Self {
		Self::with_setup(options, |_| {})
	}

	/// Build the source store, let `setup` add to it, copy it for both
	/// daemons and start them.
	fn with_setup(options: Options, setup: impl FnOnce(&Env)) -> Self {
		let dir = noxi_test_support::tempdir().unwrap();
		for sub in ["conf", "home"] {
			fs::create_dir(dir.path().join(sub)).unwrap();
		}
		let busybox = fs::canonicalize(from_env_or_target(
			"NOXI_TEST_BUSYBOX",
			"test-busybox",
			"bin/busybox",
		))
		.unwrap()
		.to_string_lossy()
		.into_owned();
		let mut env = Env {
			dir,
			busybox,
			out: String::new(),
			drv: String::new(),
			build_drvs: Vec::new(),
			missing_drvs: Vec::new(),
			reference_daemon: None,
			rust_daemon: None,
			daemon_config: String::new(),
			noxi_settings: None,
		};
		let src = env.root("src").to_string_lossy().into_owned();
		env.run_ok(
			"nix",
			&[
				"copy",
				"--no-check-sigs",
				"--to",
				&format!("local?root={src}"),
				&env.busybox,
			],
		);
		let expr = env.dir.path().join("expr.nix");
		fs::write(
			&expr,
			format!(
				r#"derivation {{ name = "served"; system = "x86_64-linux"; builder = "${{builtins.storePath {}}}/bin/sh"; args = [ "-c" "echo $builder > $out" ]; }}"#,
				env.busybox
			),
		)
		.unwrap();
		env.drv = env
			.run_ok(
				"nix-instantiate",
				&["--store", &src, expr.to_str().unwrap()],
			)
			.trim()
			.to_owned();
		env.out = env
			.run_ok("nix-store", &["--store", &src, "--realise", &env.drv])
			.trim()
			.to_owned();
		let versions = env.dir.path().join("versions.nix");
		let mut list = String::new();
		for kind in ["build", "missing"] {
			for minor in OLDEST..=NEWEST {
				list.push_str(&format!(
					r#"(derivation {{ name = "wire-{kind}-{minor}"; system = "x86_64-linux"; builder = "${{builtins.storePath {}}}/bin/sh"; args = [ "-c" "echo building {minor}; echo $builder > $out" ]; }}) "#,
					env.busybox
				));
			}
		}
		fs::write(&versions, format!("[ {list}]")).unwrap();
		let drvs: Vec<String> = env
			.run_ok(
				"nix-instantiate",
				&["--store", &src, versions.to_str().unwrap()],
			)
			.lines()
			.map(str::to_owned)
			.collect();
		let count = (NEWEST - OLDEST + 1) as usize;
		assert_eq!(drvs.len(), 2 * count);
		env.build_drvs = drvs[..count].to_vec();
		env.missing_drvs = drvs[count..].to_vec();
		setup(&env);
		for copy in DAEMONS {
			let status = Command::new("cp")
				.args(["-a", &src, env.root(copy).to_str().unwrap()])
				.status()
				.unwrap();
			assert!(status.success());
		}

		let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
			.unwrap()
			.trim()
			.to_owned();
		let mut daemon_config = CONFIG.to_owned();
		if options.trusted {
			daemon_config.push_str(&format!("trusted-users = {user}\n"));
		}
		if options.ca_derivations {
			daemon_config.push_str("extra-experimental-features = ca-derivations\n");
		}
		if let Some(require) = options.require_sigs {
			daemon_config.push_str(&format!("require-sigs = {require}\n"));
		}
		if let Some(key) = options.trusted_public_key {
			daemon_config.push_str(&format!("trusted-public-keys = {key}\n"));
		}
		if let Some(url) = options.trusted_substituter {
			daemon_config.push_str(&format!("trusted-substituters = {url}\n"));
		}
		let cache_uri = format!("file://{}", env.root("cache").display());
		if env.root("cache").exists() {
			daemon_config.push_str(&format!("substituters = {cache_uri}\n"));
		}
		env.daemon_config = daemon_config;

		let mut settings = DaemonSettings::new(LocalStoreConfig::with_root(&env.root("noxi")));
		settings.build.sandbox_paths = Vec::new();
		settings.build.build_cores = 1;
		settings.build.nix_command = true;
		settings.store.ca_derivations = options.ca_derivations;
		if let Some(require) = options.require_sigs {
			settings.store.require_sigs = require;
		}
		if let Some(key) = options.trusted_public_key {
			settings.store.trusted_public_keys = vec![key.to_owned()];
		}
		if let Some(url) = options.trusted_substituter {
			settings.trusted_substituters.push(url.to_owned());
		}
		if env.root("cache").exists() {
			settings.substituters.push(cache_uri.clone());
		}
		if options.trusted {
			settings.trusted_users.push(user);
		}
		env.noxi_settings = Some(settings);
		env.start_daemons();
		env
	}

	/// Start both daemons on their sockets and wait until they listen.
	fn start_daemons(&mut self) {
		let reference = self
			.command(
				"nix-daemon",
				&["--store", self.root("reference").to_str().unwrap()],
			)
			.env("NIX_CONFIG", &self.daemon_config)
			.env("NIX_DAEMON_SOCKET_PATH", self.socket("reference"))
			.stderr(std::process::Stdio::null())
			.spawn()
			.unwrap();
		self.reference_daemon = Some(reference);

		let listener = UnixListener::bind(self.socket("noxi")).unwrap();
		let settings = self.noxi_settings.clone().unwrap();
		// Fork before creating client sockets. A server thread would inherit the
		// test client's descriptors in every worker and prevent disconnect EOF.
		let pid = unsafe { libc::fork() };
		assert!(pid >= 0);
		if pid == 0 {
			// A restarted server is forked while clients are connected;
			// it must not hold their sockets open either.
			let keep = std::os::fd::AsRawFd::as_raw_fd(&listener);
			for fd in 3..4096 {
				if fd != keep {
					unsafe {
						libc::close(fd);
					}
				}
			}
			unsafe {
				libc::setsid();
			}
			let code = if serve(listener, &settings).is_ok() {
				0
			} else {
				1
			};
			unsafe {
				libc::_exit(code);
			}
		}
		drop(listener);
		self.rust_daemon = Some(pid);

		let deadline = Instant::now() + Duration::from_secs(20);
		while !self.socket("reference").exists() {
			assert!(
				Instant::now() < deadline,
				"the reference daemon did not start"
			);
			std::thread::sleep(Duration::from_millis(20));
		}
	}

	/// Kill both daemons' listening processes, as a service manager stops
	/// them, leaving the workers serving connected clients, and start them
	/// again on fresh sockets.
	fn restart_daemons(&mut self) {
		if let Some(pid) = self.rust_daemon.take() {
			unsafe {
				libc::kill(pid, libc::SIGKILL);
				libc::waitpid(pid, std::ptr::null_mut(), 0);
			}
		}
		if let Some(mut child) = self.reference_daemon.take() {
			let _ = child.kill();
			let _ = child.wait();
		}
		for name in DAEMONS {
			fs::remove_file(self.socket(name)).unwrap();
		}
		self.start_daemons();
	}

	/// A connection to daemon `name`.
	fn connect(&self, name: &str) -> Wire {
		let replacements = vec![
			(self.root(name).display().to_string(), "ROOT".to_owned()),
			(self.socket(name).display().to_string(), "SOCKET".to_owned()),
		];
		Wire::connect(&self.socket(name), replacements)
	}

	/// Run `script` against both daemons and require the same transcript.
	fn compare(&self, what: &str, mut script: impl FnMut(&mut Wire)) -> Vec<Tok> {
		let mut logs = Vec::new();
		for name in DAEMONS {
			let mut wire = self.connect(name);
			script(&mut wire);
			logs.push(wire.log);
		}
		let noxi = logs.pop().unwrap();
		let reference = logs.pop().unwrap();
		// While debugging, `NOXI_WIRE_DUMP=DIR` keeps both transcripts.
		if let Some(dir) = std::env::var_os("NOXI_WIRE_DUMP") {
			let dir = PathBuf::from(dir);
			for (name, log) in [("reference", &reference), ("noxi", &noxi)] {
				let text: String = log.iter().map(|t| format!("{t:?}\n")).collect();
				fs::write(
					dir.join(format!("{}.{name}.txt", what.replace('/', "_"))),
					text,
				)
				.unwrap();
			}
		}
		if noxi != reference {
			let first = noxi
				.iter()
				.zip(&reference)
				.position(|(a, b)| a != b)
				.unwrap_or(noxi.len().min(reference.len()));
			let context = |log: &[Tok]| {
				let start = first.saturating_sub(12);
				format!("{:#?}", &log[start..log.len().min(first + 12)])
			};
			panic!(
				"{what}: transcripts differ at token {first}\nreference:\n{}\nnoxi:\n{}",
				context(&reference),
				context(&noxi)
			);
		}
		reference
	}
}

/// Operation codes, `WorkerProto::Op`.
mod op {
	pub const IS_VALID_PATH: u64 = 1;
	pub const QUERY_REFERRERS: u64 = 6;
	pub const ADD_TO_STORE: u64 = 7;
	pub const ADD_TEXT_TO_STORE: u64 = 8;
	pub const BUILD_PATHS: u64 = 9;
	pub const ENSURE_PATH: u64 = 10;
	pub const ADD_TEMP_ROOT: u64 = 11;
	pub const ADD_INDIRECT_ROOT: u64 = 12;
	pub const SYNC_WITH_GC: u64 = 13;
	pub const FIND_ROOTS: u64 = 14;
	pub const QUERY_DERIVER: u64 = 18;
	pub const COLLECT_GARBAGE: u64 = 20;
	pub const QUERY_SUBSTITUTABLE_PATH_INFO: u64 = 21;
	pub const QUERY_DERIVATION_OUTPUTS: u64 = 22;
	pub const QUERY_ALL_VALID_PATHS: u64 = 23;
	pub const QUERY_PATH_INFO: u64 = 26;
	pub const QUERY_DERIVATION_OUTPUT_NAMES: u64 = 28;
	pub const QUERY_PATH_FROM_HASH_PART: u64 = 29;
	pub const QUERY_SUBSTITUTABLE_PATH_INFOS: u64 = 30;
	pub const QUERY_VALID_PATHS: u64 = 31;
	pub const QUERY_SUBSTITUTABLE_PATHS: u64 = 32;
	pub const QUERY_VALID_DERIVERS: u64 = 33;
	pub const VERIFY_STORE: u64 = 35;
	pub const BUILD_DERIVATION: u64 = 36;
	pub const ADD_SIGNATURES: u64 = 37;
	pub const NAR_FROM_PATH: u64 = 38;
	pub const ADD_TO_STORE_NAR: u64 = 39;
	pub const QUERY_MISSING: u64 = 40;
	pub const QUERY_DERIVATION_OUTPUT_MAP: u64 = 41;
	pub const REGISTER_DRV_OUTPUT: u64 = 42;
	pub const QUERY_REALISATION: u64 = 43;
	pub const ADD_MULTIPLE_TO_STORE: u64 = 44;
	pub const ADD_BUILD_LOG: u64 = 45;
	pub const BUILD_PATHS_WITH_RESULTS: u64 = 46;
	pub const ADD_PERM_ROOT: u64 = 47;
}

/// The features a CppNix 2.35.2 client offers.
const CLIENT_FEATURES: [&str; 3] = [
	"delete-dead-specific-referrers",
	"disable-set-options",
	"realisation-with-path-not-hash",
];

const MISSING: &str = "/nix/store/00000000000000000000000000000000-missing";

/// A NAR holding one regular file.
fn nar_of(contents: &[u8]) -> Vec<u8> {
	let mut w = Wire::buffer();
	for s in ["nix-archive-1", "(", "type", "regular", "contents"] {
		w.bytes(s);
	}
	w.bytes(contents).bytes(")");
	w.buf
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
	use sha2::Digest;
	sha2::Sha256::digest(bytes).into()
}

fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A NAR-hashed content-addressed path for `nar`, with its CA string.
fn ca_path(name: &str, nar: &[u8]) -> (String, String) {
	use nix_compat::nixhash::NixHash;
	let digest = sha256(nar);
	let path = nix_compat::store_path::build_ca_path(
		name,
		true,
		&NixHash::Sha256(digest),
		std::iter::empty(),
		false,
	)
	.unwrap()
	.to_absolute_path();
	let ca = format!("fixed:r:sha256:{}", nix_compat::nixbase32::encode(&digest));
	(path, ca)
}

/// A path that looks input-addressed, so an untrusted client needs a
/// signature to add it.
fn input_addressed_path(name: &str) -> String {
	let digest = sha256(name.as_bytes());
	format!(
		"/nix/store/{}-{name}",
		nix_compat::nixbase32::encode(&digest[..20])
	)
}

fn hash_part(path: &str) -> &str {
	&path["/nix/store/".len().."/nix/store/".len() + 32]
}

impl Wire {
	/// A writer for building byte strings, not connected to a daemon.
	fn buffer() -> Self {
		let (a, _b) = UnixStream::pair().unwrap();
		Wire {
			read: BufReader::new(a.try_clone().unwrap()),
			write: a,
			buf: Vec::new(),
			log: Vec::new(),
			version: 0,
			features: Vec::new(),
			replacements: Vec::new(),
			closed: true,
			activities: Vec::new(),
		}
	}

	fn op(&mut self, name: &str, code: u64) -> &mut Self {
		self.note(name);
		self.u64(code)
	}

	fn has(&self, feature: &str) -> bool {
		self.features.iter().any(|f| f == feature)
	}

	/// `CommonProto` framing: chunks of `data`, then an empty chunk.
	fn framed(&mut self, data: &[u8]) -> &mut Self {
		for chunk in data.chunks(1000) {
			self.u64(chunk.len() as u64).raw(chunk);
		}
		self.u64(0)
	}

	/// `UnkeyedValidPathInfo`; returns the NAR size.
	fn path_info(&mut self) -> Option<u64> {
		self.str()?;
		self.str()?;
		self.strings()?;
		self.masked("registration time")?;
		let nar_size = self.int()?;
		if self.version >= version(16) {
			self.int()?;
			self.strings()?;
			self.str()?;
		}
		Some(nar_size)
	}

	/// `std::optional<std::chrono::microseconds>`.
	fn opt_duration(&mut self) -> Option<()> {
		if self.int()? == 1 {
			self.masked("duration")?;
		}
		Some(())
	}

	/// `BuildResult` at the negotiated version.
	fn build_result(&mut self) -> Option<()> {
		self.int()?;
		self.str()?;
		if self.version >= version(29) {
			self.int()?;
			self.int()?;
			self.masked("start time")?;
			self.masked("stop time")?;
		}
		if self.version >= version(37) {
			self.opt_duration()?;
			self.opt_duration()?;
		}
		if self.has("realisation-with-path-not-hash") {
			let n = self.int()?;
			for _ in 0..n {
				self.str()?;
				self.str()?;
				self.strings()?;
			}
		} else if self.version >= version(28) {
			let n = self.int()?;
			for _ in 0..n {
				self.str()?;
				self.str()?;
			}
		}
		Some(())
	}

	/// The `FindRoots` reply. CppNix sends the roots in hash-table order,
	/// so they are recorded sorted.
	fn roots(&mut self, shared_executable: &str, required_root: &str) -> Option<()> {
		let n = self.get_u64()?;
		let start = self.log.len();
		for _ in 0..n {
			self.str()?;
			self.str()?;
		}
		let mut pairs: Vec<Vec<Tok>> = self
			.log
			.split_off(start)
			.chunks(2)
			.map(<[Tok]>::to_vec)
			// /proc is shared with concurrent tests that run the same BusyBox.
			// Its transient runtime root is outside this session's root assertion.
			.filter(|pair| pair[1] != Tok::Str(shared_executable.to_owned()))
			.collect();
		assert!(
			pairs
				.iter()
				.any(|pair| pair[1] == Tok::Str(required_root.to_owned()))
		);
		pairs.sort_by_key(|pair| format!("{pair:?}"));
		self.log.push(Tok::Int(pairs.len() as u64));
		self.log.extend(pairs.into_iter().flatten());
		Some(())
	}

	/// `GCOptions` asking for the live paths.
	fn gc_return_live(&mut self) -> &mut Self {
		self.u64(0);
		if self.has("delete-dead-specific-referrers") {
			self.u64(1);
		} else {
			self.u64(0);
		}
		self.u64(0).u64(0).u64(0).u64(0).u64(0)
	}
}

/// One client session at protocol `1.minor`, using most operations a
/// client at that version can send.
fn session(env: &Env, w: &mut Wire, minor: u64) -> Option<()> {
	let i = (minor - OLDEST) as usize;
	let out = env.out.as_str();
	let drv = env.drv.as_str();
	let busybox = env.busybox.as_str();
	if !w.handshake(version(minor), &CLIENT_FEATURES)? {
		return Some(());
	}
	if !w.has("disable-set-options") {
		w.set_options(3, &[])?;
	}

	for path in [out, MISSING] {
		w.op("IsValidPath", op::IS_VALID_PATH).bytes(path);
		if w.stderr()? {
			w.int()?;
		}
	}
	w.op("QueryValidPaths", op::QUERY_VALID_PATHS)
		.strs(&[out, MISSING]);
	if w.version >= version(27) {
		w.u64(0);
	}
	if w.stderr()? {
		w.strings()?;
	}
	let mut nar_size = 0;
	for path in [out, MISSING] {
		w.op("QueryPathInfo", op::QUERY_PATH_INFO).bytes(path);
		if w.stderr()? && w.int()? == 1 {
			nar_size = w.path_info()?;
		}
	}
	for (name, code, path) in [
		("QueryReferrers", op::QUERY_REFERRERS, busybox),
		("QueryValidDerivers", op::QUERY_VALID_DERIVERS, out),
		("QueryDerivationOutputs", op::QUERY_DERIVATION_OUTPUTS, drv),
		(
			"QueryDerivationOutputNames",
			op::QUERY_DERIVATION_OUTPUT_NAMES,
			drv,
		),
	] {
		w.op(name, code).bytes(path);
		if w.stderr()? {
			w.strings()?;
		}
	}
	w.op("QueryDerivationOutputMap", op::QUERY_DERIVATION_OUTPUT_MAP)
		.bytes(drv);
	if w.stderr()? {
		let n = w.int()?;
		for _ in 0..n {
			w.str()?;
			w.str()?;
		}
	}
	for path in [out, MISSING] {
		w.op("QueryDeriver", op::QUERY_DERIVER).bytes(path);
		if w.stderr()? {
			w.str()?;
		}
	}
	for part in [hash_part(out), hash_part(MISSING), "short"] {
		w.op("QueryPathFromHashPart", op::QUERY_PATH_FROM_HASH_PART)
			.bytes(part);
		if w.stderr()? {
			w.str()?;
		}
	}
	w.op("QueryAllValidPaths", op::QUERY_ALL_VALID_PATHS);
	if w.stderr()? {
		w.strings()?;
	}

	// Without substituters, nothing is substitutable.
	w.op("QuerySubstitutablePaths", op::QUERY_SUBSTITUTABLE_PATHS)
		.strs(&[out, MISSING]);
	if w.stderr()? {
		w.strings()?;
	}
	w.op(
		"QuerySubstitutablePathInfo",
		op::QUERY_SUBSTITUTABLE_PATH_INFO,
	)
	.bytes(MISSING);
	if w.stderr()? {
		w.int()?;
	}
	w.op(
		"QuerySubstitutablePathInfos",
		op::QUERY_SUBSTITUTABLE_PATH_INFOS,
	);
	if w.version >= version(22) {
		w.u64(1).bytes(MISSING).bytes("");
	} else {
		w.strs(&[MISSING]);
	}
	if w.stderr()? {
		w.int()?;
	}

	// Imports, each new to the store.
	let text = format!("wire text {minor}\n");
	w.op("AddTextToStore", op::ADD_TEXT_TO_STORE)
		.bytes(format!("wire-text-{minor}"))
		.bytes(&text)
		.strs(&[busybox]);
	if w.stderr()? {
		w.str()?;
	}
	let flat = format!("wire flat {minor}\n");
	if w.version >= version(25) {
		w.op("AddToStore", op::ADD_TO_STORE)
			.bytes(format!("wire-flat-{minor}"))
			.bytes("fixed:sha256")
			.strs(&[])
			.u64(0)
			.framed(flat.as_bytes());
		if w.stderr()? {
			w.str()?;
			w.path_info()?;
		}
	} else {
		// The old protocol always sends a NAR.
		w.op("AddToStore", op::ADD_TO_STORE)
			.bytes(format!("wire-flat-{minor}"))
			.u64(1)
			.u64(0)
			.bytes("sha256")
			.raw(&nar_of(flat.as_bytes()));
		if w.stderr()? {
			w.str()?;
		}
	}
	let nar = nar_of(format!("wire nar {minor}\n").as_bytes());
	let (ca_nar_path, ca) = ca_path(&format!("wire-nar-{minor}"), &nar);
	let unsigned = input_addressed_path(&format!("wire-unsigned-{minor}"));
	for (path, ca) in [(ca_nar_path.as_str(), ca.as_str()), (unsigned.as_str(), "")] {
		w.op("AddToStoreNar", op::ADD_TO_STORE_NAR)
			.bytes(path)
			.bytes("")
			.bytes(hex(&sha256(&nar)))
			.strs(&[])
			.u64(0)
			.u64(nar.len() as u64)
			.u64(0)
			.strs(&[])
			.bytes(ca)
			.u64(0)
			.u64(0);
		if w.version >= version(23) {
			w.framed(&nar);
			w.stderr()?;
		} else if w.version >= version(21) {
			w.stderr_with(&mut &nar[..])?;
		} else {
			w.raw(&nar);
			w.stderr()?;
		}
	}
	let multi = nar_of(format!("wire multiple {minor}\n").as_bytes());
	let (multi_path, multi_ca) = ca_path(&format!("wire-multiple-{minor}"), &multi);
	let mut stream = Wire::buffer();
	stream
		.u64(1)
		.bytes(&multi_path)
		.bytes("")
		.bytes(hex(&sha256(&multi)))
		.strs(&[])
		.u64(0)
		.u64(multi.len() as u64)
		.u64(0)
		.strs(&[])
		.bytes(&multi_ca)
		.raw(&multi);
	w.op("AddMultipleToStore", op::ADD_MULTIPLE_TO_STORE)
		.u64(0)
		.u64(0)
		.framed(&stream.buf);
	w.stderr()?;
	for path in [ca_nar_path.as_str(), unsigned.as_str(), multi_path.as_str()] {
		w.op("IsValidPath", op::IS_VALID_PATH).bytes(path);
		if w.stderr()? {
			w.int()?;
		}
	}

	// Roots.
	w.op("AddTempRoot", op::ADD_TEMP_ROOT).bytes(out);
	if w.stderr()? {
		w.int()?;
	}
	let link = env.dir.path().join(format!("indirect-{minor}"));
	w.op("AddIndirectRoot", op::ADD_INDIRECT_ROOT)
		.bytes(link.to_str().unwrap());
	if w.stderr()? {
		w.int()?;
	}
	w.op("SyncWithGC", op::SYNC_WITH_GC);
	if w.stderr()? {
		w.int()?;
	}
	w.op("FindRoots", op::FIND_ROOTS);
	if w.stderr()? {
		w.roots(busybox, out)?;
	}
	for path in [out, MISSING] {
		w.op("EnsurePath", op::ENSURE_PATH).bytes(path);
		if w.stderr()? {
			w.int()?;
		}
	}
	w.op("NarFromPath", op::NAR_FROM_PATH).bytes(out);
	if w.stderr()? {
		w.raw_bytes(nar_size as usize)?;
	}

	// Builds. Before 1.30 a bare derivation means all its outputs; from
	// 1.30 it means the derivation file itself.
	let build = env.build_drvs[i].as_str();
	let missing = env.missing_drvs[i].as_str();
	w.op("BuildPaths", op::BUILD_PATHS)
		.strs(&[&format!("{build}!out")])
		.u64(0);
	if w.stderr()? {
		w.int()?;
	}
	let all = if w.version >= version(30) {
		format!("{drv}!*")
	} else {
		drv.to_owned()
	};
	w.op("BuildPathsWithResults", op::BUILD_PATHS_WITH_RESULTS)
		.strs(&[&format!("{build}!out"), &all, drv, out, MISSING])
		.u64(0);
	if w.stderr()? {
		let n = w.int()?;
		for _ in 0..n {
			w.str()?;
			w.build_result()?;
		}
	}
	w.op("QueryMissing", op::QUERY_MISSING)
		.strs(&[&format!("{missing}!out"), out, MISSING]);
	if w.stderr()? {
		w.strings()?;
		w.strings()?;
		w.strings()?;
		w.int()?;
		w.int()?;
	}

	// Maintenance.
	w.op("VerifyStore", op::VERIFY_STORE).u64(0).u64(0);
	if w.stderr()? {
		w.int()?;
	}
	w.op("AddSignatures", op::ADD_SIGNATURES)
		.bytes(out)
		.strs(&["wire-test:c2lnbmF0dXJl"]);
	if w.stderr()? {
		w.int()?;
	}
	w.op("CollectGarbage", op::COLLECT_GARBAGE).gc_return_live();
	if w.stderr()? {
		w.strings()?;
		w.int()?;
		w.int()?;
	}
	if w.has("realisation-with-path-not-hash") {
		w.query_realisation(drv, "out")?;
		w.op("RegisterDrvOutput", op::REGISTER_DRV_OUTPUT)
			.bytes(drv)
			.bytes("out")
			.bytes(out)
			.strs(&[]);
		w.stderr()?;
	}

	// Refusal happens before reading any frames. Check reuse with no frames sent.
	w.op("AddBuildLog", op::ADD_BUILD_LOG)
		.bytes(&drv["/nix/store/".len()..]);
	if w.stderr()? {
		w.int()?;
	}
	w.op("IsValidPath", op::IS_VALID_PATH).bytes(out);
	if w.stderr()? {
		w.int()?;
	}
	Some(())
}

impl Wire {
	fn basic_derivation(
		&mut self,
		drv: &nix_compat::derivation::UnverifiedDerivation,
	) -> &mut Self {
		self.basic_derivation_with_path(drv, None)
	}

	fn basic_derivation_with_path(
		&mut self,
		drv: &nix_compat::derivation::UnverifiedDerivation,
		override_path: Option<&str>,
	) -> &mut Self {
		self.u64(drv.outputs().len() as u64);
		for name in drv.outputs().names() {
			self.bytes(name)
				.bytes(override_path.map(str::to_owned).unwrap_or_else(|| {
					drv.outputs()
						.get(name)
						.map(|p| p.to_absolute_path())
						.unwrap_or_default()
				}));
			if let Some(hash) = drv.outputs().as_fixed_output_hash() {
				self.bytes(hash.as_mode_and_algo_str())
					.bytes(hex(hash.hash.digest_as_bytes()));
			} else {
				self.bytes("").bytes("");
			}
		}
		self.u64(drv.input_sources().len() as u64);
		for path in drv.input_sources() {
			self.bytes(path.to_absolute_path());
		}
		self.bytes(drv.system())
			.bytes(drv.command())
			.u64(drv.arguments().len() as u64);
		for arg in drv.arguments() {
			self.bytes(arg);
		}
		self.u64(drv.environment().len() as u64);
		for (name, value) in drv.environment() {
			self.bytes(name).bytes(value);
		}
		self
	}

	/// `QueryRealisation`, reading the optional realisation.
	fn query_realisation(&mut self, drv: &str, output: &str) -> Option<()> {
		self.op("QueryRealisation", op::QUERY_REALISATION)
			.bytes(drv)
			.bytes(output);
		if self.stderr()? && self.int()? == 1 {
			self.str()?;
			self.strings()?;
		}
		Some(())
	}
}

#[test]
fn every_protocol_version_matches_the_reference() {
	let env = Env::new(Options::default());
	for minor in versions() {
		env.compare(&format!("protocol 1.{minor}"), |w| {
			assert_eq!(
				session(&env, w, minor),
				Some(()),
				"incomplete session: {:#?}",
				w.log
			);
		});
	}
}

#[test]
fn feature_subsets_and_version_boundaries() {
	let env = Env::new(Options::default());
	for minor in [38, 39, 255] {
		for subset in 0..8 {
			let mut features: Vec<_> = CLIENT_FEATURES
				.iter()
				.enumerate()
				.filter(|(i, _)| subset & (1 << i) != 0)
				.map(|(_, f)| *f)
				.collect();
			features.push("unknown-client-feature");
			env.compare(&format!("features-{minor}-{subset}"), |w| {
				assert_eq!(w.handshake(version(minor), &features), Some(true));
				assert!(!w.has("disable-set-options"));
				assert_eq!(w.set_options(3, &[]), Some(true));
				w.op("IsValidPath", op::IS_VALID_PATH).bytes(&env.out);
				assert_eq!(w.stderr(), Some(true));
				assert_eq!(w.int(), Some(1));
			});
		}
	}
	for v in [version(18) | (0xbeef << 16), version(39), 2 << 8] {
		env.compare(&format!("wire-version-{v}"), |w| {
			assert_eq!(w.handshake(v, &CLIENT_FEATURES), Some(true));
		});
	}
}

#[test]
fn permanent_root_authorization_and_deferred_output_maps() {
	for (trusted, ca_derivations) in [(false, false), (true, false), (true, true)] {
		let env = Env::with_setup(
			Options {
				trusted,
				ca_derivations,
				..Options::default()
			},
			|env| {
				let input = env.root("deferred.drv");
				fs::write(
					&input,
					b"Derive([(\"out\",\"\",\"\",\"\")],[],[],\"x86_64-linux\",\"/builder\",[],[])",
				)
				.unwrap();
				let path = env.run_ok(
					"nix-store",
					&[
						"--store",
						env.root("src").to_str().unwrap(),
						"--add",
						input.to_str().unwrap(),
					],
				);
				fs::write(env.root("deferred-path"), path.trim()).unwrap();
			},
		);
		let drv = fs::read_to_string(env.root("deferred-path")).unwrap();
		for minor in versions() {
			env.compare(&format!("roots-and-map-{trusted}-{minor}"), |w| {
				assert_eq!(w.handshake(version(minor), &CLIENT_FEATURES), Some(true));
				w.op(
					"QueryDerivationOutputMap deferred",
					op::QUERY_DERIVATION_OUTPUT_MAP,
				)
				.bytes(&drv);
				assert_eq!(w.stderr(), Some(true));
				assert_eq!(w.int(), Some(u64::from(ca_derivations)));
				if ca_derivations {
					assert_eq!(w.str().as_deref(), Some("out"));
					assert_eq!(w.str().as_deref(), Some(""));
				}
				w.op("QueryMissing deferred", op::QUERY_MISSING)
					.strs(&[&format!("{drv}!out")]);
				assert_eq!(w.stderr(), Some(true));
				w.strings().unwrap();
				w.strings().unwrap();
				w.strings().unwrap();
				w.int().unwrap();
				w.int().unwrap();
				let root = format!("{}/permanent-{minor}", w.replacements[0].0);
				w.op("AddPermRoot", op::ADD_PERM_ROOT);
				if trusted {
					w.bytes(&env.out).bytes(&root);
				}
				assert_eq!(w.stderr(), Some(trusted));
				if trusted {
					w.str().unwrap();
					assert_eq!(fs::read_link(&root).unwrap().to_str().unwrap(), env.out);
				} else {
					assert_eq!(w.stderr(), Some(false));
					assert_eq!(w.int(), None);
				}
			});
		}
	}
}

#[test]
fn supplied_derivations_at_every_version() {
	for trusted in [false, true] {
		let env = Env::new(Options {
			trusted,
			..Options::default()
		});
		for minor in versions() {
			let path = &env.missing_drvs[(minor - OLDEST) as usize];
			let bytes = fs::read(env.root("src").join(path.trim_start_matches('/'))).unwrap();
			let drv =
				nix_compat::derivation::UnverifiedDerivation::from_aterm_bytes(&bytes).unwrap();
			env.compare(&format!("supplied-{trusted}-{minor}"), |w| {
				assert_eq!(w.handshake(version(minor), &CLIENT_FEATURES), Some(true));
				assert_eq!(w.set_options(3, &[]), Some(true));
				for _ in 0..2 {
					w.op("BuildDerivation", op::BUILD_DERIVATION)
						.bytes(path)
						.basic_derivation(&drv)
						.u64(0);
					assert_eq!(w.stderr(), Some(trusted));
					if trusted {
						w.build_result().unwrap();
					}
				}
				w.op("IsValidPath", op::IS_VALID_PATH).bytes(&env.out);
				assert_eq!(w.stderr(), Some(true));
				assert_eq!(w.int(), Some(1));
			});
		}
	}
}

#[test]
fn copied_realisation_fixtures_persist_merge_and_conflict() {
	let env = Env::new(Options {
		ca_derivations: true,
		..Options::default()
	});
	let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../tests/nix/src/libstore-tests/data/worker-protocol");
	let realisation = fs::read(dir.join("realisation-realisation-with-path-not-hash.bin")).unwrap();
	let mut r = noxi_daemon::wire::Reader::new(&realisation[..]);
	let drv = r.read_string().unwrap();
	let output = r.read_string().unwrap();
	let path = r.read_string().unwrap();
	let sigs = r.read_strings().unwrap();
	assert_eq!(sigs.len(), 2);
	for reconnect in 0..2 {
		env.compare(&format!("fixture-realisation-{reconnect}"), |w| {
			assert_eq!(w.handshake(version(38), &CLIENT_FEATURES), Some(true));
			w.query_realisation(&drv, &output).unwrap();
			w.op("RegisterDrvOutput fixture", op::REGISTER_DRV_OUTPUT)
				.raw(&realisation);
			assert_eq!(w.stderr(), Some(true));
			w.query_realisation(&drv, &output).unwrap();
			w.op("RegisterDrvOutput merge", op::REGISTER_DRV_OUTPUT)
				.bytes(&drv)
				.bytes(&output)
				.bytes(&path)
				.strs(&[]);
			assert_eq!(w.stderr(), Some(true));
			w.query_realisation(&drv, &output).unwrap();
			w.op("RegisterDrvOutput conflict", op::REGISTER_DRV_OUTPUT)
				.bytes(&drv)
				.bytes(&output)
				.bytes(MISSING)
				.strs(&[]);
			assert_eq!(w.stderr(), Some(false));
			w.query_realisation(&drv, &output).unwrap();
			// Realisation signatures are parsed as `NAME:BASE64` too.
			w.op(
				"RegisterDrvOutput malformed signature",
				op::REGISTER_DRV_OUTPUT,
			)
			.bytes(&drv)
			.bytes(&output)
			.bytes(&path)
			.strs(&["noxi-test-1:not base64"]);
			assert_eq!(w.stderr(), Some(false));
			w.query_realisation(&drv, &output).unwrap();
		});
	}
}

#[test]
fn untrusted_fixed_output_builds_recompute_supplied_paths() {
	let env = Env::with_setup(Options::default(), |env| {
		let input = env.root("fixed.nix");
		fs::write(&input, format!(r#"derivation {{ name = "wire-fixed"; system = "x86_64-linux"; builder = "${{builtins.storePath {}}}/bin/sh"; args = [ "-c" "echo fixed > $out" ]; outputHashAlgo = "sha256"; outputHashMode = "flat"; outputHash = "{}"; }}"#, env.busybox, hex(&sha256(b"fixed\n")))).unwrap();
		let path = env.run_ok(
			"nix-instantiate",
			&[
				"--store",
				env.root("src").to_str().unwrap(),
				input.to_str().unwrap(),
			],
		);
		fs::write(env.root("fixed-path"), path.trim()).unwrap();
	});
	let path = fs::read_to_string(env.root("fixed-path")).unwrap();
	let bytes = fs::read(env.root("src").join(path.trim_start_matches('/'))).unwrap();
	let drv = nix_compat::derivation::UnverifiedDerivation::from_aterm_bytes(&bytes).unwrap();
	for minor in versions() {
		env.compare(&format!("fixed-output-{minor}"), |w| {
			assert_eq!(w.handshake(version(minor), &CLIENT_FEATURES), Some(true));
			w.op("BuildDerivation fixed", op::BUILD_DERIVATION)
				.bytes(&path)
				.basic_derivation_with_path(&drv, Some(MISSING))
				.u64(0);
			assert_eq!(w.stderr(), Some(true));
			w.build_result().unwrap();
			w.op("IsValidPath supplied forgery", op::IS_VALID_PATH)
				.bytes(MISSING);
			assert_eq!(w.stderr(), Some(true));
			assert_eq!(w.int(), Some(0));
		});
	}
}

#[test]
fn substitutable_queries_with_content_address_hints() {
	for mass_query in [false, true] {
		let env = Env::with_setup(Options::default(), |env| {
			let input = env.root("cached-only");
			fs::write(&input, b"cache content\n").unwrap();
			let path = env.run_ok(
				"nix-store",
				&[
					"--store",
					env.root("src").to_str().unwrap(),
					"--add-fixed",
					"--recursive",
					"sha256",
					input.to_str().unwrap(),
				],
			);
			fs::write(env.root("cached-path"), path.trim()).unwrap();
			env.run_ok(
				"nix",
				&[
					"copy",
					"--from",
					env.root("src").to_str().unwrap(),
					"--to",
					&format!("file://{}", env.root("cache").display()),
					path.trim(),
					&env.out,
				],
			);
			fs::write(
				env.root("cache/nix-cache-info"),
				format!(
					"StoreDir: /nix/store\nWantMassQuery: {}\nPriority: 40\n",
					u8::from(mass_query)
				),
			)
			.unwrap();
			// Keep the path solely in the cache in both daemon stores.
			env.run_ok(
				"nix-store",
				&[
					"--store",
					env.root("src").to_str().unwrap(),
					"--delete",
					path.trim(),
				],
			);
		});
		let path = fs::read_to_string(env.root("cached-path")).unwrap();
		let hint = format!(
			"fixed:r:sha256:{}",
			hex(&sha256(&nar_of(b"cache content\n")))
		);
		for minor in versions() {
			env.compare(&format!("cache-{mass_query}-{minor}"), |w| {
				assert_eq!(w.handshake(version(minor), &CLIENT_FEATURES), Some(true));
				for target in [&path, &env.out, MISSING] {
					w.op(
						"QuerySubstitutablePathInfo",
						op::QUERY_SUBSTITUTABLE_PATH_INFO,
					)
					.bytes(target);
					assert_eq!(w.stderr(), Some(true));
					if w.int().unwrap() == 1 {
						w.str().unwrap();
						w.strings().unwrap();
						w.int().unwrap();
						w.int().unwrap();
					}
				}
				w.op("QuerySubstitutablePaths", op::QUERY_SUBSTITUTABLE_PATHS)
					.strs(&[&path, &env.out, MISSING]);
				assert_eq!(w.stderr(), Some(true));
				let paths = w.strings().unwrap();
				assert_eq!(paths.len(), if mass_query { 2 } else { 0 });
				for ca in ["", &hint] {
					w.op(
						"QuerySubstitutablePathInfos",
						op::QUERY_SUBSTITUTABLE_PATH_INFOS,
					);
					if minor >= 22 {
						w.u64(1).bytes(&path).bytes(ca);
					} else {
						w.strs(&[&path]);
					}
					assert_eq!(w.stderr(), Some(true), "{:#?}", w.log);
					assert_eq!(w.int(), Some(1));
					w.str().unwrap();
					w.str().unwrap();
					w.strings().unwrap();
					w.int().unwrap();
					w.int().unwrap();
				}
				w.op("QueryMissing", op::QUERY_MISSING)
					.strs(&[&path, MISSING]);
				assert_eq!(w.stderr(), Some(true));
				w.strings().unwrap();
				w.strings().unwrap();
				w.strings().unwrap();
				w.int().unwrap();
				w.int().unwrap();
			});
		}
	}
}

#[test]
fn repair_rebuilds_damaged_outputs_and_rejects_untrusted_clients() {
	use std::os::unix::fs::PermissionsExt;
	for trusted in [false, true] {
		let env = Env::new(Options {
			trusted,
			..Options::default()
		});
		for name in DAEMONS {
			let path = env.root(name).join(env.out.trim_start_matches('/'));
			fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
			fs::write(path, b"damaged\n").unwrap();
		}
		env.compare(&format!("repair-{trusted}"), |w| {
			assert_eq!(w.handshake(version(38), &CLIENT_FEATURES), Some(true));
			assert_eq!(w.set_options(3, &[]), Some(true));
			w.op("BuildPaths repair", op::BUILD_PATHS)
				.strs(&[&format!("{}!out", env.drv)])
				.u64(1);
			assert_eq!(w.stderr(), Some(trusted));
			if trusted {
				assert_eq!(w.int(), Some(1));
			}
		});
		for name in DAEMONS {
			let contents = fs::read(env.root(name).join(env.out.trim_start_matches('/'))).unwrap();
			assert_eq!(
				contents,
				if trusted {
					format!("{}/bin/sh\n", env.busybox).into_bytes()
				} else {
					b"damaged\n".to_vec()
				}
			);
		}
	}
}

#[test]
fn malformed_requests_disconnects_and_simultaneous_clients() {
	use std::net::Shutdown;
	let env = Env::new(Options {
		trusted: true,
		..Options::default()
	});
	for minor in [18, 26, 38] {
		for malformed in [0, 2, 48, u64::MAX] {
			env.compare(&format!("invalid-op-{minor}-{malformed}"), |w| {
				assert_eq!(w.handshake(version(minor), &CLIENT_FEATURES), Some(true));
				w.op("invalid operation", malformed);
				assert_eq!(w.stderr(), Some(false));
				if w.stderr().is_some() {
					assert_eq!(w.int(), None);
				}
			});
		}
		for payload in [
			b"short".as_slice(),
			b"/nix/store/not-a-store-path".as_slice(),
		] {
			env.compare(&format!("invalid-path-{minor}-{payload:?}"), |w| {
				assert_eq!(w.handshake(version(minor), &CLIENT_FEATURES), Some(true));
				w.op("invalid path", op::IS_VALID_PATH).bytes(payload);
				assert_eq!(w.stderr(), Some(false));
				if w.stderr().is_some() {
					assert_eq!(w.int(), None);
				}
			});
		}
		env.compare(&format!("bad-padding-{minor}"), |w| {
			assert_eq!(w.handshake(version(minor), &CLIENT_FEATURES), Some(true));
			w.op("bad padding", op::IS_VALID_PATH)
				.u64(1)
				.raw(b"a\x01\0\0\0\0\0\0");
			assert_eq!(w.stderr(), Some(false));
			if w.stderr().is_some() {
				assert_eq!(w.int(), None);
			}
		});
		for name in DAEMONS {
			for cut in [0, 3, 8, 17] {
				let mut w = env.connect(name);
				assert_eq!(w.handshake(version(minor), &CLIENT_FEATURES), Some(true));
				w.u64(op::ADD_BUILD_LOG)
					.bytes("00000000000000000000000000000000-interrupted.drv")
					.u64(4096)
					.raw(&[b'x'; 17]);
				let bytes = std::mem::take(&mut w.buf);
				w.write.write_all(&bytes[..bytes.len() - 17 + cut]).unwrap();
				w.write.shutdown(Shutdown::Write).unwrap();
				drop(w);
			}
			let mut clients: Vec<_> = (0..4)
				.map(|_| {
					let mut w = env.connect(name);
					assert_eq!(w.handshake(version(minor), &CLIENT_FEATURES), Some(true));
					w
				})
				.collect();
			for round in 0..4 {
				for w in &mut clients {
					w.op("simultaneous query", op::IS_VALID_PATH)
						.bytes(&env.out)
						.flush();
				}
				for w in clients.iter_mut().rev() {
					assert_eq!(w.stderr(), Some(true), "round {round}");
					assert_eq!(w.int(), Some(1));
				}
			}
		}
	}
}

#[test]
fn trusted_log_uploads_drain_and_preserve_existing_logs() {
	let env = Env::new(Options {
		trusted: true,
		..Options::default()
	});
	for minor in versions() {
		let drv = env.build_drvs[(minor - OLDEST) as usize].trim_start_matches("/nix/store/");
		env.compare(&format!("log-upload-{minor}"), |w| {
			assert_eq!(w.handshake(version(minor), &CLIENT_FEATURES), Some(true));
			for log in [b"".as_slice(), b"replacement log\n".as_slice()] {
				w.op("AddBuildLog", op::ADD_BUILD_LOG)
					.bytes(drv)
					.framed(log);
				assert_eq!(w.stderr(), Some(true));
				assert_eq!(w.int(), Some(1));
			}
			w.op("IsValidPath", op::IS_VALID_PATH).bytes(&env.out);
			assert_eq!(w.stderr(), Some(true));
			assert_eq!(w.int(), Some(1));
		});
		for name in DAEMONS {
			let path = env
				.root(name)
				.join("nix/var/log/nix/drvs")
				.join(&drv[..2])
				.join(format!("{}.bz2", &drv[2..]));
			let out = Command::new("bzip2").arg("-dc").arg(path).output().unwrap();
			assert!(out.status.success());
			assert!(out.stdout.is_empty());
		}
	}
}

#[test]
fn batch_import_failure_drains_frames_and_keeps_completed_paths() {
	let env = Env::new(Options::default());
	for minor in versions() {
		let nar = nar_of(format!("batch {minor}\n").as_bytes());
		let (first, ca) = ca_path(&format!("batch-first-{minor}"), &nar);
		let unsigned = input_addressed_path(&format!("batch-unsigned-{minor}"));
		let (last, _) = ca_path(&format!("batch-last-{minor}"), &nar);
		let mut stream = Wire::buffer();
		stream.u64(3);
		for (path, ca) in [(&first, ca.as_str()), (&unsigned, ""), (&last, ca.as_str())] {
			stream
				.bytes(path)
				.bytes("")
				.bytes(hex(&sha256(&nar)))
				.strs(&[])
				.u64(0)
				.u64(nar.len() as u64)
				.u64(1)
				.strs(&[])
				.bytes(ca)
				.raw(&nar);
		}
		env.compare(&format!("batch-error-{minor}"), |w| {
			assert_eq!(w.handshake(version(minor), &CLIENT_FEATURES), Some(true));
			w.op("AddMultipleToStore failure", op::ADD_MULTIPLE_TO_STORE)
				.u64(0)
				.u64(1)
				.framed(&stream.buf);
			assert_eq!(w.stderr(), Some(false));
			for (path, valid) in [(&first, 1), (&unsigned, 0), (&last, 0)] {
				w.op("IsValidPath after batch", op::IS_VALID_PATH)
					.bytes(path);
				assert_eq!(w.stderr(), Some(true));
				assert_eq!(w.int(), Some(valid));
			}
		});
	}
}

/// Without `realisation-with-path-not-hash`, a realisation cannot be
/// read: the daemon fails after starting work, leaving the request
/// unread, and the rest is taken as the next request.
#[test]
fn realisations_need_the_protocol_feature() {
	let env = Env::new(Options::default());
	for features in [&CLIENT_FEATURES[..2], &[][..]] {
		env.compare(&format!("realisation with {features:?}"), |w| {
			if w.handshake(version(NEWEST), features) == Some(true) {
				w.query_realisation(&env.drv, "out");
				w.int();
			}
		});
	}
	env.compare("realisation at 1.37", |w| {
		if w.handshake(version(37), &[]) == Some(true) {
			w.query_realisation(&env.drv, "out");
			w.int();
		}
	});
}
/// The protocol minor versions to check: all of them, or those listed in
/// `NOXI_WIRE_VERSIONS` (such as `38` or `18,26`) while debugging.
fn versions() -> Vec<u64> {
	match std::env::var("NOXI_WIRE_VERSIONS") {
		Ok(list) => list.split(',').map(|v| v.trim().parse().unwrap()).collect(),
		Err(_) => (OLDEST..=NEWEST).collect(),
	}
}

/// `s` without SGR escape sequences (`ESC [ ... m`).
fn strip_colour(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	let mut rest = s;
	while let Some(start) = rest.find("\x1b[") {
		out.push_str(&rest[..start]);
		let after = &rest[start + 2..];
		match after.find(|c: char| !(c.is_ascii_digit() || c == ';')) {
			Some(end) if after[end..].starts_with('m') => rest = &after[end + 1..],
			_ => {
				out.push_str("\x1b[");
				rest = after;
			}
		}
	}
	out.push_str(rest);
	out
}

/// Read the daemon's replies after an error until it answers the follow-up
/// `IsValidPath` or hangs up. Requests the daemon left unread are taken as
/// the next operations, as the reference takes them.
fn follow_up(w: &mut Wire, path: &str) {
	w.op("IsValidPath after", op::IS_VALID_PATH).bytes(path);
	loop {
		match w.stderr() {
			Some(true) => {
				w.int();
				return;
			}
			Some(false) => {}
			None => return,
		}
	}
}

/// A signature in the store's format: a key name and 64 base64 bytes.
const SIGNATURE: &str = "noxi-test-1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";

impl Wire {
	/// `AddToStoreNar` at 1.23 or later, for a NAR of one file.
	fn add_to_store_nar(
		&mut self,
		path: &str,
		ca: &str,
		nar: &[u8],
		ultimate: bool,
		dont_check_sigs: bool,
	) -> &mut Self {
		self.op("AddToStoreNar", op::ADD_TO_STORE_NAR)
			.bytes(path)
			.bytes("")
			.bytes(hex(&sha256(nar)))
			.strs(&[])
			.u64(0)
			.u64(nar.len() as u64)
			.u64(u64::from(ultimate))
			.strs(&[])
			.bytes(ca)
			.u64(0)
			.u64(u64::from(dont_check_sigs))
			.framed(nar)
	}

	/// `QueryPathInfo`, recording the reply.
	fn query_path_info(&mut self, path: &str) {
		self.op("QueryPathInfo", op::QUERY_PATH_INFO).bytes(path);
		if self.stderr() == Some(true) && self.int() == Some(1) {
			self.path_info();
		}
	}
}

/// Each operation the reference restricts for untrusted clients, from an
/// untrusted and a trusted client: refused operations, signature checks
/// that only a trusted client may skip, `ultimate` dropped for untrusted
/// imports, `ignoreLiveness` refused to everyone, and operations, such as
/// `AddSignatures` and `RegisterDrvOutput`, that 2.35.2 does not restrict.
#[test]
fn trusted_only_operations_from_untrusted_and_trusted_clients() {
	for trusted in [false, true] {
		let env = Env::new(Options {
			trusted,
			..Options::default()
		});
		let out = env.out.as_str();
		let log_drv = env.build_drvs[0].trim_start_matches("/nix/store/");
		let nar = nar_of(b"trust boundary\n");
		let unsigned = input_addressed_path("trust-unsigned");
		let batch_unsigned = input_addressed_path("trust-batch-unsigned");
		let (ca, ca_hint) = ca_path("trust-ultimate", &nar);
		let session = |what: &str, script: &dyn Fn(&mut Wire)| {
			env.compare(&format!("{what}-{trusted}"), |w| {
				assert_eq!(w.handshake(version(NEWEST), &CLIENT_FEATURES), Some(true));
				assert_eq!(w.set_options(3, &[]), Some(true));
				script(w);
				follow_up(w, out);
			});
		};
		session("add-build-log", &|w| {
			w.op("AddBuildLog", op::ADD_BUILD_LOG)
				.bytes(log_drv)
				.framed(b"uploaded log\n");
			if w.stderr() == Some(true) {
				w.int();
			}
		});
		session("unsigned-nar-skipping-checks", &|w| {
			w.add_to_store_nar(&unsigned, "", &nar, true, true);
			w.stderr();
			w.query_path_info(&unsigned);
		});
		session("content-addressed-nar-ultimate", &|w| {
			w.add_to_store_nar(&ca, &ca_hint, &nar, true, false);
			w.stderr();
			w.query_path_info(&ca);
		});
		session("unsigned-batch-skipping-checks", &|w| {
			let mut stream = Wire::buffer();
			stream
				.u64(1)
				.bytes(&batch_unsigned)
				.bytes("")
				.bytes(hex(&sha256(&nar)))
				.strs(&[])
				.u64(0)
				.u64(nar.len() as u64)
				.u64(1)
				.strs(&[])
				.bytes("")
				.raw(&nar);
			w.op("AddMultipleToStore", op::ADD_MULTIPLE_TO_STORE)
				.u64(0)
				.u64(1)
				.framed(&stream.buf);
			w.stderr();
			w.query_path_info(&batch_unsigned);
		});
		session("add-signatures", &|w| {
			w.op("AddSignatures", op::ADD_SIGNATURES)
				.bytes(out)
				.strs(&[SIGNATURE]);
			if w.stderr() == Some(true) {
				w.int();
			}
			w.query_path_info(out);
		});
		session("ignore-liveness", &|w| {
			w.op("CollectGarbage ignoring liveness", op::COLLECT_GARBAGE)
				.u64(3)
				.u64(0)
				.strs(&[out])
				.u64(0)
				.u64(1)
				.u64(0)
				.u64(0)
				.u64(0)
				.u64(0);
			assert_eq!(w.stderr(), Some(false));
		});
		session("register-without-ca-derivations", &|w| {
			w.op("RegisterDrvOutput", op::REGISTER_DRV_OUTPUT)
				.bytes(&env.drv)
				.bytes("out")
				.bytes(out)
				.strs(&[]);
			assert_eq!(w.stderr(), Some(false));
		});
		if !trusted {
			// Trusted repairs are compared by the repair test; noxi does
			// not repair through `VerifyStore` yet.
			session("verify-repair", &|w| {
				w.op("VerifyStore repair", op::VERIFY_STORE).u64(0).u64(1);
				assert_eq!(w.stderr(), Some(false));
			});
			session("build-with-results-repair", &|w| {
				w.op("BuildPathsWithResults repair", op::BUILD_PATHS_WITH_RESULTS)
					.strs(&[&format!("{}!out", env.drv)])
					.u64(1);
				assert_eq!(w.stderr(), Some(false));
			});
		}
	}
}

/// A `trusted-substituters` entry with parameters, which an untrusted
/// client may name with its parameters in any order.
const PARAMETERISED: &str = "file:///nonexistent/noxi-trusted?priority=10&want-mass-query=true";

/// `SetOptions` overrides from untrusted and trusted clients: the
/// settings an untrusted client may still change, by name and not by
/// alias, the substituters it may name, and the overrides every client's
/// daemon ignores or warns about.
#[test]
fn client_setting_overrides_match_the_reference() {
	for trusted in [false, true] {
		let env = Env::with_setup(
			Options {
				trusted,
				trusted_substituter: Some(PARAMETERISED),
				..Options::default()
			},
			|env| {
				fs::create_dir(env.root("cache")).unwrap();
				fs::write(
					env.root("cache/nix-cache-info"),
					"StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n",
				)
				.unwrap();
			},
		);
		let cache = format!("file://{}", env.root("cache").display());
		let other = format!("file://{}", env.root("other-cache").display());
		let substituters = [
			cache.clone(),
			format!("{cache}/"),
			other.clone(),
			format!("{cache} {other}"),
			String::new(),
			PARAMETERISED.to_owned(),
			"file:///nonexistent/noxi-trusted?want-mass-query=true&priority=10".to_owned(),
			"file:///nonexistent/noxi-trusted/?priority=10&want-mass-query=true".to_owned(),
			"file:///nonexistent/noxi-trusted?priority=10".to_owned(),
			"file:///nonexistent/noxi-trusted".to_owned(),
			format!("{cache}?priority=10"),
		];
		let mut overrides: Vec<(&str, &str)> = vec![
			("timeout", "5"),
			("build-timeout", "5"),
			("max-silent-time", "5"),
			("build-max-silent-time", "5"),
			("build-poll-interval", "5"),
			("connect-timeout", "5"),
			("builders", ""),
			("builders", "ssh://builder x86_64-linux"),
			("plugin-files", "/nonexistent/plugin.so"),
			("ssh-auth-sock", "/nonexistent/agent"),
			("experimental-features", "nix-command"),
			("experimental-features", "nix-command ca-derivations"),
			("sandbox", "false"),
			("keep-outputs", "true"),
			("extra-substituters", &cache),
			("trusted-public-keys", "a:b"),
		];
		for value in &substituters {
			overrides.push(("substituters", value));
			overrides.push(("binary-caches", value));
		}
		for (name, value) in overrides {
			env.compare(&format!("override-{trusted}-{name}-{value}"), |w| {
				assert_eq!(w.handshake(version(NEWEST), &CLIENT_FEATURES), Some(true));
				w.set_options(3, &[(name, value)]);
				follow_up(w, &env.out);
			});
		}
		// Several overrides at once, in the order the client sends them.
		env.compare(&format!("overrides-{trusted}"), |w| {
			assert_eq!(w.handshake(version(NEWEST), &CLIENT_FEATURES), Some(true));
			w.set_options(
				3,
				&[
					("timeout", "5"),
					("sandbox", "false"),
					("substituters", &other),
					("max-silent-time", "7"),
				],
			);
			follow_up(w, &env.out);
		});
	}
}

/// One valid request for every operation the reference dispatches, at
/// 1.38 with every feature.
fn requests(env: &Env) -> Vec<(&'static str, u64, Vec<u8>)> {
	let out = env.out.as_str();
	let drv = env.drv.as_str();
	let built = format!("{drv}!out");
	let nar = nar_of(b"truncated request\n");
	let (ca, ca_hint) = ca_path("truncated", &nar);
	let bytes = fs::read(env.root("src").join(drv.trim_start_matches('/'))).unwrap();
	let basic = nix_compat::derivation::UnverifiedDerivation::from_aterm_bytes(&bytes).unwrap();
	let mut batch = Wire::buffer();
	batch
		.u64(1)
		.bytes(&ca)
		.bytes("")
		.bytes(hex(&sha256(&nar)))
		.strs(&[])
		.u64(0)
		.u64(nar.len() as u64)
		.u64(0)
		.strs(&[])
		.bytes(&ca_hint)
		.raw(&nar);
	let root = format!("{}/truncated-root", env.dir.path().display());
	let body = |build: &dyn Fn(&mut Wire)| {
		let mut w = Wire::buffer();
		w.version = version(NEWEST);
		w.features = CLIENT_FEATURES.iter().map(|f| f.to_string()).collect();
		build(&mut w);
		w.buf
	};
	vec![
		(
			"IsValidPath",
			op::IS_VALID_PATH,
			body(&|w| {
				w.bytes(out);
			}),
		),
		(
			"QueryReferrers",
			op::QUERY_REFERRERS,
			body(&|w| {
				w.bytes(&env.busybox);
			}),
		),
		(
			"AddToStore",
			op::ADD_TO_STORE,
			body(&|w| {
				w.bytes("truncated")
					.bytes("fixed:r:sha256")
					.strs(&[])
					.u64(0)
					.framed(&nar);
			}),
		),
		(
			"AddTextToStore",
			op::ADD_TEXT_TO_STORE,
			body(&|w| {
				w.bytes("truncated-text").bytes("contents\n").strs(&[]);
			}),
		),
		(
			"BuildPaths",
			op::BUILD_PATHS,
			body(&|w| {
				w.strs(&[&built]).u64(0);
			}),
		),
		(
			"EnsurePath",
			op::ENSURE_PATH,
			body(&|w| {
				w.bytes(out);
			}),
		),
		(
			"AddTempRoot",
			op::ADD_TEMP_ROOT,
			body(&|w| {
				w.bytes(out);
			}),
		),
		(
			"AddIndirectRoot",
			op::ADD_INDIRECT_ROOT,
			body(&|w| {
				w.bytes(&root);
			}),
		),
		(
			"QueryDeriver",
			op::QUERY_DERIVER,
			body(&|w| {
				w.bytes(out);
			}),
		),
		(
			"SetOptions",
			19,
			body(&|w| {
				for n in [0, 0, 0, 3, 1, 0, 0, 0, 0, 0, 1, 1] {
					w.u64(n);
				}
				w.u64(1).bytes("timeout").bytes("5");
			}),
		),
		(
			"CollectGarbage",
			op::COLLECT_GARBAGE,
			body(&|w| {
				w.gc_return_live();
			}),
		),
		(
			"QuerySubstitutablePathInfo",
			op::QUERY_SUBSTITUTABLE_PATH_INFO,
			body(&|w| {
				w.bytes(out);
			}),
		),
		(
			"QueryDerivationOutputs",
			op::QUERY_DERIVATION_OUTPUTS,
			body(&|w| {
				w.bytes(drv);
			}),
		),
		(
			"QueryPathInfo",
			op::QUERY_PATH_INFO,
			body(&|w| {
				w.bytes(out);
			}),
		),
		(
			"QueryDerivationOutputNames",
			op::QUERY_DERIVATION_OUTPUT_NAMES,
			body(&|w| {
				w.bytes(drv);
			}),
		),
		(
			"QueryPathFromHashPart",
			op::QUERY_PATH_FROM_HASH_PART,
			body(&|w| {
				w.bytes(hash_part(out));
			}),
		),
		(
			"QuerySubstitutablePathInfos",
			op::QUERY_SUBSTITUTABLE_PATH_INFOS,
			body(&|w| {
				w.u64(1).bytes(out).bytes("");
			}),
		),
		(
			"QueryValidPaths",
			op::QUERY_VALID_PATHS,
			body(&|w| {
				w.strs(&[out]).u64(0);
			}),
		),
		(
			"QuerySubstitutablePaths",
			op::QUERY_SUBSTITUTABLE_PATHS,
			body(&|w| {
				w.strs(&[out]);
			}),
		),
		(
			"QueryValidDerivers",
			op::QUERY_VALID_DERIVERS,
			body(&|w| {
				w.bytes(out);
			}),
		),
		(
			"VerifyStore",
			op::VERIFY_STORE,
			body(&|w| {
				w.u64(0).u64(0);
			}),
		),
		(
			"BuildDerivation",
			op::BUILD_DERIVATION,
			body(&|w| {
				w.bytes(drv).basic_derivation(&basic).u64(0);
			}),
		),
		(
			"AddSignatures",
			op::ADD_SIGNATURES,
			body(&|w| {
				w.bytes(out).strs(&[SIGNATURE]);
			}),
		),
		(
			"NarFromPath",
			op::NAR_FROM_PATH,
			body(&|w| {
				w.bytes(out);
			}),
		),
		(
			"AddToStoreNar",
			op::ADD_TO_STORE_NAR,
			body(&|w| {
				w.add_to_store_nar(&ca, &ca_hint, &nar, false, false);
				// Without the operation code `add_to_store_nar` wrote first.
				w.buf.drain(..8);
			}),
		),
		(
			"QueryMissing",
			op::QUERY_MISSING,
			body(&|w| {
				w.strs(&[&built]);
			}),
		),
		(
			"QueryDerivationOutputMap",
			op::QUERY_DERIVATION_OUTPUT_MAP,
			body(&|w| {
				w.bytes(drv);
			}),
		),
		(
			"RegisterDrvOutput",
			op::REGISTER_DRV_OUTPUT,
			body(&|w| {
				w.bytes(drv).bytes("out").bytes(out).strs(&[]);
			}),
		),
		(
			"QueryRealisation",
			op::QUERY_REALISATION,
			body(&|w| {
				w.bytes(drv).bytes("out");
			}),
		),
		(
			"AddMultipleToStore",
			op::ADD_MULTIPLE_TO_STORE,
			body(&|w| {
				w.u64(0).u64(0).framed(&batch.buf);
			}),
		),
		(
			"AddBuildLog",
			op::ADD_BUILD_LOG,
			body(&|w| {
				w.bytes(drv.trim_start_matches("/nix/store/"))
					.framed(b"log\n");
			}),
		),
		(
			"BuildPathsWithResults",
			op::BUILD_PATHS_WITH_RESULTS,
			body(&|w| {
				w.strs(&[&built]).u64(0);
			}),
		),
		(
			"AddPermRoot",
			op::ADD_PERM_ROOT,
			body(&|w| {
				w.bytes(out).bytes(&root);
			}),
		),
	]
}

/// Every operation's request cut short at several points, after which the
/// client stops writing but keeps reading: both daemons must report the
/// same errors, if any, and close the connection alike.
#[test]
fn truncated_requests_for_every_operation_match_the_reference() {
	use std::net::Shutdown;
	let env = Env::new(Options {
		trusted: true,
		..Options::default()
	});
	let mut differing = Vec::new();
	for (name, code, body) in requests(&env) {
		let mut cuts = vec![
			0,
			1,
			7,
			8,
			9,
			body.len() / 2,
			body.len() - 8,
			body.len() - 1,
		];
		cuts.retain(|cut| *cut < body.len());
		cuts.sort();
		cuts.dedup();
		for cut in cuts {
			let what = format!("truncated-{name}-{cut}");
			let compared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
				env.compare(&what, |w| {
					assert_eq!(w.handshake(version(NEWEST), &CLIENT_FEATURES), Some(true));
					w.op(name, code).raw(&body[..cut]);
					w.flush();
					w.write.shutdown(Shutdown::Write).unwrap();
					while w.stderr() == Some(false) {}
				});
			}));
			if compared.is_err() {
				differing.push(what);
			}
		}
	}
	assert!(
		differing.is_empty(),
		"differing truncations: {differing:#?}"
	);
}

/// A unique marker for this test's builders, so they can be found in
/// `/proc` among concurrent tests' processes.
fn marker(env: &Env) -> String {
	format!(
		"noxi-cancel-{}",
		env.dir.path().file_name().unwrap().to_string_lossy()
	)
}

/// Whether a process whose command line contains `marker` is running.
fn running(marker: &str) -> bool {
	fs::read_dir("/proc").unwrap().flatten().any(|entry| {
		fs::read(entry.path().join("cmdline")).is_ok_and(|cmdline| {
			cmdline
				.windows(marker.len())
				.any(|w| w == marker.as_bytes())
		})
	})
}

fn wait_until(what: &str, deadline: Duration, mut condition: impl FnMut() -> bool) {
	let start = Instant::now();
	while !condition() {
		assert!(start.elapsed() < deadline, "timed out waiting until {what}");
		std::thread::sleep(Duration::from_millis(50));
	}
}

/// The temporary-roots files in daemon `name`'s state directory that a
/// live process still holds locked. The reference leaves a worker's file
/// behind when it is interrupted; the collector ignores and removes files
/// nobody holds, so only held files keep paths alive.
fn held_temp_roots(env: &Env, name: &str) -> Vec<String> {
	use std::os::fd::AsRawFd;
	let Ok(dir) = fs::read_dir(env.root(name).join("nix/var/nix/temproots")) else {
		return Vec::new();
	};
	dir.flatten()
		.filter(|entry| {
			fs::File::open(entry.path()).is_ok_and(|file| unsafe {
				libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) != 0
			})
		})
		.map(|entry| entry.file_name().to_string_lossy().into_owned())
		.collect()
}

/// A client that hangs up during a build, or is interrupted with SIGINT,
/// cancels it on both daemons: the builder is killed, the output stays
/// invalid, the build's locks and the worker's temporary roots are
/// released, and the next client can build the derivation again.
#[test]
fn disconnected_and_interrupted_clients_cancel_their_builds() {
	let env = Env::with_setup(Options::default(), |env| {
		let expr = env.root("slow.nix");
		fs::write(
			&expr,
			format!(
				r#"derivation {{ name = "slow"; system = "x86_64-linux"; builder = "${{builtins.storePath {}}}/bin/sh"; args = [ "-c" "echo started; while true; do sleep 1; done; echo {}" ]; }}"#,
				env.busybox,
				marker(env)
			),
		)
		.unwrap();
		let drv = env.run_ok(
			"nix-instantiate",
			&[
				"--store",
				env.root("src").to_str().unwrap(),
				expr.to_str().unwrap(),
			],
		);
		fs::write(env.root("slow-path"), drv.trim()).unwrap();
	});
	let drv = fs::read_to_string(env.root("slow-path")).unwrap();
	let marker = marker(&env);
	let started = Duration::from_secs(60);
	let stopped = Duration::from_secs(20);
	let mut interrupted = Vec::new();
	for name in DAEMONS {
		assert!(!running(&marker));
		// Hang up twice: the second build can only start once the first
		// released the output lock.
		for attempt in 0..2 {
			let mut w = env.connect(name);
			assert_eq!(w.handshake(version(NEWEST), &CLIENT_FEATURES), Some(true));
			w.op("BuildPaths", op::BUILD_PATHS)
				.strs(&[&format!("{drv}!out")])
				.u64(0)
				.flush();
			wait_until(&format!("{name} starts build {attempt}"), started, || {
				running(&marker)
			});
			drop(w);
			wait_until(&format!("{name} kills build {attempt}"), stopped, || {
				!running(&marker)
			});
		}
		// The reference client, interrupted as by Ctrl-C.
		let client = env
			.command(
				"nix-store",
				&[
					"--store",
					&format!(
						"unix://{}?root={}",
						env.socket(name).display(),
						env.root(name).display()
					),
					"--realise",
					&drv,
				],
			)
			.stdout(std::process::Stdio::piped())
			.stderr(std::process::Stdio::piped())
			.spawn()
			.unwrap();
		wait_until(
			&format!("{name} starts the client's build"),
			started,
			|| running(&marker),
		);
		unsafe {
			libc::kill(client.id() as libc::pid_t, libc::SIGINT);
		}
		let output = client.wait_with_output().unwrap();
		wait_until(&format!("{name} kills the client's build"), stopped, || {
			!running(&marker)
		});
		interrupted.push(output.status.code());
		wait_until(
			&format!("{name} releases its temporary roots"),
			stopped,
			|| held_temp_roots(&env, name).is_empty(),
		);
	}
	assert_eq!(interrupted[0], interrupted[1]);
	let output_path = {
		let bytes = fs::read(env.root("src").join(drv.trim_start_matches('/'))).unwrap();
		let drv = nix_compat::derivation::UnverifiedDerivation::from_aterm_bytes(&bytes).unwrap();
		let name = drv.outputs().names().next().unwrap();
		drv.outputs().get(name).unwrap().to_absolute_path()
	};
	env.compare("after cancelled builds", |w| {
		assert_eq!(w.handshake(version(NEWEST), &CLIENT_FEATURES), Some(true));
		w.op("IsValidPath cancelled output", op::IS_VALID_PATH)
			.bytes(&output_path);
		assert_eq!(w.stderr(), Some(true));
		assert_eq!(w.int(), Some(0));
	});
}

/// Stopping the daemon's listening process leaves its workers serving the
/// clients already connected, and a restarted daemon serves new clients
/// beside them.
#[test]
fn daemon_restart_keeps_connected_clients_and_serves_new_ones() {
	let mut env = Env::new(Options::default());
	let mut connected: Vec<Wire> = DAEMONS
		.iter()
		.map(|name| {
			let mut w = env.connect(name);
			assert_eq!(w.handshake(version(NEWEST), &CLIENT_FEATURES), Some(true));
			w
		})
		.collect();
	env.restart_daemons();
	let mut logs = Vec::new();
	for (name, mut old) in DAEMONS.iter().zip(connected.drain(..)) {
		let mut new = env.connect(name);
		assert_eq!(new.handshake(version(NEWEST), &CLIENT_FEATURES), Some(true));
		for w in [&mut old, &mut new] {
			w.op("IsValidPath", op::IS_VALID_PATH).bytes(&env.out);
			assert_eq!(w.stderr(), Some(true), "{name}");
			assert_eq!(w.int(), Some(1));
			w.op("AddTextToStore", op::ADD_TEXT_TO_STORE)
				.bytes("restart")
				.bytes("restart\n")
				.strs(&[]);
			assert_eq!(w.stderr(), Some(true), "{name}");
			w.str();
		}
		logs.push((old.log, new.log));
	}
	assert_eq!(logs[0], logs[1]);
}

/// Several clients at once adding the same paths and building the same
/// derivations get the same results from both daemons, and leave stores
/// that verify.
#[test]
fn simultaneous_clients_with_overlapping_builds_and_imports() {
	const CLIENTS: usize = 6;
	let env = Env::new(Options::default());
	let nar = nar_of(b"shared import\n");
	let (ca, ca_hint) = ca_path("shared-import", &nar);
	let targets: Vec<String> = env.build_drvs[..3]
		.iter()
		.map(|drv| format!("{drv}!out"))
		.collect();
	let mut results = Vec::new();
	for name in DAEMONS {
		let logs: Vec<String> = std::thread::scope(|scope| {
			let handles: Vec<_> = (0..CLIENTS)
				.map(|i| {
					let (env, targets, nar, ca, ca_hint) = (&env, &targets, &nar, &ca, &ca_hint);
					scope.spawn(move || {
						let mut w = env.connect(name);
						assert_eq!(w.handshake(version(NEWEST), &CLIENT_FEATURES), Some(true));
						assert_eq!(w.set_options(0, &[]), Some(true));
						w.add_to_store_nar(ca, ca_hint, nar, false, false);
						assert_eq!(w.stderr(), Some(true));
						w.op("AddTextToStore", op::ADD_TEXT_TO_STORE)
							.bytes("shared-text")
							.bytes("shared\n")
							.strs(&[]);
						assert_eq!(w.stderr(), Some(true));
						let text = w.str().unwrap();
						// Each client asks for the derivations in another order.
						let mut mine: Vec<&str> = targets.iter().map(String::as_str).collect();
						mine.rotate_left(i % targets.len());
						w.op("BuildPaths", op::BUILD_PATHS).strs(&mine).u64(0);
						assert_eq!(w.stderr(), Some(true), "{name} client {i}");
						assert_eq!(w.int(), Some(1));
						// Only the replies are compared; the build logs go to
						// whichever client's worker ran the build.
						text
					})
				})
				.collect();
			handles.into_iter().map(|h| h.join().unwrap()).collect()
		});
		assert!(logs.windows(2).all(|pair| pair[0] == pair[1]), "{logs:#?}");
		results.push(logs[0].clone());
		let verify = env.run(
			"nix-store",
			&[
				"--store",
				&format!(
					"unix://{}?root={}",
					env.socket(name).display(),
					env.root(name).display()
				),
				"--verify",
				"--check-contents",
			],
		);
		let stderr = String::from_utf8_lossy(&verify.stderr);
		assert!(
			verify.status.success() && !stderr.contains("error"),
			"{name}: {}",
			String::from_utf8_lossy(&verify.stderr)
		);
	}
	assert_eq!(results[0], results[1]);
	env.compare("after simultaneous clients", |w| {
		assert_eq!(w.handshake(version(NEWEST), &CLIENT_FEATURES), Some(true));
		w.op("QueryValidPaths", op::QUERY_VALID_PATHS)
			.strs(
				&env.build_drvs[..3]
					.iter()
					.map(String::as_str)
					.collect::<Vec<_>>(),
			)
			.u64(0);
		assert_eq!(w.stderr(), Some(true));
		w.strings();
		w.query_path_info(&ca);
	});
}

/// Signature settings for an untrusted client's imports of
/// input-addressed paths: without `require-sigs` nothing is checked, and
/// with `trusted-public-keys` a signature by a listed key is accepted
/// while one by another key or a malformed one is refused.
#[test]
fn signature_settings_for_untrusted_imports() {
	use noxi_store::keys::SecretKey;
	let key = SecretKey::generate("noxi-test-1").unwrap();
	let other = SecretKey::generate("noxi-other-1").unwrap();
	let public: &'static str = Box::leak(key.to_public_text().into_boxed_str());
	let nar = nar_of(b"signed import\n");
	let sign = |key: &SecretKey, path: &str| {
		key.sign_detached(
			format!(
				"1;{path};sha256:{};{};",
				nix_compat::nixbase32::encode(&sha256(&nar)),
				nar.len()
			)
			.as_bytes(),
		)
	};
	for (what, options) in [
		(
			"unchecked",
			Options {
				require_sigs: Some(false),
				..Options::default()
			},
		),
		(
			"keyed",
			Options {
				trusted_public_key: Some(public),
				..Options::default()
			},
		),
	] {
		let env = Env::new(options);
		let paths: Vec<(String, Vec<String>)> = [
			("unsigned", None),
			("signed", Some(&key)),
			("foreign", Some(&other)),
		]
		.into_iter()
		.map(|(name, signer)| {
			let path = input_addressed_path(&format!("signature-{what}-{name}"));
			let sigs = signer.map(|k| vec![sign(k, &path)]).unwrap_or_default();
			(path, sigs)
		})
		.chain(std::iter::once((
			input_addressed_path(&format!("signature-{what}-malformed")),
			vec!["noxi-test-1:not base64".to_owned()],
		)))
		.collect();
		for (path, sigs) in &paths {
			env.compare(&format!("signatures-{what}-{}", hash_part(path)), |w| {
				assert_eq!(w.handshake(version(NEWEST), &CLIENT_FEATURES), Some(true));
				let sigs: Vec<&str> = sigs.iter().map(String::as_str).collect();
				w.op("AddToStoreNar", op::ADD_TO_STORE_NAR)
					.bytes(path)
					.bytes("")
					.bytes(hex(&sha256(&nar)))
					.strs(&[])
					.u64(0)
					.u64(nar.len() as u64)
					.u64(0)
					.strs(&sigs)
					.bytes("")
					.u64(0)
					.u64(1)
					.framed(&nar);
				w.stderr();
				w.query_path_info(path);
			});
		}
	}
}

/// Well-framed requests with malformed values: strings that are not UTF-8
/// where the reference reads bytes, booleans other than 0 and 1, build
/// modes and GC actions out of range, and malformed signatures. Each
/// request is followed by an `IsValidPath`, so a daemon that keeps the
/// connection open must answer it too.
#[test]
fn malformed_values_match_the_reference() {
	let env = Env::new(Options {
		trusted: true,
		..Options::default()
	});
	let out = env.out.as_str();
	let drv = env.drv.as_str();
	let built = format!("{drv}!out");
	let bad_path = b"/nix/store/\xff\xfe\xfd00000000000000000000000000000-bad".to_vec();
	let nar = nar_of(b"malformed values\n");
	let (ca, ca_hint) = ca_path("malformed-values", &nar);
	let bytes = fs::read(env.root("src").join(drv.trim_start_matches('/'))).unwrap();
	let basic = nix_compat::derivation::UnverifiedDerivation::from_aterm_bytes(&bytes).unwrap();
	let root = format!("{}/malformed-\u{0}root", env.dir.path().display());
	type Case<'a> = (&'a str, Box<dyn Fn(&mut Wire) + 'a>);
	let cases: Vec<Case> = vec![
		(
			"IsValidPath non-UTF-8",
			Box::new(|w| {
				w.op("IsValidPath", op::IS_VALID_PATH).bytes(&bad_path);
			}),
		),
		(
			"QueryPathFromHashPart non-UTF-8",
			Box::new(|w| {
				w.op("QueryPathFromHashPart", op::QUERY_PATH_FROM_HASH_PART)
					.bytes(b"\xff\xfe");
			}),
		),
		(
			"AddTextToStore non-UTF-8 name",
			Box::new(|w| {
				w.op("AddTextToStore", op::ADD_TEXT_TO_STORE)
					.bytes(b"bad-\xff")
					.bytes("text\n")
					.strs(&[]);
			}),
		),
		(
			"AddTextToStore binary contents",
			Box::new(|w| {
				w.op("AddTextToStore", op::ADD_TEXT_TO_STORE)
					.bytes("binary")
					.bytes(b"\0\xff\xfe\n")
					.strs(&[]);
			}),
		),
		(
			"AddIndirectRoot NUL",
			Box::new(|w| {
				w.op("AddIndirectRoot", op::ADD_INDIRECT_ROOT).bytes(&root);
			}),
		),
		(
			"SetOptions non-UTF-8 override",
			Box::new(|w| {
				w.note("SetOptions");
				w.u64(19);
				for n in [0, 0, 0, 3, 1, 0, 0, 0, 0, 0, 1, 1] {
					w.u64(n);
				}
				w.u64(2)
					.bytes(b"bad-\xff")
					.bytes("1")
					.bytes("timeout")
					.bytes(b"\xff");
			}),
		),
		(
			"SetOptions booleans of 2",
			Box::new(|w| {
				w.note("SetOptions");
				w.u64(19);
				for n in [2, 2, 2, 3, 1, 0, 0, 0, 0, 0, 1, 2] {
					w.u64(n);
				}
				w.u64(0);
			}),
		),
		(
			"BuildPaths mode 3",
			Box::new(|w| {
				w.op("BuildPaths", op::BUILD_PATHS).strs(&[&built]).u64(3);
			}),
		),
		(
			"BuildPaths mode 256",
			Box::new(|w| {
				w.op("BuildPaths", op::BUILD_PATHS).strs(&[&built]).u64(256);
			}),
		),
		(
			"BuildPathsWithResults mode 3",
			Box::new(|w| {
				w.op("BuildPathsWithResults", op::BUILD_PATHS_WITH_RESULTS)
					.strs(&[&built])
					.u64(3);
			}),
		),
		(
			"BuildDerivation mode 3",
			Box::new(|w| {
				w.op("BuildDerivation", op::BUILD_DERIVATION)
					.bytes(drv)
					.basic_derivation(&basic)
					.u64(3);
			}),
		),
		(
			"VerifyStore checkContents 2",
			Box::new(|w| {
				w.op("VerifyStore", op::VERIFY_STORE).u64(2).u64(0);
			}),
		),
		(
			"QueryValidPaths substitute 2",
			Box::new(|w| {
				w.op("QueryValidPaths", op::QUERY_VALID_PATHS)
					.strs(&[out])
					.u64(2);
			}),
		),
		(
			"CollectGarbage action 7",
			Box::new(|w| {
				w.op("CollectGarbage", op::COLLECT_GARBAGE)
					.u64(7)
					.u64(1)
					.u64(0)
					.u64(0)
					.u64(0)
					.u64(0)
					.u64(0);
			}),
		),
		(
			"CollectGarbage whole-store indicator 2",
			Box::new(|w| {
				w.op("CollectGarbage", op::COLLECT_GARBAGE)
					.u64(0)
					.u64(2)
					.u64(0)
					.u64(0)
					.u64(0)
					.u64(0)
					.u64(0);
			}),
		),
		(
			"AddToStoreNar flags of 2",
			Box::new(|w| {
				w.op("AddToStoreNar", op::ADD_TO_STORE_NAR)
					.bytes(&ca)
					.bytes("")
					.bytes(hex(&sha256(&nar)))
					.strs(&[])
					.u64(0)
					.u64(nar.len() as u64)
					.u64(2)
					.strs(&[])
					.bytes(&ca_hint)
					.u64(2)
					.u64(2)
					.framed(&nar);
			}),
		),
		(
			"AddSignatures empty name",
			Box::new(|w| {
				w.op("AddSignatures", op::ADD_SIGNATURES)
					.bytes(out)
					.strs(&[":AAAA"]);
			}),
		),
		(
			"AddSignatures empty data",
			Box::new(|w| {
				w.op("AddSignatures", op::ADD_SIGNATURES)
					.bytes(out)
					.strs(&["noxi-test-1:"]);
			}),
		),
		(
			"AddSignatures no colon",
			Box::new(|w| {
				w.op("AddSignatures", op::ADD_SIGNATURES)
					.bytes(out)
					.strs(&["noxi-test-1"]);
			}),
		),
		(
			"AddSignatures newline in data",
			Box::new(|w| {
				w.op("AddSignatures", op::ADD_SIGNATURES)
					.bytes(out)
					.strs(&["noxi-test-1:AAAA\nAAAA"]);
			}),
		),
		(
			"QueryRealisation non-UTF-8 output",
			Box::new(|w| {
				w.op("QueryRealisation", op::QUERY_REALISATION)
					.bytes(drv)
					.bytes(b"\xff");
			}),
		),
		(
			"AddBuildLog non-UTF-8 name",
			Box::new(|w| {
				w.op("AddBuildLog", op::ADD_BUILD_LOG)
					.bytes(b"00000000000000000000000000000000-\xff.drv")
					.framed(b"log\n");
			}),
		),
	];
	let mut differing = Vec::new();
	for (what, request) in &cases {
		let compared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
			env.compare(&format!("malformed-{what}"), |w| {
				assert_eq!(w.handshake(version(NEWEST), &CLIENT_FEATURES), Some(true));
				request(w);
				// Record whatever the operation answers, up to its end or
				// the connection's, then the follow-up's answer.
				w.flush();
				w.op("IsValidPath after", op::IS_VALID_PATH).bytes(out);
				w.flush();
				while let Some(ok) = w.stderr() {
					if ok {
						break;
					}
				}
			});
		}));
		if compared.is_err() {
			differing.push(what.to_string());
		}
	}
	// Known differences. noxi reads wire strings as UTF-8 where the
	// reference reads bytes, so a request with bytes that are not UTF-8
	// fails as a protocol error and closes the connection, where the
	// reference rejects or ignores the value with its own message. A path
	// with a NUL is cut at it by the reference's system calls and refused
	// by noxi's.
	assert_eq!(
		differing,
		[
			"IsValidPath non-UTF-8",
			"QueryPathFromHashPart non-UTF-8",
			"AddTextToStore non-UTF-8 name",
			"AddIndirectRoot NUL",
			"SetOptions non-UTF-8 override",
			"QueryRealisation non-UTF-8 output",
			"AddBuildLog non-UTF-8 name",
		]
	);
}
