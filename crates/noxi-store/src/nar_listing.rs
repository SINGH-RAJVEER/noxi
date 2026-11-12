//! Indexing a NAR without unpacking it, as CppNix's `parseNarListing`
//! does for `nix nar ls` and `nix nar cat`. Errors use the reference's
//! messages from `src/libutil/archive.cc` and `serialise.cc`.

use std::collections::BTreeMap;
use std::io::{self, Read};

use nix_compat::nar::MAX_DEPTH;

/// A member of a NAR, with the offset of each regular file's contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Listing {
	Regular {
		executable: bool,
		size: u64,
		offset: u64,
	},
	Directory(BTreeMap<Vec<u8>, Listing>),
	Symlink(Vec<u8>),
}

impl Listing {
	/// The member at `components`, walking only through directories.
	pub fn find(&self, components: &[&[u8]]) -> Option<&Listing> {
		let mut current = self;
		for name in components {
			let Listing::Directory(entries) = current else {
				return None;
			};
			current = entries.get(*name)?;
		}
		Some(current)
	}
}

struct Source<'a> {
	inner: &'a mut dyn Read,
	pos: u64,
}

const EOF: &str = "unexpected end-of-file";

impl Source<'_> {
	fn fill(&mut self, buf: &mut [u8]) -> Result<(), String> {
		let mut done = 0;
		while done < buf.len() {
			match self.inner.read(&mut buf[done..]) {
				Ok(0) => return Err(EOF.into()),
				Ok(n) => done += n,
				Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
				Err(e) => return Err(e.to_string()),
			}
		}
		self.pos += buf.len() as u64;
		Ok(())
	}

	fn skip(&mut self, mut len: u64) -> Result<(), String> {
		let mut buf = [0u8; 65536];
		while len > 0 {
			let n = len.min(buf.len() as u64) as usize;
			self.fill(&mut buf[..n])?;
			len -= n as u64;
		}
		Ok(())
	}

	fn num(&mut self) -> Result<u64, String> {
		let mut buf = [0u8; 8];
		self.fill(&mut buf)?;
		Ok(u64::from_le_bytes(buf))
	}

	fn padding(&mut self, len: u64) -> Result<(), String> {
		if !len.is_multiple_of(8) {
			let mut buf = [0u8; 8];
			let n = (8 - len % 8) as usize;
			self.fill(&mut buf[..n])?;
			if buf.iter().any(|&b| b != 0) {
				return Err("non-zero padding".into());
			}
		}
		Ok(())
	}

	/// `readString`; a string longer than `max` is a `SerialisationError`,
	/// reported as `Err(Ok(..))` so the version check can catch it.
	fn string(&mut self, max: usize) -> Result<Vec<u8>, Result<String, String>> {
		let len = self.num().map_err(Err)?;
		if len > max as u64 {
			return Err(Ok("string is too long".into()));
		}
		let mut buf = vec![0; len as usize];
		self.fill(&mut buf).map_err(Err)?;
		self.padding(len).map_err(Ok)?;
		Ok(buf)
	}
}

fn bad(message: impl std::fmt::Display) -> String {
	format!("bad archive: {message}")
}

fn flatten(error: Result<String, String>) -> String {
	match error {
		Ok(message) | Err(message) => message,
	}
}

const MAX_TAG: usize = 32;
const MAX_NAME: usize = 255;
const MAX_TARGET: usize = 4095;

fn tag(source: &mut Source<'_>) -> Result<Vec<u8>, String> {
	source.string(MAX_TAG).map_err(flatten)
}

fn expect(source: &mut Source<'_>, expected: &str) -> Result<(), String> {
	let got = tag(source)?;
	if got != expected.as_bytes() {
		return Err(bad(format_args!(
			"expected tag '{expected}', got '{}'",
			String::from_utf8_lossy(&got)
		)));
	}
	Ok(())
}

fn parse(source: &mut Source<'_>, depth: usize) -> Result<Listing, String> {
	if depth >= MAX_DEPTH {
		return Err(bad(format_args!(
			"NAR directory nesting exceeds maximum depth of {MAX_DEPTH}"
		)));
	}
	expect(source, "(")?;
	expect(source, "type")?;
	let kind = tag(source)?;
	match kind.as_slice() {
		b"regular" => {
			let mut executable = false;
			let mut next = tag(source)?;
			if next == b"executable" {
				if !source.string(0).map_err(flatten)?.is_empty() {
					return Err(bad("executable marker has non-empty value"));
				}
				executable = true;
				next = tag(source)?;
			}
			if next != b"contents" {
				return Err(bad(format_args!(
					"expected tag 'contents', got '{}'",
					String::from_utf8_lossy(&next)
				)));
			}
			let size = source.num()?;
			let offset = source.pos;
			source.skip(size.div_ceil(8) * 8)?;
			expect(source, ")")?;
			Ok(Listing::Regular {
				executable,
				size,
				offset,
			})
		}
		b"directory" => {
			let mut entries = BTreeMap::new();
			let mut previous: Vec<u8> = Vec::new();
			loop {
				let next = tag(source)?;
				if next == b")" {
					break;
				}
				if next != b"entry" {
					return Err(bad(format_args!(
						"expected tag 'entry' or ')', got '{}'",
						String::from_utf8_lossy(&next)
					)));
				}
				expect(source, "(")?;
				expect(source, "name")?;
				let name = source.string(MAX_NAME).map_err(flatten)?;
				if name.is_empty()
					|| name == b"."
					|| name == b".."
					|| name.contains(&b'/')
					|| name.contains(&0)
				{
					return Err(bad(format_args!(
						"NAR contains invalid file name '{}'",
						String::from_utf8_lossy(&name)
					)));
				}
				if name <= previous {
					return Err(bad("NAR directory is not sorted"));
				}
				previous = name.clone();
				expect(source, "node")?;
				let child = parse(source, depth + 1)?;
				entries.insert(name, child);
				expect(source, ")")?;
			}
			Ok(Listing::Directory(entries))
		}
		b"symlink" => {
			expect(source, "target")?;
			let target = source.string(MAX_TARGET).map_err(flatten)?;
			if target.is_empty() || target.contains(&0) {
				return Err(bad("NAR contains invalid symlink target"));
			}
			expect(source, ")")?;
			Ok(Listing::Symlink(target))
		}
		_ => Err(bad(format_args!(
			"unknown file type '{}'",
			String::from_utf8_lossy(&kind)
		))),
	}
}

/// `parseNarListing`: index the NAR read from `reader`.
pub fn parse_listing(reader: &mut dyn Read) -> Result<Listing, String> {
	let mut source = Source {
		inner: reader,
		pos: 0,
	};
	let version = match source.string(13) {
		Ok(version) => version,
		Err(Ok(_)) => Vec::new(),
		Err(Err(message)) => return Err(message),
	};
	if version != b"nix-archive-1" {
		return Err(bad("input doesn't look like a Nix archive"));
	}
	parse(&mut source, 0)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::fs;

	#[test]
	fn lists_members_with_content_offsets() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("tree");
		fs::create_dir_all(root.join("sub")).unwrap();
		fs::write(root.join("a"), b"a\0b\n").unwrap();
		fs::write(root.join("sub/b"), b"nested\n").unwrap();
		std::os::unix::fs::symlink("a", root.join("link")).unwrap();
		let mut nar = Vec::new();
		crate::nar::dump_path(&mut nar, &root).unwrap();
		let listing = parse_listing(&mut nar.as_slice()).unwrap();
		let Some(Listing::Regular { size, offset, .. }) = listing.find(&[b"a"]) else {
			panic!("missing file");
		};
		assert_eq!(
			&nar[*offset as usize..(*offset + *size) as usize],
			b"a\0b\n"
		);
		assert_eq!(
			listing.find(&[b"link"]),
			Some(&Listing::Symlink(b"a".to_vec()))
		);
		assert!(listing.find(&[b"a", b"x"]).is_none());
		assert_eq!(
			parse_listing(&mut b"hello\n".as_slice()).unwrap_err(),
			"unexpected end-of-file"
		);
		assert_eq!(
			parse_listing(&mut b"0123456789abcdef".as_slice()).unwrap_err(),
			"bad archive: input doesn't look like a Nix archive"
		);
	}
}
