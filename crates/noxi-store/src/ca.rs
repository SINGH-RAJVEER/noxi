//! Content hashing modulo a store path's hash part, as CppNix's
//! `HashModuloSink` does, and equal-length output reference rewriting.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Seek, Write};
use std::path::Path;

use nix_compat::nixhash::{HashAlgo, NixHash, NixHashDigester};

use crate::{FileIngestionMethod, Result, delete_path, nar};

/// A streaming replacement writer. All patterns have the same length and
/// replacements must preserve it, so NAR framing and padding stay valid.
struct Rewriter<W> {
	inner: W,
	rewrites: BTreeMap<Vec<u8>, Vec<u8>>,
	pending: Vec<u8>,
	position: u64,
	matches: Vec<u64>,
	width: usize,
}

impl<W: Write> Rewriter<W> {
	fn new(inner: W, rewrites: BTreeMap<Vec<u8>, Vec<u8>>) -> Self {
		let width = rewrites.keys().next().map_or(1, Vec::len);
		assert!(
			width > 0
				&& rewrites
					.iter()
					.all(|(k, v)| k.len() == width && v.len() == width)
		);
		Self {
			inner,
			rewrites,
			pending: Vec::new(),
			position: 0,
			matches: Vec::new(),
			width,
		}
	}

	fn drain(&mut self, finish: bool) -> io::Result<()> {
		let mut consumed = 0;
		let mut unwritten = 0;
		while consumed + self.width <= self.pending.len() {
			let key = &self.pending[consumed..consumed + self.width];
			if let Some(replacement) = self.rewrites.get(key) {
				self.inner.write_all(&self.pending[unwritten..consumed])?;
				self.inner.write_all(replacement)?;
				self.matches.push(self.position + consumed as u64);
				consumed += self.width;
				unwritten = consumed;
			} else {
				consumed += 1;
			}
		}
		if finish {
			consumed = self.pending.len();
		}
		self.inner.write_all(&self.pending[unwritten..consumed])?;
		self.position += consumed as u64;
		self.pending.drain(..consumed);
		Ok(())
	}
}

impl<W: Write> Write for Rewriter<W> {
	fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
		self.pending.extend_from_slice(bytes);
		self.drain(false)?;
		Ok(bytes.len())
	}
	fn flush(&mut self) -> io::Result<()> {
		self.inner.flush()
	}
}

struct DigestWriter(NixHashDigester);
impl Write for DigestWriter {
	fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
		self.0.update(bytes);
		Ok(bytes.len())
	}
	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

/// Hash NAR or flat data with every occurrence of `modulus` replaced by
/// zero bytes, then append `|OFFSET` for each occurrence before finalizing.
/// This also works when a match crosses a streaming write boundary.
pub fn hash_modulo(
	path: &Path,
	method: FileIngestionMethod,
	algo: HashAlgo,
	modulus: &[u8],
) -> Result<NixHash> {
	let mut writer = Rewriter::new(
		DigestWriter(NixHashDigester::new(algo)),
		BTreeMap::from([(modulus.to_vec(), vec![0; modulus.len()])]),
	);
	match method {
		FileIngestionMethod::NixArchive => nar::dump_path(&mut writer, path)?,
		FileIngestionMethod::Flat | FileIngestionMethod::Text => {
			io::copy(&mut File::open(path)?, &mut writer)?;
		}
	}
	writer.drain(true)?;
	for offset in writer.matches {
		writer.inner.0.update(format!("|{offset}").as_bytes());
	}
	Ok(writer.inner.0.finalize())
}

/// Rewrite hash parts throughout file contents, symlink targets and names
/// in a NAR. Spool to disk instead of holding an entire output in memory.
pub fn rewrite_path(path: &Path, rewrites: &BTreeMap<Vec<u8>, Vec<u8>>) -> Result<()> {
	if rewrites.is_empty() {
		return Ok(());
	}
	let mut file = tempfile::tempfile()?;
	let mut writer = Rewriter::new(&mut file, rewrites.clone());
	nar::dump_path(&mut writer, path)?;
	writer.drain(true)?;
	if writer.matches.is_empty() {
		return Ok(());
	}
	drop(writer);
	file.rewind()?;
	delete_path(path)?;
	nar::restore_path(path, &mut io::BufReader::new(file))?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use sha2::{Digest, Sha256};
	use std::fs;

	#[test]
	fn matches_across_chunks_include_positions() {
		let mut writer =
			Rewriter::new(Vec::new(), BTreeMap::from([(b"abcd".to_vec(), vec![0; 4])]));
		for chunk in [b"xab".as_slice(), b"cdqab", b"cd!"] {
			writer.write_all(chunk).unwrap();
		}
		writer.drain(true).unwrap();
		assert_eq!(writer.matches, [1, 6]);
		assert_eq!(writer.inner, b"x\0\0\0\0q\0\0\0\0!");
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("file");
		fs::write(&path, "xabcdqabcd!").unwrap();
		let expected: [u8; 32] = Sha256::digest(b"x\0\0\0\0q\0\0\0\0!|1|6").into();
		assert_eq!(
			hash_modulo(&path, FileIngestionMethod::Flat, HashAlgo::Sha256, b"abcd").unwrap(),
			NixHash::Sha256(expected)
		);
	}
}
