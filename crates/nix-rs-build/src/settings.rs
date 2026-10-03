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
	pub keep_going: bool,
	pub try_fallback: bool,
	pub use_substitutes: bool,
	pub max_build_jobs: u64,
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
			keep_going: false,
			try_fallback: false,
			use_substitutes: true,
			max_build_jobs: 1,
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
	/// Apply the shared configuration loader's typed values. Fields absent
	/// from the configuration retain their existing defaults.
	pub fn apply_config(&mut self, settings: &Settings) {
		self.keep_failed = settings.get_bool("keep-failed", self.keep_failed);
		self.keep_going = settings.get_bool("keep-going", self.keep_going);
		self.try_fallback = settings.get_bool("fallback", self.try_fallback);
		self.use_substitutes = settings.get_bool("substitute", self.use_substitutes);
		self.build_cores = settings.get_int("cores", self.build_cores.into()) as u32;
		self.max_silent_time =
			settings.get_int("max-silent-time", self.max_silent_time as i64) as u64;
		self.timeout = settings.get_int("timeout", self.timeout as i64) as u64;
		self.keep_build_log = settings.get_bool("keep-build-log", self.keep_build_log);
		self.compress_build_log = settings.get_bool("compress-build-log", self.compress_build_log);
		self.log_lines = settings.get_int("log-lines", self.log_lines as i64) as usize;
		self.sandbox_fallback = settings.get_bool("sandbox-fallback", self.sandbox_fallback);
		self.allow_new_privileges =
			settings.get_bool("allow-new-privileges", self.allow_new_privileges);
		self.filter_syscalls = settings.get_bool("filter-syscalls", self.filter_syscalls);
		self.always_allow_substitutes =
			settings.get_bool("always-allow-substitutes", self.always_allow_substitutes);
		if settings.get("experimental-features").is_some() {
			self.nix_command = settings.experimental_features().contains("nix-command");
		}
		if let Some(value) = settings.get("max-jobs") {
			self.max_build_jobs = if value == "auto" {
				std::thread::available_parallelism().map_or(1, |n| n.get() as u64)
			} else {
				value.parse().unwrap_or(self.max_build_jobs)
			};
		}
		if let Some(value) = settings.get("sandbox") {
			self.sandbox = match value {
				"false" => SandboxMode::Disabled,
				"relaxed" => SandboxMode::Relaxed,
				_ => SandboxMode::Enabled,
			};
		}
		if settings.get("sandbox-paths").is_some() {
			self.sandbox_paths = settings
				.get_list("sandbox-paths", &[])
				.into_iter()
				.map(SandboxPath::parse)
				.collect();
		}
		if let Some(value) = settings.get("build-dir") {
			self.build_dir = (!value.is_empty()).then(|| PathBuf::from(value));
		}
		for (name, target) in [
			("system", &mut self.system),
			("sandbox-build-dir", &mut self.sandbox_build_dir),
			("sandbox-shm-size", &mut self.sandbox_shm_size),
		] {
			if let Some(value) = settings.get(name) {
				*target = value.to_owned();
			}
		}
		for (name, target) in [
			("extra-platforms", &mut self.extra_platforms),
			("system-features", &mut self.system_features),
		] {
			if settings.get(name).is_some() {
				*target = settings
					.get_list(name, &[])
					.into_iter()
					.map(str::to_owned)
					.collect();
			}
		}
	}

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
		let mut result = BuildSettings {
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
		};
		result.apply_config(settings);
		result
	}
}
