//! Serialising filesystem trees to NAR and restoring them, as CppNix's
//! `dumpPath` and `restorePath` do.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use nix_compat::nar::reader as nar_reader;
use nix_compat::nar::writer::sync as nar_writer;
use sha2::{Digest, Sha256};

/// Write `path` to `writer` as a NAR. Directory entries are written in
/// byte order of their names; a regular file is executable when its owner
/// may execute it.
pub fn dump_path<W: Write>(writer: &mut W, path: &Path) -> io::Result<()> {
	dump(nar_writer::open(writer)?, path)
}

fn dump<W: Write>(node: nar_writer::Node<'_, W>, path: &Path) -> io::Result<()> {
	let metadata = fs::symlink_metadata(path)?;
	let file_type = metadata.file_type();
	if file_type.is_symlink() {
		node.symlink(fs::read_link(path)?.as_os_str().as_bytes())
	} else if file_type.is_file() {
		let executable = metadata.permissions().mode() & 0o100 != 0;
		let mut reader = BufReader::new(File::open(path)?);
		node.file(executable, metadata.len(), &mut reader)
	} else if file_type.is_dir() {
		let mut names = fs::read_dir(path)?
			.map(|entry| entry.map(|entry| entry.file_name()))
			.collect::<io::Result<Vec<_>>>()?;
		names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
		let mut directory = node.directory()?;
		for name in names {
			dump(directory.entry(name.as_bytes())?, &path.join(&name))?;
		}
		directory.close()
	} else {
		Err(io::Error::new(
			io::ErrorKind::Unsupported,
			format!("file '{}' has an unsupported type", path.display()),
		))
	}
}

/// A writer that hashes with SHA-256 and counts what it is given.
#[derive(Default)]
pub struct HashSink {
	hasher: Sha256,
	size: u64,
}

impl HashSink {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn finish(self) -> ([u8; 32], u64) {
		(self.hasher.finalize().into(), self.size)
	}
}

impl Write for HashSink {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		self.hasher.update(buf);
		self.size += buf.len() as u64;
		Ok(buf.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

/// The SHA-256 digest and size of `path` serialised as a NAR.
pub fn hash_path(path: &Path) -> io::Result<([u8; 32], u64)> {
	let mut sink = HashSink::new();
	dump_path(&mut sink, path)?;
	Ok(sink.finish())
}

/// A reader that hashes and counts every byte its consumer takes.
pub struct HashingReader<R> {
	inner: R,
	sink: HashSink,
}

impl<R: BufRead> HashingReader<R> {
	pub fn new(inner: R) -> Self {
		Self {
			inner,
			sink: HashSink::new(),
		}
	}

	pub fn finish(self) -> ([u8; 32], u64) {
		self.sink.finish()
	}
}

impl<R: BufRead> Read for HashingReader<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let n = self.inner.read(buf)?;
		self.sink.write_all(&buf[..n])?;
		Ok(n)
	}
}

impl<R: BufRead> BufRead for HashingReader<R> {
	fn fill_buf(&mut self) -> io::Result<&[u8]> {
		self.inner.fill_buf()
	}

	fn consume(&mut self, amount: usize) {
		// The bytes are still buffered, so this does not read.
		if let Ok(buf) = self.inner.fill_buf() {
			let amount = amount.min(buf.len());
			self.sink.hasher.update(&buf[..amount]);
			self.sink.size += amount as u64;
		}
		self.inner.consume(amount);
	}
}

/// Create `dest` from the NAR read from `reader`. `dest` must not exist.
pub fn restore_path(dest: &Path, reader: &mut (dyn BufRead + Send)) -> io::Result<()> {
	let node = nar_reader::open(reader)?;
	restore(dest, node)
}

fn restore(dest: &Path, node: nar_reader::Node<'_, '_>) -> io::Result<()> {
	match node {
		nar_reader::Node::Symlink { target } => {
			std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&target), dest)
		}
		nar_reader::Node::File {
			executable,
			mut reader,
		} => {
			let mut file = create_file(dest, executable)?;
			reader.copy(&mut file)?;
			Ok(())
		}
		nar_reader::Node::Directory(mut directory) => {
			fs::DirBuilder::new().mode(0o777).create(dest)?;
			while let Some(entry) = directory.next()? {
				let child = dest.join(std::ffi::OsStr::from_bytes(entry.name));
				restore(&child, entry.node)?;
			}
			Ok(())
		}
	}
}

fn create_file(dest: &Path, executable: bool) -> io::Result<File> {
	let file = OpenOptions::new()
		.write(true)
		.create_new(true)
		.mode(0o666)
		.custom_flags(libc::O_CLOEXEC)
		.open(dest)?;
	if executable {
		let mode = file.metadata()?.permissions().mode();
		file.set_permissions(fs::Permissions::from_mode(mode | 0o111))?;
	}
	Ok(file)
}

/// Create the regular file `dest` holding the bytes read from `reader`, as
/// CppNix restores a flat (non-NAR) serialisation.
pub fn restore_flat(dest: &Path, reader: &mut dyn Read) -> io::Result<u64> {
	let mut file = create_file(dest, false)?;
	io::copy(reader, &mut file)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn dump_restore_round_trip() {
		let dir = tempfile::tempdir().unwrap();
		let src = dir.path().join("src");
		fs::create_dir(&src).unwrap();
		fs::write(src.join("b"), b"bee").unwrap();
		fs::write(src.join("a"), b"").unwrap();
		fs::set_permissions(src.join("a"), fs::Permissions::from_mode(0o755)).unwrap();
		fs::create_dir(src.join("dir")).unwrap();
		std::os::unix::fs::symlink("../b", src.join("dir/link")).unwrap();

		let mut nar = Vec::new();
		dump_path(&mut nar, &src).unwrap();

		let dest = dir.path().join("dest");
		let mut reader = HashingReader::new(&nar[..]);
		restore_path(&dest, &mut reader).unwrap();
		let (digest, size) = reader.finish();
		assert_eq!(size, nar.len() as u64);
		assert_eq!(digest, <[u8; 32]>::from(Sha256::digest(&nar)));

		let mut again = Vec::new();
		dump_path(&mut again, &dest).unwrap();
		assert_eq!(nar, again);
		assert_eq!(hash_path(&dest).unwrap(), (digest, size));
	}
}
