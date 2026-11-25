//! Copying one NAR from a stream that holds more than the NAR, CppNix's
//! `copyNAR`. Nothing past the NAR's end is read.

use std::io::{self, Read, Write};

use crate::error::{Error, Result};

/// CppNix refuses deeper directories when it parses a NAR.
const MAX_DEPTH: usize = 64 * 1024;

struct Copier<'a> {
	from: &'a mut dyn Read,
	to: &'a mut dyn Write,
}

fn bad(message: &str) -> Error {
	Error::Nix(format!("bad archive: {message}"))
}

impl Copier<'_> {
	fn exact(&mut self, buf: &mut [u8]) -> Result<()> {
		self.from.read_exact(buf).map_err(|e| match e.kind() {
			io::ErrorKind::UnexpectedEof => Error::EndOfFile,
			_ => e.into(),
		})?;
		Ok(self.to.write_all(buf)?)
	}

	fn length(&mut self) -> Result<u64> {
		let mut buf = [0; 8];
		self.exact(&mut buf)?;
		Ok(u64::from_le_bytes(buf))
	}

	fn padding(&mut self, length: u64) -> Result<()> {
		let mut buf = [0; 8];
		let n = (length.next_multiple_of(8) - length) as usize;
		self.exact(&mut buf[..n])?;
		if buf.iter().any(|&c| c != 0) {
			return Err(bad("non-zero padding"));
		}
		Ok(())
	}

	/// A token, entry name or symlink target.
	fn string(&mut self) -> Result<Vec<u8>> {
		let length = self.length()?;
		if length > 64 * 1024 {
			return Err(bad("string is too long"));
		}
		let mut buf = vec![0; length as usize];
		self.exact(&mut buf)?;
		self.padding(length)?;
		Ok(buf)
	}

	fn expect(&mut self, tokens: &[&str]) -> Result<()> {
		for token in tokens {
			if self.string()? != token.as_bytes() {
				return Err(bad(&format!("expected tag '{token}'")));
			}
		}
		Ok(())
	}

	fn contents(&mut self) -> Result<()> {
		let length = self.length()?;
		let copied = io::copy(&mut Read::take(&mut *self.from, length), &mut *self.to)?;
		if copied != length {
			return Err(Error::EndOfFile);
		}
		self.padding(length)
	}

	fn archive(&mut self) -> Result<()> {
		self.expect(&["nix-archive-1"])?;
		// The open directories. Each entry is closed after its node.
		let mut depth = 0usize;
		loop {
			self.expect(&["(", "type"])?;
			match &self.string()?[..] {
				b"regular" => {
					let mut tag = self.string()?;
					if tag == b"executable" {
						self.expect(&[""])?;
						tag = self.string()?;
					}
					if tag == b"contents" {
						self.contents()?;
						tag = self.string()?;
					}
					if tag != b")" {
						return Err(bad("expected tag ')'"));
					}
				}
				b"symlink" => {
					self.expect(&["target"])?;
					self.string()?;
					self.expect(&[")"])?;
				}
				b"directory" => match &self.string()?[..] {
					b"entry" => {
						depth += 1;
						if depth > MAX_DEPTH {
							return Err(bad("directories are nested too deeply"));
						}
						self.entry()?;
						continue;
					}
					b")" => {}
					_ => return Err(bad("expected tag 'entry'")),
				},
				_ => return Err(bad("unknown file type")),
			}
			// A node is complete. Close the entries it ends.
			loop {
				if depth == 0 {
					return Ok(());
				}
				self.expect(&[")"])?;
				match &self.string()?[..] {
					b"entry" => {
						self.entry()?;
						break;
					}
					b")" => depth -= 1,
					_ => return Err(bad("expected tag 'entry'")),
				}
			}
		}
	}

	/// An entry up to its node.
	fn entry(&mut self) -> Result<()> {
		self.expect(&["(", "name"])?;
		self.string()?;
		self.expect(&["node"])
	}
}

/// Copy the NAR at the start of `from` to `to`.
pub fn copy_nar(from: &mut dyn Read, to: &mut dyn Write) -> Result<()> {
	Copier { from, to }.archive()
}

/// A reader of the NAR at the start of a stream, which it consumes to its
/// end even when the reader of the NAR stops early.
pub fn with_nar<T>(
	from: &mut dyn Read,
	use_nar: impl FnOnce(&mut std::fs::File) -> T,
) -> Result<T> {
	use std::io::Seek;
	let mut file = tempfile::tempfile()?;
	copy_nar(from, &mut file)?;
	file.rewind()?;
	Ok(use_nar(&mut file))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn sample() -> Vec<u8> {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("x");
		std::fs::create_dir_all(root.join("a/b")).unwrap();
		std::fs::create_dir(root.join("empty")).unwrap();
		std::fs::write(root.join("a/b/file"), b"hello").unwrap();
		std::fs::write(root.join("a/nothing"), b"").unwrap();
		std::os::unix::fs::symlink("a/b", root.join("link")).unwrap();
		let mut nar = Vec::new();
		noxi_store::nar::dump_path(&mut nar, &root).unwrap();
		nar
	}

	#[test]
	fn stops_at_the_end_of_the_archive() {
		let nar = sample();
		let mut stream = nar.clone();
		stream.extend(b"next request");
		let mut rest = &stream[..];
		let mut out = Vec::new();
		copy_nar(&mut rest, &mut out).unwrap();
		assert_eq!(out, nar);
		assert_eq!(rest, b"next request");
	}

	#[test]
	fn refuses_truncated_and_malformed_archives() {
		let nar = sample();
		for length in [0, 7, 8, 40, nar.len() - 1] {
			assert!(copy_nar(&mut &nar[..length], &mut io::sink()).is_err());
		}
		let mut bad = nar;
		bad[8] = b'x';
		assert!(copy_nar(&mut &bad[..], &mut io::sink()).is_err());
	}
}
