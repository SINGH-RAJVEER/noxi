//! Opening the store database as CppNix's `LocalStore::openDB` does.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::error::Result;

/// The current store schema version, `nixSchemaVersion`.
pub const SCHEMA_VERSION: u32 = 10;

/// The initial schema, copied from CppNix 2.35.2's `schema.sql`.
const SCHEMA: &str = include_str!("schema.sql");

/// Named migrations applied after the numbered schema, in order. Migrations
/// that depend on experimental features (`20251017-ca-derivations`) are not
/// listed.
const MIGRATIONS: &[(&str, &str)] = &[(
	"20260309-drop-redundant-indexreferrer",
	"drop index if exists IndexReferrer",
)];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
	/// Read-only, without locking or a write-ahead log.
	Immutable,
	/// Read-write, creating the file.
	Create,
	/// Read-write, failing if the file is missing.
	NoCreate,
}

pub struct DbSettings {
	pub use_wal: bool,
	pub fsync_metadata: bool,
}

pub fn open(path: &Path, mode: OpenMode, settings: &DbSettings) -> Result<Connection> {
	let mut flags = OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
	flags |= match mode {
		OpenMode::Immutable => OpenFlags::SQLITE_OPEN_READ_ONLY,
		OpenMode::Create => OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
		OpenMode::NoCreate => OpenFlags::SQLITE_OPEN_READ_WRITE,
	};
	let uri = format!(
		"file:{}?immutable={}",
		percent_encode(&path.to_string_lossy()),
		if mode == OpenMode::Immutable { 1 } else { 0 }
	);
	// Without WAL, CppNix uses dot-file locking for NFS and WSL.
	let conn = if settings.use_wal {
		Connection::open_with_flags(&uri, flags)?
	} else {
		Connection::open_with_flags_and_vfs(&uri, flags, c"unix-dotfile")?
	};
	conn.busy_timeout(Duration::from_secs(60 * 60))?;
	conn.execute_batch("pragma foreign_keys = 1")?;

	let sync_mode = if settings.fsync_metadata {
		"normal"
	} else {
		"off"
	};
	conn.execute_batch(&format!("pragma synchronous = {sync_mode}"))?;

	let mode_name = if settings.use_wal { "wal" } else { "truncate" };
	let previous: String = conn.query_row("pragma main.journal_mode", [], |row| row.get(0))?;
	if previous != mode_name {
		let _: String = conn.query_row(
			&format!("pragma main.journal_mode = {mode_name}"),
			[],
			|row| row.get(0),
		)?;
	}

	if settings.use_wal {
		// Keep the WAL files when closing, so read-only users without write
		// access to the directory can still open the database.
		if conn
			.query_row("pragma main.journal_size_limit = 1099511627776", [], |_| {
				Ok(())
			})
			.is_ok()
		{
			let mut enable: libc::c_int = 1;
			// SAFETY: the handle is open, and PERSIST_WAL reads an int.
			unsafe {
				rusqlite::ffi::sqlite3_file_control(
					conn.handle(),
					std::ptr::null(),
					rusqlite::ffi::SQLITE_FCNTL_PERSIST_WAL,
					(&raw mut enable).cast(),
				);
			}
		}
		conn.query_row("pragma wal_autocheckpoint = 40000", [], |_| Ok(()))?;
	}

	if mode == OpenMode::Create {
		conn.execute_batch(SCHEMA)?;
	}
	Ok(conn)
}

/// Apply the named migrations. With `dry_run`, only report whether any is
/// needed. Returns whether a migration was (or would be) applied.
pub fn upgrade_schema(conn: &Connection, dry_run: bool) -> Result<bool> {
	let mut changed = false;
	let has_table = conn
		.query_row(
			"SELECT 1 FROM sqlite_master WHERE type='table' AND name='SchemaMigrations'",
			[],
			|_| Ok(()),
		)
		.optional()?
		.is_some();
	if !has_table {
		if dry_run {
			return Ok(true);
		}
		conn.execute_batch("create table SchemaMigrations (migration text primary key not null);")?;
		changed = true;
	}

	let applied: Vec<String> = conn
		.prepare("select migration from SchemaMigrations")?
		.query_map([], |row| row.get(0))?
		.collect::<Result<_, _>>()?;

	for (name, statement) in MIGRATIONS {
		if applied.iter().any(|m| m == name) {
			continue;
		}
		changed = true;
		if dry_run {
			continue;
		}
		tracing::debug!("executing Nix database schema migration '{name}'...");
		let tx = conn.unchecked_transaction()?;
		tx.execute_batch(&format!(
			"{statement};\ninsert or ignore into SchemaMigrations values('{name}')"
		))?;
		tx.commit()?;
	}
	Ok(changed)
}

/// Percent-encode everything except RFC 3986 unreserved characters and `/`,
/// as CppNix encodes the database path in its SQLite URI.
fn percent_encode(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for b in s.bytes() {
		if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
			out.push(b as char);
		} else {
			out.push_str(&format!("%{b:02X}"));
		}
	}
	out
}
