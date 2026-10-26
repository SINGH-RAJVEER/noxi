//! One client connection, following CppNix 2.35.2's `processConnection`,
//! `TunnelLogger` and `performOp` in `daemon.cc`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufReader, Read, Write};

use nix_compat::log::VerbosityLevel;
use nix_compat::nixbase32;
use nix_compat::nixhash::{HashAlgo, NixHash};
use nix_compat::store_path::StorePath;
use nix_rs_build::local::{BuildEvent, LocalBuilder};
use nix_rs_build::missing::{DerivedPath, query_missing};
use nix_rs_store::FileIngestionMethod;
use nix_rs_store::gc::{GcAction, GcOptions};
use nix_rs_store::{LocalStore, ValidPathInfo};

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

/// `TunnelLogger`: messages are queued until an operation starts work,
/// then sent as they happen.
pub struct Logger {
	can_send: bool,
	pending: Vec<Vec<u8>>,
	pub verbosity: u64,
	version: Version,
	next_activity: u64,
}

/// A field of an activity or result, `Logger::Field`.
pub enum Field<'a> {
	Int(u64),
	Str(&'a str),
}

fn write_fields<W: Write>(out: &mut Writer<W>, fields: &[Field<'_>]) {
	out.u64(fields.len() as u64);
	for field in fields {
		match field {
			Field::Int(n) => {
				out.u64(0).u64(*n);
			}
			Field::Str(s) => {
				out.u64(1).string(s);
			}
		}
	}
}

/// `ActivityType` and `ResultType` values.
pub const ACT_UNKNOWN: u64 = 0;
pub const ACT_BUILD: u64 = 105;
pub const RES_BUILD_LOG_LINE: u64 = 101;

impl Logger {
	fn new(version: Version) -> Self {
		Self {
			can_send: false,
			pending: Vec::new(),
			verbosity: 3,
			version,
			next_activity: 0,
		}
	}

	fn enqueue<W: Write>(&mut self, out: &mut Writer<W>, message: Vec<u8>) -> Result<()> {
		if self.can_send {
			out.raw(&message);
			out.flush()
		} else {
			self.pending.push(message);
			Ok(())
		}
	}

	/// `Logger::log`: a line at level `level`.
	pub fn log<W: Write>(&mut self, out: &mut Writer<W>, level: u64, line: &str) -> Result<()> {
		if level > self.verbosity {
			return Ok(());
		}
		let mut message = Writer::new(std::io::sink());
		message.u64(STDERR_NEXT).string(&format!("{line}\n"));
		self.enqueue(out, message.take())
	}

	/// `warn`.
	pub fn warn<W: Write>(&mut self, out: &mut Writer<W>, message: &str) -> Result<()> {
		self.log(
			out,
			LVL_WARN,
			&format!("\x1b[35;1mwarning:\x1b[0m {message}"),
		)
	}

	/// `startActivity`; returns the activity's ID.
	pub fn start_activity<W: Write>(
		&mut self,
		out: &mut Writer<W>,
		level: u64,
		kind: u64,
		text: &str,
		fields: &[Field<'_>],
	) -> Result<u64> {
		if self.version < version(1, 20) {
			if !text.is_empty() {
				self.log(out, level, &format!("{text}..."))?;
			}
			return Ok(0);
		}
		self.next_activity += 1;
		let id = (u64::from(std::process::id()) << 32) + self.next_activity;
		let mut message = Writer::new(std::io::sink());
		message
			.u64(STDERR_START_ACTIVITY)
			.u64(id)
			.u64(level)
			.u64(kind)
			.string(text);
		write_fields(&mut message, fields);
		message.u64(0);
		self.enqueue(out, message.take())?;
		Ok(id)
	}

	/// `stopActivity`.
	pub fn stop_activity<W: Write>(&mut self, out: &mut Writer<W>, id: u64) -> Result<()> {
		if self.version < version(1, 20) {
			return Ok(());
		}
		let mut message = Writer::new(std::io::sink());
		message.u64(STDERR_STOP_ACTIVITY).u64(id);
		self.enqueue(out, message.take())
	}

	/// `result`.
	pub fn result<W: Write>(
		&mut self,
		out: &mut Writer<W>,
		id: u64,
		kind: u64,
		fields: &[Field<'_>],
	) -> Result<()> {
		if self.version < version(1, 20) {
			return Ok(());
		}
		let mut message = Writer::new(std::io::sink());
		message.u64(STDERR_RESULT).u64(id).u64(kind);
		write_fields(&mut message, fields);
		self.enqueue(out, message.take())
	}

	fn build_event<W: Write>(
		&mut self,
		to: &mut Writer<W>,
		event: BuildEvent<'_>,
		mode: u64,
		built: &mut bool,
		buffers: &mut BTreeMap<StorePath, (u64, LineBuffer)>,
	) -> Result<()> {
		match event {
			BuildEvent::PathChecked { path, valid } => {
				self.log(
					to,
					LVL_INFO,
					&format!("checking path '{}'...", path.to_absolute_path()),
				)?;
				if !valid {
					self.log(
						to,
						LVL_WARN,
						&format!(
							"path '{}' is corrupted or missing!",
							path.to_absolute_path()
						),
					)?;
				}
			}
			BuildEvent::Started { drv_path } => {
				*built = true;
				let path = drv_path.to_absolute_path();
				let id = self.start_activity(
					to,
					LVL_INFO,
					ACT_BUILD,
					&if mode == 1 {
						format!("repairing outputs of '{path}'")
					} else {
						format!("building '{path}'")
					},
					&[
						Field::Str(&path),
						Field::Str(""),
						Field::Int(1),
						Field::Int(1),
					],
				)?;
				buffers.insert(drv_path.clone(), (id, LineBuffer::default()));
			}
			BuildEvent::Output(data) => {
				if let Some((id, buffer)) = buffers.values_mut().last() {
					for line in buffer.feed(data) {
						self.result(
							to,
							*id,
							RES_BUILD_LOG_LINE,
							&[Field::Str(&String::from_utf8_lossy(&line))],
						)?;
					}
				}
			}
			BuildEvent::Finished { drv_path, .. } => {
				if let Some((id, mut buffer)) = buffers.remove(drv_path) {
					if let Some(line) = buffer.flush() {
						self.result(
							to,
							id,
							RES_BUILD_LOG_LINE,
							&[Field::Str(&String::from_utf8_lossy(&line))],
						)?;
					}
					self.stop_activity(to, id)?;
				}
			}
		}
		Ok(())
	}

	pub fn start_work<W: Write>(&mut self, out: &mut Writer<W>) -> Result<()> {
		self.can_send = true;
		for message in std::mem::take(&mut self.pending) {
			out.raw(&message);
		}
		out.flush()
	}

	/// `stopWork`: `STDERR_LAST`, or the error.
	pub fn stop_work<W: Write>(&mut self, out: &mut Writer<W>, error: Option<&str>) {
		self.can_send = false;
		match error {
			None => {
				out.u64(STDERR_LAST);
			}
			Some(message) if self.version >= version(1, 26) => {
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
	pub to: Writer<W>,
	pub logger: Logger,
	pub version: Version,
	pub features: BTreeSet<String>,
	pub trust: Trust,
	pub store: &'a LocalStore,
	pub settings: DaemonSettings,
	queried_substitutes: BTreeSet<(String, StorePath)>,
}

/// `BasicServerConnection::handshake` and `postHandshake`, then the
/// operation loop.
pub fn process_connection<R: Read + Send, W: Write + Send>(
	read: R,
	write: W,
	store: &LocalStore,
	settings: &DaemonSettings,
	trust: Trust,
) -> Result<()> {
	let mut from = Reader::new(read);
	let mut to = Writer::new(write);
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
		logger: Logger::new(protocol),
		version: protocol,
		features,
		trust,
		store,
		settings: settings.clone(),
		queried_substitutes: BTreeSet::new(),
	};

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
			Err(Error::EndOfFile) => return Ok(()),
			Err(e @ Error::Io(_)) => return Err(e),
			Err(e) => {
				// If the error happened before the operation could send
				// replies, the input is probably not fully read; report the
				// error and give up on the connection, as CppNix does.
				let error_allowed = conn.logger.can_send;
				conn.logger.stop_work(&mut conn.to, Some(&message_of(&e)));
				conn.to.flush()?;
				if !error_allowed {
					conn.logger.stop_work(&mut conn.to, Some(&message_of(&e)));
					conn.to.flush()?;
					return Err(e);
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

impl<R: Read + Send, W: Write + Send> Connection<'_, R, W> {
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
					let sigs = self.from.read_strings()?.into_iter().collect();
					self.store.register_drv_output(
						&drv,
						&output,
						&nix_rs_store::Realisation {
							out_path: path,
							signatures: sigs,
						},
					)?;
					self.stop();
				} else {
					let info = self.store.query_realisation(&drv, &output)?;
					self.stop();
					self.to.bool(info.is_some());
					if let Some(nix_rs_store::Realisation {
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
				source.drain()?;
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
						nix_rs_store::gc::render_size(stats.bytes_freed as i64, false),
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
						for cache in &self.settings.substituters {
							if op == op::QUERY_SUBSTITUTABLE_PATHS && !cache.info().want_mass_query
							{
								continue;
							}
							let activity = if self
								.queried_substitutes
								.insert((cache.uri().into(), path.clone()))
							{
								let text = path.to_absolute_path();
								Some(self.logger.start_activity(
									&mut self.to,
									LVL_TALKATIVE,
									109,
									&format!("querying info about '{text}' on '{}'", cache.uri()),
									&[Field::Str(&text), Field::Str(cache.uri())],
								)?)
							} else {
								None
							};
							let info = cache.query_path_info(&path);
							if let Some(id) = activity {
								self.logger.stop_activity(&mut self.to, id)?;
							}
							if let Some(info) = info? {
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
				let substitute = self.version >= version(1, 27) && self.from.read_bool()?;
				self.start()?;
				if substitute
					&& self.settings.build.use_substitutes
					&& !self.settings.substituters.is_empty()
				{
					for path in &paths {
						if !self.store.is_valid_path(path)? {
							let _ = self.store.substitute(&self.settings.substituters, path);
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
					self.store.substitute(&self.settings.substituters, &path)?;
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
								.map_err(|e| nix_rs_build::BuildError::Other(e.to_string()))
						})
						.map_err(|e| Error::Nix(e.to_string()))?,
					);
				}
				self.start()?;
				let act = self.logger.start_activity(
					&mut self.to,
					LVL_DEBUG,
					ACT_UNKNOWN,
					"querying info about missing paths",
					&[],
				)?;
				let missing = query_missing(
					self.store,
					&self.settings.substituters,
					self.settings.build.use_substitutes,
					self.settings.build.always_allow_substitutes,
					&targets,
				);
				self.logger.stop_activity(&mut self.to, act)?;
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
				if repair {
					return Err(Error::Nix(
						"nix-rs does not support repairing paths yet".into(),
					));
				}
				let errors = self.with_log(|store, log| store.verify_store(check_contents, log))?;
				self.stop();
				self.to.bool(errors);
			}
			op::ADD_SIGNATURES => {
				let path = self.from.read_store_path()?;
				let sigs: BTreeSet<String> = self.from.read_strings()?.into_iter().collect();
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
		f: impl FnOnce(&LocalStore, &mut dyn FnMut(&str)) -> nix_rs_store::Result<T>,
	) -> Result<T> {
		self.with_log(|store, log| f(store, &mut |line| log(VerbosityLevel::Info, line)))
	}

	/// Run `f` on the store, sending the lines it logs to the client at
	/// their levels as they happen.
	fn with_log<T>(
		&mut self,
		f: impl FnOnce(&LocalStore, &mut dyn FnMut(VerbosityLevel, &str)) -> nix_rs_store::Result<T>,
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
					.map_err(|e| nix_rs_build::BuildError::Other(e.to_string()))
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
			source.drain()?;
			result
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
		framed.drain()?;
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
		framed.drain()?;
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
		let mode = self.from.read_u64()?;
		self.start()?;
		if !drv.outputs().is_fixed() && self.trust != Trust::Trusted {
			return Err(Error::Nix(
				"you are not privileged to build input-addressed derivations".into(),
			));
		}
		if mode > 1 {
			return Err(Error::Nix(
				"nix-rs does not support checking builds yet".into(),
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
		let builder = LocalBuilder::new(store, &self.settings.build).with_repair(mode == 1);
		let start = now();
		let mut built = false;
		let mut io_error = None;
		let mut buffers: BTreeMap<StorePath, (u64, LineBuffer)> = BTreeMap::new();
		let logger = &mut self.logger;
		let to = &mut self.to;
		let mut events = |event: BuildEvent<'_>| {
			if let Err(e) = logger.build_event(to, event, mode, &mut built, &mut buffers) {
				io_error.get_or_insert(e);
			}
		};
		let result = builder.build_derivation(&path, &drv, &mut events);
		if let Some(e) = io_error {
			return Err(e);
		}
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
		let mode = self.from.read_u64()?;
		if mode == 1 && self.trust != Trust::Trusted {
			return Err(Error::Nix(
				"repairing is not allowed because you are not in 'trusted-users'".into(),
			));
		}
		if mode > 1 {
			return Err(Error::Nix(
				"nix-rs does not support repairing or checking builds yet".into(),
			));
		}
		self.start()?;
		let store = self.store;
		let settings = &self.settings;
		let builder = LocalBuilder::new(store, &settings.build)
			.with_substituters(&settings.substituters)
			.with_repair(mode == 1);
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
							.substitute(&settings.substituters, path)
							.map(|()| BuildOutcome::Substituted(BTreeMap::new()))
							.map_err(|e| (e.to_string(), 14))
					}
				}
				DerivedPath::Built { drv_path, outputs } => {
					let mut built = false;
					let mut io_error = None;
					let mut buffers: BTreeMap<StorePath, (u64, LineBuffer)> = BTreeMap::new();
					let logger = &mut self.logger;
					let to = &mut self.to;
					let mut events = |event: BuildEvent<'_>| {
						if let Err(e) =
							logger.build_event(to, event, mode, &mut built, &mut buffers)
						{
							io_error.get_or_insert(e);
						}
					};
					let result =
						builder.realise_with_events(drv_path, outputs.as_ref(), &mut events);
					if let Some(e) = io_error {
						return Err(e);
					}
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
		let keep_failed = self.from.read_bool()?;
		let keep_going = self.from.read_bool()?;
		let try_fallback = self.from.read_bool()?;
		let verbosity = self.from.read_u64()?;
		let max_build_jobs = self.from.read_u64()?;
		let max_silent_time = self.from.read_u64()?;
		self.from.read_u64()?; // obsolete useBuildHook
		let _verbose_build = self.from.read_u64()?;
		self.from.read_u64()?; // obsolete logType
		self.from.read_u64()?; // obsolete printBuildTrace
		let build_cores = self.from.read_u64()?;
		let use_substitutes = self.from.read_bool()?;
		let n = self.from.read_u64()?;
		let mut overrides = BTreeMap::new();
		for _ in 0..n {
			let name = self.from.read_string()?;
			let value = self.from.read_string()?;
			overrides.insert(name, value);
		}
		self.start()?;
		self.logger.verbosity = verbosity;
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
			let unrestricted = matches!(
				name.as_str(),
				"build-timeout"
					| "timeout"
					| "max-silent-time"
					| "build-max-silent-time"
					| "build-poll-interval"
					| "connect-timeout"
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
				.map(nix_rs_store::binary_cache::BinaryCache::open)
				.collect::<nix_rs_store::Result<Vec<_>>>()?;
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
	info.sigs = from.read_strings()?.into_iter().collect();
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
fn failure_code(e: &nix_rs_build::BuildError) -> u64 {
	use nix_rs_build::FailureStatus::*;
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

/// Splits builder output into log lines as CppNix's `BuildLog` does: a
/// carriage return restarts the line.
#[derive(Default)]
pub struct LineBuffer {
	line: Vec<u8>,
	pos: usize,
}

impl LineBuffer {
	pub fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
		let mut lines = Vec::new();
		for &c in data {
			match c {
				b'\r' => self.pos = 0,
				b'\n' => {
					self.line.truncate(self.pos);
					lines.push(std::mem::take(&mut self.line));
					self.pos = 0;
				}
				_ => {
					if self.pos >= self.line.len() {
						self.line.resize(self.pos + 1, 0);
					}
					self.line[self.pos] = c;
					self.pos += 1;
				}
			}
		}
		lines
	}

	pub fn flush(&mut self) -> Option<Vec<u8>> {
		if self.line.is_empty() {
			return None;
		}
		self.line.truncate(self.pos);
		self.pos = 0;
		Some(std::mem::take(&mut self.line))
	}
}
