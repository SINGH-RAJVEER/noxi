//! A store reached through a Nix daemon's socket, following CppNix
//! 2.35.2's `UDSRemoteStore` and `RemoteStore` clients
//! (`remote-store.cc`, `worker-protocol-connection.cc`).
//!
//! The daemon's messages, activities and results go to this process's
//! logger (`nix_compat::log::logger`) with the daemon's activity ids, so
//! each `--log-format` renders them as CppNix's client does.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use nix_compat::derivation::UnverifiedDerivation;
use nix_compat::log::VerbosityLevel;
use nix_compat::log::logger::LogField;
use nix_compat::nixhash::HashAlgo;
use nix_compat::store_path::{StorePath, store_dir};
use noxi_store::gc::{GcAction, GcOptions, GcResults, Roots, make_root_symlink};
use noxi_store::{
	BuildMode, DerivedPath, FileIngestionMethod, ImpureOutputs, Missing, Store, ValidPathInfo,
};

use crate::connection::{
	MINIMUM_PROTOCOL_VERSION, PROTOCOL_VERSION, STDERR_ERROR, STDERR_LAST, STDERR_NEXT,
	STDERR_READ, STDERR_RESULT, STDERR_START_ACTIVITY, STDERR_STOP_ACTIVITY, STDERR_WRITE,
	WORKER_MAGIC_1, WORKER_MAGIC_2, op, read_path_info,
};
use crate::error::{Error, Result};
use crate::peer::BuildResult;
use crate::wire::{Reader, Version, Writer, version};

/// The features this client offers: `WorkerProto::latest.features`, and
/// `disable-set-options`, which lets a daemon that allows it skip
/// `SetOptions`.
const CLIENT_FEATURES: [&str; 3] = [
	"delete-dead-specific-referrers",
	"disable-set-options",
	"realisation-with-path-not-hash",
];

/// The client side of `SetOptions`.
#[derive(Clone, Debug)]
pub struct ClientOptions {
	pub keep_failed: bool,
	pub keep_going: bool,
	pub try_fallback: bool,
	/// CppNix's verbosity; `lvlInfo` (3) by default.
	pub verbosity: u64,
	pub max_build_jobs: u64,
	pub max_silent_time: u64,
	pub verbose_build: bool,
	pub build_cores: u64,
	pub use_substitutes: bool,
	/// The other overridden store and file-transfer settings, by name.
	pub overrides: BTreeMap<String, String>,
}

impl Default for ClientOptions {
	fn default() -> Self {
		Self {
			keep_failed: false,
			keep_going: false,
			try_fallback: false,
			verbosity: 3,
			max_build_jobs: 1,
			max_silent_time: 0,
			verbose_build: true,
			build_cores: 0,
			use_substitutes: true,
			overrides: BTreeMap::new(),
		}
	}
}

#[derive(Clone, Debug, Default)]
pub struct ConnectionInfo {
	pub version: Option<String>,
	pub trusted: Option<bool>,
}

enum Transport {
	Socket(PathBuf),
	/// A daemon started with a command over `ssh`, for `ssh-ng`.
	Ssh(crate::ssh::SshMaster, Vec<std::ffi::OsString>),
}

struct Connection {
	/// Buffered, so a reply costs a few reads rather than one per field.
	from: Reader<BufReader<UnixStream>>,
	to: Writer<UnixStream>,
	version: Version,
	features: BTreeSet<String>,
	/// Whether the daemon trusts this client and its version, if it says.
	info: ConnectionInfo,
	/// The `ssh` process the connection goes through.
	_process: Option<crate::ssh::Process>,
}

/// How many path infos a [`DaemonStore`] remembers.
const PATH_INFO_CACHE_SIZE: usize = 65536;

/// A store whose owner is a daemon.
pub struct DaemonStore {
	transport: Transport,
	/// The store's name in connection errors.
	uri: String,
	options: ClientOptions,
	/// The connection, opened by the first operation as CppNix's
	/// connection pool does.
	conn: RefCell<Option<Connection>>,
	/// Path infos the daemon has returned, as CppNix's `pathInfoCache`.
	/// Garbage collection clears it.
	path_infos: RefCell<HashMap<StorePath, ValidPathInfo>>,
	/// Where this process sees the store's files.
	real_store_dir: PathBuf,
	/// Ask the daemon to write added paths again even when they are valid.
	repair_additions: std::cell::Cell<bool>,
}

pub(crate) fn store_error(e: Error) -> noxi_store::Error {
	match e {
		Error::Store(e) => e,
		e => noxi_store::Error::Other(e.to_string()),
	}
}

/// A verbosity read as CppNix's `(Verbosity) readInt(from)`; a level past
/// `lvlVomit` is as verbose as `lvlVomit`.
fn verbosity_level(level: u64) -> VerbosityLevel {
	VerbosityLevel::try_from(level).unwrap_or(VerbosityLevel::Vomit)
}

/// CppNix's `chomp`.
fn chomp(s: &[u8]) -> &[u8] {
	let end = s
		.iter()
		.rposition(|b| !b" \n\r\t".contains(b))
		.map_or(0, |i| i + 1);
	&s[..end]
}

/// CppNix's `isTTY`: whether standard error is a terminal that takes
/// colour.
fn stderr_is_tty() -> bool {
	// SAFETY: `isatty` has no preconditions.
	let tty = unsafe { libc::isatty(libc::STDERR_FILENO) == 1 };
	tty && std::env::var("TERM").is_ok_and(|term| term != "dumb")
		&& std::env::var_os("NO_COLOR").is_none()
		&& std::env::var_os("NOCOLOR").is_none()
}

impl Connection {
	/// `processStderr`: hand the daemon's messages, activities and results
	/// to this process's logger until the daemon finishes the operation,
	/// returning its error if it failed. Activities keep the daemon's ids,
	/// parents and types, so `internal-json` and the progress bar render
	/// them as they render local ones.
	fn process_stderr(&mut self) -> Result<()> {
		use nix_compat::log::logger;
		use nix_compat::log::{ActivityType, ResultType};
		self.to.flush()?;
		loop {
			let message = self.from.read_u64()?;
			match message {
				// `printError(chomp(s))`.
				STDERR_NEXT => {
					let text = self.from.read_bytes()?;
					logger::log(
						VerbosityLevel::Error,
						&String::from_utf8_lossy(chomp(&text)),
					);
				}
				STDERR_START_ACTIVITY => {
					let id = self.from.read_u64()?;
					let level = self.from.read_u64()?;
					let kind = self.from.read_u64()?;
					let text = self.from.read_bytes()?;
					let fields = self.read_fields()?;
					let parent = self.from.read_u64()?;
					logger::start_activity(
						id,
						verbosity_level(level),
						u8::try_from(kind)
							.ok()
							.and_then(|kind| ActivityType::try_from(kind).ok())
							.unwrap_or(ActivityType::Unknown),
						String::from_utf8_lossy(&text).into_owned(),
						fields,
						parent,
					);
				}
				STDERR_STOP_ACTIVITY => logger::stop_activity(self.from.read_u64()?),
				STDERR_RESULT => {
					let id = self.from.read_u64()?;
					let kind = self.from.read_u64()?;
					let fields = self.read_fields()?;
					// 2.35.2 sends no other result types.
					if let Some(kind) = u8::try_from(kind)
						.ok()
						.and_then(|kind| ResultType::try_from(kind).ok())
					{
						logger::result(id, kind, &fields);
					}
				}
				STDERR_ERROR => return Err(Error::Nix(self.read_error()?)),
				STDERR_LAST => return Ok(()),
				STDERR_WRITE | STDERR_READ => {
					return Err(Error::Protocol(format!(
						"got unexpected message type {message:x} from Nix daemon"
					)));
				}
				_ => {
					return Err(Error::Protocol(format!(
						"got unknown message type {message:x} from Nix daemon"
					)));
				}
			}
		}
	}

	/// `readFields`: integer and string fields, kept apart.
	fn read_fields(&mut self) -> Result<Vec<LogField>> {
		let count = self.from.read_u64()?;
		let mut fields = Vec::new();
		for _ in 0..count {
			match self.from.read_u64()? {
				0 => fields.push(LogField::Int(self.from.read_u64()?)),
				1 => fields.push(LogField::Str(
					String::from_utf8_lossy(&self.from.read_bytes()?).into_owned(),
				)),
				kind => {
					return Err(Error::Protocol(format!(
						"got unsupported field type {kind:x} from Nix daemon"
					)));
				}
			}
		}
		Ok(fields)
	}

	/// `readError`: the message of a structured error, with its traces.
	fn read_error(&mut self) -> Result<String> {
		if self.version < version(1, 26) {
			let message = self.from.read_string()?;
			self.from.read_u64()?;
			return Ok(message);
		}
		let _kind = self.from.read_string()?;
		let _level = self.from.read_u64()?;
		let _name = self.from.read_string()?;
		let mut message = self.from.read_string()?;
		self.from.read_u64()?; // no position
		let traces = self.from.read_u64()?;
		for _ in 0..traces {
			self.from.read_u64()?; // no position
			let trace = self.from.read_string()?;
			message.push_str(&format!("\n… {trace}"));
		}
		// The client prints the error through its logger, which filters
		// it as it does log lines.
		let filtered = nix_compat::log::filter_ansi_escapes(message.as_bytes(), !stderr_is_tty());
		Ok(String::from_utf8_lossy(&filtered).into_owned())
	}
}

impl DaemonStore {
	/// The store served by the daemon at `socket`. The store's files are
	/// read below `root` if given, as with `unix://SOCKET?root=ROOT`.
	pub fn new(socket: &Path, root: Option<&Path>, options: &ClientOptions) -> Self {
		// `UDSRemoteStoreConfig::getReference`: `daemon` for the default
		// socket, `unix://PATH` for any other.
		let default_socket = std::env::var_os("NIX_DAEMON_SOCKET_PATH")
			.filter(|path| !path.is_empty())
			.map(PathBuf::from)
			.unwrap_or_else(|| {
				std::env::var_os("NIX_STATE_DIR")
					.filter(|dir| !dir.is_empty())
					.map_or_else(|| PathBuf::from("/nix/var/nix"), PathBuf::from)
					.join("daemon-socket/socket")
			});
		let uri = if socket == default_socket {
			"daemon".to_owned()
		} else {
			format!("unix://{}", socket.display())
		};
		let real_store_dir = match root {
			Some(root) => root.join("nix/store"),
			None => PathBuf::from(store_dir()),
		};
		Self::with_transport(
			Transport::Socket(socket.to_owned()),
			uri,
			options,
			real_store_dir,
		)
	}

	fn with_transport(
		transport: Transport,
		uri: String,
		options: &ClientOptions,
		real_store_dir: PathBuf,
	) -> Self {
		Self {
			transport,
			uri,
			options: options.clone(),
			conn: RefCell::new(None),
			path_infos: RefCell::default(),
			real_store_dir,
			repair_additions: std::cell::Cell::new(false),
		}
	}

	/// `SSHStore`: a daemon started with `command` over `ssh`.
	pub fn over_ssh(
		master: crate::ssh::SshMaster,
		command: Vec<std::ffi::OsString>,
		uri: String,
	) -> Self {
		Self::with_transport(
			Transport::Ssh(master, command),
			uri,
			&ClientOptions::default(),
			PathBuf::from(store_dir()),
		)
	}

	/// `UDSRemoteStore::openConnection` and `initConnection`.
	fn open(&self) -> noxi_store::Result<Connection> {
		let (stream, process) = match &self.transport {
			Transport::Socket(socket) => (
				noxi_store::unix_socket::connect(socket).map_err(|e| {
					noxi_store::Error::Other(format!(
						"cannot connect to socket at '{}': {}",
						socket.display(),
						io_message(&e)
					))
				})?,
				None,
			),
			Transport::Ssh(master, command) => {
				let (stream, process) = master.start_command(command)?;
				(stream, Some(process))
			}
		};
		let read = stream.try_clone().map_err(|e| store_error(e.into()))?;
		let mut conn = Connection {
			from: Reader::new(BufReader::new(read)),
			to: Writer::new(stream),
			version: PROTOCOL_VERSION,
			features: BTreeSet::new(),
			info: ConnectionInfo::default(),
			_process: process,
		};
		let features = handshake(&mut conn).map_err(|e| {
			// CppNix nests the cause's full message, `error: ` included; a
			// failed read is its `FdSource`'s, which reads 32768 bytes at a
			// time, as when a daemon refuses the user and closes.
			let cause = match e {
				Error::Io(e) => format!("read of 32768 bytes: {}", io_message(&e)),
				e => e.to_string(),
			};
			noxi_store::Error::Other(format!(
				"cannot open connection to remote store '{}': error: {cause}",
				self.uri
			))
		})?;
		// `SSHStore::setOptions` sends nothing: a remote machine's settings
		// are its own.
		if matches!(self.transport, Transport::Socket(_))
			&& !features.contains("disable-set-options")
		{
			set_options(&mut conn, &self.options).map_err(store_error)?;
		}
		conn.features = features;
		Ok(conn)
	}

	/// Run `f` on the connection, opening it first if needed. A failure
	/// other than the daemon's own error closes it.
	fn with_conn<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> noxi_store::Result<T> {
		let mut slot = self.conn.borrow_mut();
		if slot.is_none() {
			*slot = Some(self.open()?);
		}
		let result = f(slot.as_mut().expect("connection was opened"));
		if matches!(result, Err(ref e) if !matches!(e, Error::Nix(_))) {
			*slot = None;
		}
		result.map_err(store_error)
	}

	pub fn connection_info(&self) -> noxi_store::Result<ConnectionInfo> {
		self.with_conn(|conn| Ok(conn.info.clone()))
	}

	pub fn uri(&self) -> &str {
		&self.uri
	}

	/// An operation on one store path.
	fn path_op<T>(
		&self,
		code: u64,
		path: &StorePath,
		reply: impl FnOnce(&mut Connection) -> Result<T>,
	) -> noxi_store::Result<T> {
		self.with_conn(|conn| {
			conn.to.u64(code).store_path(path);
			conn.process_stderr()?;
			reply(conn)
		})
	}

	fn add_to_store(
		&self,
		source: &mut dyn Read,
		name: &str,
		method: &str,
		references: &BTreeSet<StorePath>,
	) -> noxi_store::Result<StorePath> {
		self.with_conn(|conn| {
			conn.to
				.u64(op::ADD_TO_STORE)
				.string(name)
				.string(method)
				.store_paths(references.iter())
				.bool(self.repair_additions.get());
			conn.to.flush()?;
			// `FramedSink`: length-prefixed chunks, then an empty one.
			let mut buf = vec![0u8; 32768];
			loop {
				let n = source.read(&mut buf)?;
				conn.to.u64(n as u64);
				if n == 0 {
					break;
				}
				conn.to.raw(&buf[..n]);
				conn.to.flush()?;
			}
			conn.process_stderr()?;
			let path = conn.from.read_store_path()?;
			read_path_info(&mut conn.from, path.clone())?;
			Ok(path)
		})
	}
}

/// `FramedSink`: data as length-prefixed frames, then an empty frame.
struct FramedSink<'a> {
	to: &'a mut Writer<UnixStream>,
	buf: Vec<u8>,
}

impl<'a> FramedSink<'a> {
	const FRAME: usize = 65536;

	fn new(to: &'a mut Writer<UnixStream>) -> Self {
		Self {
			to,
			buf: Vec::with_capacity(Self::FRAME),
		}
	}

	fn send(&mut self) -> Result<()> {
		if !self.buf.is_empty() {
			self.to.u64(self.buf.len() as u64).raw(&self.buf);
			self.to.flush()?;
			self.buf.clear();
		}
		Ok(())
	}

	fn finish(mut self) -> Result<()> {
		self.send()?;
		self.to.u64(0);
		self.to.flush()
	}
}

impl Write for FramedSink<'_> {
	fn write(&mut self, data: &[u8]) -> io::Result<usize> {
		let n = data.len().min(Self::FRAME - self.buf.len());
		self.buf.extend_from_slice(&data[..n]);
		if self.buf.len() == Self::FRAME {
			self.send().map_err(io::Error::other)?;
		}
		Ok(n)
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

/// `WorkerProto::Serialise<UnkeyedValidPathInfo>::write`.
pub(crate) fn write_path_info<W: Write>(to: &mut Writer<W>, info: &ValidPathInfo) {
	let hash: String = info.nar_hash.iter().map(|b| format!("{b:02x}")).collect();
	to.opt_store_path(info.deriver.as_ref())
		.string(&hash)
		.store_paths(info.references.iter())
		.u64(info.registration_time as u64)
		.u64(info.nar_size)
		.bool(info.ultimate)
		.strings(info.sigs.iter().map(String::as_str))
		.string(
			&info
				.ca
				.as_ref()
				.map(|ca| ca.to_string())
				.unwrap_or_default(),
		);
}

fn io_message(e: &io::Error) -> String {
	// `SysError` appends `strerror`, without Rust's error code suffix.
	let text = e.to_string();
	match text.find(" (os error") {
		Some(end) => text[..end].to_owned(),
		None => text,
	}
}

/// `BasicClientConnection::handshake` and `postHandshake`: the negotiated
/// features.
fn handshake(conn: &mut Connection) -> Result<BTreeSet<String>> {
	conn.to.u64(WORKER_MAGIC_1).u64(PROTOCOL_VERSION);
	conn.to.flush()?;
	if conn.from.read_u64()? != WORKER_MAGIC_2 {
		return Err(Error::Nix("nix-daemon protocol mismatch from".into()));
	}
	let daemon_version = crate::wire::from_wire(conn.from.read_u64()?);
	if daemon_version >> 8 != 1 {
		return Err(Error::Nix(
			"Nix daemon protocol version not supported".into(),
		));
	}
	if daemon_version < MINIMUM_PROTOCOL_VERSION {
		return Err(Error::Nix("the Nix daemon version is too old".into()));
	}
	conn.version = daemon_version.min(PROTOCOL_VERSION);
	let mut features = BTreeSet::new();
	if conn.version >= version(1, 38) {
		conn.to.strings(CLIENT_FEATURES.iter().copied());
		conn.to.flush()?;
		for feature in conn.from.read_strings()? {
			if CLIENT_FEATURES.contains(&feature.as_str()) {
				features.insert(feature);
			}
		}
	}
	if conn.version >= version(1, 14) {
		conn.to.u64(0); // obsolete CPU affinity
	}
	if conn.version >= version(1, 11) {
		conn.to.bool(false); // obsolete reserveSpace
	}
	conn.to.flush()?;
	if conn.version >= version(1, 33) {
		conn.info.version = Some(conn.from.read_string()?);
	}
	if conn.version >= version(1, 35) {
		conn.info.trusted = match conn.from.read_u64()? {
			1 => Some(true),
			2 => Some(false),
			_ => None,
		};
	}
	conn.process_stderr()?;
	Ok(features)
}

/// `RemoteStore::setOptions`.
fn set_options(conn: &mut Connection, options: &ClientOptions) -> Result<()> {
	conn.to
		.u64(op::SET_OPTIONS)
		.bool(options.keep_failed)
		.bool(options.keep_going)
		.bool(options.try_fallback)
		.u64(options.verbosity)
		.u64(options.max_build_jobs)
		.u64(options.max_silent_time)
		.bool(true)
		.u64(if options.verbose_build { 0 } else { 7 })
		.u64(0)
		.u64(0)
		.u64(options.build_cores)
		.bool(options.use_substitutes)
		.u64(options.overrides.len() as u64);
	for (name, value) in &options.overrides {
		conn.to.string(name).string(value);
	}
	conn.process_stderr()
}

/// `DerivedPath::to_string_legacy`.
fn legacy_string(path: &DerivedPath) -> String {
	match path {
		DerivedPath::Opaque(path) => path.to_absolute_path(),
		DerivedPath::Built { drv, outputs } if outputs.is_empty() => {
			format!("{}!*", drv.to_absolute_path())
		}
		DerivedPath::Built { drv, outputs } => {
			let outputs: Vec<&str> = outputs.iter().map(String::as_str).collect();
			format!("{}!{}", drv.to_absolute_path(), outputs.join(","))
		}
	}
}

fn mode_number(mode: BuildMode) -> u64 {
	match mode {
		BuildMode::Normal => 0,
		BuildMode::Repair => 1,
		BuildMode::Check => 2,
	}
}

/// A `BuildResult`: its outputs, or the message of the failure.
fn read_build_result(
	conn: &mut Connection,
) -> Result<std::result::Result<BTreeMap<String, StorePath>, String>> {
	let result = read_full_build_result(conn)?;
	Ok(if result.success() {
		Ok(result.outputs)
	} else {
		Err(result.message)
	})
}

fn read_full_build_result(conn: &mut Connection) -> Result<BuildResult> {
	let mut result = BuildResult {
		status: conn.from.read_u64()?,
		message: conn.from.read_string()?,
		..BuildResult::default()
	};
	if conn.version >= version(1, 29) {
		result.times_built = conn.from.read_u64()?;
		result.non_deterministic = conn.from.read_bool()?;
		result.start = conn.from.read_u64()?;
		result.stop = conn.from.read_u64()?;
	}
	if conn.version >= version(1, 37) {
		// The optional user and system CPU times.
		for _ in 0..2 {
			if conn.from.read_u64()? != 0 {
				conn.from.read_u64()?;
			}
		}
	}
	if conn.features.contains("realisation-with-path-not-hash") {
		result.outputs = crate::peer::read_outputs(&mut conn.from)?;
	} else if conn.version >= version(1, 28) {
		for _ in 0..conn.from.read_u64()? {
			let id = conn.from.read_string()?;
			let realisation = conn.from.read_string()?;
			let name = id
				.split_once('!')
				.ok_or_else(|| Error::Nix(format!("Invalid derivation output id {id}")))?
				.1;
			let path = serde_json::from_str::<serde_json::Value>(&realisation)
				.ok()
				.and_then(|json| StorePath::from_bytes(json["outPath"].as_str()?.as_bytes()).ok())
				.ok_or_else(|| Error::Nix(format!("invalid realisation '{realisation}'")))?;
			result.outputs.insert(name.to_owned(), path);
		}
	}
	Ok(result)
}

impl crate::peer::Peer for DaemonStore {
	fn connect(&self) -> noxi_store::Result<()> {
		self.with_conn(|_| Ok(()))
	}

	fn trusted(&self) -> noxi_store::Result<Option<bool>> {
		self.with_conn(|conn| Ok(conn.info.trusted))
	}

	fn valid_paths(
		&self,
		paths: &BTreeSet<StorePath>,
		substitute: bool,
	) -> noxi_store::Result<BTreeSet<StorePath>> {
		self.with_conn(|conn| {
			conn.to.u64(op::QUERY_VALID_PATHS).store_paths(paths.iter());
			if conn.version >= version(1, 27) {
				conn.to.bool(substitute);
			}
			conn.process_stderr()?;
			conn.from.read_store_paths()
		})
	}

	fn path_info(&self, path: &StorePath) -> noxi_store::Result<Option<ValidPathInfo>> {
		self.query_path_info(path)
	}

	fn nar(&self, path: &StorePath, sink: &mut dyn Write) -> noxi_store::Result<()> {
		self.path_op(op::NAR_FROM_PATH, path, |conn| {
			crate::nar_stream::copy_nar(conn.from.get_mut(), sink)
		})
	}

	fn add_paths(
		&self,
		infos: &[ValidPathInfo],
		nar: &mut crate::peer::NarSource<'_>,
	) -> noxi_store::Result<()> {
		self.add_multiple_to_store(infos, nar, false, false)
	}

	fn build_derivation(
		&self,
		drv_path: &StorePath,
		drv: &noxi_store::build_derivation::BuildDerivation,
	) -> noxi_store::Result<BuildResult> {
		self.with_conn(|conn| {
			conn.to.u64(op::BUILD_DERIVATION).store_path(drv_path);
			crate::peer::write_basic_derivation(&mut conn.to, drv, drv.basic.input_sources());
			conn.to.u64(mode_number(BuildMode::Normal));
			conn.process_stderr()?;
			read_full_build_result(conn)
		})
	}

	fn build_registered(&self, drv_path: &StorePath) -> noxi_store::Result<BuildResult> {
		self.with_conn(|conn| {
			let target = format!("{}!*", drv_path.to_absolute_path());
			conn.to
				.u64(op::BUILD_PATHS_WITH_RESULTS)
				.strings(std::iter::once(target.as_str()))
				.u64(mode_number(BuildMode::Normal));
			conn.process_stderr()?;
			if conn.from.read_u64()? != 1 {
				return Err(Error::Protocol(
					"one path to build should produce one build result".into(),
				));
			}
			conn.from.read_string()?;
			read_full_build_result(conn)
		})
	}
}

impl Store for DaemonStore {
	fn optimise_store(&self, _log: &mut dyn FnMut(&str)) -> noxi_store::Result<()> {
		self.with_conn(|conn| {
			conn.to.u64(op::OPTIMISE_STORE);
			conn.process_stderr()?;
			conn.from.read_u64().map(drop)
		})
	}

	fn find_roots(&self, _censor: bool, _log: &mut dyn FnMut(&str)) -> noxi_store::Result<Roots> {
		self.with_conn(|conn| {
			conn.to.u64(op::FIND_ROOTS);
			conn.process_stderr()?;
			let count = conn.from.read_u64()?;
			let mut roots = Roots::new();
			for _ in 0..count {
				let link = conn.from.read_string()?;
				let path = conn.from.read_store_path()?;
				roots.entry(path).or_default().insert(link);
			}
			Ok(roots)
		})
	}

	fn collect_garbage(
		&self,
		options: &GcOptions,
		_log: &mut dyn FnMut(&str),
	) -> noxi_store::Result<GcResults> {
		self.path_infos.borrow_mut().clear();
		self.with_conn(|conn| {
			let specific = conn.features.contains("delete-dead-specific-referrers");
			if !specific && options.paths.is_some() {
				if options.action != GcAction::DeleteSpecific {
					return Err(Error::Nix("Your daemon version is too old to support garbage collecting a specific set of paths".into()));
				}
				if options.delete_referrers {
					return Err(Error::Nix("Your daemon version is too old to support deleting referrers.".into()));
				}
			}
			conn.to.u64(op::COLLECT_GARBAGE).u64(match options.action {
				GcAction::ReturnLive => 0,
				GcAction::ReturnDead => 1,
				GcAction::DeleteDead => 2,
				GcAction::DeleteSpecific => 3,
			});
			if specific {
				match &options.paths {
					None => { conn.to.u64(1); }
					Some(paths) => {
						conn.to.u64(0).store_paths(paths.iter()).bool(options.delete_referrers);
					}
				}
			} else {
				let empty = BTreeSet::new();
				conn.to.store_paths(options.paths.as_ref().unwrap_or(&empty).iter());
			}
			conn.to.bool(options.ignore_liveness).u64(options.max_freed).u64(0).u64(0).u64(0);
			conn.process_stderr()?;
			let result = GcResults {
				paths: conn.from.read_strings()?.into_iter().collect(),
				bytes_freed: conn.from.read_u64()?,
			};
			conn.from.read_u64()?; // obsolete
			Ok(result)
		})
	}

	fn real_path(&self, path: &StorePath) -> PathBuf {
		self.real_store_dir.join(path.to_string())
	}

	fn is_valid_path(&self, path: &StorePath) -> noxi_store::Result<bool> {
		if self.path_infos.borrow().contains_key(path) {
			return Ok(true);
		}
		self.path_op(op::IS_VALID_PATH, path, |conn| conn.from.read_bool())
	}

	fn query_path_info(&self, path: &StorePath) -> noxi_store::Result<Option<ValidPathInfo>> {
		if let Some(info) = self.path_infos.borrow().get(path) {
			return Ok(Some(info.clone()));
		}
		let info = self.path_op(op::QUERY_PATH_INFO, path, |conn| {
			if !conn.from.read_bool()? {
				return Ok(None);
			}
			read_path_info(&mut conn.from, path.clone()).map(Some)
		})?;
		if let Some(info) = &info {
			let mut infos = self.path_infos.borrow_mut();
			// CppNix's default `path-info-cache-size` bounds the cache.
			if infos.len() >= PATH_INFO_CACHE_SIZE {
				infos.clear();
			}
			infos.insert(path.clone(), info.clone());
		}
		Ok(info)
	}

	fn query_referrers(&self, path: &StorePath) -> noxi_store::Result<BTreeSet<StorePath>> {
		self.path_op(op::QUERY_REFERRERS, path, |conn| {
			conn.from.read_store_paths()
		})
	}

	fn query_valid_derivers(&self, path: &StorePath) -> noxi_store::Result<BTreeSet<StorePath>> {
		self.path_op(op::QUERY_VALID_DERIVERS, path, |conn| {
			conn.from.read_store_paths()
		})
	}

	fn query_derivation_outputs(
		&self,
		drv: &StorePath,
	) -> noxi_store::Result<BTreeMap<String, StorePath>> {
		self.path_op(op::QUERY_DERIVATION_OUTPUT_MAP, drv, |conn| {
			let count = conn.from.read_u64()?;
			let mut outputs = BTreeMap::new();
			for _ in 0..count {
				let name = conn.from.read_string()?;
				let path = conn.from.read_string()?;
				if !path.is_empty() {
					outputs.insert(name, crate::wire::parse_store_path(&path)?);
				}
			}
			Ok(outputs)
		})
	}

	fn read_derivation(&self, drv: &StorePath) -> noxi_store::Result<UnverifiedDerivation> {
		let bytes = std::fs::read(self.real_path(drv)).map_err(|e| {
			noxi_store::Error::io(
				format!("reading derivation '{}'", drv.to_absolute_path()),
				e,
			)
		})?;
		UnverifiedDerivation::from_aterm_bytes(&bytes).map_err(|e| {
			noxi_store::Error::Other(format!(
				"error parsing derivation '{}': {e:?}",
				drv.to_absolute_path()
			))
		})
	}

	fn add_text_to_store(
		&self,
		name: &str,
		contents: &[u8],
		references: &BTreeSet<StorePath>,
	) -> noxi_store::Result<StorePath> {
		self.add_to_store(&mut &contents[..], name, "text:sha256", references)
	}

	fn add_to_store_from_dump(
		&self,
		source: &mut (dyn Read + Send),
		name: &str,
		dump_method: FileIngestionMethod,
		hash_method: FileIngestionMethod,
		hash_algo: HashAlgo,
		references: &BTreeSet<StorePath>,
	) -> noxi_store::Result<StorePath> {
		let method = match (dump_method, hash_method) {
			(_, FileIngestionMethod::Text) => format!("text:{hash_algo}"),
			(_, FileIngestionMethod::NixArchive) => format!("fixed:r:{hash_algo}"),
			(_, FileIngestionMethod::Flat) => format!("fixed:{hash_algo}"),
			(_, FileIngestionMethod::Git) => format!("fixed:git:{hash_algo}"),
		};
		self.add_to_store(source, name, &method, references)
	}

	fn repair_additions(&self, repair: bool) {
		self.repair_additions.set(repair);
	}

	fn add_temp_root(&self, path: &StorePath) -> noxi_store::Result<()> {
		self.path_op(op::ADD_TEMP_ROOT, path, |conn| {
			conn.from.read_u64().map(drop)
		})
	}

	/// `IndirectRootStore::addPermRoot`: the link is made here, and the
	/// daemon is told about it.
	fn add_perm_root(&self, path: &StorePath, gc_root: &Path) -> noxi_store::Result<PathBuf> {
		if gc_root.starts_with(store_dir()) {
			return Err(noxi_store::Error::Other(format!(
				"creating a garbage collector root (\"{}\") in the Nix store is forbidden (are you running nix-build inside the store?)",
				gc_root.display()
			)));
		}
		self.add_temp_root(path)?;
		if let Ok(metadata) = std::fs::symlink_metadata(gc_root) {
			let points_into_store = metadata.file_type().is_symlink()
				&& std::fs::read_link(gc_root).is_ok_and(|t| t.starts_with(store_dir()));
			if !points_into_store {
				return Err(noxi_store::Error::Other(format!(
					"cannot create symlink \"{}\"; already exists",
					gc_root.display()
				)));
			}
		}
		make_root_symlink(Path::new(&path.to_absolute_path()), gc_root)?;
		self.with_conn(|conn| {
			conn.to
				.u64(op::ADD_INDIRECT_ROOT)
				.string(&gc_root.to_string_lossy());
			conn.process_stderr()?;
			conn.from.read_u64().map(drop)
		})?;
		Ok(gc_root.to_owned())
	}

	fn verify_store(
		&self,
		check_contents: bool,
		repair: bool,
		_log: &mut dyn FnMut(VerbosityLevel, &str),
	) -> noxi_store::Result<bool> {
		self.with_conn(|conn| {
			conn.to
				.u64(op::VERIFY_STORE)
				.bool(check_contents)
				.bool(repair);
			conn.process_stderr()?;
			conn.from.read_bool()
		})
	}

	fn query_all_valid_paths(&self) -> noxi_store::Result<BTreeSet<StorePath>> {
		self.with_conn(|conn| {
			conn.to.u64(op::QUERY_ALL_VALID_PATHS);
			conn.process_stderr()?;
			conn.from.read_store_paths()
		})
	}

	fn query_substitutable_paths(
		&self,
		paths: &BTreeSet<StorePath>,
	) -> noxi_store::Result<BTreeSet<StorePath>> {
		self.with_conn(|conn| {
			conn.to
				.u64(op::QUERY_SUBSTITUTABLE_PATHS)
				.store_paths(paths.iter());
			conn.process_stderr()?;
			conn.from.read_store_paths()
		})
	}

	fn query_valid_paths(
		&self,
		paths: &BTreeSet<StorePath>,
	) -> noxi_store::Result<BTreeSet<StorePath>> {
		self.query_valid_paths_with_substitute(paths, false)
	}

	fn query_valid_paths_with_substitute(
		&self,
		paths: &BTreeSet<StorePath>,
		substitute: bool,
	) -> noxi_store::Result<BTreeSet<StorePath>> {
		self.with_conn(|conn| {
			conn.to.u64(op::QUERY_VALID_PATHS).store_paths(paths.iter());
			if conn.version >= version(1, 27) {
				conn.to.bool(substitute);
			}
			conn.process_stderr()?;
			conn.from.read_store_paths()
		})
	}

	fn nar_from_path(&self, path: &StorePath, sink: &mut dyn Write) -> noxi_store::Result<()> {
		self.path_op(op::NAR_FROM_PATH, path, |conn| {
			crate::connection::copy_nar(conn.from.get_mut(), sink)
		})
	}

	fn add_multiple_to_store(
		&self,
		paths: &[ValidPathInfo],
		nar: &mut dyn FnMut(&StorePath, &mut dyn Write) -> noxi_store::Result<()>,
		repair: bool,
		check_sigs: bool,
	) -> noxi_store::Result<()> {
		self.with_conn(|conn| {
			if conn.version < version(1, 32) {
				return Err(Error::Nix(
					"the daemon is too old to add multiple paths".into(),
				));
			}
			conn.to
				.u64(op::ADD_MULTIPLE_TO_STORE)
				.bool(repair)
				.bool(!check_sigs);
			conn.to.flush()?;
			let mut sink = FramedSink::new(&mut conn.to);
			let mut header = Writer::new(Vec::new());
			header.u64(paths.len() as u64);
			sink.write_all(&header.take())?;
			for info in paths {
				header.store_path(&info.path);
				write_path_info(&mut header, info);
				sink.write_all(&header.take())?;
				nar(&info.path, &mut sink).map_err(Error::Store)?;
			}
			sink.finish()?;
			conn.process_stderr()
		})
	}

	fn build_paths(&self, paths: &[DerivedPath]) -> noxi_store::Result<()> {
		self.build_paths_with_mode(paths, BuildMode::Normal)
	}

	fn repair_path(&self, _path: &StorePath) -> noxi_store::Result<()> {
		Err(noxi_store::Error::Other(
			"operation 'repairPath' is not supported by store 'daemon'".to_owned(),
		))
	}

	fn build_paths_with_mode(
		&self,
		paths: &[DerivedPath],
		mode: BuildMode,
	) -> noxi_store::Result<()> {
		self.with_conn(|conn| {
			let targets: Vec<String> = paths.iter().map(legacy_string).collect();
			conn.to
				.u64(op::BUILD_PATHS)
				.strings(targets.iter().map(String::as_str))
				.u64(mode_number(mode));
			conn.process_stderr()?;
			conn.from.read_u64().map(drop)
		})
	}

	/// The outputs of impure derivations are known only from the results of
	/// building them.
	fn build_paths_with_results(
		&self,
		paths: &[DerivedPath],
		mode: BuildMode,
	) -> noxi_store::Result<ImpureOutputs> {
		let impure = paths.iter().any(|path| match path {
			DerivedPath::Opaque(_) => false,
			DerivedPath::Built { drv, .. } => self
				.read_build_derivation(drv)
				.is_ok_and(|drv| drv.is_impure()),
		});
		if !impure {
			return match self.build_paths_with_mode(paths, mode) {
				Ok(()) => Ok(ImpureOutputs::new()),
				Err(noxi_store::Error::Build { message, .. }) => {
					Err(noxi_store::Error::Other(message))
				}
				Err(error) => Err(error),
			};
		}
		let targets: Vec<String> = paths.iter().map(legacy_string).collect();
		let results = self.with_conn(|conn| {
			if conn.version < version(1, 34) {
				return Err(Error::Nix(
					"the daemon is too old to return the outputs of impure derivations".into(),
				));
			}
			conn.to
				.u64(op::BUILD_PATHS_WITH_RESULTS)
				.strings(targets.iter().map(String::as_str))
				.u64(mode_number(mode));
			conn.process_stderr()?;
			let count = conn.from.read_u64()?;
			let mut results = Vec::new();
			for _ in 0..count {
				results.push((conn.from.read_string()?, read_build_result(conn)?));
			}
			Ok(results)
		})?;
		let mut failures = Vec::new();
		let mut outputs = ImpureOutputs::new();
		for (target, result) in results {
			match result {
				Err(message) => failures.push((target, message)),
				Ok(built) => {
					if let Some(DerivedPath::Built { drv, .. }) =
						paths.iter().find(|path| legacy_string(path) == target)
					{
						outputs.entry(drv.clone()).or_default().extend(built);
					}
				}
			}
		}
		// `throwBuildErrors`.
		if failures.len() > 1 {
			let mut targets = BTreeSet::new();
			for (target, message) in &failures {
				if !message.is_empty() {
					nix_compat::log::logger::log_error(
						&nix_compat::log::format_error(message),
						message,
					);
				}
				targets.insert(format!("'{}'", target.replace('!', "^")));
			}
			let targets: Vec<String> = targets.into_iter().collect();
			return Err(noxi_store::Error::Other(format!(
				"build of {} failed",
				targets.join(", ")
			)));
		}
		match failures.pop() {
			Some((_, message)) => Err(noxi_store::Error::Other(message)),
			None => Ok(outputs),
		}
	}

	fn query_missing(&self, paths: &[DerivedPath]) -> noxi_store::Result<Missing> {
		self.with_conn(|conn| {
			let targets: Vec<String> = paths.iter().map(legacy_string).collect();
			conn.to
				.u64(op::QUERY_MISSING)
				.strings(targets.iter().map(String::as_str));
			conn.process_stderr()?;
			Ok(Missing {
				ignored_dynamic: BTreeSet::new(),
				will_build: conn.from.read_store_paths()?,
				will_substitute: conn.from.read_store_paths()?,
				unknown: conn.from.read_store_paths()?,
				download_size: conn.from.read_u64()?,
				nar_size: conn.from.read_u64()?,
			})
		})
	}

	fn ensure_path(&self, path: &StorePath) -> noxi_store::Result<()> {
		self.path_op(op::ENSURE_PATH, path, |conn| conn.from.read_u64().map(drop))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn connected(features: &[&str]) -> (DaemonStore, UnixStream) {
		let (client, daemon) = UnixStream::pair().unwrap();
		for stream in [&client, &daemon] {
			stream
				.set_read_timeout(Some(std::time::Duration::from_secs(5)))
				.unwrap();
			stream
				.set_write_timeout(Some(std::time::Duration::from_secs(5)))
				.unwrap();
		}
		let store = DaemonStore::new(Path::new("unused.sock"), None, &ClientOptions::default());
		*store.conn.borrow_mut() = Some(Connection {
			from: Reader::new(BufReader::new(client.try_clone().unwrap())),
			to: Writer::new(client),
			version: version(1, 38),
			info: ConnectionInfo::default(),
			_process: None,
			features: features
				.iter()
				.map(|feature| (*feature).to_owned())
				.collect(),
		});
		(store, daemon)
	}

	#[test]
	fn gc_encodings_follow_negotiated_features_and_leave_the_connection_aligned() {
		let path = StorePath::from_bytes(b"00000000000000000000000000000000-x").unwrap();
		for feature in [false, true] {
			for specific in [false, true] {
				let (store, daemon) = connected(if feature {
					&["delete-dead-specific-referrers"]
				} else {
					&[]
				});
				let expected = path.clone();
				let worker = std::thread::spawn(move || {
					let mut from = Reader::new(daemon.try_clone().unwrap());
					let mut to = Writer::new(daemon);
					assert_eq!(from.read_u64().unwrap(), op::COLLECT_GARBAGE);
					assert_eq!(from.read_u64().unwrap(), if specific { 3 } else { 2 });
					if feature {
						assert_eq!(from.read_u64().unwrap(), if specific { 0 } else { 1 });
					}
					if specific {
						assert_eq!(from.read_store_paths().unwrap(), [expected.clone()].into());
						if feature {
							assert!(from.read_bool().unwrap());
						}
					} else if !feature {
						assert!(from.read_store_paths().unwrap().is_empty());
					}
					assert!(!from.read_bool().unwrap());
					assert_eq!(from.read_u64().unwrap(), 1024);
					for _ in 0..3 {
						assert_eq!(from.read_u64().unwrap(), 0);
					}
					to.u64(STDERR_LAST)
						.strings([expected.to_absolute_path().as_str()].into_iter())
						.u64(99)
						.u64(0);
					to.flush().unwrap();
					assert_eq!(from.read_u64().unwrap(), op::IS_VALID_PATH);
					assert_eq!(from.read_store_path().unwrap(), expected);
					to.u64(STDERR_LAST).bool(false);
					to.flush().unwrap();
				});
				let mut options = GcOptions::new(if specific {
					GcAction::DeleteSpecific
				} else {
					GcAction::DeleteDead
				});
				options.max_freed = 1024;
				if specific {
					options.paths = Some([path.clone()].into());
					options.delete_referrers = feature;
				}
				assert_eq!(
					store.collect_garbage(&options, &mut |_| {}).unwrap(),
					GcResults {
						paths: [path.to_absolute_path()].into(),
						bytes_freed: 99
					}
				);
				assert!(!store.is_valid_path(&path).unwrap());
				worker.join().unwrap();
			}
		}
	}

	#[test]
	fn rejects_specific_gc_without_the_negotiated_feature_before_sending_an_operation() {
		let (store, mut daemon) = connected(&[]);
		for action in [GcAction::DeleteDead, GcAction::DeleteSpecific] {
			let mut options = GcOptions::new(action);
			options.paths = Some(BTreeSet::new());
			options.delete_referrers = true;
			let error = store.collect_garbage(&options, &mut |_| {}).unwrap_err();
			assert!(error.to_string().contains("Your daemon version is too old"));
		}
		daemon.set_nonblocking(true).unwrap();
		assert_eq!(
			daemon.read(&mut [0u8; 1]).unwrap_err().kind(),
			io::ErrorKind::WouldBlock
		);
	}

	#[test]
	fn renders_derived_paths_as_cppnix_does() {
		let drv = StorePath::from_bytes(b"00000000000000000000000000000000-a.drv").unwrap();
		assert_eq!(
			legacy_string(&DerivedPath::Built {
				drv: drv.clone(),
				outputs: BTreeSet::new()
			}),
			"/nix/store/00000000000000000000000000000000-a.drv!*"
		);
		assert_eq!(
			legacy_string(&DerivedPath::Built {
				drv,
				outputs: ["out".to_owned(), "dev".to_owned()].into()
			}),
			"/nix/store/00000000000000000000000000000000-a.drv!dev,out"
		);
	}
}
