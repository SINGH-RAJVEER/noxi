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

/// Open the store named by `uri`. `dummy://` and other stores that cannot
/// hold paths yield `None`.
pub fn open(uri: &str, settings: &Settings) -> Result<Option<OpenStore>, String> {
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
	let local = match scheme {
		"dummy" | "dummy://" => return Ok(None),
		"local" => true,
		"auto" if root.is_some() => true,
		"auto" => {
			let state_dir = LocalStoreConfig::system().state_dir;
			if accessible(&state_dir) {
				true
			} else if state_dir.join("daemon-socket/socket").exists() {
				false
			} else {
				true
			}
		}
		"daemon" | "unix" => false,
		_ if scheme.starts_with("unix://") => false,
		_ => {
			return Err(format!(
				"don't know how to open Nix store with scheme '{scheme}'"
			));
		}
	};
	if !local {
		return Err(
			"connecting to the Nix daemon is not supported by nix-rs yet; use a local store"
				.to_owned(),
		);
	}

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
	let substituters = if settings.get_bool("substitute", true) {
		settings
			.get_list("substituters", &["https://cache.nixos.org/"])
			.into_iter()
			.map(str::to_owned)
			.collect()
	} else {
		Vec::new()
	};
	let backend = LocalBackend::new(store, BuildSettings::from_settings(settings), substituters);
	Ok(Some(OpenStore {
		store: Rc::new(backend),
		real_store_dir,
	}))
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
