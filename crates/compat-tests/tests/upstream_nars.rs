use std::{
	fs,
	io::{self, Cursor, Write},
	path::PathBuf,
};

use nix_compat::nar;

fn fixture(name: &str) -> Vec<u8> {
	let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
		.join("../../tests/upstream/nix/src/libutil-tests/data/nars")
		.join(format!("{name}.nar"));
	fs::read(path).expect("upstream NAR fixture must exist")
}

#[test]
fn valid_archive_round_trips_without_consuming_trailing_bytes() {
	let source = fixture("regular-one-byte-zero-padding");
	let mut input = source.clone();
	input.extend_from_slice(b"sentinel");
	let mut reader = Cursor::new(input);
	let mut output = Vec::new();

	nar::copy(&mut reader, &mut output).expect("CppNix accepts the archive");
	assert_eq!(output, source);
	assert_eq!(reader.position(), source.len() as u64);
}

#[test]
fn malformed_archives_are_rejected() {
	for (name, expected_kind) in [
		(
			"invalid-tag-instead-of-contents",
			io::ErrorKind::InvalidData,
		),
		("nul-character", io::ErrorKind::InvalidData),
		("dot", io::ErrorKind::InvalidData),
		("dotdot", io::ErrorKind::InvalidData),
		("slash", io::ErrorKind::InvalidData),
		("empty", io::ErrorKind::InvalidData),
		("executable-after-contents", io::ErrorKind::InvalidData),
		("name-after-node", io::ErrorKind::InvalidData),
		(
			"regular-one-byte-non-zero-padding",
			io::ErrorKind::InvalidData,
		),
		(
			"regular-truncated-large-contents",
			io::ErrorKind::UnexpectedEof,
		),
		("regular-truncated-padding", io::ErrorKind::UnexpectedEof),
	] {
		let source = fixture(name);
		let mut output = Vec::new();
		let error = nar::copy(&mut Cursor::new(source), &mut output)
			.expect_err("CppNix rejects the malformed archive");
		assert_eq!(error.kind(), expected_kind, "{name}");
	}
}

struct ZeroWriter;

impl Write for ZeroWriter {
	fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
		Ok(0)
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

#[test]
fn archive_copy_reports_a_stalled_output() {
	let source = fixture("regular-one-byte-zero-padding");
	let mut input = Cursor::new(source);
	let node = nar::reader::open(&mut input).expect("valid upstream NAR");
	let nar::reader::Node::File { mut reader, .. } = node else {
		panic!("fixture must contain a regular file");
	};

	let error = reader
		.copy(ZeroWriter)
		.expect_err("stalled writes must fail");
	assert_eq!(error.kind(), io::ErrorKind::WriteZero);
}
