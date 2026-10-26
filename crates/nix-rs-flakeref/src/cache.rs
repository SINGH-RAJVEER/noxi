//! CppNix 2.35.2's fetcher-cache-v4.sqlite schema and lookup policy.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::input::{Attrs, attrs_to_json, json_to_attrs};

pub fn cache_dir() -> PathBuf {
	if let Some(path) = std::env::var_os("NIX_CACHE_HOME") {
		return path.into();
	}
	std::env::var_os("XDG_CACHE_HOME")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
		})
		.join("nix")
}

pub fn now() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_secs()
}

#[derive(Debug)]
pub struct Cache {
	db: Connection,
}

#[derive(Debug)]
pub struct Cached {
	pub value: Attrs,
	pub timestamp: u64,
	pub expired: bool,
}

fn db_error(error: impl std::fmt::Display) -> Error {
	Error::new(error.to_string())
}

impl Cache {
	pub fn open(directory: &Path) -> Result<Self> {
		std::fs::create_dir_all(directory)?;
		let db = Connection::open(directory.join("fetcher-cache-v4.sqlite")).map_err(db_error)?;
		db.busy_timeout(std::time::Duration::from_secs(60))
			.map_err(db_error)?;
		db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=OFF; CREATE TABLE IF NOT EXISTS Cache (domain TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, timestamp INTEGER NOT NULL, PRIMARY KEY (domain, key));").map_err(db_error)?;
		Ok(Self { db })
	}

	pub fn upsert_at(
		&self,
		domain: &str,
		key: &Attrs,
		value: &Attrs,
		timestamp: u64,
	) -> Result<()> {
		let timestamp = i64::try_from(timestamp).map_err(db_error)?;
		let key = crate::json::dump(&serde_json::Value::Object(attrs_to_json(key)?), None);
		let value = crate::json::dump(&serde_json::Value::Object(attrs_to_json(value)?), None);
		self.db
			.execute(
				"INSERT OR REPLACE INTO Cache(domain,key,value,timestamp) VALUES (?1,?2,?3,?4)",
				params![domain, key, value, timestamp],
			)
			.map_err(db_error)?;
		Ok(())
	}

	pub fn upsert(&self, domain: &str, key: &Attrs, value: &Attrs) -> Result<()> {
		self.upsert_at(domain, key, value, now())
	}

	pub fn lookup_expired_at(
		&self,
		domain: &str,
		key: &Attrs,
		ttl: u64,
		time: u64,
	) -> Result<Option<Cached>> {
		let key = crate::json::dump(&serde_json::Value::Object(attrs_to_json(key)?), None);
		let row: Option<(String, i64)> = self
			.db
			.query_row(
				"SELECT value,timestamp FROM Cache WHERE domain=?1 AND key=?2",
				params![domain, key],
				|row| Ok((row.get(0)?, row.get(1)?)),
			)
			.optional()
			.map_err(db_error)?;
		row.map(|(value, timestamp)| {
			let timestamp = u64::try_from(timestamp).map_err(db_error)?;
			let json = serde_json::from_str(&value).map_err(db_error)?;
			Ok(Cached {
				value: json_to_attrs(&json)?,
				timestamp,
				expired: ttl == 0 || timestamp.saturating_add(ttl) < time,
			})
		})
		.transpose()
	}

	pub fn lookup_expired(&self, domain: &str, key: &Attrs, ttl: u64) -> Result<Option<Cached>> {
		self.lookup_expired_at(domain, key, ttl, now())
	}
	pub fn lookup(&self, domain: &str, key: &Attrs) -> Result<Option<Attrs>> {
		Ok(self
			.lookup_expired(domain, key, 0)?
			.map(|entry| entry.value))
	}
	pub fn lookup_with_ttl(&self, domain: &str, key: &Attrs, ttl: u64) -> Result<Option<Attrs>> {
		Ok(self
			.lookup_expired(domain, key, ttl)?
			.filter(|entry| !entry.expired)
			.map(|entry| entry.value))
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn reference_schema_json_and_ttl_boundaries() {
		let path = std::env::temp_dir().join(format!("nixrs-cache-test-{}", std::process::id()));
		let cache = Cache::open(&path).unwrap();
		let key: Attrs = [("url".into(), "https://example.invalid/source".into())].into();
		let value = [("lastModified".into(), 42u64.into())].into();
		cache.upsert_at("tarball", &key, &value, 100).unwrap();
		assert!(
			!cache
				.lookup_expired_at("tarball", &key, 10, 110)
				.unwrap()
				.unwrap()
				.expired
		);
		assert!(
			cache
				.lookup_expired_at("tarball", &key, 10, 111)
				.unwrap()
				.unwrap()
				.expired
		);
		assert!(
			cache
				.lookup_expired_at("tarball", &key, 0, 100)
				.unwrap()
				.unwrap()
				.expired
		);
		drop(cache);
		assert_eq!(
			Cache::open(&path).unwrap().lookup("tarball", &key).unwrap(),
			Some(value)
		);
		std::fs::remove_dir_all(path).unwrap();
	}
}
