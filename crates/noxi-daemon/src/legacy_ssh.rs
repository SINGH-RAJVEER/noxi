//! `ssh://` stores, CppNix 2.35.2's `LegacySSHStore` and the client of the
//! serve protocol (`serve-protocol-connection.cc`). Only what building on a
//! machine needs.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;

use nix_compat::nixhash::{HashAlgo, NixHash};
use nix_compat::store_path::StorePath;
use noxi_store::build_derivation::BuildDerivation;
use noxi_store::{Result, ValidPathInfo};

use crate::error::Error;
use crate::nar_stream::copy_nar;
use crate::peer::{BuildResult, NarSource, Peer, read_outputs, write_basic_derivation};
use crate::ssh::{Process, SshMaster, close_write};
use crate::wire::{Reader, Version, Writer, parse_content_address, parse_store_path, version};

pub const SERVE_MAGIC_1: u64 = 0x390c9deb;
pub const SERVE_MAGIC_2: u64 = 0x5452eecb;
/// `ServeProto::latest`.
pub const SERVE_VERSION: Version = version(2, 8);

/// `ServeProto::Command`.
pub mod command {
	pub const QUERY_VALID_PATHS: u64 = 1;
	pub const QUERY_PATH_INFOS: u64 = 2;
	pub const DUMP_STORE_PATH: u64 = 3;
	pub const IMPORT_PATHS: u64 = 4;
	pub const BUILD_PATHS: u64 = 6;
	pub const QUERY_CLOSURE: u64 = 7;
	pub const BUILD_DERIVATION: u64 = 8;
	pub const ADD_TO_STORE_NAR: u64 = 9;
}

/// `ServeProto::BuildOptions`.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuildOptions {
	pub max_silent_time: u64,
	pub build_timeout: u64,
	pub max_log_size: u64,
	pub keep_failed: bool,
}

struct Connection {
	from: Reader<BufReader<UnixStream>>,
	to: Writer<UnixStream>,
	version: Version,
	_process: Process,
}

/// A store served by `nix-store --serve --write` over `ssh`.
pub struct LegacySshStore {
	master: SshMaster,
	command: Vec<OsString>,
	host: String,
	options: BuildOptions,
	conn: RefCell<Option<Connection>>,
}

fn store_error(error: Error) -> noxi_store::Error {
	match error {
		Error::Store(error) => error,
		error => noxi_store::Error::Other(error.to_string()),
	}
}

/// Keeps what is read, for the message of a failed greeting.
struct Tee<'a> {
	from: &'a mut BufReader<UnixStream>,
	saved: Vec<u8>,
}

impl Read for Tee<'_> {
	fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		let n = self.from.read(buf)?;
		self.saved.extend(&buf[..n]);
		Ok(n)
	}
}

impl LegacySshStore {
	/// A store served by `nix-store --serve --write` on the host of
	/// `master`, which builds with `options`.
	pub fn new(master: SshMaster, options: BuildOptions) -> Self {
		let config = master.config();
		let command = config.remote_command(true);
		let host = config.authority.host.clone();
		Self {
			master,
			command,
			host,
			options,
			conn: RefCell::new(None),
		}
	}

	fn open(&self) -> Result<Connection> {
		let (stream, process) = self.master.start_command(&self.command)?;
		let mut from = BufReader::new(stream.try_clone()?);
		let mut to = Writer::new(stream);
		let mut tee = Tee {
			from: &mut from,
			saved: Vec::new(),
		};
		let version = match handshake(&mut to, &mut Reader::new(&mut tee), &self.host) {
			Ok(version) => version,
			Err(Error::EndOfFile | Error::Io(_)) => {
				return Err(noxi_store::Error::Other(format!(
					"cannot connect to '{}'",
					self.host
				)));
			}
			Err(error) => return Err(store_error(error)),
		};
		Ok(Connection {
			from: Reader::new(from),
			to,
			version,
			_process: process,
		})
	}

	/// Run `f` on the connection. Any failure closes it: what follows on
	/// the stream is unknown.
	fn with_conn<T>(
		&self,
		f: impl FnOnce(&mut Connection) -> crate::error::Result<T>,
	) -> Result<T> {
		let mut slot = self.conn.borrow_mut();
		if slot.is_none() {
			*slot = Some(self.open()?);
		}
		let result = f(slot.as_mut().expect("connection was opened"));
		if result.is_err() {
			*slot = None;
		}
		result.map_err(store_error)
	}
}

/// `BasicClientConnection::handshake`.
fn handshake(
	to: &mut Writer<UnixStream>,
	from: &mut Reader<&mut Tee<'_>>,
	host: &str,
) -> crate::error::Result<Version> {
	to.u64(SERVE_MAGIC_1).u64(SERVE_VERSION);
	to.flush()?;
	// `readInt`: a larger number is a `SerialisationError`.
	let magic = from.read_u64()?;
	if magic > u64::from(u32::MAX) {
		close_write(to.inner_mut());
		let tee = from.get_mut();
		let _ = std::io::copy(*tee, &mut std::io::sink());
		return Err(Error::Nix(format!(
			"'nix-store --serve' protocol mismatch from '{host}', got '{}'",
			String::from_utf8_lossy(tee.saved.trim_ascii_end())
		)));
	}
	if magic != SERVE_MAGIC_2 {
		return Err(Error::Nix(format!(
			"'nix-store --serve' protocol mismatch from '{host}'"
		)));
	}
	let remote = crate::wire::from_wire(from.read_int()?);
	if remote >> 8 != 2 || remote < version(2, 5) {
		return Err(Error::Nix(format!(
			"unsupported 'nix-store --serve' protocol version on '{host}'"
		)));
	}
	Ok(remote.min(SERVE_VERSION))
}

fn write_build_options(conn: &mut Connection, options: &BuildOptions) {
	conn.to
		.u64(options.max_silent_time)
		.u64(options.build_timeout)
		.u64(options.max_log_size)
		.u64(0)
		.u64(0);
	if conn.version >= version(2, 7) {
		conn.to.bool(options.keep_failed);
	}
}

/// `ServeProto::Serialise<BuildResult>::read`.
fn read_build_result(conn: &mut Connection) -> crate::error::Result<BuildResult> {
	let status = conn.from.read_num(u64::from(u8::MAX), "h")?;
	if status > 14 {
		return Err(Error::Nix(format!(
			"Invalid BuildResult status {status} from remote"
		)));
	}
	let mut result = BuildResult {
		status,
		message: conn.from.read_string()?,
		times_built: conn.from.read_u64()?,
		non_deterministic: conn.from.read_bool()?,
		start: conn.from.read_u64()?,
		stop: conn.from.read_u64()?,
		..BuildResult::default()
	};
	if conn.version >= version(2, 8) {
		result.outputs = read_outputs(&mut conn.from)?;
	} else if conn.version >= version(2, 6) {
		for _ in 0..conn.from.read_u64()? {
			let id = conn.from.read_string()?;
			let realisation = conn.from.read_string()?;
			let (_, name) = id
				.split_once('!')
				.ok_or_else(|| Error::Nix(format!("Invalid derivation output id {id}")))?;
			let path = serde_json::from_str::<serde_json::Value>(&realisation)
				.ok()
				.and_then(|json| StorePath::from_bytes(json["outPath"].as_str()?.as_bytes()).ok())
				.ok_or_else(|| Error::Nix(format!("invalid realisation '{realisation}'")))?;
			result.outputs.insert(name.to_owned(), path);
		}
	}
	Ok(result)
}

/// `ServeProto::Serialise<UnkeyedValidPathInfo>::read`.
fn read_path_info(conn: &mut Connection, path: StorePath) -> crate::error::Result<ValidPathInfo> {
	let deriver = conn.from.read_string()?;
	let references = conn.from.read_store_paths()?;
	conn.from.read_u64()?; // download size
	let nar_size = conn.from.read_u64()?;
	let hash = conn.from.read_string()?;
	let ca = conn.from.read_string()?;
	let sigs = conn.from.read_signatures()?;
	if hash.is_empty() {
		return Err(Error::Nix("NAR hash is now mandatory".into()));
	}
	let Ok(NixHash::Sha256(digest)) = NixHash::from_str(&hash, None) else {
		return Err(Error::Nix(format!("invalid NAR hash '{hash}'")));
	};
	let mut info = ValidPathInfo::new(path, digest);
	info.deriver = (!deriver.is_empty())
		.then(|| parse_store_path(&deriver))
		.transpose()?;
	info.references = references;
	info.nar_size = nar_size;
	info.ca = (!ca.is_empty())
		.then(|| parse_content_address(&ca))
		.transpose()?;
	info.sigs = sigs.into_iter().collect();
	Ok(info)
}

impl Peer for LegacySshStore {
	fn connect(&self) -> Result<()> {
		self.with_conn(|_| Ok(()))
	}

	/// The protocol does not say.
	fn trusted(&self) -> Result<Option<bool>> {
		Ok(None)
	}

	fn valid_paths(
		&self,
		paths: &BTreeSet<StorePath>,
		substitute: bool,
	) -> Result<BTreeSet<StorePath>> {
		self.with_conn(|conn| {
			conn.to
				.u64(command::QUERY_VALID_PATHS)
				.bool(false)
				.bool(substitute)
				.store_paths(paths.iter());
			conn.to.flush()?;
			conn.from.read_store_paths()
		})
	}

	fn path_info(&self, path: &StorePath) -> Result<Option<ValidPathInfo>> {
		self.with_conn(|conn| {
			conn.to
				.u64(command::QUERY_PATH_INFOS)
				.store_paths(std::iter::once(path));
			conn.to.flush()?;
			let mut info = None;
			loop {
				let answer = conn.from.read_string()?;
				if answer.is_empty() {
					return Ok(info);
				}
				// Information about another path must not be taken for this
				// one's.
				if info.is_some() || parse_store_path(&answer)? != *path {
					return Err(Error::Nix("More path infos returned than queried".into()));
				}
				info = Some(read_path_info(conn, path.clone())?);
			}
		})
	}

	fn nar(&self, path: &StorePath, sink: &mut dyn Write) -> Result<()> {
		self.with_conn(|conn| {
			conn.to.u64(command::DUMP_STORE_PATH).store_path(path);
			conn.to.flush()?;
			copy_nar(conn.from.get_mut(), sink)
		})
	}

	fn add_paths(&self, infos: &[ValidPathInfo], nar: &mut NarSource<'_>) -> Result<()> {
		for info in infos {
			self.with_conn(|conn| {
				let hash: String = info.nar_hash.iter().map(|b| format!("{b:02x}")).collect();
				conn.to
					.u64(command::ADD_TO_STORE_NAR)
					.store_path(&info.path)
					.opt_store_path(info.deriver.as_ref())
					.string(&hash)
					.store_paths(info.references.iter())
					.u64(info.registration_time as u64)
					.u64(info.nar_size)
					.bool(false)
					.strings(info.sigs.iter().map(String::as_str))
					.string(
						&info
							.ca
							.as_ref()
							.map(ToString::to_string)
							.unwrap_or_default(),
					);
				conn.to.flush()?;
				nar(&info.path, conn.to.inner_mut()).map_err(Error::Store)?;
				conn.to.flush()?;
				if conn.from.read_int()? != 1 {
					return Err(Error::Nix(format!(
						"failed to add path '{}' to remote host '{}'",
						info.path.to_absolute_path(),
						self.host
					)));
				}
				Ok(())
			})?;
		}
		Ok(())
	}

	fn build_derivation(&self, drv_path: &StorePath, drv: &BuildDerivation) -> Result<BuildResult> {
		self.with_conn(|conn| {
			conn.to.u64(command::BUILD_DERIVATION).store_path(drv_path);
			write_basic_derivation(&mut conn.to, drv, drv.basic.input_sources());
			write_build_options(conn, &self.options);
			conn.to.flush()?;
			read_build_result(conn)
		})
	}

	/// Never asked of a store that cannot say whether it trusts its client.
	fn build_registered(&self, _drv_path: &StorePath) -> Result<BuildResult> {
		Err(noxi_store::Error::Other(
			"the legacy ssh protocol cannot build a derivation by its path".to_owned(),
		))
	}
}

/// The algorithm of a NAR hash sent without one.
pub(crate) fn parse_nar_hash(hash: &str) -> crate::error::Result<[u8; 32]> {
	match NixHash::from_str(hash, Some(HashAlgo::Sha256)) {
		Ok(NixHash::Sha256(digest)) => Ok(digest),
		_ => Err(Error::Nix(format!("invalid NAR hash '{hash}'"))),
	}
}

fn unsupported<T>(operation: &str) -> Result<T> {
	Err(noxi_store::Error::Other(format!(
		"legacy SSH store does not support {operation}"
	)))
}

/// What commands such as `nix copy` need of an `ssh://` store.
impl noxi_store::Store for LegacySshStore {
	fn real_path(&self, path: &StorePath) -> std::path::PathBuf {
		std::path::PathBuf::from(path.to_absolute_path())
	}

	fn is_valid_path(&self, path: &StorePath) -> Result<bool> {
		Ok(self
			.valid_paths(&BTreeSet::from([path.clone()]), false)?
			.contains(path))
	}

	fn query_path_info(&self, path: &StorePath) -> Result<Option<ValidPathInfo>> {
		self.path_info(path)
	}

	fn query_valid_paths(&self, paths: &BTreeSet<StorePath>) -> Result<BTreeSet<StorePath>> {
		self.valid_paths(paths, false)
	}

	fn query_valid_paths_with_substitute(
		&self,
		paths: &BTreeSet<StorePath>,
		substitute: bool,
	) -> Result<BTreeSet<StorePath>> {
		self.valid_paths(paths, substitute)
	}

	fn query_referrers(&self, _: &StorePath) -> Result<BTreeSet<StorePath>> {
		unsupported("querying referrers")
	}

	fn query_valid_derivers(&self, _: &StorePath) -> Result<BTreeSet<StorePath>> {
		unsupported("querying valid derivers")
	}

	fn query_derivation_outputs(
		&self,
		_: &StorePath,
	) -> Result<std::collections::BTreeMap<String, StorePath>> {
		unsupported("querying derivation outputs")
	}

	fn read_derivation(
		&self,
		_: &StorePath,
	) -> Result<nix_compat::derivation::UnverifiedDerivation> {
		unsupported("reading derivations")
	}

	fn add_temp_root(&self, _: &StorePath) -> Result<()> {
		Ok(())
	}

	fn add_perm_root(&self, _: &StorePath, _: &std::path::Path) -> Result<std::path::PathBuf> {
		unsupported("creating roots")
	}

	fn build_paths(&self, _: &[noxi_store::DerivedPath]) -> Result<()> {
		unsupported("building paths")
	}

	fn nar_from_path(&self, path: &StorePath, sink: &mut dyn Write) -> Result<()> {
		self.nar(path, sink)
	}

	fn add_text_to_store(&self, _: &str, _: &[u8], _: &BTreeSet<StorePath>) -> Result<StorePath> {
		unsupported("adding text paths")
	}

	fn add_to_store_from_dump(
		&self,
		source: &mut (dyn Read + Send),
		name: &str,
		dump_method: noxi_store::FileIngestionMethod,
		hash_method: noxi_store::FileIngestionMethod,
		hash_algo: HashAlgo,
		references: &BTreeSet<StorePath>,
	) -> Result<StorePath> {
		use std::io::Seek;
		if dump_method != noxi_store::FileIngestionMethod::NixArchive || hash_method != dump_method
		{
			return unsupported("adding paths other than recursive NARs");
		}
		let mut nar = tempfile::tempfile()?;
		let mut hash = noxi_store::nar::HashingReader::new(BufReader::new(source));
		copy_nar(&mut hash, &mut nar).map_err(store_error)?;
		let (digest, size) = hash.finish();
		nar.rewind()?;
		let ca_hash = if hash_algo == HashAlgo::Sha256 {
			NixHash::Sha256(digest)
		} else {
			let mut hasher = nix_compat::nixhash::NixHashDigester::new(hash_algo);
			let mut chunk = [0; 65536];
			loop {
				let size = nar.read(&mut chunk)?;
				if size == 0 {
					break;
				}
				hasher.update(&chunk[..size]);
			}
			nar.rewind()?;
			hasher.finalize()
		};
		let path = StorePath::from_name_and_digest_fixed(name, [0; 20])
			.map_err(|error| noxi_store::Error::Other(error.to_string()))?;
		let mut info = ValidPathInfo::new(path, digest);
		info.nar_size = size;
		info.references = references.clone();
		info.ca = Some(hash_method.content_address(ca_hash)?);
		info.path = info.ca_path().ok_or_else(|| {
			noxi_store::Error::Other(format!(
				"cannot compute a content-addressed path for '{name}'"
			))
		})?;
		self.add_paths(std::slice::from_ref(&info), &mut |_, sink| {
			std::io::copy(&mut nar, sink).map(drop).map_err(Into::into)
		})?;
		Ok(info.path)
	}

	fn add_multiple_to_store(
		&self,
		infos: &[ValidPathInfo],
		nar: &mut dyn FnMut(&StorePath, &mut dyn Write) -> Result<()>,
		_repair: bool,
		_check_sigs: bool,
	) -> Result<()> {
		self.add_paths(infos, nar)
	}
}
