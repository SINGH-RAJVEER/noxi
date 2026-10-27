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
	/// Positive and negative HTTP narinfo lifetimes, in seconds.
	pub narinfo_cache_positive_ttl: u64,
	pub narinfo_cache_negative_ttl: u64,
	/// Bounds for metadata prefetch and substitution downloads.
	pub max_substitution_jobs: usize,
	pub http_connections: usize,
	/// Whether the `ca-derivations` experimental feature is enabled, which
	/// adds the realisation tables and lets builds record realisations.
	pub ca_derivations: bool,
}

impl LocalStoreConfig {
	pub fn diverted(root: &Path) -> std::io::Result<LocalStoreConfig> {
		let root = root.canonicalize()?;
		if root == Path::new("/") || root.starts_with("/nix") || !root.is_dir() {
			return Err(std::io::Error::other(
				"writable evaluation requires a diverted store root",
			));
		}
		// Reject redirected store/state directories before LocalStore opens
		// anything. In particular, a root with nix -> /nix is not diverted.
		for relative in [
			"nix/store/.links",
			"nix/var/nix/profiles/per-user",
			"nix/var/nix/gcroots/per-user",
			"nix/var/nix/temproots",
			"nix/var/nix/gc.lock",
			"nix/var/nix/gc-socket/socket",
			"nix/var/nix/db/reserved",
			"nix/var/nix/db/big-lock",
			"nix/var/nix/db/schema",
			"nix/var/nix/db/db.sqlite",
			"nix/var/nix/db/db.sqlite-wal",
			"nix/var/nix/db/db.sqlite-shm",
			"nix/var/log/nix",
		] {
			let mut path = root.clone();
			for component in Path::new(relative).components() {
				path.push(component);
				match std::fs::symlink_metadata(&path) {
					Ok(meta) if meta.file_type().is_symlink() => {
						return Err(std::io::Error::other(
							"diverted store directories must not be symlinks",
						));
					}
					Ok(_) => {}
					Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
					Err(e) => return Err(e),
				}
			}
		}
		Ok(Self::with_root(&root))
	}

	/// Apply local-store settings after selecting its filesystem root.
	pub fn apply_config(&mut self, settings: &nix_compat::nixcpp::settings::Settings) {
		self.reserved_size =
			settings.get_int("gc-reserved-space", self.reserved_size as i64) as u64;
		self.fsync_metadata = settings.get_bool("fsync-metadata", self.fsync_metadata);
		self.use_sqlite_wal = settings.get_bool("use-sqlite-wal", self.use_sqlite_wal);
		self.require_sigs = settings.get_bool("require-sigs", self.require_sigs);
		self.narinfo_cache_positive_ttl = settings
			.get_int(
				"narinfo-cache-positive-ttl",
				self.narinfo_cache_positive_ttl as i64,
			)
			.max(0) as u64;
		self.narinfo_cache_negative_ttl = settings
			.get_int(
				"narinfo-cache-negative-ttl",
				self.narinfo_cache_negative_ttl as i64,
			)
			.max(0) as u64;
		self.max_substitution_jobs = settings
			.get_int("max-substitution-jobs", self.max_substitution_jobs as i64)
			.max(1) as usize;
		self.http_connections = settings
			.get_int("http-connections", self.http_connections as i64)
			.max(1) as usize;
		self.ca_derivations = settings.experimental_features().contains("ca-derivations");
		if settings.get("trusted-public-keys").is_some() {
			self.trusted_public_keys = settings
				.get_list("trusted-public-keys", &[])
				.into_iter()
				.map(str::to_owned)
				.collect();
		}
		if settings.get("experimental-features").is_some() {
			self.ca_derivations = settings.experimental_features().contains("ca-derivations");
		}
	}

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
			ca_derivations: false,
			narinfo_cache_positive_ttl: 30 * 24 * 60 * 60,
			narinfo_cache_negative_ttl: 60 * 60,
			max_substitution_jobs: 16,
			http_connections: 25,
		}
	}

	pub fn db_dir(&self) -> PathBuf {
		self.state_dir.join("db")
	}
}
