use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use nix_compat::store_path::StorePath;
use noxi_flakeref::error::{Error, Result};
use noxi_flakeref::input::{Attr, Input, get_str_attr};

use crate::mount::Mount;
use crate::store::EvaluatorStore;

pub(crate) fn get_accessor(store: &EvaluatorStore, input: &Input) -> Result<(Mount, Input)> {
	let path = get_str_attr(&input.attrs, "path")?;
	if !Path::new(&path).is_absolute() {
		return Err(Error::new(format!(
			"cannot fetch input '{}' because it uses a relative path",
			input.display()
		)));
	}
	// `PathInputScheme::getAccessor` reads the path from the real file
	// system unless it is a valid object of the selected store. With a
	// diverted store (`--store /mnt`), a store path the store lacks is
	// therefore read from the host's own store directory, which
	// nixos-install relies on for inputs locked to the installer's store.
	let logical = Path::new(&path);
	let physical = match StorePath::from_absolute_path_full(logical) {
		Ok((store_path, _))
			if store.mount_of(logical).is_none()
				&& store.valid_store_path(&store_path).is_none() =>
		{
			logical.to_owned()
		}
		_ => store.physical_path(logical),
	};
	let mut result = input.clone();
	if result.get_last_modified()?.is_none() {
		result
			.attrs
			.insert("lastModified".into(), Attr::Int(last_modified(&physical)?));
	}
	Ok((Mount::new(physical), result))
}

fn last_modified(path: &Path) -> Result<u64> {
	let metadata = fs::symlink_metadata(path)?;
	let mut latest = metadata.mtime().max(0) as u64;
	if metadata.is_dir() {
		for entry in fs::read_dir(path)? {
			latest = latest.max(last_modified(&entry?.path())?);
		}
	}
	Ok(latest)
}
