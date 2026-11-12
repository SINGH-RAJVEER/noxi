/// The replacement for a byte ATerm strings escape.
fn replacement(byte: u8) -> Option<&'static [u8]> {
	match byte {
		b'\\' => Some(b"\\\\"),
		b'\n' => Some(b"\\n"),
		b'\r' => Some(b"\\r"),
		b'\t' => Some(b"\\t"),
		b'"' => Some(b"\\\""),
		_ => None,
	}
}

/// Given a byte sequence, writes it in escaped form to the passed writer.
/// Does not add surrounding quotes.
pub fn write_escaped<P: AsRef<[u8]>>(s: P, w: &mut impl std::io::Write) -> std::io::Result<()> {
	let s = s.as_ref();
	let mut pos = 0;

	// Every escaped sequence is one byte, so a byte scan finds them all; most
	// strings contain none and are written in one piece.
	for (i, &byte) in s.iter().enumerate() {
		if let Some(replacement) = replacement(byte) {
			w.write_all(&s[pos..i])?;
			w.write_all(replacement)?;
			pos = i + 1;
		}
	}

	w.write_all(&s[pos..])
}

#[cfg(test)]
mod tests {
	use super::write_escaped;
	use rstest::rstest;

	#[rstest]
	#[case::empty(b"", b"")]
	#[case::doublequote(b"\"", b"\\\"")]
	#[case::colon(b":", b":")]
	#[case::complex(b"foo\n\rbar\\baz", b"foo\\n\\rbar\\\\baz")]
	fn escape(#[case] input: &[u8], #[case] expected: &[u8]) {
		let mut buf = Vec::new();
		write_escaped(input, &mut buf).unwrap();

		assert_eq!(expected, buf.as_slice());
	}
}
