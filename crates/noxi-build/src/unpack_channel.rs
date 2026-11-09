//! The reference's native channel archive builder.
use crate::buildenv::Error;
use std::{
	collections::BTreeMap,
	fs,
	io::Read,
	path::{Path, PathBuf},
};

pub fn build(
	env: &BTreeMap<String, String>,
	out: &Path,
	real: &dyn Fn(&Path) -> PathBuf,
) -> Result<(), Error> {
	let attr = |name: &str| {
		env.get(name)
			.ok_or_else(|| Error(format!("attribute '{name}' missing")))
	};
	let name = attr("channelName")?;
	if name.is_empty() || name == "." || name == ".." || name.contains('/') {
		return Err(Error(format!(
			"channelName is not allowed to contain filesystem separators, got {name}"
		)));
	}
	let src = attr("src")?;
	let bytes = fs::read(real(Path::new(src)))?;
	let reader: Box<dyn Read> = if bytes.starts_with(b"BZh") {
		Box::new(bzip2::read::BzDecoder::new(std::io::Cursor::new(bytes)))
	} else if bytes.starts_with(&[0x1f, 0x8b]) {
		Box::new(flate2::read::GzDecoder::new(std::io::Cursor::new(bytes)))
	} else if bytes.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
		Box::new(lzma_rust2::XzReader::new(
			std::io::Cursor::new(bytes),
			false,
		))
	} else {
		Box::new(std::io::Cursor::new(bytes))
	};
	fs::create_dir_all(out)?;
	let mut archive = tar::Archive::new(reader);
	archive.set_preserve_permissions(false);
	for entry in archive.entries()? {
		if !entry?.unpack_in(out)? {
			return Err(Error(
				"tarball contains an entry outside its destination".into(),
			));
		}
	}
	let entries = fs::read_dir(out)?
		.map(|e| e.map(|e| e.path()))
		.collect::<std::io::Result<Vec<_>>>()?;
	match entries.as_slice() {
		[] => Err(Error(format!("channel tarball '{src}' is empty"))),
		[path] => {
			fs::rename(path, out.join(name))?;
			Ok(())
		}
		_ => Err(Error(format!(
			"channel tarball '{src}' contains more than one file"
		))),
	}
}
