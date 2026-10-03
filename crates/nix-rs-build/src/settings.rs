//! Build settings from `nix.conf`, with CppNix 2.35.2's defaults.

use std::collections::BTreeSet;
use std::path::PathBuf;

use nix_compat::nixcpp::settings::Settings;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxMode {
	Enabled,
	Relaxed,
	Disabled,
}

/// A path made visible in the sandbox: `target=source`, optionally
/// ignored when the source is missing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxPath {
	pub target: String,
	pub source: String,
	pub optional: bool,
}

impl SandboxPath {
	/// Parse a `sandbox-paths` entry: `PATH`, `TARGET=SOURCE`, with an
	/// optional trailing `?` on the source.
	pub fn parse(entry: &str) -> Self {
		let (target, source) = match entry.split_once('=') {
			Some((t, s)) => (t.to_owned(), s.to_owned()),
			None => (entry.trim_end_matches('?').to_owned(), entry.to_owned()),
		};
		let (source, optional) = match source.strip_suffix('?') {
			Some(s) => (s.to_owned(), true),
			None => (source, false),
		};
		SandboxPath {
			target,
			source,
			optional,
		}
	}
}

#[derive(Clone, Debug)]
pub struct BuildSettings {
	pub sandbox: SandboxMode,
	pub sandbox_fallback: bool,
	pub sandbox_paths: Vec<SandboxPath>,
	/// `build-dir`; `None` means `STATE_DIR/builds`.
	pub build_dir: Option<PathBuf>,
	pub sandbox_build_dir: String,
	pub sandbox_shm_size: String,
	pub system: String,
	pub extra_platforms: BTreeSet<String>,
	pub system_features: BTreeSet<String>,
	/// `cores`; 0 means all available.
	pub build_cores: u32,
	pub keep_failed: bool,
	pub allow_new_privileges: bool,
	pub filter_syscalls: bool,
	pub impure_env: Vec<(String, String)>,
	pub allowed_impure_host_prefixes: Vec<String>,
	/// Seconds of silence or total time after which a build is killed; 0 disables.
	pub max_silent_time: u64,
	pub timeout: u64,
	/// `keep-build-log`.
	pub keep_build_log: bool,
	/// `compress-build-log`.
	pub compress_build_log: bool,
	/// `log-lines`: how many lines of a failed build's log an error shows.
	pub log_lines: usize,
	/// Whether the `nix-command` experimental feature is enabled, which
	/// changes the log command failed builds suggest.
	pub nix_command: bool,
	/// Whether a failed build's error ends with the last `log_lines` lines
	/// of its log. CppNix does this unless its logger already prints build
	/// output, as the legacy commands and `--print-build-logs` do.
	pub show_log_tail: bool,
	/// `always-allow-substitutes`: substitute even derivations with
	/// `allowSubstitutes = false`.
	pub always_allow_substitutes: bool,
}

/// The sandbox shell compiled into this build, like CppNix's
/// `SANDBOX_SHELL`.
pub const SANDBOX_SHELL: Option<&str> = option_env!("NIXRS_SANDBOX_SHELL");

impl Default for BuildSettings {
	fn default() -> Self {
		Self {
			sandbox: SandboxMode::Enabled,
			sandbox_fallback: true,
			sandbox_paths: SANDBOX_SHELL
				.map(|sh| vec![SandboxPath::parse(&format!("/bin/sh={sh}"))])
				.unwrap_or_default(),
			build_dir: None,
			sandbox_build_dir: "/build".to_owned(),
			sandbox_shm_size: "50%".to_owned(),
			system: "x86_64-linux".to_owned(),
			extra_platforms: [
				"i686-linux",
				"x86_64-v1-linux",
				"x86_64-v2-linux",
				"x86_64-v3-linux",
				"x86_64-v4-linux",
			]
			.map(String::from)
			.into(),
			system_features: [
				"benchmark",
				"big-parallel",
				"kvm",
				"nixos-test",
				"uid-range",
			]
			.map(String::from)
			.into(),
			build_cores: 0,
			keep_failed: false,
			allow_new_privileges: false,
			filter_syscalls: true,
			impure_env: Vec::new(),
			allowed_impure_host_prefixes: Vec::new(),
			max_silent_time: 0,
			timeout: 0,
			keep_build_log: true,
			compress_build_log: true,
			log_lines: 25,
			nix_command: false,
			show_log_tail: false,
			always_allow_substitutes: false,
		}
	}
}

impl BuildSettings {
	/// The value of `NIX_BUILD_CORES`.
	pub fn effective_cores(&self) -> u32 {
		if self.build_cores != 0 {
			return self.build_cores;
		}
		std::thread::available_parallelism()
			.map(|n| n.get() as u32)
			.unwrap_or(1)
	}

	/// Whether this machine can build for `system`.
	pub fn can_build_platform(&self, system: &str) -> bool {
		system == self.system || self.extra_platforms.contains(system)
	}
}

impl BuildSettings {
	/// The build settings of a loaded configuration, with CppNix's defaults
	/// for those that are not set.
	pub fn from_settings(settings: &Settings) -> Self {
		let default = BuildSettings::default();
		let list = |name: &str, default: &BTreeSet<String>| -> BTreeSet<String> {
			let default: Vec<&str> = default.iter().map(String::as_str).collect();
			settings
				.get_list(name, &default)
				.into_iter()
				.map(str::to_owned)
				.collect()
		};
		let default_paths: Vec<String> = default
			.sandbox_paths
			.iter()
			.map(|p| {
				format!(
					"{}={}{}",
					p.target,
					p.source,
					if p.optional { "?" } else { "" }
				)
			})
			.collect();
		let default_paths: Vec<&str> = default_paths.iter().map(String::as_str).collect();
		let uint = |name: &str, default: u64| settings.get_int(name, default as i64).max(0) as u64;
		BuildSettings {
			sandbox: match settings.get("sandbox") {
				Some("false") => SandboxMode::Disabled,
				Some("relaxed") => SandboxMode::Relaxed,
				Some(_) => SandboxMode::Enabled,
				None => default.sandbox,
			},
			sandbox_fallback: settings.get_bool("sandbox-fallback", default.sandbox_fallback),
			sandbox_paths: settings
				.get_list("sandbox-paths", &default_paths)
				.into_iter()
				.map(SandboxPath::parse)
				.collect(),
			build_dir: settings
				.get("build-dir")
				.filter(|d| !d.is_empty())
				.map(PathBuf::from),
			sandbox_build_dir: settings
				.get("sandbox-build-dir")
				.map_or(default.sandbox_build_dir.clone(), str::to_owned),
			sandbox_shm_size: settings
				.get("sandbox-dev-shm-size")
				.map_or(default.sandbox_shm_size.clone(), str::to_owned),
			system: settings
				.get("system")
				.map_or(default.system.clone(), str::to_owned),
			extra_platforms: list("extra-platforms", &default.extra_platforms),
			system_features: list("system-features", &default.system_features),
			build_cores: uint("cores", default.build_cores.into()) as u32,
			keep_failed: settings.get_bool("keep-failed", default.keep_failed),
			allow_new_privileges: settings
				.get_bool("allow-new-privileges", default.allow_new_privileges),
			filter_syscalls: settings.get_bool("filter-syscalls", default.filter_syscalls),
			max_silent_time: uint("max-silent-time", default.max_silent_time),
			timeout: uint("timeout", default.timeout),
			keep_build_log: settings.get_bool("keep-build-log", default.keep_build_log),
			compress_build_log: settings.get_bool("compress-build-log", default.compress_build_log),
			log_lines: uint("log-lines", default.log_lines as u64) as usize,
			nix_command: settings.experimental_features().contains("nix-command"),
			always_allow_substitutes: settings
				.get_bool("always-allow-substitutes", default.always_allow_substitutes),
			..default
		}
	}
}
