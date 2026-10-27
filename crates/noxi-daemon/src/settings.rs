//! Daemon settings from `nix.conf`, with CppNix 2.35.2's defaults.

use noxi_build::settings::BuildSettings;
use noxi_store::LocalStoreConfig;
use noxi_store::binary_cache::BinaryCache;

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
	/// `keep-outputs`.
	pub gc_keep_outputs: bool,
	/// `keep-derivations`.
	pub gc_keep_derivations: bool,
	/// The version the daemon reports in the handshake. CppNix reports its
	/// own release; the default reports the reference release this daemon
	/// is compatible with, so clients that show it see a version they know.
	pub daemon_version: String,
	/// A command-line trust override, gated by `daemon-trust-override`.
	pub force_trust: Option<crate::connection::Trust>,
}

impl DaemonSettings {
	pub fn from_config(
		mut store: LocalStoreConfig,
		settings: &nix_compat::nixcpp::settings::Settings,
	) -> noxi_store::Result<Self> {
		store.apply_config(settings);
		let mut result = Self::new(store);
		result.build.apply_config(settings);
		result.substituters = settings
			.get_list("substituters", &["https://cache.nixos.org/"])
			.into_iter()
			.map(BinaryCache::open)
			.collect::<noxi_store::Result<Vec<_>>>()?;
		result.trusted_substituters = settings
			.get_list("trusted-substituters", &[])
			.into_iter()
			.map(str::to_owned)
			.collect();
		result.trusted_users = settings
			.get_list("trusted-users", &["root"])
			.into_iter()
			.map(str::to_owned)
			.collect();
		result.allowed_users = settings
			.get_list("allowed-users", &["*"])
			.into_iter()
			.map(str::to_owned)
			.collect();
		result.build_users_group = settings
			.get("build-users-group")
			.filter(|group| !group.is_empty())
			.map(str::to_owned);
		result.gc_keep_outputs = settings.get_bool("keep-outputs", result.gc_keep_outputs);
		result.gc_keep_derivations =
			settings.get_bool("keep-derivations", result.gc_keep_derivations);
		Ok(result)
	}

	pub fn new(store: LocalStoreConfig) -> Self {
		Self {
			store,
			// A daemon worker's logger does not print build output, so
			// failed builds end with the log tail, as in CppNix.
			build: BuildSettings {
				show_log_tail: true,
				..BuildSettings::default()
			},
			substituters: Vec::new(),
			trusted_substituters: Vec::new(),
			trusted_users: vec!["root".to_owned()],
			allowed_users: vec!["*".to_owned()],
			build_users_group: None,
			gc_keep_outputs: false,
			gc_keep_derivations: true,
			daemon_version: "2.35.2".to_owned(),
			force_trust: None,
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
