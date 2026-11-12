//! Content hashing modulo a store path's hash part, as CppNix's
//! `HashModuloSink` does, and equal-length output reference rewriting.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Seek, Write};
use std::path::Path;

use aho_corasick::{AhoCorasick, MatchKind};
use nix_compat::nixhash::{HashAlgo, NixHash, NixHashDigester};

use crate::{FileIngestionMethod, Result, delete_path, nar};

/// A streaming replacement writer. All patterns have the same length and
/// replacements must preserve it, so NAR framing and padding stay valid.
struct Rewriter<W> {
	inner: W,
	matcher: PatternSearch,
	replacements: Vec<Vec<u8>>,
	pending: Vec<u8>,
	position: u64,
	matches: Vec<u64>,
	width: usize,
}

enum PatternSearch {
	Single(memchr::memmem::Finder<'static>),
	Multiple(AhoCorasick),
}

impl PatternSearch {
	fn find(&self, bytes: &[u8]) -> Option<(usize, usize)> {
		match self {
			Self::Single(finder) => finder.find(bytes).map(|start| (start, 0)),
			Self::Multiple(matcher) => matcher
				.find(bytes)
				.map(|found| (found.start(), found.pattern().as_usize())),
		}
	}
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
		let matcher = if rewrites.len() == 1 {
			PatternSearch::Single(
				memchr::memmem::Finder::new(rewrites.keys().next().unwrap()).into_owned(),
			)
		} else {
			PatternSearch::Multiple(
				AhoCorasick::builder()
					.match_kind(MatchKind::LeftmostFirst)
					.build(rewrites.keys())
					.expect("store hash rewrite patterns fit in a matcher"),
			)
		};
		Self {
			inner,
			matcher,
			replacements: rewrites.into_values().collect(),
			pending: Vec::new(),
			position: 0,
			matches: Vec::new(),
			width,
		}
	}

	fn drain(&mut self, finish: bool) -> io::Result<()> {
		// Retain only starts that could become a match on the next write.
		let mut consumed = self.pending.len().saturating_sub(self.width - 1);
		let mut unwritten = 0;
		while let Some((offset, pattern)) = self.matcher.find(&self.pending[unwritten..]) {
			let start = unwritten + offset;
			self.inner.write_all(&self.pending[unwritten..start])?;
			self.inner.write_all(&self.replacements[pattern])?;
			self.matches.push(self.position + start as u64);
			unwritten = start + self.width;
			consumed = consumed.max(unwritten);
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

/// [`hash_modulo`] of bytes already in memory, such as a NAR.
pub fn hash_modulo_bytes(data: &[u8], algo: HashAlgo, modulus: &[u8]) -> NixHash {
	let mut writer = Rewriter::new(
		DigestWriter(NixHashDigester::new(algo)),
		BTreeMap::from([(modulus.to_vec(), vec![0; modulus.len()])]),
	);
	writer.write_all(data).expect("hashing does not fail");
	writer.drain(true).expect("hashing does not fail");
	for offset in writer.matches {
		writer.inner.0.update(format!("|{offset}").as_bytes());
	}
	writer.inner.0.finalize()
}

/// `RewritingSink` over bytes in memory: every occurrence of `from` is
/// replaced by `to`, which must have the same length.
pub fn rewrite_bytes(data: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
	let mut writer = Rewriter::new(Vec::new(), BTreeMap::from([(from.to_vec(), to.to_vec())]));
	writer
		.write_all(data)
		.expect("writing to memory does not fail");
	writer.drain(true).expect("writing to memory does not fail");
	writer.inner
}

#[cfg(test)]
mod tests {
	use super::*;
	use sha2::{Digest, Sha256};
	use std::fs;

	#[test]
	fn rewriting_matches_leftmost_nonoverlapping_search_at_every_chunk_size() {
		let rewrites = BTreeMap::from([
			(b"aaaa".to_vec(), b"1111".to_vec()),
			(b"aaab".to_vec(), b"2222".to_vec()),
			(b"\0abc".to_vec(), b"3333".to_vec()),
		]);
		let input = b"aaaaaaaabx\0abcaaabaaaaaaa";
		let mut expected = Vec::new();
		let mut offsets = Vec::new();
		let mut pos = 0;
		while pos < input.len() {
			if let Some(value) = input.get(pos..pos + 4).and_then(|key| rewrites.get(key)) {
				expected.extend_from_slice(value);
				offsets.push(pos as u64);
				pos += 4;
			} else {
				expected.push(input[pos]);
				pos += 1;
			}
		}
		for chunk_size in 1..=input.len() {
			let mut writer = Rewriter::new(Vec::new(), rewrites.clone());
			for chunk in input.chunks(chunk_size) {
				writer.write_all(chunk).unwrap();
			}
			writer.drain(true).unwrap();
			assert_eq!(writer.inner, expected, "chunk size {chunk_size}");
			assert_eq!(writer.matches, offsets, "chunk size {chunk_size}");
		}
	}

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
