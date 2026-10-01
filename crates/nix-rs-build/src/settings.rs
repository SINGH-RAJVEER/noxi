//! Build settings from `nix.conf`, with CppNix 2.35.2's defaults.

use std::collections::BTreeSet;
use std::path::PathBuf;

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
			system_features: ["benchmark", "big-parallel", "kvm", "nixos-test", "uid-range"]
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
