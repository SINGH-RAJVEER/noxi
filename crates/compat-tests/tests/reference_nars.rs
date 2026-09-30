//! Valid filesystem NARs dumped by the pinned CppNix 2.35.2 reference.
//! Sources and exact generation commands are in fixtures/nar-reference/provenance.json.

use std::io::{self, BufRead, BufReader, Cursor, Read, Write};

use data_encoding::HEXLOWER;
use nix_compat::nar;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

#[derive(Debug, PartialEq, Eq)]
enum Source {
	File { executable: bool, contents: Vec<u8> },
	Symlink(Vec<u8>),
	Directory(Vec<(Vec<u8>, Source)>),
}

fn hex(text: &str) -> Vec<u8> {
	let compact: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
	HEXLOWER.decode(&compact).expect("valid fixture hex")
}

fn source(value: &Value) -> Source {
	match value["type"].as_str().expect("source type") {
		"regular" => Source::File {
			executable: value["executable"].as_bool().expect("executable flag"),
			contents: hex(value["contents_hex"].as_str().expect("file contents")),
		},
		"symlink" => Source::Symlink(hex(value["target_hex"].as_str().expect("symlink target"))),
		"directory" => {
			let entries: Vec<_> = value["entries"]
				.as_array()
				.expect("directory entries")
				.iter()
				.map(|entry| {
					(
						hex(entry["name_hex"].as_str().expect("entry name")),
						source(&entry["node"]),
					)
				})
				.collect();
			assert!(entries.windows(2).all(|pair| pair[0].0 < pair[1].0));
			Source::Directory(entries)
		}
		other => panic!("unknown source type {other}"),
	}
}

fn fixture(name: &str) -> (Source, Vec<u8>) {
	let provenance: Value =
		serde_json::from_str(include_str!("../fixtures/nar-reference/provenance.json"))
			.expect("valid reference provenance");
	assert_eq!(provenance["reference_release"], "2.35.2");
	assert_eq!(
		provenance["source_revision"],
		"2c73b59da29606068c0c98db015dd3a66955525d"
	);
	let encoded = match name {
		"regular" => include_str!("../fixtures/nar-reference/regular.nar.hex"),
		"executable" => include_str!("../fixtures/nar-reference/executable.nar.hex"),
		"symlink" => include_str!("../fixtures/nar-reference/symlink.nar.hex"),
		"directory" => include_str!("../fixtures/nar-reference/directory.nar.hex"),
		other => panic!("unknown fixture {other}"),
	};
	let archive = hex(encoded);
	let case = &provenance["cases"][name];
	assert_eq!(case["exit_code"], 0);
	assert_eq!(case["stderr"], "");
	assert_eq!(archive.len() as u64, case["nar_size"].as_u64().unwrap());
	assert_eq!(
		HEXLOWER.encode(&Sha256::digest(&archive)),
		case["nar_sha256"].as_str().unwrap(),
		"{name}: copied hex must match the reference dump digest"
	);
	(source(&case["source"]), archive)
}

fn write_sync<W: Write>(node: nar::writer::Node<'_, W>, source: &Source) -> io::Result<()> {
	match source {
		Source::File {
			executable,
			contents,
		} => {
			// Force file payloads through multiple fill_buf/consume iterations.
			let mut reader = BufReader::with_capacity(3, contents.as_slice());
			node.file(*executable, contents.len() as u64, &mut reader)
		}
		Source::Symlink(target) => node.symlink(target),
		Source::Directory(entries) => {
			let mut directory = node.directory()?;
			for (name, child) in entries {
				write_sync(directory.entry(name)?, child)?;
			}
			directory.close()
		}
	}
}

async fn write_async(node: nar::writer::r#async::Node<'_, '_>, source: &Source) -> io::Result<()> {
	match source {
		Source::File {
			executable,
			contents,
		} => {
			let mut reader = tokio::io::BufReader::with_capacity(3, contents.as_slice());
			node.file(*executable, contents.len() as u64, &mut reader)
				.await
		}
		Source::Symlink(target) => node.symlink(target).await,
		Source::Directory(entries) => {
			let mut directory = node.directory().await?;
			for (name, child) in entries {
				Box::pin(write_async(directory.entry(name).await?, child)).await?;
			}
			directory.close().await
		}
	}
}

fn read_sync(node: nar::reader::Node<'_, '_>) -> io::Result<Source> {
	Ok(match node {
		nar::reader::Node::File {
			executable,
			mut reader,
		} => {
			let length = reader.len();
			let mut contents = Vec::new();
			reader.read_to_end(&mut contents)?;
			assert_eq!(contents.len() as u64, length);
			Source::File {
				executable,
				contents,
			}
		}
		nar::reader::Node::Symlink { target } => Source::Symlink(target),
		nar::reader::Node::Directory(mut directory) => {
			let mut entries = Vec::new();
			while let Some(entry) = directory.next()? {
				entries.push((entry.name.to_vec(), read_sync(entry.node)?));
			}
			Source::Directory(entries)
		}
	})
}

async fn read_async(node: nar::reader::r#async::Node<'_, '_>) -> io::Result<Source> {
	Ok(match node {
		nar::reader::r#async::Node::File {
			executable,
			mut reader,
		} => {
			let length = reader.len();
			let mut contents = Vec::new();
			reader.read_to_end(&mut contents).await?;
			assert_eq!(contents.len() as u64, length);
			Source::File {
				executable,
				contents,
			}
		}
		nar::reader::r#async::Node::Symlink { target } => Source::Symlink(target),
		nar::reader::r#async::Node::Directory(mut directory) => {
			let mut entries = Vec::new();
			while let Some(entry) = directory.next().await? {
				entries.push((entry.name.to_vec(), Box::pin(read_async(entry.node)).await?));
			}
			Source::Directory(entries)
		}
	})
}

macro_rules! reference_tests {
	($name:ident) => {
		mod $name {
			use super::*;

			#[test]
			fn sync_writer_matches_reference_bytes() {
				let (source, reference) = fixture(stringify!($name));
				let mut output = Vec::new();
				write_sync(nar::writer::open(&mut output).unwrap(), &source).unwrap();
				assert_eq!(output, reference);
			}

			#[tokio::test]
			async fn async_writer_matches_reference_bytes() {
				let (source, reference) = fixture(stringify!($name));
				let mut output = Vec::new();
				write_async(
					nar::writer::r#async::open(&mut output).await.unwrap(),
					&source,
				)
				.await
				.unwrap();
				assert_eq!(output, reference);
			}

			#[test]
			fn sync_reader_matches_reference_source() {
				let (source, reference) = fixture(stringify!($name));
				// Split tokens, length prefixes, payloads, and padding across reads.
				let mut input = BufReader::with_capacity(5, Cursor::new(&reference));
				let actual = read_sync(nar::reader::open(&mut input).unwrap()).unwrap();
				assert_eq!(actual, source);
				assert!(input.fill_buf().unwrap().is_empty(), "unconsumed NAR bytes");
			}

			#[tokio::test]
			async fn async_reader_matches_reference_source() {
				let (source, reference) = fixture(stringify!($name));
				let mut input = tokio::io::BufReader::with_capacity(5, Cursor::new(&reference));
				let actual = read_async(nar::reader::r#async::open(&mut input).await.unwrap())
					.await
					.unwrap();
				assert_eq!(actual, source);
				assert!(
					input.fill_buf().await.unwrap().is_empty(),
					"unconsumed NAR bytes"
				);
			}
		}
	};
}

reference_tests!(regular);
reference_tests!(executable);
reference_tests!(symlink);
reference_tests!(directory);
