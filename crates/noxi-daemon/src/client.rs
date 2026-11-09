//! A store reached through a Nix daemon's socket, following CppNix
//! 2.35.2's `UDSRemoteStore` and `RemoteStore` clients
//! (`remote-store.cc`, `worker-protocol-connection.cc`).
//!
//! Daemon log messages are printed as CppNix's simple logger prints them
//! for the legacy commands: activities at or below the verbosity with
//! `...` appended, and build log lines as they are.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use nix_compat::derivation::UnverifiedDerivation;
use nix_compat::log::VerbosityLevel;
use nix_compat::nixhash::HashAlgo;
use nix_compat::store_path::{STORE_DIR, StorePath};
use noxi_store::gc::{GcAction, GcOptions, GcResults, Roots, make_root_symlink};
use noxi_store::{DerivedPath, FileIngestionMethod, Missing, Store, ValidPathInfo};

use crate::connection::{
	MINIMUM_PROTOCOL_VERSION, PROTOCOL_VERSION, STDERR_ERROR, STDERR_LAST, STDERR_NEXT,
	STDERR_READ, STDERR_RESULT, STDERR_START_ACTIVITY, STDERR_STOP_ACTIVITY, STDERR_WRITE,
	WORKER_MAGIC_1, WORKER_MAGIC_2, op, read_path_info,
};
use crate::error::{Error, Result};
use crate::wire::{Reader, Version, Writer, version};

/// `resBuildLogLine` and `resPostBuildLogLine`.
const RES_BUILD_LOG_LINE: u64 = 101;
const RES_POST_BUILD_LOG_LINE: u64 = 107;

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

struct Connection {
	/// Buffered, so a reply costs a few reads rather than one per field.
	from: Reader<BufReader<UnixStream>>,
	to: Writer<UnixStream>,
	version: Version,
	features: BTreeSet<String>,
	verbosity: u64,
	print_build_logs: bool,
}

/// How many path infos a [`DaemonStore`] remembers.
const PATH_INFO_CACHE_SIZE: usize = 65536;

/// A store whose owner is a daemon.
pub struct DaemonStore {
	socket: PathBuf,
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
}

fn store_error(e: Error) -> noxi_store::Error {
	match e {
		Error::Store(e) => e,
		e => noxi_store::Error::Other(e.to_string()),
	}
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

/// The simple logger's `log`: one line on standard error, with escape
/// sequences filtered.
fn log_line(line: &[u8]) {
	let mut text = nix_compat::log::filter_ansi_escapes(line, !stderr_is_tty());
	text.push(b'\n');
	let _ = io::stderr().write_all(&text);
}

impl Connection {
	/// `processStderr`: print the daemon's log messages until it finishes
	/// the operation, returning its error if it failed.
	fn process_stderr(&mut self) -> Result<()> {
		self.to.flush()?;
		loop {
			let message = self.from.read_u64()?;
			match message {
				STDERR_NEXT => log_line(chomp(&self.from.read_bytes()?)),
				STDERR_START_ACTIVITY => {
					let _id = self.from.read_u64()?;
					let level = self.from.read_u64()?;
					let _kind = self.from.read_u64()?;
					let mut text = self.from.read_bytes()?;
					self.read_fields()?;
					let _parent = self.from.read_u64()?;
					if level <= self.verbosity && !text.is_empty() {
						text.extend_from_slice(b"...");
						log_line(&text);
					}
				}
				STDERR_STOP_ACTIVITY => {
					self.from.read_u64()?;
				}
				STDERR_RESULT => {
					let _id = self.from.read_u64()?;
					let kind = self.from.read_u64()?;
					let fields = self.read_fields()?;
					let line = fields.into_iter().next().unwrap_or_default();
					match kind {
						RES_BUILD_LOG_LINE if self.print_build_logs => log_line(&line),
						RES_POST_BUILD_LOG_LINE if self.print_build_logs => {
							log_line(&[b"post-build-hook: ".as_slice(), &line].concat());
						}
						_ => {}
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

	/// `readFields`: the string fields, with integers rendered.
	fn read_fields(&mut self) -> Result<Vec<Vec<u8>>> {
		let count = self.from.read_u64()?;
		let mut fields = Vec::new();
		for _ in 0..count {
			match self.from.read_u64()? {
				0 => fields.push(self.from.read_u64()?.to_string().into_bytes()),
				1 => fields.push(self.from.read_bytes()?),
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
		let uri = if root.is_none() {
			"daemon".to_owned()
		} else {
			format!("unix://{}", socket.display())
		};
		let real_store_dir = match root {
			Some(root) => root.join(STORE_DIR.trim_start_matches('/')),
			None => PathBuf::from(STORE_DIR),
		};
		Self {
			socket: socket.to_owned(),
			uri,
			options: options.clone(),
			conn: RefCell::new(None),
			path_infos: RefCell::default(),
			real_store_dir,
		}
	}

	/// `UDSRemoteStore::openConnection` and `initConnection`.
	fn open(&self) -> noxi_store::Result<Connection> {
		let stream = noxi_store::unix_socket::connect(&self.socket).map_err(|e| {
			noxi_store::Error::Other(format!(
				"cannot connect to socket at '{}': {}",
				self.socket.display(),
				io_message(&e)
			))
		})?;
		let read = stream.try_clone().map_err(|e| store_error(e.into()))?;
		let mut conn = Connection {
			from: Reader::new(BufReader::new(read)),
			to: Writer::new(stream),
			version: PROTOCOL_VERSION,
			features: BTreeSet::new(),
			verbosity: self.options.verbosity,
			print_build_logs: self.options.verbose_build,
		};
		let features = handshake(&mut conn).map_err(|e| {
			noxi_store::Error::Other(format!(
				"cannot open connection to remote store '{}': {e}",
				self.uri
			))
		})?;
		if !features.contains("disable-set-options") {
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
				.bool(false);
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
fn write_path_info<W: Write>(to: &mut Writer<W>, info: &ValidPathInfo) {
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
		conn.from.read_string()?; // the daemon's Nix version
	}
	if conn.version >= version(1, 35) {
		conn.from.read_u64()?; // whether we are trusted
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
		};
		self.add_to_store(source, name, &method, references)
	}

	fn add_temp_root(&self, path: &StorePath) -> noxi_store::Result<()> {
		self.path_op(op::ADD_TEMP_ROOT, path, |conn| {
			conn.from.read_u64().map(drop)
		})
	}

	/// `IndirectRootStore::addPermRoot`: the link is made here, and the
	/// daemon is told about it.
	fn add_perm_root(&self, path: &StorePath, gc_root: &Path) -> noxi_store::Result<PathBuf> {
		if gc_root.starts_with(STORE_DIR) {
			return Err(noxi_store::Error::Other(format!(
				"creating a garbage collector root (\"{}\") in the Nix store is forbidden (are you running nix-build inside the store?)",
				gc_root.display()
			)));
		}
		self.add_temp_root(path)?;
		if let Ok(metadata) = std::fs::symlink_metadata(gc_root) {
			let points_into_store = metadata.file_type().is_symlink()
				&& std::fs::read_link(gc_root).is_ok_and(|t| t.starts_with(STORE_DIR));
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

	fn query_valid_paths(
		&self,
		paths: &BTreeSet<StorePath>,
	) -> noxi_store::Result<BTreeSet<StorePath>> {
		self.with_conn(|conn| {
			conn.to.u64(op::QUERY_VALID_PATHS).store_paths(paths.iter());
			if conn.version >= version(1, 27) {
				conn.to.bool(false);
			}
			conn.process_stderr()?;
			conn.from.read_store_paths()
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
		self.with_conn(|conn| {
			let targets: Vec<String> = paths.iter().map(legacy_string).collect();
			conn.to
				.u64(op::BUILD_PATHS)
				.strings(targets.iter().map(String::as_str))
				.u64(0);
			conn.process_stderr()?;
			conn.from.read_u64().map(drop)
		})
	}

	fn query_missing(&self, paths: &[DerivedPath]) -> noxi_store::Result<Missing> {
		self.with_conn(|conn| {
			let targets: Vec<String> = paths.iter().map(legacy_string).collect();
			conn.to
				.u64(op::QUERY_MISSING)
				.strings(targets.iter().map(String::as_str));
			conn.process_stderr()?;
			Ok(Missing {
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
			features: features
				.iter()
				.map(|feature| (*feature).to_owned())
				.collect(),
			verbosity: 3,
			print_build_logs: true,
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
