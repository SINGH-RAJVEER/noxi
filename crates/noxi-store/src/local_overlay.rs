//! Local stores backed by an externally mounted OverlayFS.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};

use nix_compat::nixcpp::settings::Settings;
use nix_compat::store_path::StorePath;
use path_clean::PathClean;

use crate::{Error, LocalStore, LocalStoreConfig, Result, gc::delete_counting};

pub(crate) struct Overlay {
	pub lower: LocalStore,
	upper: PathBuf,
	hook: Option<PathBuf>,
	remount_required: AtomicBool,
}

/// Open a local-overlay URL. Other schemes return `None` so a caller can
/// continue resolving stores with its usual registry.
pub fn open(uri: &str, settings: &Settings) -> Result<Option<LocalStore>> {
	let (base, query) = uri.split_once('?').unwrap_or((uri, ""));
	if base != "local-overlay://" {
		return Ok(None);
	}
	if !settings
		.experimental_features()
		.contains("local-overlay-store")
	{
		return Err(Error::Other(
			"experimental Nix feature 'local-overlay-store' is disabled; add '--extra-experimental-features local-overlay-store' to enable it".into(),
		));
	}
	let params = parameters(query);
	let config = local_config(params.get("root").copied(), &params, settings)?;
	let lower_uri = params.get("lower-store").copied().unwrap_or("");
	let (lower_base, lower_query) = lower_uri.split_once('?').unwrap_or((lower_uri, ""));
	let lower_params = parameters(lower_query);
	let lower_root = match lower_base {
		"" | "auto" | "local" | "local://" => lower_params.get("root").copied(),
		path if path.starts_with('/') || path.starts_with("./") || path.starts_with("../") => {
			Some(path)
		}
		_ => {
			return Err(Error::Other(format!(
				"store '{lower_uri}' is not a local filesystem store"
			)));
		}
	};
	let lower_config = local_config(lower_root, &lower_params, settings)?;
	let upper = params.get("upper-layer").ok_or_else(|| {
		Error::Other(format!(
			"overlay store at {:?} requires the 'upper-layer' setting",
			config.real_store_dir
		))
	})?;
	let upper = absolute(upper)?;
	match params.get("check-mount").copied().unwrap_or("true") {
		"true" | "yes" | "1" => check_mount(&config.real_store_dir, &lower_config.real_store_dir, &upper)?,
		"false" | "no" | "0" => {}
		value => {
			return Err(Error::Other(format!(
				"Boolean setting 'check-mount' has invalid value '{value}'"
			)));
		}
	}
	let lower_dir = &lower_config.real_store_dir;
	for path in [&config.real_store_dir, &upper] {
		let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
		let lower = lower_dir.canonicalize().unwrap_or_else(|_| lower_dir.clone());
		if canonical.starts_with(&lower) || lower.starts_with(&canonical) {
			return Err(invalid_mount(&config.real_store_dir));
		}
	}
	let hook = params
		.get("remount-hook")
		.map(|path| absolute(path))
		.transpose()?;
	let lower = LocalStore::open(lower_config)?;
	let overlay = Overlay {
		lower,
		upper,
		hook,
		remount_required: AtomicBool::new(false),
	};
	LocalStore::open_with_overlay(config, Some(Box::new(overlay))).map(Some)
}

fn parameters(query: &str) -> std::collections::BTreeMap<&str, &str> {
	query.split('&').filter_map(|p| p.split_once('=')).collect()
}

fn absolute(path: &str) -> Result<PathBuf> {
	Ok(std::path::absolute(path)?.clean())
}

fn local_config(
	root: Option<&str>,
	params: &std::collections::BTreeMap<&str, &str>,
	settings: &Settings,
) -> Result<LocalStoreConfig> {
	let mut config = match root {
		Some(root) => LocalStoreConfig::with_root(&absolute(root)?),
		None => LocalStoreConfig::system(),
	};
	config.apply_config(settings);
	config.read_only = matches!(params.get("read-only"), Some(&("true" | "1")));
	for (name, dir) in [
		("real", &mut config.real_store_dir),
		("state", &mut config.state_dir),
		("log", &mut config.log_dir),
	] {
		if let Some(path) = params.get(name) {
			*dir = absolute(path)?;
		}
	}
	Ok(config)
}

fn check_mount(merged: &Path, lower: &Path, upper: &Path) -> Result<()> {
	#[cfg(target_os = "linux")]
	{
		let mounts = fs::read("/proc/self/mountinfo")?;
		let active = mounts
			.split(|&byte| byte == b'\n')
			.filter_map(|line| {
				let mut fields = line.split(|&byte| byte == b' ');
				let mount = fields.nth(4)?;
				if decode_mount_path(mount) != merged.as_os_str().as_bytes() {
					return None;
				}
				let mut fs = fields.skip_while(|field| *field != b"-").skip(1);
				Some((fs.next()?, fs.nth(1)?))
			})
			.last();
		if let Some((b"overlay", options)) = active {
			let has_option = |prefix: &[u8], path: &Path| {
				options.split(|&byte| byte == b',').any(|option| {
					option.strip_prefix(prefix).is_some_and(|value| {
						decode_mount_path(value) == path.as_os_str().as_bytes()
					})
				})
			};
			if has_option(b"lowerdir=", lower) && has_option(b"upperdir=", upper) {
				return Ok(());
			}
		}
	}
	Err(invalid_mount(merged))
}

fn invalid_mount(merged: &Path) -> Error {
	Error::Other(format!("overlay filesystem {merged:?} mounted incorrectly"))
}

#[cfg(target_os = "linux")]
fn decode_mount_path(mut bytes: &[u8]) -> Vec<u8> {
	let mut decoded = Vec::new();
	while let Some((&first, rest)) = bytes.split_first() {
		if first == b'\\'
			&& let [a @ b'0'..=b'3', b @ b'0'..=b'7', c @ b'0'..=b'7', tail @ ..] = rest
		{
			decoded.push((a - b'0') * 64 + (b - b'0') * 8 + (c - b'0'));
			bytes = tail;
		} else {
			decoded.push(first);
			bytes = rest;
		}
	}
	decoded
}

impl Overlay {
	pub fn delete(&self, merged_path: &Path, log: &mut dyn FnMut(&str)) -> Result<u64> {
		let Some(name) = merged_path.file_name() else {
			return Ok(0);
		};
		let Ok(path) = StorePath::from_bytes(name.as_encoded_bytes()) else {
			log(&format!(
				"local-overlay: unexpected gc path '{}'",
				merged_path.display()
			));
			return Ok(0);
		};
		let upper = self.upper.join(name);
		match fs::symlink_metadata(&upper) {
			Ok(_) => {}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
			Err(error) => return Err(error.into()),
		}
		let merged = merged_path.parent().expect("store object has a parent");
		// Upper-layer deletion of a duplicate avoids masking the lower copy.
		// Reject an upper directory that resolves into the lower store even
		// when the caller explicitly disabled mount checks.
		let upper_dir = self.upper.canonicalize()?;
		let lower_dir = self.lower.config().real_store_dir.canonicalize()?;
		if upper_dir.starts_with(&lower_dir) || lower_dir.starts_with(&upper_dir) {
			return Err(invalid_mount(merged));
		}
		if self.lower.is_valid_path(&path)? {
			self.remount_required.store(true, Ordering::Relaxed);
			delete_counting(&upper_dir.join(name))
		} else {
			if merged.canonicalize()?.starts_with(lower_dir) {
				return Err(invalid_mount(merged));
			}
			delete_counting(merged_path)
		}
	}

	pub fn remount(&self, merged: &Path, log: &mut dyn FnMut(&str)) -> Result<()> {
		if !self.remount_required.swap(false, Ordering::AcqRel) {
			return Ok(());
		}
		if let Some(hook) = &self.hook {
			let result = Command::new(hook).arg(merged).status().and_then(|status| {
				if status.success() {
					Ok(())
				} else {
					Err(std::io::Error::other(format!(
						"program '{}' failed with {status}",
						hook.display()
					)))
				}
			});
			if let Err(error) = result {
				self.remount_required.store(true, Ordering::Release);
				return Err(error.into());
			}
		} else {
			log(&format!(
				"{merged:?} needs remounting, set remount-hook to do this automatically"
			));
		}
		Ok(())
	}
}
