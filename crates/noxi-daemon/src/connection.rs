//! One client connection, following CppNix 2.35.2's `processConnection`,
//! `TunnelLogger` and `performOp` in `daemon.cc`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nix_compat::log::logger::{self, LogField};
use nix_compat::log::{ActivityType, ResultType, VerbosityLevel};
use nix_compat::nixbase32;
use nix_compat::nixhash::{HashAlgo, NixHash};
use nix_compat::store_path::StorePath;
use noxi_build::backend::BuildLogs;
use noxi_build::local::{BuildEvent, BuildMode, LocalBuilder};
use noxi_build::missing::{DerivedPath, query_missing};
use noxi_store::FileIngestionMethod;
use noxi_store::gc::{GcAction, GcOptions};
use noxi_store::{LocalStore, ValidPathInfo};

use crate::error::{Error, Result};
use crate::settings::DaemonSettings;
use crate::wire::{FramedReader, Reader, Version, Writer, version};

pub const WORKER_MAGIC_1: u64 = 0x6e697863;
pub const WORKER_MAGIC_2: u64 = 0x6478696f;
/// The newest protocol version, `WorkerProto::latest` in 2.35.2. The
/// header's `PROTOCOL_VERSION` macro says 1.39, but nothing reads it: the
/// handshake sends `latest`, 1.38.
pub const PROTOCOL_VERSION: Version = version(1, 38);
/// `WorkerProto::minimum`.
pub const MINIMUM_PROTOCOL_VERSION: Version = version(1, 18);
/// `WorkerProto::latest.features`.
pub const FEATURES: [&str; 2] = [
	"delete-dead-specific-referrers",
	"realisation-with-path-not-hash",
];

pub const STDERR_NEXT: u64 = 0x6f6c6d67;
pub const STDERR_READ: u64 = 0x64617461;
pub const STDERR_WRITE: u64 = 0x64617416;
pub const STDERR_LAST: u64 = 0x616c7473;
pub const STDERR_ERROR: u64 = 0x63787470;
pub const STDERR_START_ACTIVITY: u64 = 0x53545254;
pub const STDERR_STOP_ACTIVITY: u64 = 0x53544f50;
pub const STDERR_RESULT: u64 = 0x52534c54;

/// CppNix's `Verbosity`.
pub const LVL_ERROR: u64 = 0;
pub const LVL_WARN: u64 = 1;
pub const LVL_INFO: u64 = 3;
pub const LVL_TALKATIVE: u64 = 4;
pub const LVL_DEBUG: u64 = 6;

/// Worker operations, `WorkerProto::Op`.
pub mod op {
	pub const IS_VALID_PATH: u64 = 1;
	pub const ADD_TO_STORE: u64 = 7;
	pub const ADD_TEXT_TO_STORE: u64 = 8;
	pub const BUILD_PATHS: u64 = 9;
	pub const ADD_INDIRECT_ROOT: u64 = 12;
	pub const BUILD_PATHS_WITH_RESULTS: u64 = 46;
	pub const ADD_PERM_ROOT: u64 = 47;
	pub const QUERY_REFERRERS: u64 = 6;
	pub const ENSURE_PATH: u64 = 10;
	pub const ADD_TEMP_ROOT: u64 = 11;
	pub const SYNC_WITH_GC: u64 = 13;
	pub const FIND_ROOTS: u64 = 14;
	pub const QUERY_DERIVER: u64 = 18;
	pub const SET_OPTIONS: u64 = 19;
	pub const COLLECT_GARBAGE: u64 = 20;
	pub const QUERY_DERIVATION_OUTPUTS: u64 = 22;
	pub const QUERY_ALL_VALID_PATHS: u64 = 23;
	pub const QUERY_PATH_INFO: u64 = 26;
	pub const QUERY_DERIVATION_OUTPUT_NAMES: u64 = 28;
	pub const QUERY_PATH_FROM_HASH_PART: u64 = 29;
	pub const QUERY_VALID_PATHS: u64 = 31;
	pub const QUERY_SUBSTITUTABLE_PATH_INFO: u64 = 21;
	pub const QUERY_SUBSTITUTABLE_PATH_INFOS: u64 = 30;
	pub const QUERY_SUBSTITUTABLE_PATHS: u64 = 32;
	pub const QUERY_VALID_DERIVERS: u64 = 33;
	pub const OPTIMISE_STORE: u64 = 34;
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
}

/// The connection's output stream, shared by the replies written through
/// [`Connection::to`] and the [`Tunnel`] that sends log messages as they
/// happen, from whichever thread reports them.
pub struct Shared<W>(Arc<Mutex<W>>);

impl<W: Write> Write for Shared<W> {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		self.0.lock().unwrap_or_else(|e| e.into_inner()).write(buf)
	}

	fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
		self.0
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.write_all(buf)
	}

	fn flush(&mut self) -> io::Result<()> {
		self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
	}
}

struct TunnelState {
	can_send: bool,
	pending: Vec<Vec<u8>>,
}

/// `TunnelLogger`: the logger of a daemon worker. Messages are queued
/// until an operation starts work, then sent as they happen. It is
/// installed as the process's [`logger::LogSink`] for the connection, so
/// the activities and results the builder and the substituters report
/// (the worker's `realise`, `builds` and `copy-paths` progress, each
/// `build`, `substitute`, `copy-path` and `query-path-info`) reach the
/// client with their ids, parents and types.
pub struct Tunnel {
	state: Mutex<TunnelState>,
	out: Arc<Mutex<dyn Write + Send>>,
	version: Version,
}

fn write_fields<W: Write>(out: &mut Writer<W>, fields: &[LogField]) {
	out.u64(fields.len() as u64);
	for field in fields {
		match field {
			LogField::Int(n) => {
				out.u64(0).u64(*n);
			}
			LogField::Str(s) => {
				out.u64(1).string(s);
			}
		}
	}
}

impl Tunnel {
	/// `enqueueMsg`. A failed write means the client has gone; the
	/// message is dropped and nothing more is sent.
	fn enqueue(&self, message: Vec<u8>) -> io::Result<()> {
		let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
		if state.can_send {
			let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
			let result = out.write_all(&message).and_then(|()| out.flush());
			if result.is_err() {
				state.can_send = false;
			}
			result
		} else {
			state.pending.push(message);
			Ok(())
		}
	}

	fn message(build: impl FnOnce(&mut Writer<io::Sink>)) -> Vec<u8> {
		let mut message = Writer::new(io::sink());
		build(&mut message);
		message.take()
	}

	/// `log`: a line at `level`, if the verbosity admits it.
	fn log(&self, level: u64, line: &str) -> io::Result<()> {
		if level > nix_compat::log::verbosity() {
			return Ok(());
		}
		self.enqueue(Self::message(|m| {
			m.u64(STDERR_NEXT).string(&format!("{line}\n"));
		}))
	}
}

impl logger::LogSink for Tunnel {
	fn log(&self, level: VerbosityLevel, message: &str) {
		let _ = Tunnel::log(self, level as u64, message);
	}

	fn start_activity(
		&self,
		id: u64,
		level: VerbosityLevel,
		kind: ActivityType,
		text: &str,
		fields: &[LogField],
		parent: u64,
	) {
		if self.version < version(1, 20) {
			if !text.is_empty() {
				let _ = Tunnel::log(self, level as u64, &format!("{text}..."));
			}
			return;
		}
		let kind: u8 = kind.into();
		let _ = self.enqueue(Self::message(|m| {
			m.u64(STDERR_START_ACTIVITY)
				.u64(id)
				.u64(level as u64)
				.u64(kind.into())
				.string(text);
			write_fields(m, fields);
			m.u64(parent);
		}));
	}

	fn stop_activity(&self, id: u64) {
		if self.version < version(1, 20) {
			return;
		}
		let _ = self.enqueue(Self::message(|m| {
			m.u64(STDERR_STOP_ACTIVITY).u64(id);
		}));
	}

	fn result(&self, id: u64, kind: ResultType, fields: &[LogField]) {
		if self.version < version(1, 20) {
			return;
		}
		let kind: u8 = kind.into();
		let _ = self.enqueue(Self::message(|m| {
			m.u64(STDERR_RESULT).u64(id).u64(kind.into());
			write_fields(m, fields);
		}));
	}
}

/// The connection's side of its [`Tunnel`]: messages the operations send
/// themselves, and the start and end of each operation's work.
pub struct Logger {
	tunnel: Arc<Tunnel>,
}

/// Removes the connection's tunnel from the process's logger when the
/// connection ends.
struct SinkGuard;

impl Drop for SinkGuard {
	fn drop(&mut self) {
		logger::set_sink(None);
	}
}

impl Logger {
	fn new(version: Version, out: Arc<Mutex<dyn Write + Send>>) -> Self {
		Self {
			tunnel: Arc::new(Tunnel {
				state: Mutex::new(TunnelState {
					can_send: false,
					pending: Vec::new(),
				}),
				out,
				version,
			}),
		}
	}

	/// Whether the operation has started work, so the client reads
	/// messages before its reply.
	fn can_send(&self) -> bool {
		self.tunnel
			.state
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.can_send
	}

	/// Messages sent while the operation works go straight to the stream,
	/// so the reply buffered in `out` must not hold anything yet.
	fn flush<W: Write>(&self, out: &mut Writer<W>) -> Result<()> {
		if self.can_send() {
			out.flush()?;
		}
		Ok(())
	}

	/// `Logger::log`: a line at level `level`.
	pub fn log<W: Write>(&mut self, out: &mut Writer<W>, level: u64, line: &str) -> Result<()> {
		self.flush(out)?;
		Ok(self.tunnel.log(level, line)?)
	}

	/// `warn`.
	pub fn warn<W: Write>(&mut self, out: &mut Writer<W>, message: &str) -> Result<()> {
		self.log(
			out,
			LVL_WARN,
			&format!("\x1b[35;1mwarning:\x1b[0m {message}"),
		)
	}

	pub fn start_work<W: Write>(&mut self, out: &mut Writer<W>) -> Result<()> {
		out.flush()?;
		let mut state = self.tunnel.state.lock().unwrap_or_else(|e| e.into_inner());
		state.can_send = true;
		let mut stream = self.tunnel.out.lock().unwrap_or_else(|e| e.into_inner());
		for message in std::mem::take(&mut state.pending) {
			stream.write_all(&message)?;
		}
		stream.flush()?;
		Ok(())
	}

	/// `stopWork`: `STDERR_LAST`, or the error.
	pub fn stop_work<W: Write>(&mut self, out: &mut Writer<W>, error: Option<&str>) {
		self.tunnel
			.state
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.can_send = false;
		match error {
			None => {
				out.u64(STDERR_LAST);
			}
			Some(message) if self.tunnel.version >= version(1, 26) => {
				out.u64(STDERR_ERROR)
					.string("Error")
					.u64(LVL_ERROR)
					.string("Error")
					.string(message)
					.u64(0)
					.u64(0);
			}
			Some(message) => {
				let mut lines = message.split('\n');
				let mut rendered = format!("error: {}", lines.next().unwrap_or_default());
				for line in lines {
					rendered.push('\n');
					if !line.is_empty() {
						rendered.push_str("       ");
						rendered.push_str(line);
					}
				}
				out.u64(STDERR_ERROR).string(&rendered).u64(1);
			}
		}
	}
}

/// Whether the client is trusted, `TrustedFlag`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
	Trusted,
	NotTrusted,
}

pub struct Connection<'a, R: Read, W: Write> {
	pub from: Reader<R>,
	pub to: Writer<Shared<W>>,
	pub logger: Logger,
	pub version: Version,
	pub features: BTreeSet<String>,
	pub trust: Trust,
	pub store: &'a LocalStore,
	pub settings: DaemonSettings,
	/// The substituters, opened when an operation first needs them, as
	/// CppNix's `getDefaultSubstituters` opens them once per worker.
	caches: std::cell::OnceCell<Vec<noxi_store::binary_cache::BinaryCache>>,
	/// Set once the client hangs up, which cancels the running builds.
	cancelled: Arc<AtomicBool>,
}

/// `MonitorFdHup`: a thread that waits for the client's end of the
/// connection to hang up and then sets a flag that cancels the worker's
/// builds, as CppNix's `triggerInterrupt` interrupts them. Like the
/// reference it waits only for `POLLHUP`, so a client that merely shuts
/// down its writing side keeps its operation running. It never reads from
/// the connection.
pub struct HangupMonitor {
	notify: Option<OwnedFd>,
	thread: Option<std::thread::JoinHandle<()>>,
	cancelled: Arc<AtomicBool>,
}

impl HangupMonitor {
	pub fn new(fd: RawFd) -> io::Result<Self> {
		let mut pipe = [0; 2];
		// SAFETY: `pipe` has room for both descriptors.
		if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
			return Err(io::Error::last_os_error());
		}
		// SAFETY: pipe2 returned two new descriptors that nothing else owns.
		let (read, write) =
			unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
		let cancelled = Arc::new(AtomicBool::new(false));
		let flag = cancelled.clone();
		let thread = std::thread::Builder::new()
			.name("hangup-monitor".into())
			.spawn(move || {
				loop {
					// POSIX always reports POLLHUP; no other event is wanted.
					let mut fds = [
						libc::pollfd {
							fd,
							events: 0,
							revents: 0,
						},
						libc::pollfd {
							fd: read.as_raw_fd(),
							events: 0,
							revents: 0,
						},
					];
					// SAFETY: two valid pollfds.
					if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } == -1 {
						let error = io::Error::last_os_error();
						if matches!(error.raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) {
							continue;
						}
						return;
					}
					if fds[0].revents & libc::POLLHUP != 0 {
						flag.store(true, Ordering::Relaxed);
						return;
					}
					if fds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
						return;
					}
				}
			})?;
		Ok(Self {
			notify: Some(write),
			thread: Some(thread),
			cancelled,
		})
	}

	/// The flag set when the client hangs up.
	pub fn cancelled(&self) -> Arc<AtomicBool> {
		self.cancelled.clone()
	}
}

impl Drop for HangupMonitor {
	fn drop(&mut self) {
		// Closing the write side wakes the thread with POLLHUP.
		self.notify.take();
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
	}
}

/// `BasicServerConnection::handshake` and `postHandshake`, then the
/// operation loop.
pub fn process_connection<R: Read + Send, W: Write + Send + 'static>(
	read: R,
	write: W,
	store: &LocalStore,
	settings: &DaemonSettings,
	trust: Trust,
) -> Result<()> {
	serve_connection(
		read,
		write,
		store,
		settings,
		trust,
		Arc::new(AtomicBool::new(false)),
	)
}

/// [`process_connection`], cancelling the running builds when the client
/// end of `hangup` hangs up, as `processConnection`'s `MonitorFdHup` does.
pub fn process_monitored_connection<R: Read + Send, W: Write + Send + 'static>(
	read: R,
	write: W,
	hangup: RawFd,
	store: &LocalStore,
	settings: &DaemonSettings,
	trust: Trust,
) -> Result<()> {
	let monitor = HangupMonitor::new(hangup)?;
	let result = serve_connection(read, write, store, settings, trust, monitor.cancelled());
	drop(monitor);
	result
}

fn serve_connection<R: Read + Send, W: Write + Send + 'static>(
	read: R,
	write: W,
	store: &LocalStore,
	settings: &DaemonSettings,
	trust: Trust,
	cancelled: Arc<AtomicBool>,
) -> Result<()> {
	let mut from = Reader::new(read);
	let stream = Arc::new(Mutex::new(write));
	let mut to = Writer::new(Shared(stream.clone()));
	if from.read_u64()? != WORKER_MAGIC_1 {
		return Err(Error::Protocol("protocol mismatch".into()));
	}
	to.u64(WORKER_MAGIC_2).u64(PROTOCOL_VERSION);
	to.flush()?;
	let client_version = crate::wire::from_wire(from.read_u64()?);
	let protocol = client_version.min(PROTOCOL_VERSION);
	let mut features = BTreeSet::new();
	if protocol >= version(1, 38) {
		let client_features: BTreeSet<String> = from.read_strings()?.into_iter().collect();
		to.strings(FEATURES.iter().copied());
		to.flush()?;
		features = FEATURES
			.iter()
			.filter(|f| client_features.contains(**f))
			.map(|f| f.to_string())
			.collect();
	}
	if protocol < MINIMUM_PROTOCOL_VERSION {
		return Err(Error::Nix("the Nix client version is too old".into()));
	}

	let mut conn = Connection {
		from,
		to,
		logger: Logger::new(protocol, stream),
		version: protocol,
		features,
		trust,
		store,
		settings: settings.clone(),
		caches: std::cell::OnceCell::new(),
		cancelled,
	};
	// This process serves one connection, so its builds, substitutions and
	// messages go to this client.
	logger::set_sink(Some(conn.logger.tunnel.clone()));
	let _sink = SinkGuard;

	// postHandshake.
	if protocol >= version(1, 14) && conn.from.read_u64()? != 0 {
		conn.from.read_u64()?; // obsolete CPU affinity
	}
	if protocol >= version(1, 11) {
		conn.from.read_u64()?; // obsolete reserveSpace
	}
	if protocol >= version(1, 33) {
		conn.to.string(&settings.daemon_version);
	}
	if protocol >= version(1, 35) {
		conn.to.u64(match trust {
			Trust::Trusted => 1,
			Trust::NotTrusted => 2,
		});
	}
	conn.logger.start_work(&mut conn.to)?;
	conn.logger.stop_work(&mut conn.to, None);
	conn.to.flush()?;

	loop {
		let op = match conn.from.read_u64() {
			Ok(op) => op,
			Err(Error::EndOfFile) => return Ok(()),
			Err(e) => return Err(e),
		};
		if op > u32::MAX as u64 {
			conn.logger.stop_work(
				&mut conn.to,
				Some(&format!(
					"serialised integer {op} is too large for type 'j'"
				)),
			);
			conn.to.flush()?;
			return Ok(());
		}
		match conn.perform_op(op) {
			Ok(()) => {}
			// The client hung up and the operation was interrupted; there
			// is nobody left to tell.
			Err(_) if conn.cancelled.load(Ordering::Relaxed) => return Ok(()),
			Err(e @ Error::Io(_)) => return Err(e),
			Err(e) => {
				// If the error happened before the operation could send
				// replies, the input is probably not fully read; report the
				// error and give up on the connection, as CppNix does. A
				// request cut short is reported like any other error, as
				// `unexpected end-of-file`; only a client that hangs up
				// between requests ends the connection quietly.
				let error_allowed = conn.logger.can_send();
				conn.logger.stop_work(&mut conn.to, Some(&message_of(&e)));
				conn.to.flush()?;
				if !error_allowed {
					conn.logger.stop_work(&mut conn.to, Some(&message_of(&e)));
					conn.to.flush()?;
					// `processConnection` returns normally after reporting it.
					return Ok(());
				}
			}
		}
		conn.to.flush()?;
	}
}

/// The message CppNix's `Error::msg()` would give for `e`.
pub fn message_of(e: &Error) -> String {
	match e {
		Error::Build(e) => e.to_string(),
		Error::Store(e) => e.to_string(),
		e => e.to_string(),
	}
}

fn nar_hash_base16(info: &ValidPathInfo) -> String {
	info.nar_hash.iter().map(|b| format!("{b:02x}")).collect()
}

impl<R: Read + Send, W: Write + Send + 'static> Connection<'_, R, W> {
	fn start(&mut self) -> Result<()> {
		self.logger.start_work(&mut self.to)
	}

	fn stop(&mut self) {
		self.logger.stop_work(&mut self.to, None);
	}

	/// `WorkerProto::Serialise<UnkeyedValidPathInfo>::write`.
	fn write_path_info(&mut self, info: &ValidPathInfo) {
		let hash = nar_hash_base16(info);
		self.to
			.opt_store_path(info.deriver.as_ref())
			.string(&hash)
			.store_paths(info.references.iter())
			.u64(info.registration_time as u64)
			.u64(info.nar_size);
		if self.version >= version(1, 16) {
			self.to.bool(info.ultimate);
			self.to.strings(info.sigs.iter().map(String::as_str));
			let ca = info
				.ca
				.as_ref()
				.map(|ca| ca.to_string())
				.unwrap_or_default();
			self.to.string(&ca);
		}
	}

	fn perform_op(&mut self, op: u64) -> Result<()> {
		match op {
			op::REGISTER_DRV_OUTPUT | op::QUERY_REALISATION => {
				self.start()?;
				if !self.features.contains("realisation-with-path-not-hash") {
					return Err(Error::Nix("the daemon is missing the 'realisation-with-path-not-hash' protocol feature, needed to support content-addressing derivations".into()));
				}
				let drv = self.from.read_store_path()?;
				let output = self.from.read_string()?;
				if op == op::REGISTER_DRV_OUTPUT {
					let path = self.from.read_store_path()?;
					let sigs = self.from.read_signatures()?.into_iter().collect();
					self.store.register_drv_output(
						&drv,
						&output,
						&noxi_store::Realisation {
							out_path: path,
							signatures: sigs,
						},
					)?;
					self.stop();
				} else {
					let info = self.store.query_realisation(&drv, &output)?;
					self.stop();
					self.to.bool(info.is_some());
					if let Some(noxi_store::Realisation {
						out_path: path,
						signatures: sigs,
					}) = info
					{
						self.to
							.store_path(&path)
							.strings(sigs.iter().map(String::as_str));
					}
				}
			}
			op::ADD_BUILD_LOG => {
				let name = self.from.read_string()?;
				let path = nix_compat::nixcpp::narinfo::parse_store_path_base(&name)
					.map_err(Error::Nix)?;
				self.start()?;
				if self.trust != Trust::Trusted {
					return Err(Error::Nix("you are not privileged to add logs".into()));
				}
				let mut source = FramedReader::new(&mut self.from);
				let result = self.store.add_build_log(&path, &mut source);
				drain_frames(&mut source, &mut self.logger, &mut self.to)?;
				result?;
				self.stop();
				self.to.u64(1);
			}
			op::OPTIMISE_STORE => {
				self.start()?;
				let stats = self.store.optimise()?;
				self.logger.log(
					&mut self.to,
					LVL_INFO,
					&format!(
						"{} freed by hard-linking {} files",
						noxi_store::gc::render_size(stats.bytes_freed as i64, false),
						stats.files_linked
					),
				)?;
				self.stop();
				self.to.u64(1);
			}
			op::QUERY_SUBSTITUTABLE_PATH_INFO
			| op::QUERY_SUBSTITUTABLE_PATH_INFOS
			| op::QUERY_SUBSTITUTABLE_PATHS => {
				let single = op == op::QUERY_SUBSTITUTABLE_PATH_INFO;
				let paths = if single {
					BTreeSet::from([self.from.read_store_path()?])
				} else if op == op::QUERY_SUBSTITUTABLE_PATH_INFOS && self.version >= version(1, 22)
				{
					let count = self.from.read_u64()?;
					let mut paths = BTreeSet::new();
					for _ in 0..count {
						paths.insert(self.from.read_store_path()?);
						let ca = self.from.read_string()?;
						if !ca.is_empty() {
							crate::wire::parse_content_address(&ca)?;
						}
					}
					paths
				} else {
					self.from.read_store_paths()?
				};
				self.start()?;
				let mut infos = BTreeMap::new();
				if self.settings.build.use_substitutes {
					for path in paths {
						for cache in self
							.caches
							.get_or_init(|| self.settings.open_substituters())
						{
							if op == op::QUERY_SUBSTITUTABLE_PATHS && !cache.info().want_mass_query
							{
								continue;
							}
							// The cache reports its `query-path-info` activity
							// the first time it is asked about a path.
							if let Some(info) = cache.query_path_info(&path)? {
								infos.insert(path, info);
								break;
							}
						}
					}
				}
				self.stop();
				if op == op::QUERY_SUBSTITUTABLE_PATHS {
					self.to.store_paths(infos.keys());
				} else {
					self.to.u64(infos.len() as u64);
					for (path, cached) in infos {
						if !single {
							self.to.store_path(&path);
						}
						self.to
							.opt_store_path(cached.info.deriver.as_ref())
							.store_paths(cached.info.references.iter())
							.u64(cached.file_size.unwrap_or(0))
							.u64(cached.info.nar_size);
					}
				}
			}
			op::IS_VALID_PATH => {
				let path = self.from.read_store_path()?;
				self.start()?;
				let valid = self.store.is_valid_path(&path)?;
				self.stop();
				self.to.bool(valid);
			}
			op::QUERY_VALID_PATHS => {
				let paths = self.from.read_store_paths()?;
				// `readInt`, so any value up to `unsigned int` asks for it.
				let substitute = self.version >= version(1, 27) && self.from.read_int()? != 0;
				self.start()?;
				if substitute {
					// `substitutePaths`: `queryMissing` for the paths that are
					// not derivations, with its activity, which opens the
					// substituters if it needs them, then the paths it would
					// substitute are realised in a worker. Failures are only
					// warnings there and are ignored here.
					let targets: Vec<DerivedPath> = paths
						.iter()
						.filter(|path| !path.name().ends_with(".drv"))
						.map(|path| DerivedPath::Opaque(path.clone()))
						.collect();
					let act = logger::Activity::new(
						VerbosityLevel::Debug,
						ActivityType::Unknown,
						"querying info about missing paths",
						vec![],
						0,
					);
					let missing = needs_substituters(self.store, &targets).and_then(|needed| {
						let caches: &[noxi_store::binary_cache::BinaryCache] = if needed {
							self.caches
								.get_or_init(|| self.settings.open_substituters())
						} else {
							&[]
						};
						query_missing(
							self.store,
							caches,
							self.settings.build.use_substitutes,
							self.settings.build.always_allow_substitutes,
							&targets,
						)
						.map_err(Error::from)
					});
					drop(act);
					let missing = missing?;
					if !missing.will_substitute.is_empty() {
						let caches = self
							.caches
							.get_or_init(|| self.settings.open_substituters());
						let _worker = logger::WorkerGuard::new();
						for path in &missing.will_substitute {
							let _ = self.store.substitute(caches, path);
						}
					}
				}
				let valid = self.store.query_valid_paths(&paths)?;
				self.stop();
				self.to.store_paths(valid.iter());
			}
			op::QUERY_REFERRERS | op::QUERY_VALID_DERIVERS | op::QUERY_DERIVATION_OUTPUTS => {
				let path = self.from.read_store_path()?;
				self.start()?;
				let paths: BTreeSet<StorePath> = match op {
					op::QUERY_REFERRERS => self.store.query_referrers(&path)?,
					op::QUERY_VALID_DERIVERS => self.store.query_valid_derivers(&path)?,
					_ => self
						.store
						.query_derivation_outputs(&path)?
						.into_values()
						.collect(),
				};
				self.stop();
				self.to.store_paths(paths.iter());
			}
			op::QUERY_DERIVATION_OUTPUT_NAMES => {
				let path = self.from.read_store_path()?;
				self.start()?;
				let drv = self.store.read_derivation(&path)?;
				let names: Vec<String> = drv.outputs().names().map(|n| n.to_string()).collect();
				self.stop();
				self.to.strings(names.iter().map(String::as_str));
			}
			op::QUERY_DERIVATION_OUTPUT_MAP => {
				let path = self.from.read_store_path()?;
				self.start()?;
				let outputs = self.store.query_partial_derivation_output_map(&path)?;
				self.stop();
				self.to.u64(outputs.len() as u64);
				for (name, path) in &outputs {
					self.to.string(name).opt_store_path(path.as_ref());
				}
			}
			op::QUERY_DERIVER => {
				let path = self.from.read_store_path()?;
				self.start()?;
				let info = self.query_path_info(&path)?;
				self.stop();
				self.to.opt_store_path(info.deriver.as_ref());
			}
			op::QUERY_PATH_FROM_HASH_PART => {
				let hash = self.from.read_string()?;
				self.start()?;
				let path = self.store.query_path_from_hash_part(&hash)?;
				self.stop();
				self.to.opt_store_path(path.as_ref());
			}
			op::ENSURE_PATH => {
				let path = self.from.read_store_path()?;
				self.start()?;
				if !self.store.is_valid_path(&path)? {
					if !self.settings.build.use_substitutes {
						return Err(Error::Nix(format!(
							"path '{}' is not valid and substitution is disabled",
							path.to_absolute_path()
						)));
					}
					// `ensurePath` runs a worker with one substitution goal.
					let _worker = logger::WorkerGuard::new();
					self.store.substitute(
						self.caches
							.get_or_init(|| self.settings.open_substituters()),
						&path,
					)?;
				}
				self.stop();
				self.to.u64(1);
			}
			op::ADD_TEMP_ROOT => {
				let path = self.from.read_store_path()?;
				self.start()?;
				self.store.add_temp_root(&path)?;
				self.stop();
				self.to.u64(1);
			}
			op::SYNC_WITH_GC => {
				self.start()?;
				self.stop();
				self.to.u64(1);
			}
			op::FIND_ROOTS => {
				self.start()?;
				let censor = self.trust == Trust::NotTrusted;
				let roots = self.with_info_log(|store, log| store.find_roots(censor, log))?;
				self.stop();
				let count: usize = roots.values().map(BTreeSet::len).sum();
				self.to.u64(count as u64);
				for (target, links) in &roots {
					for link in links {
						self.to.string(link).store_path(target);
					}
				}
			}
			op::SET_OPTIONS => self.set_options()?,
			op::QUERY_ALL_VALID_PATHS => {
				self.start()?;
				let paths = self.store.query_all_valid_paths()?;
				self.stop();
				self.to.store_paths(paths.iter());
			}
			op::QUERY_PATH_INFO => {
				let path = self.from.read_store_path()?;
				self.start()?;
				let info = self.store.query_path_info(&path)?;
				self.stop();
				match info {
					Some(info) => {
						self.to.u64(1);
						self.write_path_info(&info);
					}
					None => {
						self.to.u64(0);
					}
				}
			}
			op::NAR_FROM_PATH => {
				let path = self.from.read_store_path()?;
				self.start()?;
				self.stop();
				self.to.flush()?;
				self.store.nar_from_path(&path, self.to.inner_mut())?;
			}
			op::QUERY_MISSING => {
				let n = self.from.read_u64()?;
				let mut targets = Vec::new();
				for _ in 0..n {
					let s = self.from.read_string()?;
					targets.push(
						DerivedPath::parse_legacy(&s, |p| {
							crate::wire::parse_store_path(p)
								.map_err(|e| noxi_build::BuildError::Other(e.to_string()))
						})
						.map_err(|e| Error::Nix(e.to_string()))?,
					);
				}
				self.start()?;
				let act = logger::Activity::new(
					VerbosityLevel::Debug,
					ActivityType::Unknown,
					"querying info about missing paths",
					vec![],
					0,
				);
				let missing = needs_substituters(self.store, &targets).and_then(|needed| {
					let caches: &[noxi_store::binary_cache::BinaryCache] = if needed {
						self.caches
							.get_or_init(|| self.settings.open_substituters())
					} else {
						&[]
					};
					query_missing(
						self.store,
						caches,
						self.settings.build.use_substitutes,
						self.settings.build.always_allow_substitutes,
						&targets,
					)
					.map_err(Error::from)
				});
				drop(act);
				let missing = missing?;
				self.stop();
				self.to
					.store_paths(missing.will_build.iter())
					.store_paths(missing.will_substitute.iter())
					.store_paths(missing.unknown.iter())
					.u64(missing.download_size)
					.u64(missing.nar_size);
			}
			op::ADD_TO_STORE => self.add_to_store()?,
			op::ADD_TEXT_TO_STORE => {
				let suffix = self.from.read_string()?;
				let contents = self.from.read_bytes()?;
				let refs = self.from.read_store_paths()?;
				self.start()?;
				let path = self
					.store
					.add_text_to_store(&suffix, &contents, &refs, false)?;
				self.stop();
				self.to.store_path(&path);
			}
			op::ADD_INDIRECT_ROOT => {
				let path = absolute(&self.from.read_string()?);
				self.start()?;
				self.store.add_indirect_root(&path)?;
				self.stop();
				self.to.u64(1);
			}
			op::ADD_PERM_ROOT => {
				if self.trust != Trust::Trusted {
					return Err(Error::Nix(
						"you are not privileged to create perm roots\n\nhint: you can just do this client-side without special privileges, and probably want to do that instead.".into(),
					));
				}
				let path = self.from.read_store_path()?;
				let root = absolute(&self.from.read_string()?);
				self.start()?;
				let root = self.store.add_perm_root(&path, &root)?;
				self.stop();
				self.to.string(&root.to_string_lossy());
			}
			op::BUILD_DERIVATION => self.build_derivation()?,
			op::BUILD_PATHS | op::BUILD_PATHS_WITH_RESULTS => self.build_paths(op)?,
			op::COLLECT_GARBAGE => self.collect_garbage()?,
			op::ADD_TO_STORE_NAR => self.add_to_store_nar()?,
			op::ADD_MULTIPLE_TO_STORE => self.add_multiple_to_store()?,
			op::VERIFY_STORE => {
				let check_contents = self.from.read_bool()?;
				let repair = self.from.read_bool()?;
				self.start()?;
				if repair && self.trust != Trust::Trusted {
					return Err(Error::Nix("you are not privileged to repair paths".into()));
				}
				let errors = if repair {
					let settings = self.settings.clone();
					let cancelled = self.cancelled.clone();
					let caches = settings.open_substituters();
					self.with_log(|store, log| {
						noxi_build::backend::Worker {
							store,
							settings: &settings.build,
							substituters: if settings.build.use_substitutes {
								&caches
							} else {
								&[]
							},
							cancelled: Some(&cancelled),
						}
						.verify_store(check_contents, true, log)
					})?
				} else {
					self.with_log(|store, log| store.verify_store(check_contents, None, log))?
				};
				self.stop();
				self.to.bool(errors);
			}
			op::ADD_SIGNATURES => {
				let path = self.from.read_store_path()?;
				let sigs: BTreeSet<String> = self.from.read_signatures()?.into_iter().collect();
				self.start()?;
				self.store.add_signatures(&path, &sigs)?;
				self.stop();
				self.to.u64(1);
			}
			op => return Err(Error::Nix(format!("invalid operation {op}"))),
		}
		Ok(())
	}

	/// Run `f` on the store, sending the lines it logs to the client at
	/// `lvlInfo` as they happen.
	fn with_info_log<T>(
		&mut self,
		f: impl FnOnce(&LocalStore, &mut dyn FnMut(&str)) -> noxi_store::Result<T>,
	) -> Result<T> {
		self.with_log(|store, log| f(store, &mut |line| log(VerbosityLevel::Info, line)))
	}

	/// Run `f` on the store, sending the lines it logs to the client at
	/// their levels as they happen.
	fn with_log<T>(
		&mut self,
		f: impl FnOnce(&LocalStore, &mut dyn FnMut(VerbosityLevel, &str)) -> noxi_store::Result<T>,
	) -> Result<T> {
		let store = self.store;
		let (logger, to) = (&mut self.logger, &mut self.to);
		let mut send_error = None;
		let result = f(store, &mut |level, line| {
			if send_error.is_none()
				&& let Err(e) = logger.log(to, level as u64, line)
			{
				send_error = Some(e);
			}
		});
		match send_error {
			Some(e) => Err(e),
			None => Ok(result?),
		}
	}

	/// `WorkerProto::Op::CollectGarbage`.
	fn collect_garbage(&mut self) -> Result<()> {
		let action = match self.from.read_u64()? {
			0 => GcAction::ReturnLive,
			1 => GcAction::ReturnDead,
			2 => GcAction::DeleteDead,
			3 => GcAction::DeleteSpecific,
			_ => return Err(Error::Nix("Invalid GC action".into())),
		};
		let mut options = GcOptions::new(action);
		if self.features.contains("delete-dead-specific-referrers") {
			match self.from.read_u64()? {
				1 => {}
				0 => {
					options.paths = Some(self.from.read_store_paths()?);
					options.delete_referrers = self.from.read_bool()?;
				}
				_ => {
					return Err(Error::Nix(
						"Invalid whole store indicator from remote".into(),
					));
				}
			}
		} else {
			let paths = self.from.read_store_paths()?;
			if !paths.is_empty() || action == GcAction::DeleteSpecific {
				if action == GcAction::DeleteDead {
					return Err(Error::Nix("Garbage collecting specific paths requested but it is not supported by the negotiated protocol".into()));
				}
				options.paths = Some(paths);
			}
		}
		options.ignore_liveness = self.from.read_bool()?;
		options.max_freed = self.from.read_u64()?;
		for _ in 0..3 {
			self.from.read_u64()?; // obsolete
		}
		options.keep_outputs = self.settings.gc_keep_outputs;
		options.keep_derivations = self.settings.gc_keep_derivations;
		self.start()?;
		if options.ignore_liveness {
			return Err(Error::Nix("you are not allowed to ignore liveness".into()));
		}
		let results = self.with_info_log(|store, log| store.collect_garbage(&options, log))?;
		self.stop();
		self.to
			.strings(results.paths.iter().map(String::as_str))
			.u64(results.bytes_freed)
			.u64(0); // obsolete
		Ok(())
	}

	fn read_derived_paths(&mut self) -> Result<Vec<(String, DerivedPath)>> {
		let n = self.from.read_u64()?;
		let mut targets = Vec::new();
		for _ in 0..n {
			let s = self.from.read_string()?;
			let mut path = DerivedPath::parse_legacy(&s, |p| {
				crate::wire::parse_store_path(p)
					.map_err(|e| noxi_build::BuildError::Other(e.to_string()))
			})
			.map_err(|e| Error::Nix(e.to_string()))?;
			if self.version < version(1, 30) {
				if let DerivedPath::Opaque(drv) = &path {
					if drv.name().ends_with(".drv") {
						path = DerivedPath::Built {
							drv_path: drv.clone(),
							outputs: None,
						};
					}
				}
			}
			targets.push((s, path));
		}
		Ok(targets)
	}

	/// `WorkerProto::Op::AddToStore` for protocol 1.25 and later.
	fn add_to_store(&mut self) -> Result<()> {
		if self.version < version(1, 25) {
			let name = self.from.read_string()?;
			let fixed = self.from.read_bool()?;
			let recursive = self.from.read_u64()?;
			let mut algo = self.from.read_string()?;
			if recursive > 1 {
				return Err(Error::Nix(format!(
					"unsupported FileIngestionMethod with value of {recursive}; you may need to upgrade nix-daemon"
				)));
			}
			let method = if recursive != 0 || !fixed {
				FileIngestionMethod::NixArchive
			} else {
				FileIngestionMethod::Flat
			};
			if !fixed {
				algo = "sha256".into();
			}
			let algo = algo
				.parse::<HashAlgo>()
				.map_err(|_| Error::Nix(format!("unknown hash algorithm '{algo}'")))?;
			let nar = read_nar(self.from.get_mut())?;
			self.start()?;
			let path = self.store.add_to_store_from_dump(
				&mut &nar[..],
				&name,
				FileIngestionMethod::NixArchive,
				method,
				algo,
				&BTreeSet::new(),
				false,
			)?;
			self.stop();
			self.to.store_path(&path);
			return Ok(());
		}
		let name = self.from.read_string()?;
		let method = self.from.read_string()?;
		let refs = self.from.read_store_paths()?;
		let repair = self.from.read_bool()?;
		self.start()?;
		let (dump, hash, algo) = parse_method_algo(&method);
		let result = {
			let mut source = FramedReader::new(&mut self.from);
			let result = match (dump, hash, algo) {
				(Some(dump), Some(hash), Some(algo)) => self
					.store
					.add_to_store_from_dump(&mut source, &name, dump, hash, algo, &refs, repair)
					.map_err(Error::from),
				_ => Err(Error::Nix(format!(
					"content address method '{method}' is not supported"
				))),
			};
			drain_frames(&mut source, &mut self.logger, &mut self.to)?;
			// The reference reads a dump that ends early as a complete one and
			// fails parsing it from memory, so the error names the in-memory
			// string rather than the connection.
			match result {
				Err(_) if source.truncated() => Err(Error::Nix("end of string reached".into())),
				result => result,
			}
		};
		let path = result?;
		let info = self.query_path_info(&path)?;
		self.stop();
		self.to.store_path(&path);
		self.write_path_info(&info);
		Ok(())
	}

	/// `WorkerProto::Op::AddToStoreNar` for protocol 1.23 and later.
	fn add_to_store_nar(&mut self) -> Result<()> {
		let path = self.from.read_store_path()?;
		let mut info = read_path_info(&mut self.from, path)?;
		let repair = self.from.read_bool()?;
		let dont_check_sigs = self.from.read_bool()? && self.trust == Trust::Trusted;
		if self.trust != Trust::Trusted {
			info.ultimate = false;
		}
		if self.version < version(1, 23) {
			if self.version >= version(1, 21) {
				self.start()?;
				let mut tunnel = TunnelReader {
					from: &mut self.from,
					to: &mut self.to,
					pending: io::Cursor::new(Vec::new()),
				};
				let mut source = (&mut tunnel).take(info.nar_size);
				let result = self.store.add_to_store(
					&info,
					&mut BufReader::with_capacity(65536, &mut source),
					repair,
					!dont_check_sigs,
				);
				io::copy(&mut source, &mut io::sink())?;
				result?;
			} else {
				let nar = read_nar(self.from.get_mut())?;
				self.start()?;
				self.store
					.add_to_store(&info, &mut &nar[..], repair, !dont_check_sigs)?;
			}
			self.stop();
			return Ok(());
		}
		self.start()?;
		let store = self.store;
		let mut framed = FramedReader::new(&mut self.from);
		let result = store.add_to_store(
			&info,
			&mut BufReader::new(&mut framed),
			repair,
			!dont_check_sigs,
		);
		drain_frames(&mut framed, &mut self.logger, &mut self.to)?;
		result?;
		self.stop();
		Ok(())
	}

	/// `WorkerProto::Op::AddMultipleToStore`: a framed stream of path
	/// infos, each followed by its NAR.
	fn add_multiple_to_store(&mut self) -> Result<()> {
		let repair = self.from.read_bool()?;
		let dont_check_sigs = self.from.read_bool()? && self.trust == Trust::Trusted;
		self.start()?;
		let store = self.store;
		let mut framed = FramedReader::new(&mut self.from);
		let result = (|| -> Result<()> {
			let mut source = Reader::new(&mut framed);
			let expected = source.read_u64()?;
			for _ in 0..expected {
				let path = source.read_store_path()?;
				let mut info = read_path_info(&mut source, path)?;
				info.ultimate = false;
				let mut nar = source.get_mut().take(info.nar_size);
				store.add_to_store(
					&info,
					&mut BufReader::new(&mut nar),
					repair,
					!dont_check_sigs,
				)?;
				io::copy(&mut nar, &mut io::sink())?;
			}
			Ok(())
		})();
		drain_frames(&mut framed, &mut self.logger, &mut self.to)?;
		result?;
		self.stop();
		Ok(())
	}

	/// Build the client's basic derivation without trusting its input-addressed paths.
	fn build_derivation(&mut self) -> Result<()> {
		let mut path = self.from.read_store_path()?;
		if !path.name().ends_with(".drv") {
			return Err(Error::Nix(format!(
				"path '{}' is not a derivation",
				path.to_absolute_path()
			)));
		}
		let drv = read_basic_derivation(&mut self.from, &path)?;
		let mode = self.from.read_build_mode()?;
		self.start()?;
		if !drv.outputs().is_fixed() && self.trust != Trust::Trusted {
			return Err(Error::Nix(
				"you are not privileged to build input-addressed derivations".into(),
			));
		}
		let store = self.store;
		if self.trust != Trust::Trusted {
			path = store.add_text_to_store(
				path.name(),
				&drv.to_aterm_bytes(),
				drv.input_sources(),
				false,
			)?;
		}
		let cancelled = self.cancelled.clone();
		let builder = LocalBuilder::new(store, &self.settings.build)
			.with_cancellation(&cancelled)
			.with_mode(build_mode(mode));
		let start = now();
		// The worker runs one goal for the derivation; its progress and the
		// build's activity go to the client through the tunnel.
		let worker = logger::WorkerGuard::new();
		// Creating the goal reports progress, then counts its expected build,
		// which is released at once if the outputs are valid.
		logger::worker_update(|_| {});
		logger::worker_update(|c| c.expected_builds += 1);
		let mut built = false;
		let mut logs = BuildLogs::default();
		let mut events = |event: BuildEvent<'_>| build_event(&mut logs, &mut built, event);
		let result = builder.build_derivation(&path, &drv, &mut events);
		if result.is_ok() && !built {
			logger::worker_update(|c| c.expected_builds = c.expected_builds.saturating_sub(1));
		}
		drop(logs);
		drop(worker);
		let result = result
			.map(|paths| {
				if built {
					BuildOutcome::Built(paths)
				} else {
					BuildOutcome::AlreadyValid(paths)
				}
			})
			.map_err(|e| (e.to_string(), failure_code(&e)));
		self.stop();
		self.write_build_result(result, start, now(), Some(0));
		Ok(())
	}

	/// `BuildPaths` and `BuildPathsWithResults`.
	fn build_paths(&mut self, op: u64) -> Result<()> {
		let targets = self.read_derived_paths()?;
		let mode = self.from.read_build_mode()?;
		if mode == 1 && self.trust != Trust::Trusted {
			return Err(Error::Nix(
				"repairing is not allowed because you are not in 'trusted-users'".into(),
			));
		}
		self.start()?;
		let store = self.store;
		let settings = &self.settings;
		let cancelled = self.cancelled.clone();
		// Nothing missing, nothing to substitute: CppNix's goals finish before
		// they open the substituters.
		let targets_only: Vec<DerivedPath> = targets.iter().map(|(_, t)| t.clone()).collect();
		let caches: &[noxi_store::binary_cache::BinaryCache] =
			if needs_substituters(store, &targets_only)? {
				self.caches.get_or_init(|| settings.open_substituters())
			} else {
				&[]
			};
		if op == op::BUILD_PATHS && mode == 0 {
			// `LocalStore::buildPaths`: the local commands' worker, which
			// orders the builds and reports failed inputs and dependency
			// failures as CppNix's does.
			let paths: Vec<noxi_store::DerivedPath> = targets
				.iter()
				.map(|(_, target)| match target {
					DerivedPath::Opaque(path) => noxi_store::DerivedPath::Opaque(path.clone()),
					DerivedPath::Built { drv_path, outputs } => noxi_store::DerivedPath::Built {
						drv: drv_path.clone(),
						outputs: outputs.clone().unwrap_or_default(),
					},
				})
				.collect();
			let worker = logger::WorkerGuard::new();
			let result = noxi_build::backend::Worker {
				store,
				settings: &settings.build,
				substituters: if settings.build.use_substitutes {
					caches
				} else {
					&[]
				},
				cancelled: Some(&cancelled),
			}
			.build_paths_in_worker(&paths);
			drop(worker);
			result.map_err(|e| Error::Nix(e.to_string()))?;
			self.stop();
			self.to.u64(1);
			return Ok(());
		}
		let builder = LocalBuilder::new(store, &settings.build)
			.with_substituters(caches)
			.with_cancellation(&cancelled)
			.with_mode(build_mode(mode));
		// CppNix's `Worker` for the requested goals: its `realise`, `builds`
		// and `copy-paths` activities run until the results are known, and
		// creating the goals reports the builds expected.
		let worker = logger::WorkerGuard::new();
		let derivations: Vec<StorePath> = targets
			.iter()
			.filter_map(|(_, target)| match target {
				DerivedPath::Built { drv_path, .. } => Some(drv_path.clone()),
				DerivedPath::Opaque(_) => None,
			})
			.fold(Vec::new(), |mut drvs, drv| {
				if !drvs.contains(&drv) {
					drvs.push(drv);
				}
				drvs
			});
		if !derivations.is_empty() {
			let paths: Vec<DerivedPath> = targets.iter().map(|(_, t)| t.clone()).collect();
			let will_build = query_missing(
				store,
				caches,
				settings.build.use_substitutes,
				settings.build.always_allow_substitutes,
				&paths,
			)?
			.will_build;
			noxi_build::backend::announce_goals(store, &derivations, &will_build)?;
		}
		let mut logs = BuildLogs::default();
		let mut results = Vec::new();
		let mut first_error = None;
		for (text, target) in &targets {
			let started = now();
			let result = match target {
				DerivedPath::Opaque(path) => {
					if store.is_valid_path(path)? {
						Ok(BuildOutcome::AlreadyValid(BTreeMap::new()))
					} else if !settings.build.use_substitutes {
						Err((
							format!(
								"path '{}' is not valid and substitution is disabled",
								path.to_absolute_path()
							),
							14,
						))
					} else {
						store
							.substitute(caches, path)
							.map(|()| BuildOutcome::Substituted(BTreeMap::new()))
							.map_err(|e| (e.to_string(), 14))
					}
				}
				DerivedPath::Built { drv_path, outputs } => {
					let mut built = false;
					let mut events =
						|event: BuildEvent<'_>| build_event(&mut logs, &mut built, event);
					let result =
						builder.realise_with_events(drv_path, outputs.as_ref(), &mut events);
					match result {
						Ok(paths) => {
							let wanted: BTreeMap<String, StorePath> = paths
								.into_iter()
								.filter(|(name, _)| {
									outputs.as_ref().is_none_or(|o| o.contains(name))
								})
								.collect();
							Ok(if built {
								BuildOutcome::Built(wanted)
							} else {
								BuildOutcome::AlreadyValid(wanted)
							})
						}
						Err(e) => Err((e.to_string(), failure_code(&e))),
					}
				}
			};
			if let Err((message, _)) = &result {
				first_error.get_or_insert(message.clone());
			}
			results.push((text.clone(), result, started, now()));
			if first_error.is_some() && !settings.build.keep_going {
				break;
			}
		}
		// The worker's activities stop before the operation ends.
		drop(logs);
		drop(worker);
		if op == op::BUILD_PATHS {
			if let Some(message) = first_error {
				return Err(Error::Nix(message));
			}
			self.stop();
			self.to.u64(1);
			return Ok(());
		}
		self.stop();
		self.to.u64(results.len() as u64);
		for (text, result, start, stop) in results {
			self.to.string(&text);
			self.write_build_result(result, start, stop, None);
		}
		Ok(())
	}

	fn write_build_result(
		&mut self,
		result: std::result::Result<BuildOutcome, (String, u64)>,
		start: u64,
		stop: u64,
		times: Option<u64>,
	) {
		let (status, message, outputs, times_built) = match result {
			Ok(BuildOutcome::Built(outputs)) => (0, String::new(), outputs, 1),
			Ok(BuildOutcome::Substituted(outputs)) => (1, String::new(), outputs, 0),
			Ok(BuildOutcome::AlreadyValid(outputs)) => (2, String::new(), outputs, 0),
			Err((message, code)) => (code, message, BTreeMap::new(), 0),
		};
		self.to.u64(status).string(&message);
		if self.version >= version(1, 29) {
			self.to
				.u64(times.unwrap_or(times_built))
				.bool(false)
				.u64(start)
				.u64(stop);
		}
		if self.version >= version(1, 37) {
			self.to.u64(0).u64(0);
		}
		if self.features.contains("realisation-with-path-not-hash") {
			self.to.u64(outputs.len() as u64);
			for (name, path) in &outputs {
				self.to.string(name).store_path(path).u64(0);
			}
		} else if self.version >= version(1, 28) {
			self.to.u64(outputs.len() as u64);
			let dummy = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
			for (name, path) in &outputs {
				let id = format!("{dummy}!{name}");
				self.to
					.string(&id)
					.string(&format!("{{\"id\":\"{id}\",\"outPath\":\"{}\"}}", path));
			}
		}
	}

	/// `queryPathInfo`, which fails for an invalid path.
	fn query_path_info(&self, path: &StorePath) -> Result<ValidPathInfo> {
		self.store
			.query_path_info(path)?
			.ok_or_else(|| Error::Nix(format!("path '{}' is not valid", path.to_absolute_path())))
	}

	/// `WorkerProto::Op::SetOptions` and `ClientSettings::apply`.
	fn set_options(&mut self) -> Result<()> {
		let keep_failed = self.from.read_int()? != 0;
		let keep_going = self.from.read_int()? != 0;
		let try_fallback = self.from.read_int()? != 0;
		let verbosity = self.from.read_int()?;
		let max_build_jobs = self.from.read_int()?;
		let max_silent_time = self.from.read_int()?;
		self.from.read_int()?; // obsolete useBuildHook
		let _verbose_build = self.from.read_int()?;
		self.from.read_int()?; // obsolete logType
		self.from.read_int()?; // obsolete printBuildTrace
		let build_cores = self.from.read_int()?;
		let use_substitutes = self.from.read_int()? != 0;
		let n = self.from.read_int()?;
		let mut overrides = BTreeMap::new();
		for _ in 0..n {
			let name = self.from.read_string()?;
			let value = self.from.read_string()?;
			overrides.insert(name, value);
		}
		self.start()?;
		// `nix::verbosity = clientSettings.verbosity`: the tunnel and the
		// builds this worker runs log at the client's verbosity.
		nix_compat::log::set_verbosity(verbosity);
		self.settings.build.keep_failed = keep_failed;
		self.settings.build.keep_going = keep_going;
		self.settings.build.try_fallback = try_fallback;
		self.settings.build.max_build_jobs = max_build_jobs;
		self.settings.build.max_silent_time = max_silent_time;
		self.settings.build.build_cores = u32::try_from(build_cores)
			.map_err(|_| Error::Protocol("build cores exceed unsigned integer range".into()))?;
		self.settings.build.use_substitutes = use_substitutes;
		let mut accepted = nix_compat::nixcpp::settings::Settings::default();
		for (name, value) in &overrides {
			// The reference compares the names, not their aliases, so
			// `build-timeout` and `build-max-silent-time` stay restricted.
			let unrestricted = matches!(
				name.as_str(),
				"timeout" | "max-silent-time" | "build-poll-interval" | "connect-timeout"
			) || (name == "builders" && value.is_empty());
			if name == "ssh-auth-sock" || name == "experimental-features" {
				continue;
			}
			if name == "plugin-files" {
				self.logger.warn(
					&mut self.to,
					"Ignoring the client-specified plugin-files.\nThe client specifying plugins to the daemon never made sense, and was removed in Nix >=2.14.",
				)?;
			} else if self.trust == Trust::Trusted || unrestricted {
				if let Err(error) = accepted.set(name, value) {
					self.logger.warn(&mut self.to, &error.to_string())?;
				}
			} else if matches!(name.as_str(), "substituters" | "binary-caches") {
				let mut permitted = Vec::new();
				for substituter in value.split_whitespace() {
					if !self.settings.trusted_substituter(substituter) {
						self.logger.warn(
							&mut self.to,
							&format!(
								"ignoring untrusted substituter '{substituter}', you are not a trusted user.\nRun `man nix.conf` for more information on the `substituters` configuration option."
							),
						)?;
					} else {
						permitted.push(substituter);
					}
				}
				accepted
					.set("substituters", &permitted.join(" "))
					.map_err(|error| Error::Nix(error.to_string()))?;
			} else {
				self.logger.warn(
					&mut self.to,
					&format!(
						"ignoring the client-specified setting '{name}', because it is a restricted setting and you are not a trusted user"
					),
				)?;
			}
		}
		self.settings.build.apply_config(&accepted);
		if accepted.get("substituters").is_some() {
			self.settings.substituters = accepted
				.get_list("substituters", &[])
				.into_iter()
				.map(str::to_owned)
				.collect();
		}
		// The substituters open with the settings in effect when they are
		// first used, so a change to them or to the transfer settings
		// reopens them.
		if [
			"substituters",
			"binary-caches",
			"connect-timeout",
			"download-attempts",
			"filetransfer-retry-attempts",
			"filetransfer-retry-delay",
			"filetransfer-retry-delay-rate-limited",
			"filetransfer-retry-max-delay",
			"filetransfer-retry-jitter",
		]
		.into_iter()
		.any(|name| accepted.get(name).is_some())
		{
			self.caches = std::cell::OnceCell::new();
		}
		self.stop();
		Ok(())
	}
}

/// `WorkerProto::Serialise<UnkeyedValidPathInfo>::read` at protocol 1.16
/// or later.
pub(crate) fn read_path_info<R: Read>(
	from: &mut Reader<R>,
	path: StorePath,
) -> Result<ValidPathInfo> {
	let deriver = match from.read_string()? {
		s if s.is_empty() => None,
		s => Some(crate::wire::parse_store_path(&s)?),
	};
	let hash = from.read_string()?;
	let nar_hash = match NixHash::from_str(&hash, Some(HashAlgo::Sha256)) {
		Ok(NixHash::Sha256(digest)) => digest,
		_ => return Err(Error::Nix(format!("invalid SHA-256 hash '{hash}'"))),
	};
	let mut info = ValidPathInfo::new(path, nar_hash);
	info.deriver = deriver;
	info.references = from.read_store_paths()?;
	info.registration_time = from.read_u64()? as i64;
	info.nar_size = from.read_u64()?;
	info.ultimate = from.read_bool()?;
	info.sigs = from.read_signatures()?.into_iter().collect();
	let ca = from.read_string()?;
	if !ca.is_empty() {
		info.ca = Some(crate::wire::parse_content_address(&ca)?);
	}
	Ok(info)
}

/// The hash part of a store path, as `QueryPathFromHashPart` receives it.
pub fn hash_part(path: &StorePath) -> String {
	nixbase32::encode(path.digest())
}

enum BuildOutcome {
	Built(BTreeMap<String, StorePath>),
	Substituted(BTreeMap<String, StorePath>),
	AlreadyValid(BTreeMap<String, StorePath>),
}

/// The wire code of a failed build's `BuildResult` status.
fn failure_code(e: &noxi_build::BuildError) -> u64 {
	use noxi_build::FailureStatus::*;
	match e.status() {
		PermanentFailure => 3,
		InputRejected => 4,
		OutputRejected | HashMismatch => 5,
		TransientFailure => 6,
		CachedFailure => 7,
		TimedOut => 8,
		MiscFailure => 9,
		DependencyFailed => 10,
		LogLimitExceeded => 11,
		NotDeterministic => 12,
		NoSubstituters => 14,
	}
}

struct TunnelReader<'a, R: Read, W: Write> {
	from: &'a mut Reader<R>,
	to: &'a mut Writer<W>,
	pending: io::Cursor<Vec<u8>>,
}

impl<R: Read, W: Write> Read for TunnelReader<'_, R, W> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if buf.is_empty() {
			return Ok(0);
		}
		if self.pending.position() == self.pending.get_ref().len() as u64 {
			self.to.u64(STDERR_READ).u64(32768);
			self.to.flush().map_err(io::Error::other)?;
			let chunk = self.from.read_bytes().map_err(io::Error::other)?;
			if chunk.len() > 32768 {
				return Err(io::Error::other("string is too long"));
			}
			if chunk.is_empty() {
				return Err(io::ErrorKind::UnexpectedEof.into());
			}
			self.pending = io::Cursor::new(chunk);
		}
		self.pending.read(buf)
	}
}

/// `readDerivation` for a BasicDerivation. ATerm preserves arbitrary byte strings
/// and the parser shares output validation with stored derivations.
fn read_basic_derivation<R: Read>(
	from: &mut Reader<R>,
	drv_path: &StorePath,
) -> Result<nix_compat::derivation::UnverifiedDerivation> {
	fn quote(s: &[u8]) -> Vec<u8> {
		let mut out = vec![b'"'];
		for &c in s {
			match c {
				b'"' | b'\\' => {
					out.push(b'\\');
					out.push(c);
				}
				b'\n' => out.extend_from_slice(b"\\n"),
				b'\r' => out.extend_from_slice(b"\\r"),
				b'\t' => out.extend_from_slice(b"\\t"),
				_ => out.push(c),
			}
		}
		out.push(b'"');
		out
	}
	fn list(items: Vec<Vec<u8>>) -> Vec<u8> {
		let mut out = vec![b'['];
		for (i, item) in items.iter().enumerate() {
			if i != 0 {
				out.push(b',');
			}
			out.extend(item);
		}
		out.push(b']');
		out
	}
	let mut outputs = Vec::new();
	for _ in 0..from.read_u64()? {
		let name = from.read_string()?;
		let mut path = from.read_string()?;
		let algo = from.read_string()?;
		let hash = from.read_string()?;
		if !algo.is_empty() && !hash.is_empty() && hash != "impure" {
			// A fixed output's supplied path is syntax only. Recompute the
			// actual path from its hash even for a trusted client.
			crate::wire::parse_store_path(&path)?;
			let ca = crate::wire::parse_content_address(&format!("fixed:{algo}:{hash}"))?;
			let mut info = ValidPathInfo::new(
				StorePath::from_bytes(
					format!(
						"00000000000000000000000000000000-{}{}",
						drv_path.name().strip_suffix(".drv").unwrap(),
						if name == "out" {
							String::new()
						} else {
							format!("-{name}")
						}
					)
					.as_bytes(),
				)
				.map_err(|e| Error::Nix(e.to_string()))?,
				[0; 32],
			);
			info.ca = Some(ca);
			path = info
				.ca_path()
				.ok_or_else(|| Error::Nix("invalid fixed-output path".into()))?
				.to_absolute_path();
		}
		let fields = vec![
			quote(name.as_bytes()),
			quote(path.as_bytes()),
			quote(algo.as_bytes()),
			quote(hash.as_bytes()),
		];
		let mut tuple = list(fields);
		tuple[0] = b'(';
		*tuple.last_mut().unwrap() = b')';
		outputs.push(tuple);
	}
	let mut sources = Vec::new();
	for path in from.read_store_paths()? {
		sources.push(quote(path.to_absolute_path().as_bytes()));
	}
	let system = quote(&from.read_bytes()?);
	let builder = quote(&from.read_bytes()?);
	let mut args = Vec::new();
	for _ in 0..from.read_u64()? {
		args.push(quote(&from.read_bytes()?));
	}
	let mut env = BTreeMap::new();
	for _ in 0..from.read_u64()? {
		let key = from.read_bytes()?;
		env.insert(key, from.read_bytes()?);
	}
	let env = env
		.into_iter()
		.map(|(k, v)| {
			let mut tuple = list(vec![quote(&k), quote(&v)]);
			tuple[0] = b'(';
			*tuple.last_mut().unwrap() = b')';
			tuple
		})
		.collect();
	let fields = vec![
		list(outputs),
		b"[]".to_vec(),
		list(sources),
		system,
		builder,
		list(args),
		list(env),
	];
	let mut term = list(fields);
	term[0] = b'(';
	*term.last_mut().unwrap() = b')';
	let mut bytes = b"Derive".to_vec();
	bytes.extend(term);
	nix_compat::derivation::UnverifiedDerivation::from_aterm_bytes(&bytes)
		.map_err(|e| Error::Nix(format!("error parsing derivation: {e:?}")))
}

/// Read exactly one legacy NAR without buffering bytes of the next request.
fn read_nar<R: Read + Send>(from: &mut R) -> Result<Vec<u8>> {
	struct Recording<'a, R> {
		from: &'a mut R,
		bytes: Vec<u8>,
	}
	impl<R: Read> Read for Recording<'_, R> {
		fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
			let n = self.from.read(buf)?;
			self.bytes.extend_from_slice(&buf[..n]);
			Ok(n)
		}
	}
	fn drain(node: nix_compat::nar::reader::Node<'_, '_>) -> io::Result<()> {
		use nix_compat::nar::reader::Node;
		match node {
			Node::Symlink { .. } => Ok(()),
			Node::File { mut reader, .. } => reader.copy(io::sink()),
			Node::Directory(mut dir) => {
				while let Some(entry) = dir.next()? {
					drain(entry.node)?;
				}
				Ok(())
			}
		}
	}
	let mut recording = Recording {
		from,
		bytes: Vec::new(),
	};
	drain(nix_compat::nar::reader::open(
		&mut BufReader::with_capacity(1, &mut recording),
	)?)?;
	Ok(recording.bytes)
}

fn now() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0)
}

/// `absPath` relative to the daemon's working directory, `/`.
fn absolute(path: &str) -> std::path::PathBuf {
	let path = std::path::Path::new(path);
	if path.is_absolute() {
		path.to_owned()
	} else {
		std::path::Path::new("/").join(path)
	}
}

/// `ContentAddressMethod::parseWithAlgo` for the methods the store can
/// ingest: the dump format, hashing method and algorithm.
fn parse_method_algo(
	s: &str,
) -> (
	Option<FileIngestionMethod>,
	Option<FileIngestionMethod>,
	Option<HashAlgo>,
) {
	let (dump, hash, algo) = if let Some(algo) = s.strip_prefix("text:") {
		(FileIngestionMethod::Flat, FileIngestionMethod::Text, algo)
	} else if let Some(algo) = s.strip_prefix("fixed:r:") {
		(
			FileIngestionMethod::NixArchive,
			FileIngestionMethod::NixArchive,
			algo,
		)
	} else if let Some(algo) = s.strip_prefix("fixed:") {
		if algo.starts_with("git:") {
			return (None, None, None);
		}
		(FileIngestionMethod::Flat, FileIngestionMethod::Flat, algo)
	} else {
		return (None, None, None);
	};
	(Some(dump), Some(hash), algo.parse().ok())
}

/// The build mode the protocol's number names; `read_build_mode` rejects
/// others.
fn build_mode(mode: u64) -> BuildMode {
	match mode {
		1 => BuildMode::Repair,
		2 => BuildMode::Check,
		_ => BuildMode::Normal,
	}
}

/// A builder event of a daemon build: the build's activity and output go
/// through [`BuildLogs`] as for a local build. `built` records that a build started.
fn build_event(logs: &mut BuildLogs, built: &mut bool, event: BuildEvent<'_>) {
	if matches!(event, BuildEvent::Started { .. }) {
		*built = true;
	}
	logs.event(None, event);
}

/// Whether realising `targets` needs the substituters. CppNix's
/// `queryMissing` and the worker's goals finish for valid paths and
/// derivations whose outputs are valid before they call
/// `getDefaultSubstituters`, so a substituter that cannot be opened is
/// reported only when something is missing.
fn needs_substituters(store: &LocalStore, targets: &[DerivedPath]) -> Result<bool> {
	for target in targets {
		match target {
			DerivedPath::Opaque(path) => {
				if !store.is_valid_path(path)? {
					return Ok(true);
				}
			}
			DerivedPath::Built { drv_path, outputs } => {
				if !store.is_valid_path(drv_path)? {
					return Ok(true);
				}
				for (name, path) in store.query_partial_derivation_output_map(drv_path)? {
					if outputs
						.as_ref()
						.is_some_and(|wanted| !wanted.contains(&name))
					{
						continue;
					}
					match path {
						Some(path) if store.is_valid_path(&path)? => {}
						_ => return Ok(true),
					}
				}
			}
		}
	}
	Ok(false)
}

/// `~FramedSource`: read the rest of the frames. A connection that ended
/// first is logged to the client and ignored, as
/// `ignoreExceptionInDestructor` does.
fn drain_frames<R: Read, W: Write>(
	framed: &mut FramedReader<'_, R>,
	logger: &mut Logger,
	to: &mut Writer<W>,
) -> Result<()> {
	match framed.drain() {
		Err(Error::EndOfFile) => logger.log(
			to,
			LVL_ERROR,
			"\x1b[31;1merror (ignored):\x1b[0m unexpected end-of-file",
		),
		result => result,
	}
}
