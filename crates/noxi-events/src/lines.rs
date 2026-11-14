//! Whole-line splitting of concurrent output streams.
//!
//! Builders write arbitrary chunks. Splitting each activity's bytes into
//! complete lines before anything is rendered or logged means two builds
//! running at once can never interleave within a line.

use std::collections::HashMap;

use crate::event::ActivityId;

#[derive(Default)]
struct Partial {
	line: Vec<u8>,
	/// The write position; a carriage return moves it back to the start,
	/// as a terminal would.
	pos: usize,
}

/// Splits each activity's output into complete lines.
#[derive(Default)]
pub struct LineSplitter {
	partial: HashMap<ActivityId, Partial>,
}

impl LineSplitter {
	pub fn new() -> Self {
		Self::default()
	}

	/// Add `data` from `activity`, calling `line` for every line it
	/// completes. Invalid UTF-8 is replaced.
	pub fn push(&mut self, activity: ActivityId, data: &[u8], mut line: impl FnMut(String)) {
		let partial = self.partial.entry(activity).or_default();
		for &byte in data {
			match byte {
				b'\n' => line(take(partial)),
				b'\r' => partial.pos = 0,
				byte => {
					if partial.pos < partial.line.len() {
						partial.line[partial.pos] = byte;
					} else {
						partial.line.push(byte);
					}
					partial.pos += 1;
				}
			}
		}
	}

	/// End `activity`'s stream, returning its unterminated last line.
	pub fn finish(&mut self, activity: ActivityId) -> Option<String> {
		let mut partial = self.partial.remove(&activity)?;
		(!partial.line.is_empty()).then(|| take(&mut partial))
	}
}

fn take(partial: &mut Partial) -> String {
	let line = String::from_utf8_lossy(&partial.line).into_owned();
	partial.line.clear();
	partial.pos = 0;
	line
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn interleaved_chunks_stay_whole_per_activity() {
		let (a, b) = (ActivityId(1), ActivityId(2));
		let mut splitter = LineSplitter::new();
		let mut out = Vec::new();
		splitter.push(a, b"CC foo", |l| out.push((a, l)));
		splitter.push(b, b"checking for gcc... ", |l| out.push((b, l)));
		splitter.push(a, b".o\n50%", |l| out.push((a, l)));
		splitter.push(b, b"yes\n", |l| out.push((b, l)));
		splitter.push(a, b"\r100%\n", |l| out.push((a, l)));
		assert_eq!(
			out,
			vec![
				(a, "CC foo.o".to_owned()),
				(b, "checking for gcc... yes".to_owned()),
				(a, "100%".to_owned()),
			]
		);
		splitter.push(b, b"tail", |_| unreachable!());
		assert_eq!(splitter.finish(b), Some("tail".to_owned()));
		assert_eq!(splitter.finish(a), None);
	}
}
