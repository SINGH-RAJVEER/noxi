//! Owned import bytes whose SHA-256 was computed during capture.

use std::io::{self, Seek, Write};

use sha2::{Digest, Sha256};
use tempfile::SpooledTempFile;

/// Capture a source once, spilling to a private file above `memory_limit`.
pub struct SourceCapture {
	bytes: SpooledTempFile,
	hash: Sha256,
}

impl SourceCapture {
	pub fn new(memory_limit: usize) -> Self {
		Self {
			bytes: tempfile::spooled_tempfile(memory_limit),
			hash: Sha256::new(),
		}
	}

	/// Freeze the bytes and their digest. The resulting source cannot be written.
	pub fn finish(mut self) -> io::Result<CapturedSource> {
		let size = self.bytes.stream_position()?;
		self.bytes.rewind()?;
		Ok(CapturedSource {
			bytes: self.bytes,
			hash: self.hash.finalize().into(),
			size,
		})
	}
}

impl Write for SourceCapture {
	fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
		let count = self.bytes.write(bytes)?;
		self.hash.update(&bytes[..count]);
		Ok(count)
	}

	fn flush(&mut self) -> io::Result<()> {
		self.bytes.flush()
	}
}

/// An immutable snapshot. Its contents and digest are accessible together only
/// inside the store crate, so importing it can reuse the digest safely.
pub struct CapturedSource {
	bytes: SpooledTempFile,
	hash: [u8; 32],
	size: u64,
}

impl CapturedSource {
	pub fn sha256(&self) -> [u8; 32] {
		self.hash
	}

	/// Whether capture spilled beyond its memory limit.
	pub fn is_rolled(&self) -> bool {
		self.bytes.is_rolled()
	}

	pub(crate) fn hash_and_size(&self) -> ([u8; 32], u64) {
		(self.hash, self.size)
	}

	pub(crate) fn reader(&mut self) -> io::Result<&mut SpooledTempFile> {
		self.bytes.rewind()?;
		Ok(&mut self.bytes)
	}
}
