//! Daemon settings from `nix.conf`, with CppNix 2.35.2's defaults.

use std::time::Duration;

use noxi_build::settings::BuildSettings;
use noxi_store::LocalStoreConfig;
use noxi_store::binary_cache::BinaryCache;

#[derive(Clone, Debug)]
pub struct DaemonSettings {
	/// The store the daemon owns.
	pub store: LocalStoreConfig,
	pub build: BuildSettings,
	/// `substituters`, as configured. Each connection opens them when it
	/// first needs them, as CppNix's `getDefaultSubstituters` does, so one
	/// that cannot be opened is reported to the client and skipped.
	pub substituters: Vec<String>,
	/// `trusted-substituters` and the configured `substituters`, as URLs.
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
	/// A noxi extension: when the listening sockets came from socket
	/// activation, exit after this long with no connection and no live
	/// worker, so no daemon stays resident between operations. Read from
	/// `NOXI_DAEMON_IDLE_TIMEOUT` (seconds; unset, empty or 0 disables it).
	pub idle_timeout: Option<Duration>,
}

impl DaemonSettings {
	pub fn from_config(
		mut store: LocalStoreConfig,
		settings: &nix_compat::nixcpp::settings::Settings,
	) -> noxi_store::Result<Self> {
		store.apply_config(settings);
		let mut result = Self::new(store);
		result.build.apply_config(settings);
		let substituters = settings.get_list("substituters", &["https://cache.nixos.org/"]);
		result.substituters = substituters.iter().map(|uri| (*uri).to_owned()).collect();
		// Untrusted clients may name a configured substituter even when it
		// cannot be opened, as CppNix compares against the setting.
		result.trusted_substituters = settings
			.get_list("trusted-substituters", &[])
			.into_iter()
			.chain(substituters)
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
		result.idle_timeout =
			idle_timeout(std::env::var("NOXI_DAEMON_IDLE_TIMEOUT").ok().as_deref())?;
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
			idle_timeout: None,
		}
	}

	/// Whether an untrusted client may use `url` as a substituter: it is
	/// one of `substituters` or `trusted-substituters`, or becomes one with
	/// a trailing slash added, as `ClientSettings::apply`'s `tryTrust`
	/// checks. A client URL with a slash the configured one lacks is not
	/// trusted.
	pub fn trusted_substituter(&self, url: &str) -> bool {
		let slashed = (!url.ends_with('/')).then(|| format!("{url}/"));
		self.trusted_substituters
			.iter()
			.chain(&self.substituters)
			.any(|t| t == url || slashed.as_deref() == Some(t.as_str()))
	}

	/// Open the substituters as the builder does, with the transfer and
	/// retry settings in effect and the store's narinfo cache TTLs. One that
	/// cannot be opened, such as an unreachable cache on an offline machine,
	/// is logged as CppNix's warning and skipped rather than failing the
	/// connection; the rest are sorted by priority.
	pub fn open_substituters(&self) -> Vec<BinaryCache> {
		noxi_build::backend::open_substituters(
			self.substituters.iter().map(String::as_str),
			self.build.transfer,
			&self.store,
		)
	}
}

/// Parse `NOXI_DAEMON_IDLE_TIMEOUT`: whole seconds, with unset, empty or 0
/// meaning the daemon never exits on its own.
fn idle_timeout(value: Option<&str>) -> noxi_store::Result<Option<Duration>> {
	let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
		return Ok(None);
	};
	let seconds: u64 = value.parse().map_err(|_| {
		noxi_store::Error::Other(format!(
			"invalid NOXI_DAEMON_IDLE_TIMEOUT '{value}': expected a number of seconds"
		))
	})?;
	Ok((seconds > 0).then(|| Duration::from_secs(seconds)))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn idle_timeout_parses_seconds_and_disables_on_zero() {
		assert_eq!(idle_timeout(None).unwrap(), None);
		assert_eq!(idle_timeout(Some("")).unwrap(), None);
		assert_eq!(idle_timeout(Some("0")).unwrap(), None);
		assert_eq!(
			idle_timeout(Some(" 30 ")).unwrap(),
			Some(Duration::from_secs(30))
		);
		assert!(idle_timeout(Some("5m")).is_err());
		assert!(idle_timeout(Some("-1")).is_err());
	}

	/// Daemon settings for a store in `dir` with `config` as `nix.conf`.
	fn settings(dir: &std::path::Path, config: &str) -> DaemonSettings {
		let mut settings = nix_compat::nixcpp::settings::Settings::default();
		for line in config.lines() {
			let (name, value) = line.split_once(" = ").unwrap();
			settings.set(name, value).unwrap();
		}
		DaemonSettings::from_config(LocalStoreConfig::with_root(&dir.join("root")), &settings)
			.unwrap()
	}

	#[test]
	fn substituters_that_cannot_be_opened_are_skipped_but_trusted() {
		let dir = tempfile::tempdir().unwrap();
		let local = format!("file://{}", dir.path().join("cache").display());
		let settings = settings(
			dir.path(),
			&format!("substituters = unknown://cache {local}"),
		);
		let opened = settings.open_substituters();
		assert_eq!(
			opened.iter().map(BinaryCache::uri).collect::<Vec<_>>(),
			[local.as_str()]
		);
		assert!(settings.trusted_substituter("unknown://cache"));
		// The reference only adds a missing slash to the client's URL.
		assert!(!settings.trusted_substituter(&format!("{local}/")));
	}

	#[test]
	fn substituters_are_ordered_by_priority() {
		let dir = tempfile::tempdir().unwrap();
		let cache = |name: &str, priority: u32| {
			let path = dir.path().join(name);
			std::fs::create_dir(&path).unwrap();
			std::fs::write(
				path.join("nix-cache-info"),
				format!("StoreDir: /nix/store\nPriority: {priority}\n"),
			)
			.unwrap();
			format!("file://{}", path.display())
		};
		let (slow, fast, also_fast) = (cache("slow", 40), cache("fast", 10), cache("also", 10));
		let settings = settings(
			dir.path(),
			&format!("substituters = {slow} {fast} {also_fast}"),
		);
		// Equal priorities keep their configured order.
		assert_eq!(
			settings
				.open_substituters()
				.iter()
				.map(BinaryCache::uri)
				.collect::<Vec<_>>(),
			[fast.as_str(), also_fast.as_str(), slow.as_str()]
		);
	}

	#[test]
	fn substituters_use_the_configured_transfer_settings() {
		let dir = tempfile::tempdir().unwrap();
		let local = format!("file://{}", dir.path().join("cache").display());
		let settings = settings(
			dir.path(),
			&format!(
				"substituters = {local}\nconnect-timeout = 3\nfiletransfer-retry-attempts = 2"
			),
		);
		let opened = settings.open_substituters();
		assert_eq!(opened.len(), 1);
		assert_eq!(opened[0].transfer().connect_timeout, 3);
		assert_eq!(opened[0].transfer().attempts, 2);
	}
}
