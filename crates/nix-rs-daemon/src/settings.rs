//! Daemon settings from `nix.conf`, with CppNix 2.35.2's defaults.

use nix_rs_build::settings::BuildSettings;
use nix_rs_store::LocalStoreConfig;
use nix_rs_store::binary_cache::BinaryCache;

#[derive(Clone, Debug)]
pub struct DaemonSettings {
	/// The store the daemon owns.
	pub store: LocalStoreConfig,
	pub build: BuildSettings,
	/// `substituters`.
	pub substituters: Vec<BinaryCache>,
	/// `trusted-substituters`, as URLs.
	pub trusted_substituters: Vec<String>,
	/// `trusted-users`: names, `@group`, or `*`.
	pub trusted_users: Vec<String>,
	/// `allowed-users`.
	pub allowed_users: Vec<String>,
	/// `build-users-group`, whose members may not connect.
	pub build_users_group: Option<String>,
	/// The version the daemon reports in the handshake. CppNix reports its
	/// own release; the default reports the reference release this daemon
	/// is compatible with, so clients that show it see a version they know.
	pub daemon_version: String,
}

impl DaemonSettings {
	pub fn new(store: LocalStoreConfig) -> Self {
		Self {
			store,
			build: BuildSettings::default(),
			substituters: Vec::new(),
			trusted_substituters: Vec::new(),
			trusted_users: vec!["root".to_owned()],
			allowed_users: vec!["*".to_owned()],
			build_users_group: None,
			daemon_version: "2.35.2".to_owned(),
		}
	}

	/// Whether an untrusted client may use `url` as a substituter: it is
	/// one of `substituters` or `trusted-substituters`, with or without a
	/// trailing slash.
	pub fn trusted_substituter(&self, url: &str) -> bool {
		let trimmed = url.trim_end_matches('/');
		self.trusted_substituters
			.iter()
			.map(String::as_str)
			.chain(self.substituters.iter().map(BinaryCache::uri))
			.any(|t| t.trim_end_matches('/') == trimmed)
	}
}
