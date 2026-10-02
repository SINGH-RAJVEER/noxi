//! The primitive encodings of CppNix's worker protocol (`serialise.cc`):
//! little-endian 64-bit integers, and byte strings as a length followed by
//! the bytes padded with zeros to a multiple of eight.

use std::collections::BTreeSet;
use std::io::{self, Read, Write};

use nix_compat::store_path::StorePath;

use crate::error::{Error, Result};

/// The protocol version, `major << 8 | minor`.
pub type Version = u64;

pub const fn version(major: u64, minor: u64) -> Version {
	major << 8 | minor
}

pub struct Reader<R> {
	inner: R,
}

impl<R: Read> Reader<R> {
	pub fn new(inner: R) -> Self {
		Self { inner }
	}

	pub fn get_mut(&mut self) -> &mut R {
		&mut self.inner
	}

	pub fn read_u64(&mut self) -> Result<u64> {
		let mut buf = [0u8; 8];
		self.inner.read_exact(&mut buf).map_err(eof)?;
		Ok(u64::from_le_bytes(buf))
	}

	pub fn read_bool(&mut self) -> Result<bool> {
		Ok(self.read_u64()? != 0)
	}

	pub fn read_bytes(&mut self) -> Result<Vec<u8>> {
		let len = self.read_u64()?;
		let len = usize::try_from(len).map_err(|_| Error::Protocol("string is too long".into()))?;
		let mut bytes = vec![0u8; len];
		self.inner.read_exact(&mut bytes).map_err(eof)?;
		let padding = (8 - len % 8) % 8;
		let mut zeros = [0u8; 8];
		self.inner.read_exact(&mut zeros[..padding]).map_err(eof)?;
		if zeros[..padding].iter().any(|&b| b != 0) {
			return Err(Error::Protocol("non-zero padding".into()));
		}
		Ok(bytes)
	}

	pub fn read_string(&mut self) -> Result<String> {
		String::from_utf8(self.read_bytes()?)
			.map_err(|_| Error::Protocol("string is not valid UTF-8".into()))
	}

	pub fn read_strings(&mut self) -> Result<Vec<String>> {
		let n = self.read_u64()?;
		(0..n).map(|_| self.read_string()).collect()
	}

	/// `WorkerProto::Serialise<StorePath>::read`: an absolute store path.
	pub fn read_store_path(&mut self) -> Result<StorePath> {
		let s = self.read_string()?;
		parse_store_path(&s)
	}

	pub fn read_store_paths(&mut self) -> Result<BTreeSet<StorePath>> {
		let n = self.read_u64()?;
		(0..n).map(|_| self.read_store_path()).collect()
	}
}

/// CppNix's `FramedSource`: data sent as length-prefixed frames ending
/// with an empty frame.
pub struct FramedReader<'a, R: Read> {
	from: &'a mut Reader<R>,
	pending: Vec<u8>,
	pos: usize,
	eof: bool,
}

impl<'a, R: Read> FramedReader<'a, R> {
	pub fn new(from: &'a mut Reader<R>) -> Self {
		Self {
			from,
			pending: Vec::new(),
			pos: 0,
			eof: false,
		}
	}

	/// Read and discard the rest of the frames, as `~FramedSource` does.
	pub fn drain(&mut self) -> Result<()> {
		let mut sink = [0u8; 65536];
		while self.read(&mut sink)? != 0 {}
		Ok(())
	}
}

impl<R: Read> Read for FramedReader<'_, R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if self.eof {
			return Ok(0);
		}
		if self.pos >= self.pending.len() {
			let len = self.from.read_u64().map_err(io::Error::other)? as usize;
			if len == 0 {
				self.eof = true;
				return Ok(0);
			}
			self.pending.resize(len, 0);
			self.from.get_mut().read_exact(&mut self.pending)?;
			self.pos = 0;
		}
		let n = buf.len().min(self.pending.len() - self.pos);
		buf[..n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
		self.pos += n;
		Ok(n)
	}
}

/// CppNix's `parseStorePath` and its error messages.
pub fn parse_store_path(s: &str) -> Result<StorePath> {
	StorePath::from_absolute_path(s.as_bytes()).map_err(|e| {
		if !s.starts_with(nix_compat::store_path::STORE_DIR_WITH_SLASH) {
			Error::Nix(format!("path '{s}' is not in the Nix store"))
		} else {
			Error::Nix(format!("path '{s}' is not a valid store path: {e}"))
		}
	})
}

fn eof(e: io::Error) -> Error {
	if e.kind() == io::ErrorKind::UnexpectedEof {
		Error::EndOfFile
	} else {
		Error::Io(e)
	}
}

/// A buffer of protocol output, sent with [`Writer::flush`].
pub struct Writer<W: Write> {
	inner: W,
	buf: Vec<u8>,
}

impl<W: Write> Writer<W> {
	pub fn new(inner: W) -> Self {
		Self {
			inner,
			buf: Vec::new(),
		}
	}

	pub fn u64(&mut self, n: u64) -> &mut Self {
		self.buf.extend_from_slice(&n.to_le_bytes());
		self
	}

	pub fn bool(&mut self, b: bool) -> &mut Self {
		self.u64(b as u64)
	}

	pub fn bytes(&mut self, bytes: &[u8]) -> &mut Self {
		self.u64(bytes.len() as u64);
		self.buf.extend_from_slice(bytes);
		self.buf
			.resize(self.buf.len() + (8 - bytes.len() % 8) % 8, 0);
		self
	}

	pub fn string(&mut self, s: &str) -> &mut Self {
		self.bytes(s.as_bytes())
	}

	pub fn strings<'a>(&mut self, items: impl ExactSizeIterator<Item = &'a str>) -> &mut Self {
		self.u64(items.len() as u64);
		for item in items {
			self.string(item);
		}
		self
	}

	pub fn store_path(&mut self, path: &StorePath) -> &mut Self {
		self.string(&path.to_absolute_path())
	}

	/// `std::optional<StorePath>`: the empty string for none.
	pub fn opt_store_path(&mut self, path: Option<&StorePath>) -> &mut Self {
		match path {
			Some(path) => self.store_path(path),
			None => self.string(""),
		}
	}

	pub fn store_paths<'a>(
		&mut self,
		paths: impl ExactSizeIterator<Item = &'a StorePath>,
	) -> &mut Self {
		self.u64(paths.len() as u64);
		for path in paths {
			self.store_path(path);
		}
		self
	}

	/// Raw bytes with no framing, such as a NAR.
	pub fn raw(&mut self, bytes: &[u8]) -> &mut Self {
		self.buf.extend_from_slice(bytes);
		self
	}

	pub fn flush(&mut self) -> Result<()> {
		self.inner.write_all(&self.buf)?;
		self.buf.clear();
		self.inner.flush()?;
		Ok(())
	}

	/// Take the buffered output without sending it.
	pub fn take(&mut self) -> Vec<u8> {
		std::mem::take(&mut self.buf)
	}

	/// The underlying stream, for output written past the buffer, after
	/// a [`Writer::flush`].
	pub fn inner_mut(&mut self) -> &mut W {
		&mut self.inner
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn strings_are_padded_to_eight_bytes() {
		let mut out = Vec::new();
		let mut w = Writer::new(&mut out);
		w.string("abc").string("").string("12345678");
		w.flush().unwrap();
		assert_eq!(out.len(), 8 + 8 + 8 + 8 + 8);
		let mut r = Reader::new(&out[..]);
		assert_eq!(r.read_string().unwrap(), "abc");
		assert_eq!(r.read_string().unwrap(), "");
		assert_eq!(r.read_string().unwrap(), "12345678");
		assert!(matches!(r.read_u64(), Err(Error::EndOfFile)));
	}

	#[test]
	fn non_zero_padding_is_rejected() {
		let mut bytes = 1u64.to_le_bytes().to_vec();
		bytes.extend_from_slice(b"a\x01\0\0\0\0\0\0");
		assert!(Reader::new(&bytes[..]).read_bytes().is_err());
	}
}
