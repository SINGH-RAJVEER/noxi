//! CppNix 2.35.2's `NarInfoDiskCache`: what HTTP binary caches (and
//! `file://` caches under `_NIX_FORCE_HTTP=1`) answered, kept per user in
//! `binary-cache-v8.sqlite` in the cache directory, so that later processes
//! need not ask again. The schema, statements, pragmas and expiry rules are
//! those of `nar-info-disk-cache.cc`, so CppNix and noxi share the file.
//!
//! A cache's `nix-cache-info` is kept for `narinfo-cache-meta-ttl` seconds,
//! a path's `.narinfo` for `narinfo-cache-positive-ttl`, its absence for
//! `narinfo-cache-negative-ttl`, and build trace entries likewise. Entries
//! are purged at most once a day, with at least an hour's and thirty days'
//! grace so that `--refresh` does not empty the file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::error::{Error, Result};

const SCHEMA: &str = r#"

create table if not exists BinaryCaches (
    id        integer primary key autoincrement not null,
    url       text unique not null,
    timestamp integer not null,
    storeDir  text not null,
    wantMassQuery integer not null,
    priority  integer not null
);

create table if not exists NARs (
    cache            integer not null,
    hashPart         text not null,
    namePart         text,
    url              text,
    compression      text,
    fileHash         text,
    fileSize         integer,
    narHash          text,
    narSize          integer,
    refs             text,
    deriver          text,
    sigs             text,
    ca               text,
    timestamp        integer not null,
    present          integer not null,
    primary key (cache, hashPart),
    foreign key (cache) references BinaryCaches(id) on delete cascade
);

create table if not exists BuildTrace (
    cache integer not null,

    drvPath text not null,
    outputName text not null,

    -- The following are null if the realisation is absent
    outputPath text,
    sigs text,

    timestamp        integer not null,
    primary key (cache, drvPath, outputName),
    foreign key (cache) references BinaryCaches(id) on delete cascade
);

create table if not exists LastPurge (
    dummy            text primary key,
    value            integer
);

"#;

/// How often expired entries are purged, `purgeInterval`.
const PURGE_INTERVAL: i64 = 24 * 3600;

static POSITIVE_TTL: AtomicU64 = AtomicU64::new(30 * 24 * 3600);
static NEGATIVE_TTL: AtomicU64 = AtomicU64::new(3600);
static META_TTL: AtomicU64 = AtomicU64::new(7 * 24 * 3600);
static USE_WAL: AtomicBool = AtomicBool::new(true);

/// Take the lifetimes (`narinfo-cache-positive-ttl`,
/// `narinfo-cache-negative-ttl`, `narinfo-cache-meta-ttl`) and
/// `use-sqlite-wal` from `settings` where it sets them. CppNix reads them
/// from its global settings.
pub fn configure(settings: &nix_compat::nixcpp::settings::Settings) {
	for (name, value) in [
		("narinfo-cache-positive-ttl", &POSITIVE_TTL),
		("narinfo-cache-negative-ttl", &NEGATIVE_TTL),
		("narinfo-cache-meta-ttl", &META_TTL),
	] {
		if let Some(ttl) = settings.get(name).and_then(|v| v.trim().parse().ok()) {
			value.store(ttl, Ordering::Relaxed);
		}
	}
	if settings.get("use-sqlite-wal").is_some() {
		USE_WAL.store(settings.get_bool("use-sqlite-wal", true), Ordering::Relaxed);
	}
}

fn ttl(value: &AtomicU64) -> i64 {
	value.load(Ordering::Relaxed).min(i64::MAX as u64) as i64
}

/// How long answers are believed, in seconds: CppNix's
/// `narinfo-cache-positive-ttl` and `narinfo-cache-negative-ttl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ttls {
	pub positive: i64,
	pub negative: i64,
}

impl Ttls {
	/// The configured lifetimes.
	pub fn configured() -> Self {
		Self {
			positive: ttl(&POSITIVE_TTL),
			negative: ttl(&NEGATIVE_TTL),
		}
	}
}

fn now() -> i64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |d| d.as_secs() as i64)
}

/// `getCacheDir`: `NIX_CACHE_HOME`, else `XDG_CACHE_HOME/nix`, else
/// `HOME/.cache/nix`.
pub fn cache_dir() -> PathBuf {
	if let Some(dir) = std::env::var_os("NIX_CACHE_HOME") {
		return dir.into();
	}
	std::env::var_os("XDG_CACHE_HOME")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
		})
		.join("nix")
}

/// What `nix-cache-info` said, as the cache's row keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheInfo {
	pub want_mass_query: bool,
	pub priority: i64,
}

/// A row of `NARs` for a path the cache has, with NULL columns as `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NarRow {
	pub name_part: String,
	pub url: Option<String>,
	pub compression: Option<String>,
	pub file_hash: Option<String>,
	pub file_size: Option<u64>,
	/// `sha256:` and Nix base32.
	pub nar_hash: String,
	pub nar_size: u64,
	/// The references' base names, separated by spaces.
	pub refs: String,
	pub deriver: Option<String>,
	/// `NAME:BASE64` signatures, separated by spaces.
	pub sigs: String,
	/// The rendered content address, or empty.
	pub ca: String,
}

/// `NarInfoDiskCache::Outcome`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome<T> {
	/// Nothing (unexpired) is known.
	Unknown,
	/// The cache is known not to have it.
	Invalid,
	Valid(T),
}

/// A build trace entry the cache has: the output's base name and its
/// signatures as JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RealisationRow {
	pub out_path: String,
	pub sigs: String,
}

pub struct DiskCache {
	/// Closed when the process exits, as the reference's static is destroyed.
	db: Mutex<Option<Connection>>,
	/// The caches whose row this process found or created, by URL.
	caches: Mutex<BTreeMap<String, (i64, CacheInfo)>>,
}

/// `NarInfoDiskCache::get`: the process's cache, opened on first use. As
/// with CppNix's function-local static, an open that fails is tried again
/// the next time.
pub fn get() -> Result<Arc<DiskCache>> {
	let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
	if let Some(cache) = &*cache {
		return Ok(cache.clone());
	}
	let opened = Arc::new(DiskCache::open(&database_path())?);
	*cache = Some(opened.clone());
	// SAFETY: registering a function without arguments that only takes
	// locks it does not wait for.
	unsafe { libc::atexit(close_at_exit) };
	Ok(opened)
}

static CACHE: Mutex<Option<Arc<DiskCache>>> = Mutex::new(None);

/// Close the database when the process exits, as destroying CppNix's static
/// does, so SQLite checkpoints the write-ahead log and removes it. A thread
/// still using the database keeps it open.
extern "C" fn close_at_exit() {
	if let Ok(cache) = CACHE.try_lock()
		&& let Some(cache) = cache.as_ref()
		&& let Ok(mut db) = cache.db.try_lock()
	{
		db.take();
	}
}

fn closed() -> Error {
	Error::Other("the NAR info disk cache is closed".to_owned())
}

fn connection(db: &Option<Connection>) -> Result<&Connection> {
	db.as_ref().ok_or_else(closed)
}

fn sqlite_error(what: &str, error: rusqlite::Error) -> Error {
	Error::Other(format!("{what}: {error}"))
}

impl DiskCache {
	/// `NarInfoDiskCacheImpl`'s constructor.
	pub fn open(path: &Path) -> Result<Self> {
		let dir = path.parent().unwrap_or(Path::new("/"));
		std::fs::create_dir_all(dir).map_err(|e| {
			Error::Other(format!(
				"creating directory \"{}\": {}",
				dir.display(),
				strerror(&e)
			))
		})?;
		// `SQLite::SQLite`: without the write-ahead log, the `unix-dotfile`
		// VFS.
		let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
			| OpenFlags::SQLITE_OPEN_CREATE
			| OpenFlags::SQLITE_OPEN_NO_MUTEX;
		let db = if USE_WAL.load(Ordering::Relaxed) {
			Connection::open_with_flags(path, flags)
		} else {
			Connection::open_with_flags_and_vfs(path, flags, c"unix-dotfile")
		}
		.map_err(|e| {
			let code = match &e {
				rusqlite::Error::SqliteFailure(error, _) => {
					rusqlite::ffi::code_to_str(error.extended_code).to_owned()
				}
				other => other.to_string(),
			};
			Error::Other(format!(
				"cannot open SQLite database \"{}\": {code}",
				path.display()
			))
		})?;
		let fail = |e| sqlite_error(&format!("opening '{}'", path.display()), e);
		db.busy_timeout(std::time::Duration::from_secs(60 * 60))
			.map_err(fail)?;
		db.execute_batch("pragma foreign_keys = 1").map_err(fail)?;
		// `SQLite::isCache`.
		db.execute_batch("pragma synchronous = off").map_err(fail)?;
		db.execute_batch("pragma main.journal_mode = wal")
			.map_err(fail)?;
		db.execute_batch(SCHEMA).map_err(fail)?;
		// Purge expired entries at most once per interval.
		let now = now();
		let last: Option<i64> = db
			.query_row("select value from LastPurge", [], |row| row.get(0))
			.optional()
			.map_err(fail)?;
		if last.is_none_or(|last| last < now - PURGE_INTERVAL) {
			db.execute(
				"delete from NARs where ((present = 0 and timestamp < ?) or (present = 1 and timestamp < ?))",
				params![
					now - ttl(&NEGATIVE_TTL).max(3600),
					now - ttl(&POSITIVE_TTL).max(30 * 24 * 3600)
				],
			)
			.map_err(fail)?;
			db.execute(
				"insert or replace into LastPurge(dummy, value) values ('', ?)",
				params![now],
			)
			.map_err(fail)?;
		}
		Ok(Self {
			db: Mutex::new(Some(db)),
			caches: Mutex::new(BTreeMap::new()),
		})
	}

	fn db(&self) -> std::sync::MutexGuard<'_, Option<Connection>> {
		self.db.lock().unwrap_or_else(|e| e.into_inner())
	}

	/// `queryCacheRaw`: the cache's row, if this process knows it or it was
	/// written within `narinfo-cache-meta-ttl`.
	fn query_cache(&self, db: &Connection, uri: &str) -> Result<Option<(i64, CacheInfo)>> {
		let mut caches = self.caches.lock().unwrap_or_else(|e| e.into_inner());
		if let Some(cache) = caches.get(uri) {
			return Ok(Some(*cache));
		}
		let row = db
			.query_row(
				"select id, storeDir, wantMassQuery, priority from BinaryCaches where url = ? and timestamp > ?",
				params![uri, now() - ttl(&META_TTL)],
				|row| {
					Ok((
						row.get::<_, i64>(0)?,
						CacheInfo {
							want_mass_query: row.get::<_, i64>(2)? != 0,
							priority: row.get(3)?,
						},
					))
				},
			)
			.optional()
			.map_err(|e| sqlite_error("querying the binary cache disk cache", e))?;
		if let Some(row) = row {
			caches.insert(uri.to_owned(), row);
		}
		Ok(row)
	}

	/// `upToDateCacheExists`.
	pub fn up_to_date_cache_exists(&self, uri: &str) -> Result<Option<CacheInfo>> {
		let guard = self.db();
		Ok(self
			.query_cache(connection(&guard)?, uri)?
			.map(|(_, info)| info))
	}

	/// `createCache`: record what `nix-cache-info` said, unless another
	/// process did so meanwhile.
	pub fn create_cache(&self, uri: &str, store_dir: &str, info: CacheInfo) -> Result<()> {
		let mut guard = self.db();
		let db = guard.as_mut().ok_or_else(closed)?;
		let fail = |e| sqlite_error("recording a binary cache in the disk cache", e);
		let txn = db.transaction().map_err(fail)?;
		if self.query_cache(&txn, uri)?.is_some() {
			return Ok(());
		}
		let id: i64 = txn
			.query_row(
				"insert into BinaryCaches(url, timestamp, storeDir, wantMassQuery, priority) values (?1, ?2, ?3, ?4, ?5) on conflict (url) do update set timestamp = ?2, storeDir = ?3, wantMassQuery = ?4, priority = ?5 returning id;",
				params![uri, now(), store_dir, info.want_mass_query, info.priority],
				|row| row.get(0),
			)
			.map_err(fail)?;
		txn.commit().map_err(fail)?;
		self.caches
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.insert(uri.to_owned(), (id, info));
		Ok(())
	}

	fn cache_id(&self, uri: &str) -> Result<i64> {
		self.caches
			.lock()
			.unwrap_or_else(|e| e.into_inner())
			.get(uri)
			.map(|(id, _)| *id)
			.ok_or_else(|| Error::Other(format!("binary cache '{uri}' is not in the disk cache")))
	}

	/// `lookupNarInfo`.
	pub fn lookup_narinfo(
		&self,
		uri: &str,
		hash_part: &str,
		ttls: Ttls,
	) -> Result<Outcome<NarRow>> {
		let id = self.cache_id(uri)?;
		let now = now();
		let guard = self.db();
		let row = connection(&guard)?
			.query_row(
				"select present, namePart, url, compression, fileHash, fileSize, narHash, narSize, refs, deriver, sigs, ca from NARs where cache = ? and hashPart = ? and ((present = 0 and timestamp > ?) or (present = 1 and timestamp > ?))",
				params![id, hash_part, now - ttls.negative, now - ttls.positive],
				|row| {
					if row.get::<_, i64>(0)? == 0 {
						return Ok(None);
					}
					Ok(Some(NarRow {
						name_part: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
						url: row.get(2)?,
						compression: row.get(3)?,
						file_hash: row.get(4)?,
						file_size: row.get::<_, Option<i64>>(5)?.map(|s| s as u64),
						nar_hash: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
						nar_size: row.get::<_, Option<i64>>(7)?.unwrap_or(0) as u64,
						refs: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
						deriver: row.get(9)?,
						sigs: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
						ca: row.get::<_, Option<String>>(11)?.unwrap_or_default(),
					}))
				},
			)
			.optional()
			.map_err(|e| sqlite_error("querying the NAR info disk cache", e))?;
		Ok(match row {
			None => Outcome::Unknown,
			Some(None) => Outcome::Invalid,
			Some(Some(row)) => Outcome::Valid(row),
		})
	}

	/// `upsertNarInfo`: what the cache has for `hash_part`, or that it has
	/// nothing.
	pub fn upsert_narinfo(&self, uri: &str, hash_part: &str, row: Option<&NarRow>) -> Result<()> {
		let id = self.cache_id(uri)?;
		let fail = |e| sqlite_error("updating the NAR info disk cache", e);
		let guard = self.db();
		let db = connection(&guard)?;
		match row {
			Some(row) => db
				.execute(
					"insert or replace into NARs(cache, hashPart, namePart, url, compression, fileHash, fileSize, narHash, narSize, refs, deriver, sigs, ca, timestamp, present) values (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1)",
					params![
						id,
						hash_part,
						row.name_part,
						row.url,
						row.compression,
						row.file_hash,
						row.file_size.map(|s| s as i64),
						row.nar_hash,
						row.nar_size as i64,
						row.refs,
						row.deriver,
						row.sigs,
						row.ca,
						now()
					],
				)
				.map_err(fail)?,
			None => db
				.execute(
					"insert or replace into NARs(cache, hashPart, timestamp, present) values (?, ?, ?, 0)",
					params![id, hash_part, now()],
				)
				.map_err(fail)?,
		};
		Ok(())
	}

	/// `lookupRealisation`: `drv_path` is the derivation's base name.
	pub fn lookup_realisation(
		&self,
		uri: &str,
		drv_path: &str,
		output: &str,
		ttls: Ttls,
	) -> Result<Outcome<RealisationRow>> {
		let id = self.cache_id(uri)?;
		let now = now();
		let guard = self.db();
		let row = connection(&guard)?
			.query_row(
				"
                select outputPath, sigs from BuildTrace
                    where cache = ? and drvPath = ? and outputName = ? and
                        ((outputPath is null and timestamp > ?) or
                         (outputPath is not null and timestamp > ?))
            ",
				params![
					id,
					drv_path,
					output,
					now - ttls.negative,
					now - ttls.positive
				],
				|row| {
					Ok(row
						.get::<_, Option<String>>(0)?
						.map(|out_path| -> rusqlite::Result<_> {
							Ok(RealisationRow {
								out_path,
								sigs: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
							})
						})
						.transpose()?)
				},
			)
			.optional()
			.map_err(|e| sqlite_error("querying the build trace disk cache", e))?;
		Ok(match row {
			None => Outcome::Unknown,
			Some(None) => Outcome::Invalid,
			Some(Some(row)) => Outcome::Valid(row),
		})
	}

	/// `upsertRealisation` and `upsertAbsentRealisation`.
	pub fn upsert_realisation(
		&self,
		uri: &str,
		drv_path: &str,
		output: &str,
		row: Option<&RealisationRow>,
	) -> Result<()> {
		let id = self.cache_id(uri)?;
		let fail = |e| sqlite_error("updating the build trace disk cache", e);
		let guard = self.db();
		let db = connection(&guard)?;
		match row {
			Some(row) => db
				.execute(
					"
                insert or replace into BuildTrace(cache, drvPath, outputName, outputPath, sigs, timestamp)
                    values (?, ?, ?, ?, ?, ?)
            ",
					params![id, drv_path, output, row.out_path, row.sigs, now()],
				)
				.map_err(fail)?,
			None => db
				.execute(
					"
                insert or replace into BuildTrace(cache, drvPath, outputName, timestamp)
                    values (?, ?, ?, ?)
            ",
					params![id, drv_path, output, now()],
				)
				.map_err(fail)?,
		};
		Ok(())
	}
}

/// The C library's message for an I/O error, as CppNix's `SysError` shows it.
fn strerror(error: &std::io::Error) -> String {
	match error.raw_os_error() {
		// SAFETY: strerror returns a static or thread-local string.
		Some(code) => unsafe { std::ffi::CStr::from_ptr(libc::strerror(code)) }
			.to_string_lossy()
			.into_owned(),
		None => error.to_string(),
	}
}

impl std::fmt::Debug for DiskCache {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_str("DiskCache")
	}
}

/// `getCacheDir() / "binary-cache-v8.sqlite"`.
#[cfg(not(test))]
fn database_path() -> PathBuf {
	cache_dir().join("binary-cache-v8.sqlite")
}

/// This crate's unit tests serve caches from local ports, which later runs
/// reuse; they must not leave answers in the user's cache.
#[cfg(test)]
fn database_path() -> PathBuf {
	static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
	DIR.get_or_init(|| tempfile::tempdir().expect("create a temporary directory"))
		.path()
		.join("binary-cache-v8.sqlite")
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The tables, indexes and their SQL, as `sqlite_master` records them.
	fn schema(db: &Connection) -> Vec<(String, String, Option<String>)> {
		let mut statement = db
			.prepare("select type, name, sql from sqlite_master order by name")
			.unwrap();
		statement
			.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
			.unwrap()
			.map(Result::unwrap)
			.collect()
	}

	#[test]
	fn answers_expire_as_configured() {
		let dir = tempfile::tempdir().unwrap();
		let cache = DiskCache::open(&dir.path().join("binary-cache-v8.sqlite")).unwrap();
		let uri = "https://cache.example.org";
		assert_eq!(cache.up_to_date_cache_exists(uri).unwrap(), None);
		let info = CacheInfo {
			want_mass_query: true,
			priority: 40,
		};
		cache.create_cache(uri, "/nix/store", info).unwrap();
		assert_eq!(cache.up_to_date_cache_exists(uri).unwrap(), Some(info));
		assert_eq!(
			cache.lookup_narinfo(uri, "a", Ttls::configured()).unwrap(),
			Outcome::Unknown
		);
		cache.upsert_narinfo(uri, "a", None).unwrap();
		assert_eq!(
			cache.lookup_narinfo(uri, "a", Ttls::configured()).unwrap(),
			Outcome::Invalid
		);
		let row = NarRow {
			name_part: "hello".to_owned(),
			url: Some("nar/x.nar.xz".to_owned()),
			compression: Some("xz".to_owned()),
			nar_hash: "sha256:0000000000000000000000000000000000000000000000000000".to_owned(),
			nar_size: 8,
			..NarRow::default()
		};
		cache.upsert_narinfo(uri, "b", Some(&row)).unwrap();
		assert_eq!(
			cache.lookup_narinfo(uri, "b", Ttls::configured()).unwrap(),
			Outcome::Valid(row)
		);
		cache.upsert_realisation(uri, "c.drv", "out", None).unwrap();
		assert_eq!(
			cache
				.lookup_realisation(uri, "c.drv", "out", Ttls::configured())
				.unwrap(),
			Outcome::Invalid
		);
		let schema = schema(connection(&cache.db()).unwrap());
		let names: Vec<&str> = schema.iter().map(|(_, name, _)| name.as_str()).collect();
		assert_eq!(
			names,
			[
				"BinaryCaches",
				"BuildTrace",
				"LastPurge",
				"NARs",
				"sqlite_autoindex_BinaryCaches_1",
				"sqlite_autoindex_BuildTrace_1",
				"sqlite_autoindex_LastPurge_1",
				"sqlite_autoindex_NARs_1",
				"sqlite_sequence",
			]
		);
	}
}
