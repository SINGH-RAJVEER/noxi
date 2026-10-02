//! The local store, following CppNix's `LocalStore`.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, BufRead, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use nix_compat::derivation::{DrvHashModulo, UnverifiedDerivation, lookup_fn};
use nix_compat::log::VerbosityLevel;
use nix_compat::narinfo::VerifyingKey;
use nix_compat::nixbase32;
use nix_compat::nixhash::{CAHash, HashAlgo, NixHash, NixHashDigester};
use nix_compat::store_path::{STORE_DIR, StorePath, StorePathRef};
use rusqlite::{Connection, OptionalExtension, params};

use crate::canonicalise::canonicalise_path_metadata;
use crate::config::LocalStoreConfig;
use crate::db::{self, DbSettings, OpenMode, SCHEMA_VERSION};
use crate::error::{Error, IoContext, Result};
use crate::lock::{LockType, PathLocks, lock_file, open_lock_file};
use crate::nar::{self, HashingReader};
use crate::path_info::ValidPathInfo;

/// How file system data is serialised or hashed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileIngestionMethod {
	/// A single regular file's contents.
	Flat,
	/// A NAR serialisation.
	NixArchive,
	/// The contents of a text file, which may reference other store paths
	/// (`text:` content addresses). Only valid as a hash method.
	Text,
}

pub struct LocalStore {
	config: LocalStoreConfig,
	db: Mutex<Connection>,
	/// Held in shared mode while the store is open, so that no schema
	/// upgrade happens underneath.
	_global_lock: Option<File>,
	temp_roots: Mutex<Option<Arc<TempRootsFile>>>,
	gc_lock: Mutex<Option<File>>,
	roots_socket: Mutex<Option<UnixStream>>,
	public_keys: Vec<VerifyingKey>,
}

/// Each store's open `temproots/<pid>` file, by path.
static TEMP_ROOTS_FILES: Mutex<BTreeMap<PathBuf, Weak<TempRootsFile>>> =
	Mutex::new(BTreeMap::new());

/// A `temproots/<pid>` file, locked while open. It is closed and removed
/// when the last store handle using it is dropped, as CppNix's
/// `~LocalStore` does.
struct TempRootsFile {
	path: PathBuf,
	file: Mutex<File>,
}

impl TempRootsFile {
	/// Whether `path` still names this file.
	fn is_at(&self, path: &Path) -> bool {
		let file = self.file.lock().unwrap_or_else(|e| e.into_inner());
		match (fs::metadata(path), file.metadata()) {
			(Ok(named), Ok(open)) => (named.dev(), named.ino()) == (open.dev(), open.ino()),
			_ => false,
		}
	}
}

impl Drop for TempRootsFile {
	fn drop(&mut self) {
		// Holding the map keeps another handle from creating the file
		// again between the check and the removal.
		let mut files = TEMP_ROOTS_FILES.lock().unwrap_or_else(|e| e.into_inner());
		if self.is_at(&self.path) {
			let _ = fs::remove_file(&self.path);
			files.remove(&self.path);
		}
	}
}

impl std::fmt::Debug for LocalStore {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("LocalStore")
			.field("config", &self.config)
			.finish_non_exhaustive()
	}
}

fn create_dirs(path: &Path) -> Result<()> {
	fs::create_dir_all(path).context(|| format!("creating directory '{}'", path.display()))
}

fn now() -> i64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_secs() as i64)
		.unwrap_or(0)
}

fn print(path: &StorePath) -> String {
	path.to_absolute_path()
}

fn parse_store_path(s: &str) -> Result<StorePath> {
	StorePath::from_absolute_path(s.as_bytes())
		.map_err(|e| Error::Other(format!("invalid store path '{s}': {e}")))
}

impl LocalStore {
	/// Open the store, creating it if it does not exist and upgrading its
	/// schema if needed.
	pub fn open(config: LocalStoreConfig) -> Result<Self> {
		let db_dir = config.db_dir();
		let links_dir = config.real_store_dir.join(".links");
		let profiles_dir = config.state_dir.join("profiles");
		let gc_roots_dir = config.state_dir.join("gcroots");
		let temp_roots_dir = config.state_dir.join("temproots");

		create_dirs(&config.real_store_dir)?;
		if !config.read_only {
			make_store_writable(&config.real_store_dir)?;
		}
		create_dirs(&links_dir)?;
		create_dirs(&profiles_dir)?;
		create_dirs(&temp_roots_dir)?;
		create_dirs(&db_dir)?;
		create_dirs(&gc_roots_dir)?;
		for per_user in [profiles_dir.join("per-user"), gc_roots_dir.join("per-user")] {
			create_dirs(&per_user)?;
			if !config.read_only {
				let mode = fs::metadata(&per_user)?.permissions().mode() & 0o777;
				if mode != 0o755 {
					fs::set_permissions(&per_user, fs::Permissions::from_mode(0o755))
						.context(|| format!("changing permissions of '{}'", per_user.display()))?;
				}
			}
		}

		check_not_symlinked(&config.real_store_dir)?;
		reserve_space(&db_dir.join("reserved"), config.reserved_size);

		let global_lock = if config.read_only {
			None
		} else {
			let path = db_dir.join("big-lock");
			let file = open_lock_file(&path, true)
				.context(|| format!("opening lock file '{}'", path.display()))?
				.expect("created lock file");
			if !lock_file(&file, LockType::Read, false)? {
				tracing::info!("waiting for the big Nix store lock...");
				lock_file(&file, LockType::Read, true)?;
			}
			Some(file)
		};

		let schema_path = db_dir.join("schema");
		let settings = DbSettings {
			use_wal: config.use_sqlite_wal,
			fsync_metadata: config.fsync_metadata,
		};
		let db_path = db_dir.join("db.sqlite");
		let mut current = read_schema(&schema_path)?;

		if config.read_only && current < SCHEMA_VERSION {
			return Err(Error::Other(if current == 0 {
				"database does not exist, and cannot be created in read-only mode".to_owned()
			} else {
				"database schema needs migrating, but this cannot be done in read-only mode"
					.to_owned()
			}));
		}
		if current > SCHEMA_VERSION {
			return Err(Error::Other(format!(
				"current Nix store schema is version {current}, but I only support {SCHEMA_VERSION}"
			)));
		}

		check_db_dir_access(&db_dir, config.read_only)?;
		let acquire_write = |file: &File| -> Result<()> {
			if !lock_file(file, LockType::Write, false)? {
				tracing::info!("waiting for exclusive access to the Nix store...");
				lock_file(file, LockType::None, false)?;
				lock_file(file, LockType::Write, true)?;
			}
			Ok(())
		};

		let conn = if current == 0 {
			let conn = db::open(&db_path, OpenMode::Create, &settings)?;
			write_schema(&schema_path, SCHEMA_VERSION)?;
			conn
		} else if current < SCHEMA_VERSION {
			if current < 5 {
				return Err(Error::Other(
					"Your Nix store has a database in Berkeley DB format,\nwhich is no longer supported. To convert to the new format,\nplease upgrade Nix to version 0.12 first.".to_owned(),
				));
			}
			if current < 6 {
				return Err(Error::Other(
					"Your Nix store has a database in flat file format,\nwhich is no longer supported. To convert to the new format,\nplease upgrade Nix to version 1.11 first.".to_owned(),
				));
			}
			let lock = global_lock.as_ref().expect("writable store holds the lock");
			acquire_write(lock)?;
			current = read_schema(&schema_path)?;
			let conn = db::open(&db_path, OpenMode::NoCreate, &settings)?;
			if current < 8 {
				let tx = conn.unchecked_transaction()?;
				tx.execute_batch(
					"alter table ValidPaths add column ultimate integer; alter table ValidPaths add column sigs text;",
				)?;
				tx.commit()?;
			}
			if current < 9 {
				let tx = conn.unchecked_transaction()?;
				tx.execute_batch("drop table FailedPaths")?;
				tx.commit()?;
			}
			if current < 10 {
				let tx = conn.unchecked_transaction()?;
				tx.execute_batch("alter table ValidPaths add column ca text")?;
				tx.commit()?;
			}
			write_schema(&schema_path, SCHEMA_VERSION)?;
			lock_file(lock, LockType::Read, true)?;
			conn
		} else {
			let mode = if config.read_only {
				OpenMode::Immutable
			} else {
				OpenMode::NoCreate
			};
			db::open(&db_path, mode, &settings)?
		};

		if !config.read_only && db::upgrade_schema(&conn, true)? {
			let lock = global_lock.as_ref().expect("writable store holds the lock");
			acquire_write(lock)?;
			db::upgrade_schema(&conn, false)?;
			lock_file(lock, LockType::Read, true)?;
		}

		let public_keys = config
			.trusted_public_keys
			.iter()
			.filter_map(|key| VerifyingKey::parse(key).ok())
			.collect();

		Ok(Self {
			config,
			db: Mutex::new(conn),
			_global_lock: global_lock,
			temp_roots: Mutex::new(None),
			gc_lock: Mutex::new(None),
			roots_socket: Mutex::new(None),
			public_keys,
		})
	}

	pub fn config(&self) -> &LocalStoreConfig {
		&self.config
	}

	/// Where `path`'s contents live on disk.
	pub fn real_path(&self, path: &StorePath) -> PathBuf {
		self.config.real_store_dir.join(format!(
			"{}-{}",
			nixbase32::encode(path.digest()),
			path.name()
		))
	}

	fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
		self.db.lock().unwrap_or_else(|e| e.into_inner())
	}

	// Queries

	pub fn is_valid_path(&self, path: &StorePath) -> Result<bool> {
		Ok(query_id(&self.db(), path)?.is_some())
	}

	pub fn query_path_info(&self, path: &StorePath) -> Result<Option<ValidPathInfo>> {
		query_path_info(&self.db(), path)
	}

	pub fn query_all_valid_paths(&self) -> Result<BTreeSet<StorePath>> {
		let db = self.db();
		let mut stmt = db.prepare_cached("select path from ValidPaths")?;
		let paths = stmt
			.query_map([], |row| row.get::<_, String>(0))?
			.collect::<Result<Vec<_>, _>>()?;
		paths.iter().map(|p| parse_store_path(p)).collect()
	}

	pub fn query_valid_paths(&self, paths: &BTreeSet<StorePath>) -> Result<BTreeSet<StorePath>> {
		let db = self.db();
		let mut valid = BTreeSet::new();
		for path in paths {
			if query_id(&db, path)?.is_some() {
				valid.insert(path.clone());
			}
		}
		Ok(valid)
	}

	pub fn query_referrers(&self, path: &StorePath) -> Result<BTreeSet<StorePath>> {
		query_referrers(&self.db(), path)
	}

	/// Valid derivations that list `path` as an output.
	pub fn query_valid_derivers(&self, path: &StorePath) -> Result<BTreeSet<StorePath>> {
		let db = self.db();
		let mut stmt = db.prepare_cached(
			"select v.id, v.path from DerivationOutputs d join ValidPaths v on d.drv = v.id where d.path = ?",
		)?;
		let paths = stmt
			.query_map([print(path)], |row| row.get::<_, String>(1))?
			.collect::<Result<Vec<_>, _>>()?;
		paths.iter().map(|p| parse_store_path(p)).collect()
	}

	/// The outputs a valid derivation was registered with.
	pub fn query_derivation_outputs(&self, drv: &StorePath) -> Result<BTreeMap<String, StorePath>> {
		let db = self.db();
		let id = query_id(&db, drv)?.ok_or_else(|| Error::InvalidPath(print(drv)))?;
		let mut stmt = db.prepare_cached("select id, path from DerivationOutputs where drv = ?")?;
		let rows = stmt
			.query_map([id], |row| {
				Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
			})?
			.collect::<Result<Vec<_>, _>>()?;
		rows.into_iter()
			.map(|(name, path)| Ok((name, parse_store_path(&path)?)))
			.collect()
	}

	/// The valid path whose digest is `hash_part`, in Nix base32.
	pub fn query_path_from_hash_part(&self, hash_part: &str) -> Result<Option<StorePath>> {
		if hash_part.len() != 32 {
			return Err(Error::Other("invalid hash part".to_owned()));
		}
		let prefix = format!("{STORE_DIR}/{hash_part}");
		let db = self.db();
		let found: Option<String> = db
			.prepare_cached("select path from ValidPaths where path >= ? limit 1")?
			.query_row([&prefix], |row| row.get(0))
			.optional()?;
		match found {
			Some(path) if path.starts_with(&prefix) => Ok(Some(parse_store_path(&path)?)),
			_ => Ok(None),
		}
	}

	// Registration

	pub fn register_valid_path(&self, info: &ValidPathInfo) -> Result<()> {
		self.register_valid_paths(std::slice::from_ref(info))
	}

	/// Register paths whose contents are already in place, in one
	/// transaction. Fails, registering nothing, if their references form a
	/// cycle.
	pub fn register_valid_paths(&self, infos: &[ValidPathInfo]) -> Result<()> {
		let mut db = self.db();
		let tx = db.transaction()?;
		for info in infos {
			if query_id(&tx, &info.path)?.is_some() {
				update_path_info(&tx, info)?;
			} else {
				self.add_valid_path(&tx, info)?;
			}
		}
		for info in infos {
			let referrer =
				query_id(&tx, &info.path)?.ok_or_else(|| Error::InvalidPath(print(&info.path)))?;
			for reference in &info.references {
				let reference_id = query_id(&tx, reference)?
					.ok_or_else(|| Error::InvalidPath(print(reference)))?;
				tx.prepare_cached(
					"insert or replace into Refs (referrer, reference) values (?, ?)",
				)?
				.execute(params![referrer, reference_id])?;
			}
		}

		let by_path: HashMap<&StorePath, &ValidPathInfo> =
			infos.iter().map(|i| (&i.path, i)).collect();
		if let Some((path, parent)) = find_cycle(&by_path) {
			return Err(Error::Other(format!(
				"cycle detected in the references of '{}' from '{}'",
				print(path),
				print(parent)
			)));
		}
		tx.commit()?;
		Ok(())
	}

	fn add_valid_path(&self, db: &Connection, info: &ValidPathInfo) -> Result<i64> {
		if info.ca.is_some() && !info.is_content_addressed() {
			return Err(Error::Other(format!(
				"cannot add path '{}' to the Nix store because it claims to be content-addressed but isn't",
				print(&info.path)
			)));
		}
		db.prepare_cached(
			"insert into ValidPaths (path, hash, registrationTime, deriver, narSize, ultimate, sigs, ca) values (?, ?, ?, ?, ?, ?, ?, ?)",
		)?
		.execute(params![
			print(&info.path),
			info.nar_hash_base16(),
			if info.registration_time == 0 { now() } else { info.registration_time },
			info.deriver.as_ref().map(print),
			(info.nar_size != 0).then_some(info.nar_size as i64),
			info.ultimate.then_some(1),
			(!info.sigs.is_empty()).then(|| info.sigs.iter().cloned().collect::<Vec<_>>().join(" ")),
			info.ca.as_ref().map(|ca| ca.to_string()),
		])?;
		let id = db.last_insert_rowid();

		if info.path.name().ends_with(".drv") {
			let outputs = self.check_derivation(&info.path)?;
			for (name, path) in outputs {
				db.prepare_cached(
					"insert or replace into DerivationOutputs (drv, id, path) values (?, ?, ?)",
				)?
				.execute(params![id, name, print(&path)])?;
			}
		}
		Ok(id)
	}

	/// Read the derivation at `drv_path` from disk, check that its output
	/// paths are the ones its inputs determine, and return the outputs with
	/// known paths.
	fn check_derivation(&self, drv_path: &StorePath) -> Result<Vec<(String, StorePath)>> {
		let drv = self.read_derivation(drv_path)?;
		let name = drv_path
			.name()
			.strip_suffix(".drv")
			.unwrap_or(drv_path.name());
		let memo = RefCell::new(HashMap::new());
		let lookup = lookup_fn(|path: &StorePathRef<'_>| {
			self.hash_modulo_of(&StorePath::from(path.clone()), &memo)
		});
		let outputs: Vec<(String, StorePath)> = drv
			.outputs()
			.iter()
			.map(|(name, path)| (name.to_string(), path.clone()))
			.collect();
		drv.verify(name, lookup).map_err(|e| {
			Error::Other(format!(
				"while checking derivation '{}': {e}",
				print(drv_path)
			))
		})?;
		Ok(outputs)
	}

	fn hash_modulo_of(
		&self,
		drv_path: &StorePath,
		memo: &RefCell<HashMap<StorePath, DrvHashModulo>>,
	) -> Option<DrvHashModulo> {
		if let Some(hash) = memo.borrow().get(drv_path) {
			return Some(hash.clone());
		}
		let drv = self.read_derivation(drv_path).ok()?;
		let lookup = lookup_fn(|path: &StorePathRef<'_>| {
			self.hash_modulo_of(&StorePath::from(path.clone()), memo)
		});
		let hash = drv.hash_derivation_modulo(lookup).ok()?;
		memo.borrow_mut().insert(drv_path.clone(), hash.clone());
		Some(hash)
	}

	/// Parse a derivation file from the store, valid or not.
	pub fn read_derivation(&self, drv_path: &StorePath) -> Result<UnverifiedDerivation> {
		let real = self.real_path(drv_path);
		let bytes =
			fs::read(&real).context(|| format!("reading derivation '{}'", print(drv_path)))?;
		UnverifiedDerivation::from_aterm_bytes(&bytes).map_err(|e| {
			Error::Other(format!(
				"error parsing derivation '{}': {e:?}",
				print(drv_path)
			))
		})
	}

	/// Add signatures to a valid path.
	pub fn add_signatures(&self, path: &StorePath, sigs: &BTreeSet<String>) -> Result<()> {
		let mut db = self.db();
		let tx = db.transaction()?;
		let mut info =
			query_path_info(&tx, path)?.ok_or_else(|| Error::InvalidPath(print(path)))?;
		info.sigs.extend(sigs.iter().cloned());
		update_path_info(&tx, &info)?;
		tx.commit()?;
		Ok(())
	}

	/// Whether `info` lacks the signatures `require-sigs` demands.
	pub fn path_info_is_untrusted(&self, info: &ValidPathInfo) -> bool {
		self.config.require_sigs && info.check_signatures(&self.public_keys) == 0
	}

	// Adding paths

	/// Import a path from its NAR serialisation, checking the NAR hash and
	/// size and any content address against `info`.
	pub fn add_to_store(
		&self,
		info: &ValidPathInfo,
		source: &mut (dyn BufRead + Send),
		repair: bool,
		check_sigs: bool,
	) -> Result<()> {
		if check_sigs && self.path_info_is_untrusted(info) {
			return Err(Error::Other(format!(
				"cannot add path '{}' because it lacks a signature by a trusted key",
				print(&info.path)
			)));
		}
		self.add_temp_root(&info.path)?;
		if !repair && self.is_valid_path(&info.path)? {
			// The NAR must still be consumed by the caller's protocol.
			io::copy(source, &mut io::sink())?;
			return Ok(());
		}

		let real = self.real_path(&info.path);
		let mut lock = PathLocks::lock([real.clone()])?;
		if repair || !self.is_valid_path(&info.path)? {
			delete_path(&real)?;
			let mut reader = HashingReader::new(source);
			nar::restore_path(&real, &mut reader)
				.context(|| format!("restoring '{}'", print(&info.path)))?;
			let (digest, size) = reader.finish();
			if digest != info.nar_hash {
				return Err(Error::Other(format!(
					"hash mismatch importing path '{}';\n  specified: {}\n  got:       {}",
					print(&info.path),
					NixHash::Sha256(info.nar_hash).to_sri_string(),
					NixHash::Sha256(digest).to_sri_string()
				)));
			}
			if size != info.nar_size {
				return Err(Error::Other(format!(
					"size mismatch importing path '{}';\n  specified: {}\n  got:       {}",
					print(&info.path),
					info.nar_size,
					size
				)));
			}
			if let Some(ca) = &info.ca {
				let actual = ca_hash_of(&real, ca)?;
				if &actual != ca {
					return Err(Error::Other(format!(
						"ca hash mismatch importing path '{}';\n  specified: {}\n  got:       {}",
						print(&info.path),
						nix32_with_algo(&ca.hash()),
						nix32_with_algo(&actual.hash())
					)));
				}
			}
			canonicalise_path_metadata(&real, None)?;
			self.register_valid_path(info)?;
		} else {
			io::copy(source, &mut io::sink())?;
		}
		lock.set_deletion(true);
		Ok(())
	}

	/// Add a content-addressed path from a serialisation in `dump_method`,
	/// addressed with `hash_method` and `hash_algo`. Returns its path.
	#[allow(clippy::too_many_arguments)]
	pub fn add_to_store_from_dump(
		&self,
		source: &mut (dyn Read + Send),
		name: &str,
		dump_method: FileIngestionMethod,
		hash_method: FileIngestionMethod,
		hash_algo: HashAlgo,
		references: &BTreeSet<StorePath>,
		repair: bool,
	) -> Result<StorePath> {
		let (temp_dir, _temp_lock) = self.create_temp_dir_in_store()?;
		let _cleanup = RemoveOnDrop(temp_dir.clone());
		let temp_path = temp_dir.join("x");
		match dump_method {
			FileIngestionMethod::NixArchive => {
				let mut reader = io::BufReader::new(source);
				nar::restore_path(&temp_path, &mut reader)?;
			}
			FileIngestionMethod::Flat | FileIngestionMethod::Text => {
				nar::restore_flat(&temp_path, source)?;
			}
		}

		let digest = match hash_method {
			FileIngestionMethod::Flat | FileIngestionMethod::Text => {
				let mut digester = NixHashDigester::new(hash_algo);
				let mut file = File::open(&temp_path)?;
				let mut buf = vec![0; 65536];
				loop {
					let n = file.read(&mut buf)?;
					if n == 0 {
						break;
					}
					digester.update(&buf[..n]);
				}
				digester.finalize()
			}
			FileIngestionMethod::NixArchive => {
				let mut writer = DigestWriter(NixHashDigester::new(hash_algo));
				nar::dump_path(&mut writer, &temp_path)?;
				writer.0.finalize()
			}
		};
		let ca = match hash_method {
			FileIngestionMethod::Text => match digest {
				NixHash::Sha256(d) => CAHash::Text(d),
				_ => return Err(Error::Other("text hashing requires SHA-256".to_owned())),
			},
			FileIngestionMethod::Flat => CAHash::Flat(digest),
			FileIngestionMethod::NixArchive => CAHash::Nar(digest),
		};

		let mut info = ValidPathInfo::new(
			StorePath::from_name_and_digest_fixed(name, [0; 20])
				.map_err(|e| Error::Other(format!("invalid store path name '{name}': {e}")))?,
			[0; 32],
		);
		info.references = references.clone();
		info.ca = Some(ca);
		let dst = info.ca_path().ok_or_else(|| {
			Error::Other(format!(
				"cannot compute a content-addressed path for '{name}'"
			))
		})?;
		info.path = dst.clone();

		self.add_temp_root(&dst)?;
		if repair || !self.is_valid_path(&dst)? {
			let real = self.real_path(&dst);
			let mut lock = PathLocks::lock([real.clone()])?;
			if repair || !self.is_valid_path(&dst)? {
				delete_path(&real)?;
				fs::rename(&temp_path, &real)
					.context(|| format!("moving into '{}'", real.display()))?;
				let (nar_hash, nar_size) = nar::hash_path(&real)?;
				canonicalise_path_metadata(&real, None)?;
				info.nar_hash = nar_hash;
				info.nar_size = nar_size;
				self.register_valid_path(&info)?;
			}
			lock.set_deletion(true);
		}
		Ok(dst)
	}

	/// Add `contents` as a `text:` content-addressed path.
	pub fn add_text_to_store(
		&self,
		name: &str,
		contents: &[u8],
		references: &BTreeSet<StorePath>,
		repair: bool,
	) -> Result<StorePath> {
		self.add_to_store_from_dump(
			&mut &contents[..],
			name,
			FileIngestionMethod::Flat,
			FileIngestionMethod::Text,
			HashAlgo::Sha256,
			references,
			repair,
		)
	}

	/// Write `path`'s NAR serialisation to `sink`.
	pub fn nar_from_path(&self, path: &StorePath, sink: &mut dyn Write) -> Result<()> {
		if !self.is_valid_path(path)? {
			return Err(Error::InvalidPath(print(path)));
		}
		let mut sink = sink;
		nar::dump_path(&mut sink, &self.real_path(path))?;
		Ok(())
	}

	/// A directory in the store that the garbage collector leaves alone
	/// while the returned file stays open, CppNix's `createTempDirInStore`.
	pub fn create_temp_dir_in_store(&self) -> Result<(PathBuf, File)> {
		static COUNTER: AtomicU64 = AtomicU64::new(0);
		loop {
			let dir = self.config.real_store_dir.join(format!(
				"tmp-{}-{}",
				std::process::id(),
				COUNTER.fetch_add(1, Ordering::Relaxed)
			));
			match fs::DirBuilder::new().mode(0o700).create(&dir) {
				Ok(()) => {}
				Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
				Err(e) => return Err(Error::io(format!("creating '{}'", dir.display()), e)),
			}
			let Ok(file) = File::open(&dir) else { continue };
			if lock_file(&file, LockType::Write, true)? && dir.exists() {
				return Ok((dir, file));
			}
		}
	}

	// Roots

	/// This process's temporary-roots file, `temproots/<pid>`, shared by
	/// every handle to the store. A handle that recreated the file would
	/// hide the roots other handles had written from the collector.
	fn shared_temp_roots(&self) -> Result<Arc<TempRootsFile>> {
		let path = self
			.config
			.state_dir
			.join("temproots")
			.join(std::process::id().to_string());
		let mut files = TEMP_ROOTS_FILES.lock().unwrap_or_else(|e| e.into_inner());
		let existing = files.get(&path).and_then(Weak::upgrade);
		// Unless the state directory was replaced since.
		if let Some(existing) = &existing
			&& existing.is_at(&path)
		{
			return Ok(Arc::clone(existing));
		}
		let file = loop {
			let _ = fs::remove_file(&path);
			let file = open_lock_file(&path, true)?.expect("created lock file");
			lock_file(&file, LockType::Write, true)?;
			// The collector deletes files it finds unlocked; retry if it
			// did so before we locked this one.
			if file.metadata()?.len() == 0 {
				break file;
			}
		};
		let file = Arc::new(TempRootsFile {
			path: path.clone(),
			file: Mutex::new(file),
		});
		files.insert(path, Arc::downgrade(&file));
		// Unlock the map first: dropping the last handle to a replaced file
		// locks it.
		drop(files);
		drop(existing);
		Ok(file)
	}

	pub(crate) fn open_gc_lock(&self) -> Result<File> {
		let path = self.config.state_dir.join("gc.lock");
		Ok(open_lock_file(&path, true)
			.context(|| format!("opening '{}'", path.display()))?
			.expect("created lock file"))
	}

	/// Protect `path` from garbage collection for the life of this process.
	pub fn add_temp_root(&self, path: &StorePath) -> Result<()> {
		if self.config.read_only {
			return Ok(());
		}
		let mut temp_roots = self.temp_roots.lock().unwrap_or_else(|e| e.into_inner());
		if temp_roots.is_none() {
			*temp_roots = Some(self.shared_temp_roots()?);
		}

		let mut gc_lock = self.gc_lock.lock().unwrap_or_else(|e| e.into_inner());
		if gc_lock.is_none() {
			*gc_lock = Some(self.open_gc_lock()?);
		}
		let gc_file = gc_lock.as_ref().expect("opened");
		loop {
			if lock_file(gc_file, LockType::Read, false)? {
				break;
			}
			// The collector is running: tell it about the root directly.
			if self.send_root_to_collector(path)? {
				break;
			}
			std::thread::sleep(std::time::Duration::from_millis(100));
		}

		let mut record = print(path).into_bytes();
		record.push(0);
		temp_roots
			.as_ref()
			.expect("created")
			.file
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.write_all(&record)?;
		lock_file(gc_file, LockType::None, false)?;
		Ok(())
	}

	fn send_root_to_collector(&self, path: &StorePath) -> Result<bool> {
		let mut socket = self.roots_socket.lock().unwrap_or_else(|e| e.into_inner());
		if socket.is_none() {
			let socket_path = self.config.state_dir.join("gc-socket/socket");
			match UnixStream::connect(&socket_path) {
				Ok(stream) => *socket = Some(stream),
				Err(e)
					if matches!(
						e.kind(),
						io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
					) =>
				{
					return Ok(false);
				}
				Err(e) => return Err(e.into()),
			}
		}
		let stream = socket.as_mut().expect("connected");
		let result = (|| -> io::Result<()> {
			stream.write_all(format!("{}\n", print(path)).as_bytes())?;
			let mut ack = [0u8; 1];
			stream.read_exact(&mut ack)?;
			Ok(())
		})();
		match result {
			Ok(()) => Ok(true),
			Err(_) => {
				*socket = None;
				Ok(false)
			}
		}
	}

	// Removal and verification

	/// Unregister `path`, failing if another valid path refers to it.
	pub fn invalidate_path_checked(&self, path: &StorePath) -> Result<()> {
		let mut db = self.db();
		let tx = db.transaction()?;
		if query_id(&tx, path)?.is_some() {
			let mut referrers = query_referrers(&tx, path)?;
			referrers.remove(path);
			if !referrers.is_empty() {
				return Err(Error::PathInUse {
					path: print(path),
					referrers: referrers
						.iter()
						.map(|p| format!("'{}'", print(p)))
						.collect::<Vec<_>>()
						.join(", "),
				});
			}
			tx.prepare_cached("delete from ValidPaths where path = ?")?
				.execute([print(path)])?;
		}
		tx.commit()?;
		Ok(())
	}

	/// `verifyStore`: check that valid paths exist (removing those that
	/// vanished without referrers) and, with `check_contents`, that hard
	/// links and paths have the hashes they should. Messages CppNix prints
	/// at the default verbosity go to `log` with their levels. Returns
	/// whether errors were found.
	pub fn verify_store(
		&self,
		check_contents: bool,
		log: &mut dyn FnMut(VerbosityLevel, &str),
	) -> Result<bool> {
		log(VerbosityLevel::Info, "reading the Nix store...");
		let gc_lock = self.open_gc_lock()?;
		if !lock_file(&gc_lock, LockType::Read, false)? {
			log(
				VerbosityLevel::Info,
				"waiting for the big garbage collector lock...",
			);
			lock_file(&gc_lock, LockType::Read, true)?;
		}

		let mut on_disk = BTreeSet::new();
		for entry in fs::read_dir(&self.config.real_store_dir)? {
			let name = entry?.file_name();
			if let Ok(path) = StorePath::from_bytes(name.as_encoded_bytes()) {
				on_disk.insert(path);
			}
		}

		log(VerbosityLevel::Info, "checking path existence...");
		let mut errors = false;
		let mut done = BTreeSet::new();
		let mut valid = BTreeSet::new();
		for path in self.query_all_valid_paths()? {
			self.verify_path(&path, &on_disk, &mut done, &mut valid, &mut errors, log)?;
		}

		if check_contents {
			log(VerbosityLevel::Info, "checking link hashes...");
			let links = self.config.real_store_dir.join(".links");
			for entry in fs::read_dir(&links)? {
				let entry = entry?;
				let name = entry.file_name().to_string_lossy().into_owned();
				let (digest, _) = nar::hash_path(&entry.path())?;
				let hash = nixbase32::encode(&digest);
				if hash != name {
					log(
						VerbosityLevel::Error,
						&format!(
							"link \"{}\" was modified! expected hash {name}, got '{hash}'",
							entry.path().display()
						),
					);
					errors = true;
				}
			}

			log(VerbosityLevel::Info, "checking store hashes...");
			for path in &valid {
				let Some(info) = self.query_path_info(path)? else {
					continue;
				};
				match nar::hash_path(&self.real_path(path)) {
					Ok((digest, size)) => {
						if info.nar_hash != digest {
							log(
								VerbosityLevel::Error,
								&format!(
									"path '{}' was modified! expected hash '{}', got '{}'",
									print(path),
									info.nar_hash_nix32(),
									format_args!("sha256:{}", nixbase32::encode(&digest))
								),
							);
							errors = true;
						} else if info.nar_size == 0 {
							log(
								VerbosityLevel::Info,
								&format!("updating size field on '{}' to {size}", print(path)),
							);
							let mut info = info;
							info.nar_size = size;
							update_path_info(&self.db(), &info)?;
						}
					}
					Err(e) => {
						log(
							VerbosityLevel::Error,
							&format!("\x1b[31;1merror:\x1b[0m {e}"),
						);
						errors = true;
					}
				}
			}
		}
		Ok(errors)
	}

	fn verify_path(
		&self,
		path: &StorePath,
		on_disk: &BTreeSet<StorePath>,
		done: &mut BTreeSet<StorePath>,
		valid: &mut BTreeSet<StorePath>,
		errors: &mut bool,
		log: &mut dyn FnMut(VerbosityLevel, &str),
	) -> Result<()> {
		if !done.insert(path.clone()) {
			return Ok(());
		}
		if on_disk.contains(path) {
			valid.insert(path.clone());
			return Ok(());
		}
		let mut can_invalidate = true;
		for referrer in self.query_referrers(path)? {
			if &referrer != path {
				self.verify_path(&referrer, on_disk, done, valid, errors, log)?;
				if valid.contains(&referrer) {
					can_invalidate = false;
				}
			}
		}
		if can_invalidate {
			log(
				VerbosityLevel::Info,
				&format!(
					"path '{}' disappeared, removing from database...",
					print(path)
				),
			);
			self.db()
				.prepare_cached("delete from ValidPaths where path = ?")?
				.execute([print(path)])?;
		} else {
			log(
				VerbosityLevel::Error,
				&format!(
					"path '{}' disappeared, but it still has valid referrers!",
					print(path)
				),
			);
			*errors = true;
		}
		Ok(())
	}
}

struct DigestWriter(NixHashDigester);

impl Write for DigestWriter {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		self.0.update(buf);
		Ok(buf.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
	fn drop(&mut self) {
		let _ = delete_path(&self.0);
	}
}

fn nix32_with_algo(hash: &NixHash) -> String {
	format!(
		"{}:{}",
		hash.algo().as_str(),
		nixbase32::encode(hash.digest_as_bytes())
	)
}

/// The content address of the files at `path`, computed the way `claimed`
/// was.
pub fn ca_hash_of(path: &Path, claimed: &CAHash) -> Result<CAHash> {
	let algo = claimed.hash().algo();
	Ok(match claimed {
		CAHash::Nar(_) => {
			let mut writer = DigestWriter(NixHashDigester::new(algo));
			nar::dump_path(&mut writer, path)?;
			CAHash::Nar(writer.0.finalize())
		}
		CAHash::Flat(_) | CAHash::Text(_) => {
			let mut digester = NixHashDigester::new(algo);
			digester.update(fs::read(path)?);
			match (claimed, digester.finalize()) {
				(CAHash::Text(_), NixHash::Sha256(d)) => CAHash::Text(d),
				(_, hash) => CAHash::Flat(hash),
			}
		}
	})
}

fn query_id(db: &Connection, path: &StorePath) -> Result<Option<i64>> {
	Ok(db
		.prepare_cached("select id from ValidPaths where path = ?")?
		.query_row([print(path)], |row| row.get(0))
		.optional()?)
}

fn query_path_info(db: &Connection, path: &StorePath) -> Result<Option<ValidPathInfo>> {
	type Row = (
		i64,
		String,
		i64,
		Option<String>,
		Option<i64>,
		Option<i64>,
		Option<String>,
		Option<String>,
	);
	let row: Option<Row> = db
		.prepare_cached(
			"select id, hash, registrationTime, deriver, narSize, ultimate, sigs, ca from ValidPaths where path = ?",
		)?
		.query_row([print(path)], |row| {
			Ok((
				row.get(0)?,
				row.get(1)?,
				row.get(2)?,
				row.get(3)?,
				row.get(4)?,
				row.get(5)?,
				row.get(6)?,
				row.get(7)?,
			))
		})
		.optional()?;
	let Some((id, hash, registration_time, deriver, nar_size, ultimate, sigs, ca)) = row else {
		return Ok(None);
	};
	let nar_hash = match NixHash::from_str(&hash, None) {
		Ok(NixHash::Sha256(digest)) => digest,
		_ => {
			return Err(Error::Other(format!(
				"invalid-path entry for '{}': invalid hash '{hash}'",
				print(path)
			)));
		}
	};
	let mut info = ValidPathInfo::new(path.clone(), nar_hash);
	info.registration_time = registration_time;
	info.deriver = deriver.as_deref().map(parse_store_path).transpose()?;
	info.nar_size = nar_size.unwrap_or(0) as u64;
	info.ultimate = ultimate == Some(1);
	if let Some(sigs) = sigs {
		info.sigs = sigs
			.split(' ')
			.filter(|s| !s.is_empty())
			.map(str::to_owned)
			.collect();
	}
	info.ca = ca.as_deref().and_then(CAHash::from_nix_hex_str);

	let mut stmt = db.prepare_cached(
		"select path from Refs join ValidPaths on reference = id where referrer = ?",
	)?;
	let refs = stmt
		.query_map([id], |row| row.get::<_, String>(0))?
		.collect::<Result<Vec<_>, _>>()?;
	for r in refs {
		info.references.insert(parse_store_path(&r)?);
	}
	Ok(Some(info))
}

fn query_referrers(db: &Connection, path: &StorePath) -> Result<BTreeSet<StorePath>> {
	let mut stmt = db.prepare_cached(
		"select path from Refs join ValidPaths on referrer = id where reference = (select id from ValidPaths where path = ?)",
	)?;
	let paths = stmt
		.query_map([print(path)], |row| row.get::<_, String>(0))?
		.collect::<Result<Vec<_>, _>>()?;
	paths.iter().map(|p| parse_store_path(p)).collect()
}

fn update_path_info(db: &Connection, info: &ValidPathInfo) -> Result<()> {
	db.prepare_cached(
		"update ValidPaths set narSize = ?, hash = ?, ultimate = ?, sigs = ?, ca = ? where path = ?",
	)?
	.execute(params![
		(info.nar_size != 0).then_some(info.nar_size as i64),
		info.nar_hash_base16(),
		info.ultimate.then_some(1),
		(!info.sigs.is_empty()).then(|| info.sigs.iter().cloned().collect::<Vec<_>>().join(" ")),
		info.ca.as_ref().map(|ca| ca.to_string()),
		print(&info.path),
	])?;
	Ok(())
}

/// A reference cycle among `infos`, as (path, parent).
fn find_cycle<'a>(
	infos: &HashMap<&'a StorePath, &'a ValidPathInfo>,
) -> Option<(&'a StorePath, &'a StorePath)> {
	#[derive(Clone, Copy, PartialEq)]
	enum State {
		Visiting,
		Done,
	}
	fn visit<'a>(
		path: &'a StorePath,
		parent: &'a StorePath,
		infos: &HashMap<&'a StorePath, &'a ValidPathInfo>,
		state: &mut HashMap<&'a StorePath, State>,
	) -> Option<(&'a StorePath, &'a StorePath)> {
		match state.get(path) {
			Some(State::Done) => return None,
			Some(State::Visiting) => return Some((path, parent)),
			None => {}
		}
		state.insert(path, State::Visiting);
		if let Some(info) = infos.get(path) {
			for reference in &info.references {
				if reference != path
					&& infos.contains_key(reference)
					&& let Some(cycle) = visit(reference, path, infos, state)
				{
					return Some(cycle);
				}
			}
		}
		state.insert(path, State::Done);
		None
	}
	let mut state = HashMap::new();
	let mut paths: Vec<_> = infos.keys().copied().collect();
	paths.sort();
	for path in paths {
		if let Some(cycle) = visit(path, path, infos, &mut state) {
			return Some(cycle);
		}
	}
	None
}

/// Recursively delete `path` if it exists, making directories writable
/// first.
pub fn delete_path(path: &Path) -> Result<()> {
	let metadata = match fs::symlink_metadata(path) {
		Ok(m) => m,
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
		Err(e) => {
			return Err(Error::io(
				format!("getting status of '{}'", path.display()),
				e,
			));
		}
	};
	if metadata.is_dir() {
		if metadata.mode() & 0o700 != 0o700 {
			fs::set_permissions(path, fs::Permissions::from_mode(metadata.mode() | 0o700))?;
		}
		for entry in fs::read_dir(path)? {
			delete_path(&entry?.path())?;
		}
		fs::remove_dir(path).context(|| format!("removing '{}'", path.display()))?;
	} else {
		fs::remove_file(path).context(|| format!("removing '{}'", path.display()))?;
	}
	Ok(())
}

fn read_schema(path: &Path) -> Result<u32> {
	match fs::read_to_string(path) {
		Ok(s) => s
			.trim()
			.parse()
			.map_err(|_| Error::CorruptSchema(path.to_owned())),
		Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
		Err(e) => Err(Error::io(format!("reading '{}'", path.display()), e)),
	}
}

fn write_schema(path: &Path, version: u32) -> Result<()> {
	let mut file = File::create(path).context(|| format!("writing '{}'", path.display()))?;
	file.write_all(version.to_string().as_bytes())?;
	file.sync_all()?;
	Ok(())
}

fn check_db_dir_access(db_dir: &Path, read_only: bool) -> Result<()> {
	let c_path = crate::canonicalise::c_path(db_dir)?;
	let mode = libc::R_OK | if read_only { 0 } else { libc::W_OK };
	// SAFETY: `c_path` is NUL-terminated.
	if unsafe { libc::access(c_path.as_ptr(), mode) } != 0 {
		return Err(Error::io(
			format!(
				"Nix database directory '{}' is not writable",
				db_dir.display()
			),
			io::Error::last_os_error(),
		));
	}
	Ok(())
}

/// Space the garbage collector frees before it runs on a full disk.
fn reserve_space(path: &Path, size: u64) {
	if fs::metadata(path).map(|m| m.len() == size).unwrap_or(false) {
		return;
	}
	let _ = (|| -> io::Result<()> {
		use std::os::unix::fs::OpenOptionsExt;
		let file = fs::OpenOptions::new()
			.write(true)
			.create(true)
			.truncate(false)
			.mode(0o600)
			.open(path)?;
		file.set_len(size)?;
		// SAFETY: the descriptor is open for writing.
		let _ = unsafe {
			libc::posix_fallocate(std::os::fd::AsRawFd::as_raw_fd(&file), 0, size as i64)
		};
		Ok(())
	})();
}

fn check_not_symlinked(real_store_dir: &Path) -> Result<()> {
	let mut path = Some(real_store_dir);
	while let Some(p) = path {
		if p.parent().is_none() {
			break;
		}
		if fs::symlink_metadata(p)
			.map(|m| m.file_type().is_symlink())
			.unwrap_or(false)
		{
			return Err(Error::Other(format!(
				"the path '{}' is a symlink; this is not allowed for the Nix store and its parent directories",
				p.display()
			)));
		}
		path = p.parent();
	}
	Ok(())
}

/// As root, make a read-only store mount writable for this process only,
/// in a private mount namespace.
fn make_store_writable(real_store_dir: &Path) -> Result<()> {
	// SAFETY: getuid cannot fail.
	if unsafe { libc::getuid() } != 0 {
		return Ok(());
	}
	let c_path = crate::canonicalise::c_path(real_store_dir)?;
	// SAFETY: `stat` is plain data, filled by statvfs.
	let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
	// SAFETY: `c_path` is NUL-terminated and `stat` is writable.
	if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
		return Err(Error::io(
			format!(
				"getting info about the Nix store mount point '{}'",
				real_store_dir.display()
			),
			io::Error::last_os_error(),
		));
	}
	if stat.f_flag & libc::ST_RDONLY != 0 {
		// SAFETY: these calls only affect this process's mount namespace.
		unsafe {
			if libc::unshare(libc::CLONE_NEWNS) == -1 {
				return Err(Error::io(
					"setting up a private mount namespace",
					io::Error::last_os_error(),
				));
			}
			if libc::mount(
				std::ptr::null(),
				c_path.as_ptr(),
				c"none".as_ptr(),
				libc::MS_REMOUNT | libc::MS_BIND,
				std::ptr::null(),
			) == -1
			{
				return Err(Error::io(
					format!("remounting '{}' writable", real_store_dir.display()),
					io::Error::last_os_error(),
				));
			}
		}
	}
	Ok(())
}
