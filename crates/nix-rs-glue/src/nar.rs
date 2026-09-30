//! Serialises a filesystem path as a NAR, the way CppNix's `dumpPath` does,
//! to compute the hash a copy into the store would record.

use std::fs::{self, File};
use std::io::{self, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use nix_compat::nar::writer::sync as nar;
use sha2::{Digest, Sha256};

/// Decides which entries below the root are part of the archive. It is
/// called with each entry's path and file type before the entry is read.
pub type Filter<'a> = dyn FnMut(&Path, EntryType) -> io::Result<bool> + 'a;

/// The file types a NAR can hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryType {
	Regular,
	Directory,
	Symlink,
}

impl EntryType {
	/// The name `builtins.path` and `builtins.filterSource` pass to filters.
	pub fn as_nix_str(self) -> &'static str {
		match self {
			EntryType::Regular => "regular",
			EntryType::Directory => "directory",
			EntryType::Symlink => "symlink",
		}
	}
}

fn entry_type(path: &Path, metadata: &fs::Metadata) -> io::Result<EntryType> {
	let file_type = metadata.file_type();
	if file_type.is_symlink() {
		Ok(EntryType::Symlink)
	} else if file_type.is_file() {
		Ok(EntryType::Regular)
	} else if file_type.is_dir() {
		Ok(EntryType::Directory)
	} else {
		Err(io::Error::new(
			io::ErrorKind::Unsupported,
			format!("file '{}' has an unsupported type", path.display()),
		))
	}
}

/// Write `path` as a NAR to `writer`. The root is never passed to `filter`.
pub fn dump_path<W: Write>(
	writer: &mut W,
	path: &Path,
	filter: Option<&mut Filter<'_>>,
) -> io::Result<()> {
	let metadata = fs::symlink_metadata(path)?;
	let ty = entry_type(path, &metadata)?;
	dump(nar::open(writer)?, path, &metadata, ty, filter)
}

fn dump<W: Write>(
	node: nar::Node<'_, W>,
	path: &Path,
	metadata: &fs::Metadata,
	ty: EntryType,
	mut filter: Option<&mut Filter<'_>>,
) -> io::Result<()> {
	match ty {
		EntryType::Symlink => node.symlink(fs::read_link(path)?.as_os_str().as_bytes()),
		EntryType::Regular => {
			// CppNix counts a file as executable when its owner may run it.
			let executable = metadata.permissions().mode() & 0o100 != 0;
			let mut reader = BufReader::new(File::open(path)?);
			node.file(executable, metadata.len(), &mut reader)
		}
		EntryType::Directory => {
			let mut names = fs::read_dir(path)?
				.map(|entry| entry.map(|entry| entry.file_name()))
				.collect::<io::Result<Vec<_>>>()?;
			names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

			let mut directory = node.directory()?;
			for name in names {
				let child = path.join(&name);
				let metadata = fs::symlink_metadata(&child)?;
				let ty = entry_type(&child, &metadata)?;
				if let Some(filter) = filter.as_deref_mut()
					&& !filter(&child, ty)?
				{
					continue;
				}
				dump(
					directory.entry(name.as_bytes())?,
					&child,
					&metadata,
					ty,
					filter.as_deref_mut(),
				)?;
			}
			directory.close()
		}
	}
}

/// The SHA-256 digest of `path` serialised as a NAR.
pub fn nar_sha256(path: &Path, filter: Option<&mut Filter<'_>>) -> io::Result<[u8; 32]> {
	let mut hasher = Sha256::new();
	dump_path(&mut hasher, path, filter)?;
	Ok(hasher.finalize().into())
}

/// The SHA-256 digest of a regular file's contents, for flat imports.
pub fn file_sha256(path: &Path) -> io::Result<[u8; 32]> {
	let mut hasher = Sha256::new();
	io::copy(&mut File::open(path)?, &mut hasher)?;
	Ok(hasher.finalize().into())
}

/// The SHA-256 digest of a NAR holding one non-executable regular file.
pub fn bytes_nar_sha256(contents: &[u8]) -> io::Result<[u8; 32]> {
	let mut hasher = Sha256::new();
	nar::open(&mut hasher)?.file(false, contents.len() as u64, &mut &contents[..])?;
	Ok(hasher.finalize().into())
}
