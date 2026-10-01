//! Finding which candidate store paths a file tree refers to, by scanning
//! its NAR serialisation for their hash parts, as CppNix's
//! `scanForReferences` does.

use std::collections::{BTreeSet, HashMap};
use std::io::{self, Write};
use std::path::Path;

use nix_compat::nixbase32;
use nix_compat::store_path::StorePath;

/// The length of a store path hash part in Nix base32.
const HASH_LEN: usize = 32;

const ALPHABET: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

fn is_base32(b: u8) -> bool {
	ALPHABET.contains(&b)
}

/// A sink that records which candidate hash parts appear in what it is
/// given, including across write boundaries.
pub struct RefScanSink {
	candidates: HashMap<[u8; HASH_LEN], StorePath>,
	found: BTreeSet<StorePath>,
	tail: Vec<u8>,
}

impl RefScanSink {
	pub fn new<'a>(candidates: impl IntoIterator<Item = &'a StorePath>) -> Self {
		let candidates = candidates
			.into_iter()
			.map(|path| {
				let mut hash = [0u8; HASH_LEN];
				hash.copy_from_slice(nixbase32::encode(path.digest()).as_bytes());
				(hash, path.clone())
			})
			.collect();
		Self {
			candidates,
			found: BTreeSet::new(),
			tail: Vec::new(),
		}
	}

	fn scan(&mut self, data: &[u8]) {
		let mut i = 0;
		while i + HASH_LEN <= data.len() {
			// Find the last non-base32 byte in the window and skip past it.
			let window = &data[i..i + HASH_LEN];
			if let Some(bad) = window.iter().rposition(|&b| !is_base32(b)) {
				i += bad + 1;
				continue;
			}
			let mut key = [0u8; HASH_LEN];
			key.copy_from_slice(window);
			if let Some(path) = self.candidates.get(&key) {
				self.found.insert(path.clone());
			}
			i += 1;
		}
	}

	pub fn finish(self) -> BTreeSet<StorePath> {
		self.found
	}
}

impl Write for RefScanSink {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		// Scan the previous tail joined with the start of this buffer, then
		// the buffer itself.
		let head = buf.len().min(HASH_LEN - 1);
		let mut joined = std::mem::take(&mut self.tail);
		joined.extend_from_slice(&buf[..head]);
		self.scan(&joined);
		self.scan(buf);
		// The last bytes seen so far, which may span several short writes.
		self.tail = if buf.len() >= HASH_LEN - 1 {
			buf[buf.len() - (HASH_LEN - 1)..].to_vec()
		} else {
			// `joined` is the previous tail and all of `buf`.
			joined[joined.len().saturating_sub(HASH_LEN - 1)..].to_vec()
		};
		Ok(buf.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

/// The candidates whose hash parts occur in the NAR serialisation of
/// `path`.
pub fn scan_for_references(path: &Path, candidates: &BTreeSet<StorePath>) -> io::Result<BTreeSet<StorePath>> {
	let mut sink = RefScanSink::new(candidates);
	nix_rs_store::nar::dump_path(&mut sink, path)?;
	Ok(sink.finish())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn finds_hashes_across_writes() {
		let a = StorePath::from_bytes(b"7h7qgvs4kgzsn8a6rb273saxyqh4jxlz-a").unwrap();
		let b = StorePath::from_bytes(b"ffffffffffffffffffffffffffffffff-b").unwrap();
		let mut sink = RefScanSink::new([&a, &b]);
		let text = b"xx/nix/store/7h7qgvs4kgzsn8a6rb273saxyqh4jxlz-a/bin";
		for chunk in text.chunks(5) {
			sink.write_all(chunk).unwrap();
		}
		assert_eq!(sink.finish(), BTreeSet::from([a]));
	}
}
