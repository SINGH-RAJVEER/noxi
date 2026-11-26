//! Opening the store a command uses, following CppNix 2.35.2's
//! `resolveStoreConfig`: `--store`, then the `store` setting, whose default
//! is `NIX_REMOTE`, then `auto`.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use nix_compat::nixcpp::settings::Settings;
use noxi_build::backend::LocalBackend;
use noxi_build::settings::BuildSettings;
use noxi_store::{LocalStore, LocalStoreConfig, Store};

/// An opened store and where its paths are visible to this process, if
/// that is not `/nix/store`.
pub struct OpenStore {
	pub store: Rc<dyn Store>,
	pub real_store_dir: Option<PathBuf>,
}

/// The store URI in effect: `uri` from `--store`, else the `store` setting,
/// else `NIX_REMOTE`, else `auto`.
pub fn store_uri(uri: Option<&str>, settings: &Settings) -> String {
	uri.map(str::to_owned)
		.or_else(|| settings.get("store").map(str::to_owned))
		.or_else(|| std::env::var("NIX_REMOTE").ok().filter(|v| !v.is_empty()))
		.unwrap_or_else(|| "auto".to_owned())
}

/// The libstore and file-transfer settings that `RemoteStore::setOptions`
/// forwards when they were set, from CppNix 2.35.2's `globals.hh`,
/// `local-settings.hh`, `worker-settings.hh` and `filetransfer.hh`, less
/// those it sends as fixed fields or never forwards.
const FORWARDED_SETTINGS: &[&str] = &[
	"allow-new-privileges",
	"allow-symlinked-store",
	"allowed-impure-host-deps",
	"always-allow-substitutes",
	"auto-allocate-uids",
	"auto-optimise-store",
	"build-dir",
	"build-hook",
	"build-poll-interval",
	"build-users-group",
	"builders",
	"builders-use-substitutes",
	"compress-build-log",
	"connect-timeout",
	"diff-hook",
	"download-buffer-size",
	"download-speed",
	"external-builders",
	"extra-platforms",
	"filetransfer-retry-attempts",
	"filetransfer-retry-delay",
	"filetransfer-retry-delay-rate-limited",
	"filetransfer-retry-jitter",
	"filetransfer-retry-max-delay",
	"filter-syscalls",
	"fsync-metadata",
	"fsync-store-paths",
	"gc-reserved-space",
	"hashed-mirrors",
	"http-connections",
	"http2",
	"http3",
	"id-count",
	"ignored-acls",
	"impersonate-linux-26",
	"impure-env",
	"keep-build-log",
	"keep-derivations",
	"keep-outputs",
	"log-lines",
	"max-build-log-size",
	"max-free",
	"max-substitution-jobs",
	"min-free",
	"min-free-check-interval",
	"nar-buffer-size",
	"narinfo-cache-meta-ttl",
	"narinfo-cache-negative-ttl",
	"narinfo-cache-positive-ttl",
	"netrc-file",
	"post-build-hook",
	"pre-build-hook",
	"print-missing",
	"require-drop-supplementary-groups",
	"require-sigs",
	"run-diff-hook",
	"sandbox",
	"sandbox-build-dir",
	"sandbox-dev-shm-size",
	"sandbox-fallback",
	"sandbox-paths",
	"secret-key-files",
	"ssl-cert-file",
	"stalled-download-timeout",
	"start-id",
	"store",
	"substituters",
	"sync-before-registering",
	"system",
	"system-features",
	"timeout",
	"trusted-public-keys",
	"trusted-substituters",
	"use-cgroups",
	"use-sqlite-wal",
	"use-xdg-base-directories",
	"user-agent-suffix",
	"warn-large-path-threshold",
];

/// The defaults that `extra-NAME` extends, for the forwarded list settings
/// whose default is not empty.
fn list_default(name: &str) -> &'static str {
	match name {
		"build-hook" => "nix __build-remote",
		"ignored-acls" => "security.csm security.selinux system.nfs4_acl",
		"substituters" => "https://cache.nixos.org/",
		"trusted-public-keys" => "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=",
		_ => "",
	}
}

/// What `RemoteStore::setOptions` sends for `settings`.
pub(crate) fn client_options(settings: &Settings) -> noxi_daemon::client::ClientOptions {
	let max_jobs = match settings.get("max-jobs") {
		Some("auto") => std::thread::available_parallelism().map_or(1, |n| n.get() as u64),
		Some(value) => value.parse().unwrap_or(1),
		None => 1,
	};
	let overrides = settings
		.forwarded()
		.filter(|(name, _, _)| FORWARDED_SETTINGS.contains(name))
		.map(|(name, value, replaces_default)| {
			let value = if replaces_default {
				value.to_owned()
			} else {
				[list_default(name), value]
					.into_iter()
					.filter(|part| !part.is_empty())
					.collect::<Vec<_>>()
					.join(" ")
			};
			(name.to_owned(), value)
		})
		.collect();
	noxi_daemon::client::ClientOptions {
		keep_failed: settings.get_bool("keep-failed", false),
		keep_going: settings.get_bool("keep-going", false),
		try_fallback: settings.get_bool("fallback", false),
		max_build_jobs: max_jobs,
		max_silent_time: settings.get_int("max-silent-time", 0).max(0) as u64,
		build_cores: settings.get_int("cores", 0).max(0) as u64,
		use_substitutes: settings.get_bool("substitute", true),
		verbosity: nix_compat::log::verbosity(),
		verbose_build: nix_compat::log::print_build_logs(),
		overrides,
		..Default::default()
	}
}

/// Where a store URI leads.
enum Target {
	Dummy,
	Local {
		root: Option<PathBuf>,
		real: Option<PathBuf>,
		read_only: bool,
		/// The `require-sigs` store parameter, which overrides the setting.
		require_sigs: Option<bool>,
	},
	Daemon {
		socket: PathBuf,
		root: Option<PathBuf>,
	},
	Ssh(nix_compat::nixcpp::store_reference::StoreReference),
	/// A `file://` binary cache.
	Cache,
	Overlay,
}

/// A store's `store` setting: the logical store directory, which this
/// process reads once from `NIX_STORE_DIR` the first time it needs it.
fn set_store_dir(dir: &str) -> Result<(), String> {
	let dir = nix_compat::nixcpp::url::percent_decode(dir).map_err(|e| e.to_string())?;
	nix_compat::store_path::configure_store_dir(&dir)
}

/// Resolve `uri` as CppNix's store registry does for the supported kinds.
fn resolve(uri: &str) -> Result<Target, String> {
	if noxi_store::local_overlay::is_overlay(uri) {
		return Ok(Target::Overlay);
	}
	if uri.starts_with("file://") {
		return Ok(Target::Cache);
	}
	let (scheme, query) = uri.split_once('?').unwrap_or((uri, ""));
	if matches!(
		scheme,
		"ssh-ng://localhost" | "ssh://localhost" | "mounted-ssh-ng://localhost"
	) {
		let reference = parse_reference(uri)?;
		if let Some(dir) = reference.params.get("store") {
			nix_compat::store_path::configure_store_dir(dir)?;
		}
		return Ok(Target::Ssh(reference));
	}
	let dummy = matches!(scheme, "dummy" | "dummy://");
	let mut root = None;
	let mut real = None;
	let mut read_only = false;
	let mut require_sigs = None;
	for param in query.split('&').filter(|p| !p.is_empty()) {
		match param.split_once('=') {
			Some(("root", value)) if !value.is_empty() => {
				root = Some(PathBuf::from(
					nix_compat::nixcpp::url::percent_decode(value).map_err(|e| e.to_string())?,
				))
			}
			Some(("read-only", value)) => read_only = value == "true",
			Some(("require-sigs", value)) => {
				require_sigs = Some(match value {
					"true" => true,
					"false" => false,
					_ => {
						return Err(format!(
							"Boolean setting 'require-sigs' has invalid value '{value}'"
						));
					}
				})
			}
			Some(("store", value)) => set_store_dir(value)?,
			Some(("real", value)) if !value.is_empty() => real = Some(absolute_parameter(value)?),
			// The dummy store has no other settings; CppNix warns about them.
			Some((name, _)) if dummy => {
				// The store is opened once; its URI may be resolved more often.
				static WARNED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
				let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
				if !warned.iter().any(|w| w == name) {
					warned.push(name.to_owned());
					if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn) {
						eprintln!("warning: unknown setting '{name}'");
					}
				}
			}
			_ => return Err(format!("unsupported store parameter '{param}' in '{uri}'")),
		}
	}
	let daemon_socket = || {
		std::env::var_os("NIX_DAEMON_SOCKET_PATH")
			.filter(|path| !path.is_empty())
			.map(PathBuf::from)
			.unwrap_or_else(|| {
				LocalStoreConfig::system()
					.state_dir
					.join("daemon-socket/socket")
			})
	};
	Ok(match scheme {
		"dummy" | "dummy://" => Target::Dummy,
		"local" | "local://" => Target::Local {
			root,
			real,
			read_only,
			require_sigs,
		},
		"auto" if root.is_some() => Target::Local {
			root,
			real,
			read_only,
			require_sigs,
		},
		"auto" => {
			let state_dir = LocalStoreConfig::system().state_dir;
			if accessible(&state_dir) {
				Target::Local {
					root,
					real,
					read_only,
					require_sigs,
				}
			} else if daemon_socket().exists() {
				Target::Daemon {
					socket: daemon_socket(),
					root,
				}
			} else {
				Target::Local {
					root: chroot_store_fallback(&state_dir, query.is_empty()),
					real,
					read_only,
					require_sigs,
				}
			}
		}
		"daemon" | "unix" | "unix://" => Target::Daemon {
			socket: daemon_socket(),
			root,
		},
		_ if scheme.starts_with("unix://") => Target::Daemon {
			socket: PathBuf::from(&scheme["unix://".len()..]),
			root,
		},
		// `local://PATH`: a local store rooted at the decoded PATH, unless
		// `root` is given (`LocalFSStoreConfig`).
		_ if scheme.starts_with("local://") => Target::Local {
			root: match root {
				Some(root) => Some(root),
				None => Some(absolute_parameter(&scheme["local://".len()..])?),
			},
			real,
			read_only,
			require_sigs,
		},
		// `StoreReference::parse`: a plain path is a local store rooted there.
		_ if !scheme.contains("://") && scheme.contains('/') => Target::Local {
			root: Some(crate::instantiate::absolute_path(
				&std::env::current_dir().unwrap_or_default(),
				Path::new(scheme),
			)),
			real,
			read_only,
			require_sigs,
		},
		_ => {
			return Err(format!(
				"don't know how to open Nix store with scheme '{scheme}'"
			));
		}
	})
}

/// Whether `uri` resolves to a local store this process opens itself, whose
/// paths it can read straight from disk. `auto` does when the state
/// directory is writable, as for root on NixOS, and in the chroot store
/// fallback; CppNix then reads paths through `LocalStore`'s accessor.
pub fn resolves_to_local(uri: &str) -> Result<bool, String> {
	Ok(matches!(
		resolve(uri)?,
		Target::Local { .. } | Target::Overlay
	))
}

/// The `file://` binary cache `uri` names, if it names one.
pub fn cache(uri: &str) -> Result<Option<noxi_store::binary_cache::BinaryCache>, String> {
	match resolve(uri)? {
		Target::Cache => noxi_store::binary_cache::BinaryCache::open(uri)
			.map(Some)
			.map_err(|e| e.to_string()),
		_ => Ok(None),
	}
}

fn absolute_parameter(value: &str) -> Result<PathBuf, String> {
	let value = nix_compat::nixcpp::url::percent_decode(value).map_err(|e| e.to_string())?;
	let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
	Ok(crate::instantiate::absolute_path(&cwd, Path::new(&value)))
}

fn parse_reference(
	uri: &str,
) -> Result<nix_compat::nixcpp::store_reference::StoreReference, String> {
	use nix_compat::nixcpp::store_reference::StoreReference;
	let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
	let mut warnings = Vec::new();
	let reference = StoreReference::parse(uri, &Default::default(), &cwd, &mut warnings)
		.map_err(|e| e.to_string())?;
	for warning in warnings {
		nix_compat::log::logger::warn(&warning);
	}
	Ok(reference)
}

pub enum RemoteStore {
	Worker(noxi_daemon::client::DaemonStore),
	Serve {
		store: noxi_daemon::legacy_ssh::LegacySshStore,
		uri: String,
	},
}

impl RemoteStore {
	pub fn uri(&self) -> &str {
		match self {
			Self::Worker(store) => store.uri(),
			Self::Serve { uri, .. } => uri,
		}
	}

	pub fn connection_info(&self) -> noxi_store::Result<noxi_daemon::client::ConnectionInfo> {
		match self {
			Self::Worker(store) => store.connection_info(),
			Self::Serve { store, .. } => {
				noxi_daemon::peer::Peer::connect(store)?;
				Ok(Default::default())
			}
		}
	}

	fn into_store(self) -> Rc<dyn Store> {
		match self {
			Self::Worker(store) => Rc::new(store),
			Self::Serve { store, .. } => Rc::new(store),
		}
	}
}

fn remote_store(
	target: Target,
	settings: &Settings,
) -> Result<Option<(RemoteStore, Option<PathBuf>)>, String> {
	let options = || client_options(settings);
	let store = match target {
		Target::Daemon { socket, root } => (
			RemoteStore::Worker(noxi_daemon::client::DaemonStore::new(
				&socket,
				root.as_deref(),
				&options(),
			)),
			root.map(|root| root.join("nix/store")),
		),
		Target::Ssh(reference) => {
			let uri = reference.render(false);
			let remote = match crate::machine::ssh_store(&reference, settings)? {
				crate::machine::SshStore::Legacy(store) => RemoteStore::Serve { store, uri },
				crate::machine::SshStore::Ng(store) => RemoteStore::Worker(store),
			};
			(remote, None)
		}
		_ => return Ok(None),
	};
	Ok(Some(store))
}

pub fn open_remote(uri: &str, settings: &Settings) -> Result<Option<RemoteStore>, String> {
	Ok(remote_store(resolve(uri)?, settings)?.map(|(store, _)| store))
}

/// Open the store named by `uri`. `dummy://` yields `None`.
pub fn open(uri: &str, settings: &Settings) -> Result<Option<OpenStore>, String> {
	if let Some((store, real_store_dir)) = remote_store(resolve(uri)?, settings)? {
		return Ok(Some(OpenStore {
			store: store.into_store(),
			real_store_dir,
		}));
	}
	if let Target::Cache = resolve(uri)? {
		let features = settings.experimental_features();
		let store = noxi_build::cache_store::CacheStore::open(
			uri,
			noxi_store::build_derivation::OutputFeatures {
				ca_derivations: features.contains("ca-derivations"),
				impure_derivations: features.contains("impure-derivations"),
				dynamic_derivations: features.contains("dynamic-derivations"),
				git_hashing: features.contains("git-hashing"),
			},
		)
		.map_err(|e| e.to_string())?;
		let mut build_settings = BuildSettings::from_settings(settings);
		build_settings.show_log_tail = !nix_compat::log::print_build_logs();
		build_settings.store_uri = rendered_reference(uri);
		let store = store.with_build_settings(build_settings);
		return Ok(Some(OpenStore {
			store: Rc::new(store),
			real_store_dir: None,
		}));
	}
	let Some((store, real_store_dir)) = open_local(uri, settings)? else {
		return Ok(None);
	};
	let substituters = if settings.get_bool("substitute", true) {
		settings
			.get_list("substituters", &["https://cache.nixos.org/"])
			.into_iter()
			.map(str::to_owned)
			.collect()
	} else {
		Vec::new()
	};
	let mut build_settings = BuildSettings::from_settings(settings);
	// The simple logger is verbose, so failed builds omit their log tail,
	// unless `--no-build-output` hides the build output.
	build_settings.show_log_tail = !nix_compat::log::print_build_logs();
	build_settings.store_uri = rendered_reference(uri);
	let backend = LocalBackend::new(store, build_settings, substituters);
	Ok(Some(OpenStore {
		store: Rc::new(backend),
		real_store_dir,
	}))
}

/// The store setting as CppNix prints it.
fn rendered_reference(uri: &str) -> Option<String> {
	let cwd = std::env::current_dir().ok()?;
	nix_compat::nixcpp::store_reference::StoreReference::parse(
		uri,
		&Default::default(),
		&cwd,
		&mut Vec::new(),
	)
	.ok()
	.map(|reference| reference.render(true))
}

/// Open the local store named by `uri`, and where its paths are visible to
/// this process if that is not `/nix/store`. `dummy://` yields `None`.
pub fn open_local(
	uri: &str,
	settings: &Settings,
) -> Result<Option<(LocalStore, Option<PathBuf>)>, String> {
	let (root, real, read_only, require_sigs) = match resolve(uri)? {
		Target::Overlay => {
			return noxi_store::local_overlay::open(uri, settings)
				.map(|store| {
					store.map(|store| {
						let real = store.config().real_store_dir.clone();
						(store, Some(real))
					})
				})
				.map_err(|error| error.to_string());
		}
		Target::Dummy => return Ok(None),
		Target::Local {
			root,
			real,
			read_only,
			require_sigs,
		} => (root, real, read_only, require_sigs),
		Target::Cache => {
			return Err(format!(
				"operations that need a local store are not supported by store '{uri}'"
			));
		}
		Target::Daemon { .. } | Target::Ssh(_) => {
			return Err(
				"operations through the Nix daemon other than the store interface are not supported by noxi yet; use a local store"
					.to_owned(),
			);
		}
	};
	let mut config = match &root {
		Some(root) => diverted_config(root)?,
		None => LocalStoreConfig::system(),
	};
	config.apply_config(settings);
	config.read_only = read_only;
	// CppNix names the store `local://` only when its URI has parameters.
	config.has_params = uri
		.split_once('?')
		.is_some_and(|(_, query)| !query.is_empty());
	config.require_sigs = settings.get_bool("require-sigs", config.require_sigs);
	if let Some(require_sigs) = require_sigs {
		config.require_sigs = require_sigs;
	}
	let default_keys: Vec<&str> = config
		.trusted_public_keys
		.iter()
		.map(String::as_str)
		.collect();
	config.trusted_public_keys = settings
		.get_list("trusted-public-keys", &default_keys)
		.into_iter()
		.map(str::to_owned)
		.collect();
	config.use_sqlite_wal = settings.get_bool("use-sqlite-wal", config.use_sqlite_wal);
	config.fsync_metadata = settings.get_bool("fsync-metadata", config.fsync_metadata);
	let has_real_dir = root.is_some() || real.is_some();
	if let Some(real) = real {
		config.real_store_dir = real;
	}
	let real_store_dir = has_real_dir.then(|| config.real_store_dir.clone());
	let store = LocalStore::open(config).map_err(|e| e.to_string())?;
	Ok(Some((store, real_store_dir)))
}

/// CppNix's `auto` test: whether the state directory is readable and
/// writable by this process.
fn accessible(dir: &Path) -> bool {
	let Ok(path) = CString::new(dir.as_os_str().as_bytes()) else {
		return false;
	};
	// SAFETY: `path` is a valid NUL-terminated string.
	unsafe { libc::access(path.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
}

/// `resolveStoreConfig`'s Linux fallback for `auto`: when the state
/// directory does not exist, no daemon socket does either, the URI has no
/// parameters, the user is not root and neither `NIX_STORE_DIR` nor
/// `NIX_STATE_DIR` is set, use `DATA_DIR/root` as a chroot store. Paths keep
/// their `/nix/store` names, so binary caches still apply, and no daemon or
/// privilege is involved. A root that cannot be created falls back to the
/// system store, as in CppNix.
fn chroot_store_fallback(state_dir: &Path, no_params: bool) -> Option<PathBuf> {
	if !cfg!(target_os = "linux")
		|| !no_params
		|| std::fs::symlink_metadata(state_dir).is_ok()
		// SAFETY: getuid cannot fail.
		|| unsafe { libc::getuid() } == 0
		|| std::env::var_os("NIX_STORE_DIR").is_some()
		|| std::env::var_os("NIX_STATE_DIR").is_some()
	{
		return None;
	}
	let root = data_dir().join("root");
	let level = if std::fs::symlink_metadata(&root).is_ok() {
		nix_compat::log::VerbosityLevel::Debug
	} else {
		std::fs::create_dir_all(&root).ok()?;
		nix_compat::log::VerbosityLevel::Warn
	};
	if nix_compat::log::log_enabled(level) {
		eprintln!(
			"\"{}\" does not exist, so Nix will use \"{}\" as a chroot store",
			state_dir.display(),
			root.display()
		);
	}
	Some(root)
}

/// `getDataDir`: `NIX_DATA_HOME`, else `XDG_DATA_HOME/nix`, else
/// `~/.local/share/nix`.
fn data_dir() -> PathBuf {
	let env = |name| {
		std::env::var_os(name)
			.filter(|value| !value.is_empty())
			.map(PathBuf::from)
	};
	env("NIX_DATA_HOME").unwrap_or_else(|| {
		env("XDG_DATA_HOME")
			.unwrap_or_else(|| noxi_store::profiles::home_dir().join(".local/share"))
			.join("nix")
	})
}

/// The configuration of `local?root=ROOT`, creating ROOT if missing. Neither it
/// nor its store and state directories may lead back into `/nix`, so a
/// diverted store never touches the system store.
fn diverted_config(root: &Path) -> Result<LocalStoreConfig, String> {
	// CppNix creates a missing root, as it does every store directory.
	if noxi_store::config::creatable_root(root) {
		std::fs::create_dir_all(root)
			.map_err(|e| format!("creating directory '{}': {e}", root.display()))?;
	}
	let root = root
		.canonicalize()
		.map_err(|e| format!("cannot open store root '{}': {e}", root.display()))?;
	if root == Path::new("/") || root.starts_with("/nix") || !root.is_dir() {
		return Err(format!(
			"store root '{}' must be a directory outside /nix",
			root.display()
		));
	}
	for relative in ["nix", "nix/store", "nix/var", "nix/var/nix", "nix/var/log"] {
		if std::fs::symlink_metadata(root.join(relative)).is_ok_and(|m| m.file_type().is_symlink())
		{
			return Err(format!(
				"store directory '{}' must not be a symlink",
				root.join(relative).display()
			));
		}
	}
	Ok(LocalStoreConfig::with_root(&root))
}
