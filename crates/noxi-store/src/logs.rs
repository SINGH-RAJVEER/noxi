//! Atomic build-log imports, compatible with CppNix's bzip2 log layout.

use crate::{LocalStore, Result};
use nix_compat::store_path::StorePath;
use std::io::{Read, Write};

impl LocalStore {
	pub fn add_build_log(&self, path: &StorePath, source: &mut dyn Read) -> Result<()> {
		if self.config().read_only {
			return Err(crate::Error::Other(
				"cannot add logs to a read-only store".into(),
			));
		}
		let name = path.to_string();
		if !name.ends_with(".drv") {
			return Err(crate::Error::Other(format!(
				"path '{}' is not a derivation",
				path.to_absolute_path()
			)));
		}
		let dir = self.config().log_dir.join("drvs").join(&name[..2]);
		let dest = dir.join(format!("{}.bz2", &name[2..]));
		if dest.exists() {
			std::io::copy(source, &mut std::io::sink())?;
			return Ok(());
		}
		std::fs::create_dir_all(&dir)?;
		let mut temporary = tempfile::NamedTempFile::new_in(&dir)?;
		{
			let mut compressed =
				bzip2::write::BzEncoder::new(temporary.as_file_mut(), bzip2::Compression::best());
			std::io::copy(source, &mut compressed)?;
			compressed.finish()?.flush()?;
		}
		temporary.as_file().sync_all()?;
		match temporary.persist_noclobber(&dest) {
			Ok(_) => Ok(()),
			Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
			Err(error) => Err(error.error.into()),
		}
	}
}
