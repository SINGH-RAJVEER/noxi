//! Store deduplication using the reference's SHA-256 NAR inode names,
//! following CppNix 2.35.2's `optimise-store.cc`.

use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::canonicalise::canonicalise_timestamp_and_permissions;
use crate::{LocalStore, Result, nar};
use nix_compat::nixbase32;

#[derive(Default)]
pub struct OptimiseStats {
	pub files_linked: u64,
	pub bytes_freed: u64,
}

/// `MakeReadOnly`: canonicalises a directory's mode and timestamp again
/// when dropped.
struct MakeReadOnly(Option<PathBuf>);

impl Drop for MakeReadOnly {
	fn drop(&mut self) {
		if let Some(path) = &self.0 {
			let _ = canonicalise_timestamp_and_permissions(path);
		}
	}
}

impl LocalStore {
	pub fn optimise(&self) -> Result<OptimiseStats> {
		if self.config().read_only {
			return Err(crate::Error::Other(
				"cannot optimise a read-only store".into(),
			));
		}
		if let Some(overlay) = &self.overlay {
			let mut stats = OptimiseStats::default();
			let mut warn = |message: &str| {
				if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn) {
					eprintln!("warning: {message}");
				}
			};
			for path in self.query_all_valid_paths()? {
				if overlay.lower.is_valid_path(&path)? {
					stats.bytes_freed += overlay.delete(&self.real_path(&path), &mut warn)?;
				}
			}
			overlay.remount(&self.config().real_store_dir, &mut warn)?;
			return Ok(stats);
		}
		let mut stats = OptimiseStats::default();
		for path in self.query_all_valid_paths()? {
			self.add_temp_root(&path)?;
			if !self.is_valid_path(&path)? {
				continue;
			}
			self.optimise_path_into(&self.real_path(&path), &mut stats, false)?;
		}
		Ok(stats)
	}

	/// `LocalStore::optimisePath`: deduplicate the files of the store
	/// path at `real` when `auto-optimise-store` is set.
	pub fn optimise_path(&self, real: &Path, repair: bool) -> Result<()> {
		if self.config().auto_optimise_store && self.overlay.is_none() {
			self.optimise_path_into(real, &mut OptimiseStats::default(), repair)?;
		}
		Ok(())
	}

	/// `optimisePath_`.
	fn optimise_path_into(&self, path: &Path, stats: &mut OptimiseStats, repair: bool) -> Result<()> {
		let real_store = &self.config().real_store_dir;
		let links = real_store.join(".links");
		fs::create_dir_all(&links)?;
		let metadata = fs::symlink_metadata(path)?;
		if metadata.is_dir() {
			for entry in fs::read_dir(path)? {
				self.optimise_path_into(&entry?.path(), stats, repair)?;
			}
			return Ok(());
		}
		if !(metadata.is_file() || metadata.file_type().is_symlink()) {
			return Ok(());
		}
		if metadata.is_file() && metadata.mode() & 0o200 != 0 {
			if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn) {
				eprintln!(
					"warning: skipping suspicious writable file '\"{}\"'",
					path.display()
				);
			}
			return Ok(());
		}
		let hash = nar::hash_path(path)?.0;
		let link = links.join(nixbase32::encode(&hash));
		if let Ok(existing) = fs::symlink_metadata(&link)
			&& (existing.len() != metadata.len() || (repair && nar::hash_path(&link)?.0 != hash))
		{
			if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn) {
				eprintln!("warning: removing corrupted link \"{}\"", link.display());
				eprintln!(
					"warning: There may be more corrupted paths.\nYou should run `nix-store --verify --check-contents --repair` to fix them all"
				);
			}
			match fs::remove_file(&link) {
				Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
				_ => {}
			}
		}
		if fs::symlink_metadata(&link).is_err() {
			match fs::hard_link(path, &link) {
				Ok(()) => return Ok(()),
				Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
				// A full directory index or too many links disable
				// deduplication of this file.
				Err(e) if matches!(e.raw_os_error(), Some(libc::ENOSPC | libc::EMLINK)) => {
					return Ok(());
				}
				Err(e) => return Err(e.into()),
			}
		}
		// A concurrent collection may have removed the link.
		let Ok(existing) = fs::symlink_metadata(&link) else {
			return Ok(());
		};
		if existing.ino() == metadata.ino() && existing.dev() == metadata.dev() {
			return Ok(());
		}
		let parent = path.parent().expect("store file parent");
		let toggle = parent != real_store.as_path();
		if toggle {
			let mode = fs::metadata(parent)?.permissions().mode();
			fs::set_permissions(parent, fs::Permissions::from_mode(mode | 0o200))?;
		}
		let _read_only = MakeReadOnly(toggle.then(|| parent.to_owned()));
		static COUNTER: AtomicU64 = AtomicU64::new(0);
		let temporary = real_store.join(format!(
			".tmp-link-{}-{}",
			std::process::id(),
			COUNTER.fetch_add(1, Ordering::Relaxed)
		));
		match fs::hard_link(&link, &temporary) {
			Ok(()) => {}
			Err(e) if matches!(e.raw_os_error(), Some(libc::EMLINK | libc::ENOENT)) => {
				return Ok(());
			}
			Err(e) => return Err(e.into()),
		}
		if let Err(e) = fs::rename(&temporary, path) {
			let _ = fs::remove_file(&temporary);
			if e.raw_os_error() == Some(libc::EMLINK) {
				return Ok(());
			}
			return Err(e.into());
		}
		stats.files_linked += 1;
		stats.bytes_freed += metadata.len();
		Ok(())
	}
}
