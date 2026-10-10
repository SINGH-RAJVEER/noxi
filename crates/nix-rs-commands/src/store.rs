//! Opening the store a command uses, following CppNix 2.35.2's
//! `resolveStoreConfig`: `--store`, then the `store` setting, whose default
//! is `NIX_REMOTE`, then `auto`.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use nix_compat::nixcpp::settings::Settings;
use nix_rs_build::backend::LocalBackend;
use nix_rs_build::settings::BuildSettings;
use nix_rs_store::{LocalStore, LocalStoreConfig, Store};

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
fn client_options(settings: &Settings) -> nix_rs_daemon::client::ClientOptions {
	let max_jobs = match settings.get("max-jobs") {
		Some("auto") => std::thread::available_parallelism().map_or(1, |n| n.get() as u64),
		Some(value) => value.parse().unwrap_or(1),
		None => 1,
	};
	let overrides = settings
		.overridden()
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
	nix_rs_daemon::client::ClientOptions {
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
		read_only: bool,
	},
	Daemon {
		socket: PathBuf,
		root: Option<PathBuf>,
	},
}

/// Resolve `uri` as CppNix's store registry does for the supported kinds.
fn resolve(uri: &str) -> Result<Target, String> {
	let (scheme, query) = uri.split_once('?').unwrap_or((uri, ""));
	let mut root = None;
	let mut read_only = false;
	for param in query.split('&').filter(|p| !p.is_empty()) {
		match param.split_once('=') {
			Some(("root", value)) if !value.is_empty() => root = Some(PathBuf::from(value)),
			Some(("read-only", value)) => read_only = value == "true",
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
		"local" => Target::Local { root, read_only },
		"auto" if root.is_some() => Target::Local { root, read_only },
		"auto" => {
			let state_dir = LocalStoreConfig::system().state_dir;
			if accessible(&state_dir) || !daemon_socket().exists() {
				Target::Local { root, read_only }
			} else {
				Target::Daemon {
					socket: daemon_socket(),
					root,
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
		_ => {
			return Err(format!(
				"don't know how to open Nix store with scheme '{scheme}'"
			));
		}
	})
}

/// Open the store named by `uri`. `dummy://` and other stores that cannot
/// hold paths yield `None`.
pub fn open(uri: &str, settings: &Settings) -> Result<Option<OpenStore>, String> {
	if let Target::Daemon { socket, root } = resolve(uri)? {
		let store = nix_rs_daemon::client::DaemonStore::new(
			&socket,
			root.as_deref(),
			&client_options(settings),
		);
		return Ok(Some(OpenStore {
			store: Rc::new(store),
			real_store_dir: root.map(|root| root.join("nix/store")),
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
	let backend = LocalBackend::new(store, build_settings, substituters);
	Ok(Some(OpenStore {
		store: Rc::new(backend),
		real_store_dir,
	}))
}

/// Open the local store named by `uri`, and where its paths are visible to
/// this process if that is not `/nix/store`. `dummy://` yields `None`.
pub fn open_local(
	uri: &str,
	settings: &Settings,
) -> Result<Option<(LocalStore, Option<PathBuf>)>, String> {
	let (root, read_only) = match resolve(uri)? {
		Target::Dummy => return Ok(None),
		Target::Local { root, read_only } => (root, read_only),
		Target::Daemon { .. } => {
			return Err(
				"operations through the Nix daemon other than the store interface are not supported by nix-rs yet; use a local store"
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
	config.require_sigs = settings.get_bool("require-sigs", config.require_sigs);
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
	let real_store_dir = root.as_ref().map(|_| config.real_store_dir.clone());
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

/// The configuration of `local?root=ROOT`. ROOT must exist, and neither it
/// nor its store and state directories may lead back into `/nix`, so a
/// diverted store never touches the system store.
fn diverted_config(root: &Path) -> Result<LocalStoreConfig, String> {
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
