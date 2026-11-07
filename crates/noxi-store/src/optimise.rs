//! Store deduplication using the reference's SHA-256 NAR inode names.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

use crate::{LocalStore, Result, nar};
use nix_compat::nixbase32;

#[derive(Default)]
pub struct OptimiseStats {
	pub files_linked: u64,
	pub bytes_freed: u64,
}

struct DirectoryMode(PathBuf, fs::Permissions);

impl Drop for DirectoryMode {
	fn drop(&mut self) {
		let _ = fs::set_permissions(&self.0, self.1.clone());
	}
}

impl LocalStore {
	pub fn optimise(&self) -> Result<OptimiseStats> {
		if self.config().read_only {
			return Err(crate::Error::Other(
				"cannot optimise a read-only store".into(),
			));
		}
		let links = self.config().real_store_dir.join(".links");
		fs::create_dir_all(&links)?;
		let mut stats = OptimiseStats::default();
		for path in self.query_all_valid_paths()? {
			self.add_temp_root(&path)?;
			if !self.is_valid_path(&path)? {
				continue;
			}
			let mut todo = vec![self.real_path(&path)];
			while let Some(path) = todo.pop() {
				let metadata = fs::symlink_metadata(&path)?;
				if metadata.is_dir() {
					todo.extend(
						fs::read_dir(&path)?
							.map(|entry| entry.map(|entry| entry.path()))
							.collect::<std::io::Result<Vec<_>>>()?,
					);
					continue;
				}
				if !(metadata.is_file() || metadata.file_type().is_symlink())
					|| (metadata.is_file() && metadata.mode() & 0o200 != 0)
				{
					continue;
				}
				let hash = nar::hash_path(&path)?.0;
				let link = links.join(nixbase32::encode(&hash));
				if let Ok(existing) = fs::symlink_metadata(&link) {
					if existing.ino() == metadata.ino() && existing.dev() == metadata.dev() {
						continue;
					}
					if nar::hash_path(&link)?.0 != hash {
						fs::remove_file(&link)?;
					}
				}
				match fs::hard_link(&path, &link) {
					Ok(()) => continue,
					Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
					Err(error) => return Err(error.into()),
				}
				let parent = path.parent().expect("store file parent");
				let permissions = fs::metadata(parent)?.permissions();
				let _restore = DirectoryMode(parent.to_owned(), permissions.clone());
				fs::set_permissions(
					parent,
					fs::Permissions::from_mode(permissions.mode() | 0o200),
				)?;
				let temporary = tempfile::NamedTempFile::new_in(parent)?.into_temp_path();
				fs::remove_file(&temporary)?;
				fs::hard_link(&link, &temporary)?;
				fs::rename(&temporary, &path)?;
				stats.files_linked += 1;
				stats.bytes_freed += metadata.len();
			}
		}
		Ok(stats)
	}
}
