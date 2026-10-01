//! Where a local store keeps its files, and the settings that change how it
//! writes them.

use std::path::{Path, PathBuf};

use nix_compat::store_path::STORE_DIR;

/// The settings CppNix's `LocalStoreConfig` and `LocalSettings` provide.
///
/// The logical store directory is always `/nix/store`; `real_store_dir` is
/// where its contents live on disk, which differs for a store opened with a
/// `root`.
#[derive(Clone, Debug)]
pub struct LocalStoreConfig {
	/// The directory holding store objects, `/nix/store` unless chrooted.
	pub real_store_dir: PathBuf,
	/// The state directory holding the database, roots and profiles.
	pub state_dir: PathBuf,
	/// The directory holding build logs.
	pub log_dir: PathBuf,
	/// Open the database immutably, without locks or writes.
	pub read_only: bool,
	/// Size of `db/reserved`, the `gc-reserved-space` setting.
	pub reserved_size: u64,
	/// SQLite `synchronous = normal` instead of `off`, the `fsync-metadata` setting.
	pub fsync_metadata: bool,
	/// Use SQLite's write-ahead log, the `use-sqlite-wal` setting.
	pub use_sqlite_wal: bool,
	/// Reject imported paths without a trusted signature, the `require-sigs` setting.
	pub require_sigs: bool,
	/// The `trusted-public-keys` setting, as `name:base64` strings.
	pub trusted_public_keys: Vec<String>,
}

impl LocalStoreConfig {
	/// The settings of the system store. `NIX_STATE_DIR` and `NIX_LOG_DIR`
	/// move the state and log directories; a store directory other than
	/// `/nix/store` is not supported.
	pub fn system() -> Self {
		let state_dir = std::env::var_os("NIX_STATE_DIR")
			.map(PathBuf::from)
			.unwrap_or_else(|| PathBuf::from("/nix/var/nix"));
		let log_dir = std::env::var_os("NIX_LOG_DIR")
			.map(PathBuf::from)
			.unwrap_or_else(|| PathBuf::from("/nix/var/log/nix"));
		Self::with_dirs(PathBuf::from(STORE_DIR), state_dir, log_dir)
	}

	/// The settings of `local?root=ROOT`, a store whose files all live below
	/// `root`.
	pub fn with_root(root: &Path) -> Self {
		let under = |path: &str| root.join(path.trim_start_matches('/'));
		Self::with_dirs(
			under(STORE_DIR),
			under("/nix/var/nix"),
			under("/nix/var/log/nix"),
		)
	}

	fn with_dirs(real_store_dir: PathBuf, state_dir: PathBuf, log_dir: PathBuf) -> Self {
		Self {
			real_store_dir,
			state_dir,
			log_dir,
			read_only: false,
			reserved_size: 8 * 1024 * 1024,
			fsync_metadata: true,
			use_sqlite_wal: true,
			require_sigs: true,
			trusted_public_keys: vec![
				"cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=".to_owned(),
			],
		}
	}

	pub fn db_dir(&self) -> PathBuf {
		self.state_dir.join("db")
	}
}
