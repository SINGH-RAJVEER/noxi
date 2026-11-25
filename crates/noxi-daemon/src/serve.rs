//! The legacy serve protocol used by ssh:// stores.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufReader, Read, Seek, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use nix_compat::derivation::UnverifiedDerivation;
use nix_compat::nixhash::{HashAlgo, NixHash};
use nix_compat::store_path::StorePath;
use noxi_store::{FileIngestionMethod, Store, ValidPathInfo};

use crate::client::{Worker, store_error, write_path_info};
use crate::connection::{copy_nar, read_nar_to, read_path_info};
use crate::wire::{Reader, Writer, parse_store_path};
use crate::{Error, Result};

const SERVE_MAGIC_1: u64 = 0x390c9deb;
const SERVE_MAGIC_2: u64 = 0x5452eecb;
const VERSION: u64 = 0x208;
const QUERY_VALID_PATHS: u64 = 1;
const QUERY_PATH_INFOS: u64 = 2;
const DUMP_STORE_PATH: u64 = 3;
const QUERY_CLOSURE: u64 = 7;
const ADD_TO_STORE_NAR: u64 = 9;

/// A single stdio connection to a legacy SSH store.
pub struct ServeStore {
	program: PathBuf,
	args: Vec<String>,
	conn: RefCell<Option<Connection>>,
}

struct Connection {
	from: Reader<BufReader<UnixStream>>,
	to: Writer<UnixStream>,
	_worker: Worker,
}

impl ServeStore {
	pub fn new(program: PathBuf, args: Vec<String>) -> Self {
		Self {
			program,
			args,
			conn: RefCell::new(None),
		}
	}

	fn with_conn<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> noxi_store::Result<T> {
		let mut slot = self.conn.borrow_mut();
		if slot.is_none() {
			let (stream, worker) = Worker::start(&self.program, &self.args)?;
			let mut conn = Connection {
				from: Reader::new(BufReader::new(stream.try_clone()?)),
				to: Writer::new(stream),
				_worker: worker,
			};
			let handshake = (|| {
				conn.to.u64(SERVE_MAGIC_1).u64(VERSION).flush()?;
				if conn.from.read_int()? != SERVE_MAGIC_2 {
					return Err(Error::Nix("'nix-store --serve' protocol mismatch".into()));
				}
				let version = conn.from.read_int()?;
				if !(0x205..0x300).contains(&version) {
					return Err(Error::Nix(
						"unsupported 'nix-store --serve' protocol version".into(),
					));
				}
				Ok(())
			})();
			handshake.map_err(store_error)?;
			*slot = Some(conn);
		}
		let result = f(slot.as_mut().expect("connection was opened"));
		if result.is_err() {
			*slot = None;
		}
		result.map_err(store_error)
	}

	pub fn connect(&self) -> noxi_store::Result<()> {
		self.with_conn(|_| Ok(()))
	}

	fn add_nar(
		&self,
		info: &ValidPathInfo,
		write: impl FnOnce(&mut dyn Write) -> noxi_store::Result<()>,
	) -> noxi_store::Result<()> {
		self.with_conn(|conn| {
			conn.to.u64(ADD_TO_STORE_NAR).store_path(&info.path);
			write_path_info(&mut conn.to, info);
			conn.to.flush()?;
			write(conn.to.inner_mut())?;
			if conn.from.read_int()? != 1 {
				return Err(Error::Nix(format!(
					"failed to add path '{}'",
					info.path.to_absolute_path()
				)));
			}
			Ok(())
		})
	}
}

fn unsupported<T>(operation: &str) -> noxi_store::Result<T> {
	Err(noxi_store::Error::Other(format!(
		"legacy SSH store does not support {operation}"
	)))
}

impl Store for ServeStore {
	fn real_path(&self, path: &StorePath) -> PathBuf {
		PathBuf::from(path.to_absolute_path())
	}

	fn is_valid_path(&self, path: &StorePath) -> noxi_store::Result<bool> {
		Ok(self
			.query_valid_paths(&BTreeSet::from([path.clone()]))?
			.contains(path))
	}

	fn query_path_info(&self, path: &StorePath) -> noxi_store::Result<Option<ValidPathInfo>> {
		self.with_conn(|conn| {
			conn.to
				.u64(QUERY_PATH_INFOS)
				.store_paths(std::iter::once(path))
				.flush()?;
			let actual = conn.from.read_string()?;
			if actual.is_empty() {
				return Ok(None);
			}
			if parse_store_path(&actual)? != *path {
				return Err(Error::Protocol("unexpected path in serve reply".into()));
			}
			let deriver = conn.from.read_string()?;
			let references = conn.from.read_store_paths()?;
			conn.from.read_u64()?; // download size
			let nar_size = conn.from.read_u64()?;
			let hash = conn.from.read_string()?;
			let Ok(NixHash::Sha256(hash)) = NixHash::from_str(&hash, Some(HashAlgo::Sha256)) else {
				return Err(Error::Protocol(format!("invalid NAR hash '{hash}'")));
			};
			let ca = conn.from.read_string()?;
			let mut info = ValidPathInfo::new(path.clone(), hash);
			info.deriver = (!deriver.is_empty())
				.then(|| parse_store_path(&deriver))
				.transpose()?;
			info.references = references;
			info.nar_size = nar_size;
			info.ca = (!ca.is_empty())
				.then(|| crate::wire::parse_content_address(&ca))
				.transpose()?;
			info.sigs = conn.from.read_signatures()?.into_iter().collect();
			if !conn.from.read_string()?.is_empty() {
				return Err(Error::Protocol(
					"more path infos returned than queried".into(),
				));
			}
			Ok(Some(info))
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
			conn.to
				.u64(QUERY_VALID_PATHS)
				.bool(false)
				.bool(substitute)
				.store_paths(paths.iter())
				.flush()?;
			conn.from.read_store_paths()
		})
	}

	fn query_referrers(&self, _: &StorePath) -> noxi_store::Result<BTreeSet<StorePath>> {
		unsupported("querying referrers")
	}

	fn query_valid_derivers(&self, _: &StorePath) -> noxi_store::Result<BTreeSet<StorePath>> {
		unsupported("querying valid derivers")
	}

	fn query_derivation_outputs(
		&self,
		_: &StorePath,
	) -> noxi_store::Result<BTreeMap<String, StorePath>> {
		unsupported("querying derivation outputs")
	}

	fn read_derivation(&self, _: &StorePath) -> noxi_store::Result<UnverifiedDerivation> {
		unsupported("reading derivations")
	}

	fn add_temp_root(&self, _: &StorePath) -> noxi_store::Result<()> {
		Ok(())
	}

	fn add_perm_root(&self, _: &StorePath, _: &Path) -> noxi_store::Result<PathBuf> {
		unsupported("creating roots")
	}

	fn build_paths(&self, _: &[noxi_store::DerivedPath]) -> noxi_store::Result<()> {
		unsupported("building paths")
	}

	fn nar_from_path(&self, path: &StorePath, sink: &mut dyn Write) -> noxi_store::Result<()> {
		self.with_conn(|conn| {
			conn.to.u64(DUMP_STORE_PATH).store_path(path).flush()?;
			copy_nar(conn.from.get_mut(), sink)
		})
	}

	fn add_text_to_store(
		&self,
		_: &str,
		_: &[u8],
		_: &BTreeSet<StorePath>,
	) -> noxi_store::Result<StorePath> {
		unsupported("adding text paths")
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
		if dump_method != FileIngestionMethod::NixArchive || hash_method != dump_method {
			return unsupported("adding paths other than recursive NARs");
		}
		let mut nar = tempfile::tempfile()?;
		let mut hash = noxi_store::nar::HashingReader::new(BufReader::new(source));
		read_nar_to(&mut hash, &mut nar).map_err(store_error)?;
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
		self.add_nar(&info, |sink| {
			std::io::copy(&mut nar, sink).map(drop).map_err(Into::into)
		})?;
		Ok(info.path)
	}

	fn add_multiple_to_store(
		&self,
		infos: &[ValidPathInfo],
		nar: &mut dyn FnMut(&StorePath, &mut dyn Write) -> noxi_store::Result<()>,
		_repair: bool,
		_check_sigs: bool,
	) -> noxi_store::Result<()> {
		for info in infos {
			self.add_nar(info, |sink| nar(&info.path, sink))?;
		}
		Ok(())
	}
}

/// Serve the operations used by legacy SSH copying and relay.
pub fn run(
	store: &dyn Store,
	input: impl Read + Send,
	output: impl Write,
	write: bool,
) -> Result<()> {
	let mut from = Reader::new(BufReader::new(input));
	let mut to = Writer::new(output);
	if from.read_int()? != SERVE_MAGIC_1 {
		return Err(Error::Protocol("protocol mismatch".into()));
	}
	to.u64(SERVE_MAGIC_2).u64(VERSION).flush()?;
	let version = from.read_int()?.min(VERSION);
	loop {
		match from.read_int() {
			Err(Error::EndOfFile) => return Ok(()),
			Err(error) => return Err(error),
			Ok(QUERY_VALID_PATHS) => {
				let lock = from.read_bool()?;
				let substitute = from.read_bool()?;
				let paths = from.read_store_paths()?;
				if lock && write {
					for path in &paths {
						store.add_temp_root(path)?;
					}
				}
				let valid = store.query_valid_paths_with_substitute(&paths, write && substitute)?;
				to.store_paths(valid.iter()).flush()?;
			}
			Ok(QUERY_PATH_INFOS) => {
				let paths = from.read_store_paths()?;
				for path in &paths {
					if let Some(info) = store.query_path_info(path)? {
						to.store_path(path)
							.opt_store_path(info.deriver.as_ref())
							.store_paths(info.references.iter())
							.u64(info.nar_size)
							.u64(info.nar_size);
						if version >= 0x204 {
							to.string(&info.nar_hash_nix32())
								.string(
									&info
										.ca
										.as_ref()
										.map(ToString::to_string)
										.unwrap_or_default(),
								)
								.strings(info.sigs.iter().map(String::as_str));
						}
					}
				}
				to.string("").flush()?;
			}
			Ok(DUMP_STORE_PATH) => {
				let path = from.read_store_path()?;
				if !store.is_valid_path(&path)? {
					return Err(noxi_store::Error::InvalidPath(path.to_absolute_path()).into());
				}
				store.nar_from_path(&path, to.inner_mut())?;
				to.flush()?;
			}
			Ok(QUERY_CLOSURE) => {
				let include_outputs = from.read_bool()?;
				let paths = from.read_store_paths()?;
				if include_outputs {
					return Err(Error::Protocol(
						"serve closure with outputs is unsupported".into(),
					));
				}
				let closure = store.compute_closure(&paths)?;
				to.store_paths(closure.iter()).flush()?;
			}
			Ok(ADD_TO_STORE_NAR) if write => {
				let path = from.read_store_path()?;
				let mut info = read_path_info(&mut from, path)?;
				info.ultimate = false;
				// The reference receives a NAR even if the path already exists.
				if store.is_valid_path(&info.path)? {
					copy_nar(from.get_mut(), &mut std::io::sink())?;
				} else {
					store.add_multiple_to_store(
						&[info],
						&mut |_, sink| copy_nar(from.get_mut(), sink).map_err(store_error),
						false,
						false,
					)?;
				}
				to.u64(1).flush()?;
			}
			Ok(command) => {
				return Err(Error::Protocol(format!(
					"unsupported serve command {command}"
				)));
			}
		}
	}
}
