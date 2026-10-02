//! One client connection, following CppNix 2.35.2's `processConnection`,
//! `TunnelLogger` and `performOp` in `daemon.cc`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};

use nix_compat::nixbase32;
use nix_compat::store_path::StorePath;
use nix_rs_build::missing::{DerivedPath, query_missing};
use nix_rs_store::{LocalStore, ValidPathInfo};

use crate::error::{Error, Result};
use crate::settings::DaemonSettings;
use crate::wire::{Reader, Version, Writer, version};

pub const WORKER_MAGIC_1: u64 = 0x6e697863;
pub const WORKER_MAGIC_2: u64 = 0x6478696f;
/// The newest protocol version, `WorkerProto::latest` in 2.35.2.
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

/// Worker operations, `WorkerProto::Op`.
pub mod op {
	pub const IS_VALID_PATH: u64 = 1;
	pub const QUERY_REFERRERS: u64 = 6;
	pub const ENSURE_PATH: u64 = 10;
	pub const ADD_TEMP_ROOT: u64 = 11;
	pub const SYNC_WITH_GC: u64 = 13;
	pub const FIND_ROOTS: u64 = 14;
	pub const QUERY_DERIVER: u64 = 18;
	pub const SET_OPTIONS: u64 = 19;
	pub const QUERY_DERIVATION_OUTPUTS: u64 = 22;
	pub const QUERY_ALL_VALID_PATHS: u64 = 23;
	pub const QUERY_PATH_INFO: u64 = 26;
	pub const QUERY_DERIVATION_OUTPUT_NAMES: u64 = 28;
	pub const QUERY_PATH_FROM_HASH_PART: u64 = 29;
	pub const QUERY_VALID_PATHS: u64 = 31;
	pub const QUERY_VALID_DERIVERS: u64 = 33;
	pub const NAR_FROM_PATH: u64 = 38;
	pub const QUERY_MISSING: u64 = 40;
	pub const QUERY_DERIVATION_OUTPUT_MAP: u64 = 41;
}

/// `TunnelLogger`: messages are queued until an operation starts work,
/// then sent as they happen.
pub struct Logger {
	can_send: bool,
	pending: Vec<Vec<u8>>,
	pub verbosity: u64,
	version: Version,
}

impl Logger {
	fn new(version: Version) -> Self {
		Self {
			can_send: false,
			pending: Vec::new(),
			verbosity: 3,
			version,
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
				out.u64(STDERR_ERROR).string(message).u64(1);
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
	pub settings: &'a DaemonSettings,
}

/// `BasicServerConnection::handshake` and `postHandshake`, then the
/// operation loop.
pub fn process_connection<R: Read, W: Write>(
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
	let client_version = from.read_u64()?;
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
		settings,
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
		match conn.perform_op(op) {
			Ok(()) => {}
			Err(Error::EndOfFile) => return Ok(()),
			Err(e @ (Error::Io(_) | Error::Protocol(_))) => return Err(e),
			Err(e) => {
				// If the error happened before the operation could send
				// replies, the input is probably not fully read; report the
				// error and give up on the connection, as CppNix does.
				let error_allowed = conn.logger.can_send;
				conn.logger.stop_work(&mut conn.to, Some(&message_of(&e)));
				conn.to.flush()?;
				if !error_allowed {
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

impl<R: Read, W: Write> Connection<'_, R, W> {
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
				if substitute && !self.settings.substituters.is_empty() {
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
				let drv = self.store.read_derivation(&path)?;
				let outputs: BTreeMap<String, StorePath> = drv
					.outputs()
					.iter()
					.map(|(name, path)| (name.to_string(), path.clone()))
					.collect();
				self.stop();
				self.to.u64(outputs.len() as u64);
				for (name, path) in &outputs {
					self.to.string(name).store_path(path);
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
				let mut lines = Vec::new();
				let roots = self
					.store
					.find_roots(self.trust == Trust::NotTrusted, &mut |line| {
						lines.push(line.to_owned())
					})?;
				for line in lines {
					self.logger.log(&mut self.to, LVL_INFO, &line)?;
				}
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
				let missing = query_missing(
					self.store,
					&self.settings.substituters,
					true,
					self.settings.build.always_allow_substitutes,
					&targets,
				)?;
				self.stop();
				self.to
					.store_paths(missing.will_build.iter())
					.store_paths(missing.will_substitute.iter())
					.store_paths(missing.unknown.iter())
					.u64(missing.download_size)
					.u64(missing.nar_size);
			}
			op => return Err(Error::Nix(format!("invalid operation {op}"))),
		}
		Ok(())
	}

	/// `queryPathInfo`, which fails for an invalid path.
	fn query_path_info(&self, path: &StorePath) -> Result<ValidPathInfo> {
		self.store
			.query_path_info(path)?
			.ok_or_else(|| Error::Nix(format!("path '{}' is not valid", path.to_absolute_path())))
	}

	/// `WorkerProto::Op::SetOptions` and `ClientSettings::apply`.
	fn set_options(&mut self) -> Result<()> {
		let _keep_failed = self.from.read_u64()?;
		let _keep_going = self.from.read_u64()?;
		let _try_fallback = self.from.read_u64()?;
		let verbosity = self.from.read_u64()?;
		let _max_build_jobs = self.from.read_u64()?;
		let _max_silent_time = self.from.read_u64()?;
		self.from.read_u64()?; // obsolete useBuildHook
		let _verbose_build = self.from.read_u64()?;
		self.from.read_u64()?; // obsolete logType
		self.from.read_u64()?; // obsolete printBuildTrace
		let _build_cores = self.from.read_u64()?;
		let _use_substitutes = self.from.read_u64()?;
		let n = self.from.read_u64()?;
		let mut overrides = BTreeMap::new();
		for _ in 0..n {
			let name = self.from.read_string()?;
			let value = self.from.read_string()?;
			overrides.insert(name, value);
		}
		self.start()?;
		self.logger.verbosity = verbosity;
		for (name, value) in &overrides {
			let unrestricted = matches!(
				name.as_str(),
				"build-timeout"
					| "timeout" | "max-silent-time"
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
				tracing::debug!("client setting '{name}' = '{value}'");
			} else if matches!(name.as_str(), "substituters" | "binary-caches") {
				for substituter in value.split_whitespace() {
					if !self.settings.trusted_substituter(substituter) {
						self.logger.warn(
							&mut self.to,
							&format!(
								"ignoring untrusted substituter '{substituter}', you are not a trusted user.\nRun `man nix.conf` for more information on the `substituters` configuration option."
							),
						)?;
					}
				}
			} else {
				self.logger.warn(
					&mut self.to,
					&format!(
						"ignoring the client-specified setting '{name}', because it is a restricted setting and you are not a trusted user"
					),
				)?;
			}
		}
		self.stop();
		Ok(())
	}
}

/// The hash part of a store path, as `QueryPathFromHashPart` receives it.
pub fn hash_part(path: &StorePath) -> String {
	nixbase32::encode(path.digest())
}
