use std::panic::AssertUnwindSafe;

use futures::FutureExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

mod nar {
	pub use crate::nar::reader::r#async as reader;
}

#[tokio::test]
async fn symlink() {
	let mut f = std::io::Cursor::new(include_bytes!("../../tests/symlink.nar"));
	let node = nar::reader::open(&mut f).await.unwrap();

	match node {
		nar::reader::Node::Symlink { target } => {
			assert_eq!(
				&b"/nix/store/somewhereelse"[..],
				&target,
				"target must match"
			);
		}
		_ => panic!("unexpected type"),
	}
}

#[tokio::test]
async fn file() {
	let mut f = std::io::Cursor::new(include_bytes!("../../tests/helloworld.nar"));
	let node = nar::reader::open(&mut f).await.unwrap();

	match node {
		nar::reader::Node::File {
			executable,
			mut reader,
		} => {
			assert!(!executable);
			let mut buf = vec![];
			reader
				.read_to_end(&mut buf)
				.await
				.expect("read must succeed");
			assert_eq!(&b"Hello World!"[..], &buf);
		}
		_ => panic!("unexpected type"),
	}
}

#[tokio::test]
async fn complicated() {
	let mut f = std::io::Cursor::new(include_bytes!("../../tests/complicated.nar"));
	let node = nar::reader::open(&mut f).await.unwrap();

	match node {
		nar::reader::Node::Directory(mut dir_reader) => {
			// first entry is .keep, an empty regular file.
			must_read_file(
				".keep",
				dir_reader
					.next()
					.await
					.expect("next must succeed")
					.expect("must be some"),
			)
			.await;

			// second entry is aa, a symlink to /nix/store/somewhereelse
			must_be_symlink(
				"aa",
				"/nix/store/somewhereelse",
				dir_reader
					.next()
					.await
					.expect("next must be some")
					.expect("must be some"),
			);

			{
				// third entry is a directory called "keep"
				let entry = dir_reader
					.next()
					.await
					.expect("next must be some")
					.expect("must be some");

				assert_eq!(b"keep", entry.name);

				match entry.node {
					nar::reader::Node::Directory(mut subdir_reader) => {
						{
							// first entry is .keep, an empty regular file.
							let entry = subdir_reader
								.next()
								.await
								.expect("next must succeed")
								.expect("must be some");

							must_read_file(".keep", entry).await;
						}

						// we must read the None
						assert!(
							subdir_reader
								.next()
								.await
								.expect("next must succeed")
								.is_none(),
							"keep directory contains only .keep"
						);
					}
					_ => panic!("unexpected type for keep/.keep"),
				}
			};

			// reading more entries yields None (and we actually must read until this)
			assert!(dir_reader.next().await.expect("must succeed").is_none());
		}
		_ => panic!("unexpected type"),
	}
}

#[tokio::test]
#[should_panic(expected = "Non-ready archive reader used!")]
async fn file_read_abandoned() {
	let mut f = std::io::Cursor::new(file_directory(b"Hello World!"));
	let node = nar::reader::open(&mut f).await.unwrap();

	match node {
		nar::reader::Node::Directory(mut dir_reader) => {
			// Abandon a nonempty file. Empty files are already complete when opened.
			{
				let entry = dir_reader
					.next()
					.await
					.expect("next must succeed")
					.expect("must be some");

				assert_eq!(b".keep", entry.name);
				// don't bother to finish reading it.
			};

			// this should panic (not return an error), because we are meant to abandon the archive reader now.
			dir_reader.next().await.unwrap();
		}
		_ => panic!("unexpected type"),
	}
}

#[tokio::test]
#[should_panic(expected = "Non-ready archive reader used!")]
async fn dir_read_abandoned() {
	let mut f = std::io::Cursor::new(include_bytes!("../../tests/complicated.nar"));
	let node = nar::reader::open(&mut f).await.unwrap();

	match node {
		nar::reader::Node::Directory(mut dir_reader) => {
			// first entry is .keep, an empty regular file.
			must_read_file(
				".keep",
				dir_reader
					.next()
					.await
					.expect("next must succeed")
					.expect("must be some"),
			)
			.await;

			// second entry is aa, a symlink to /nix/store/somewhereelse
			must_be_symlink(
				"aa",
				"/nix/store/somewhereelse",
				dir_reader
					.next()
					.await
					.expect("next must be some")
					.expect("must be some"),
			);

			{
				// third entry is a directory called "keep"
				let entry = dir_reader
					.next()
					.await
					.expect("next must be some")
					.expect("must be some");

				assert_eq!(b"keep", entry.name);

				match entry.node {
					nar::reader::Node::Directory(_) => {
						// don't finish using it, which poisons the archive reader
					}
					_ => panic!("unexpected type for keep/.keep"),
				}
			};

			// this should panic, because we didn't finish reading the child subdirectory
			assert!(dir_reader.next().await.expect("must succeed").is_none());
		}
		_ => panic!("unexpected type"),
	}
}

#[tokio::test]
#[should_panic(expected = "Non-ready archive reader used!")]
async fn dir_read_after_none() {
	let mut f = std::io::Cursor::new(include_bytes!("../../tests/complicated.nar"));
	let node = nar::reader::open(&mut f).await.unwrap();

	match node {
		nar::reader::Node::Directory(mut dir_reader) => {
			// first entry is .keep, an empty regular file.
			must_read_file(
				".keep",
				dir_reader
					.next()
					.await
					.expect("next must succeed")
					.expect("must be some"),
			)
			.await;

			// second entry is aa, a symlink to /nix/store/somewhereelse
			must_be_symlink(
				"aa",
				"/nix/store/somewhereelse",
				dir_reader
					.next()
					.await
					.expect("next must be some")
					.expect("must be some"),
			);

			{
				// third entry is a directory called "keep"
				let entry = dir_reader
					.next()
					.await
					.expect("next must be some")
					.expect("must be some");

				assert_eq!(b"keep", entry.name);

				match entry.node {
					nar::reader::Node::Directory(mut subdir_reader) => {
						// first entry is .keep, an empty regular file.
						must_read_file(
							".keep",
							subdir_reader
								.next()
								.await
								.expect("next must succeed")
								.expect("must be some"),
						)
						.await;

						// we must read the None
						assert!(
							subdir_reader
								.next()
								.await
								.expect("next must succeed")
								.is_none(),
							"keep directory contains only .keep"
						);
					}
					_ => panic!("unexpected type for keep/.keep"),
				}
			};

			// reading more entries yields None (and we actually must read until this)
			assert!(dir_reader.next().await.expect("must succeed").is_none());

			// this should panic, because we already got a none so we're meant to stop.
			dir_reader.next().await.unwrap();
		}
		_ => panic!("unexpected type"),
	}
}

fn file_directory(contents: &[u8]) -> Vec<u8> {
	let mut archive = vec![];
	let mut directory = crate::nar::writer::open(&mut archive)
		.unwrap()
		.directory()
		.unwrap();
	directory
		.entry(b".keep")
		.unwrap()
		.file(false, contents.len() as u64, &mut &contents[..])
		.unwrap();
	directory.close().unwrap();
	archive
}

#[tokio::test]
async fn file_completion_allows_parent() {
	for contents in [b"".as_slice(), b"Hello World!"] {
		for buffered in [false, true] {
			let mut f = std::io::Cursor::new(file_directory(contents));
			let nar::reader::Node::Directory(mut directory) =
				nar::reader::open(&mut f).await.unwrap()
			else {
				panic!("expected directory");
			};
			{
				let entry = directory.next().await.unwrap().unwrap();
				let nar::reader::Node::File { mut reader, .. } = entry.node else {
					panic!("expected file");
				};
				if buffered {
					while !reader.is_empty() {
						let n = reader.fill_buf().await.unwrap().len();
						assert_ne!(n, 0);
						reader.consume(n);
					}
				} else {
					reader
						.read_exact(&mut vec![0; contents.len()])
						.await
						.unwrap();
				}
				assert!(reader.is_empty());
				assert_eq!(reader.read(&mut [0]).await.unwrap(), 0);
			}
			assert!(directory.next().await.unwrap().is_none());
		}
	}
}

#[tokio::test]
async fn file_errors_poison_reader_and_parent() {
	for (buffered, truncated) in [(false, false), (false, true), (true, false), (true, true)] {
		let mut archive = file_directory(b"Hello World!");
		// Corrupt the file terminator, leaving the directory terminators intact.
		let offset = archive.len() - 3 * crate::nar::wire::TOK_PAR.len();
		if truncated {
			archive.truncate(offset);
		} else {
			archive[offset] ^= 1;
		}
		let mut f = std::io::Cursor::new(archive);
		let nar::reader::Node::Directory(mut directory) = nar::reader::open(&mut f).await.unwrap()
		else {
			panic!("expected directory");
		};
		{
			let entry = directory.next().await.unwrap().unwrap();
			let nar::reader::Node::File { mut reader, .. } = entry.node else {
				panic!("expected file");
			};
			let error = if buffered {
				tokio::io::copy_buf(&mut reader, &mut tokio::io::sink())
					.await
					.unwrap_err()
			} else {
				reader.read_to_end(&mut vec![]).await.unwrap_err()
			};
			assert_eq!(
				error.kind(),
				if truncated {
					std::io::ErrorKind::UnexpectedEof
				} else {
					std::io::ErrorKind::InvalidData
				}
			);
			assert!(
				AssertUnwindSafe(reader.read(&mut [0]))
					.catch_unwind()
					.await
					.is_err()
			);
		}
		assert!(
			AssertUnwindSafe(directory.next())
				.catch_unwind()
				.await
				.is_err()
		);
	}
}

#[tokio::test]
async fn directory_error_prevents_retry() {
	let mut archive = file_directory(b"");
	// An invalid child tag must poison the directory, even before a child is returned.
	let offset = archive
		.windows(crate::nar::wire::TOK_REG.len())
		.position(|bytes| bytes == crate::nar::wire::TOK_REG)
		.unwrap();
	archive[offset] ^= 1;
	let mut f = std::io::Cursor::new(archive);
	let nar::reader::Node::Directory(mut directory) = nar::reader::open(&mut f).await.unwrap()
	else {
		panic!("expected directory");
	};
	assert_eq!(
		directory.next().await.err().unwrap().kind(),
		std::io::ErrorKind::InvalidData
	);
	assert!(
		AssertUnwindSafe(directory.next())
			.catch_unwind()
			.await
			.is_err()
	);
}

#[tokio::test]
async fn cancelled_next_prevents_retry() {
	let archive = file_directory(b"");
	let prefix_len = crate::nar::wire::TOK_NAR.len() + crate::nar::wire::TOK_DIR.len() + 4;
	let (mut writer, reader) = tokio::io::duplex(1024);
	writer.write_all(&archive[..prefix_len]).await.unwrap();
	let mut reader = tokio::io::BufReader::new(reader);
	let nar::reader::Node::Directory(mut directory) = nar::reader::open(&mut reader).await.unwrap()
	else {
		panic!("expected directory");
	};
	{
		let next = directory.next();
		tokio::pin!(next);
		assert!(futures::poll!(&mut next).is_pending());
	}
	assert!(
		AssertUnwindSafe(directory.next())
			.catch_unwind()
			.await
			.is_err()
	);
}

#[tokio::test]
#[should_panic(expected = "Non-ready archive reader used!")]
async fn buffered_file_read_abandoned() {
	let mut f = std::io::Cursor::new(file_directory(b"Hello"));
	let nar::reader::Node::Directory(mut directory) = nar::reader::open(&mut f).await.unwrap()
	else {
		panic!("expected directory");
	};
	{
		let entry = directory.next().await.unwrap().unwrap();
		let nar::reader::Node::File { mut reader, .. } = entry.node else {
			panic!("expected file");
		};
		// The trailer is validated, but its payload is still unconsumed.
		assert_eq!(reader.fill_buf().await.unwrap(), b"Hello");
		reader.consume(1);
	}
	directory.next().await.unwrap();
}

async fn must_read_file(name: &'static str, entry: nar::reader::Entry<'_, '_>) {
	assert_eq!(name.as_bytes(), entry.name);

	match entry.node {
		nar::reader::Node::File {
			executable,
			mut reader,
		} => {
			assert!(!executable);
			assert_eq!(reader.read(&mut [0]).await.unwrap(), 0);
		}
		_ => panic!("unexpected type for {name}"),
	}
}

fn must_be_symlink(
	name: &'static str,
	exp_target: &'static str,
	entry: nar::reader::Entry<'_, '_>,
) {
	assert_eq!(name.as_bytes(), entry.name);

	match entry.node {
		nar::reader::Node::Symlink { target } => {
			assert_eq!(exp_target.as_bytes(), &target);
		}
		_ => panic!("unexpected type for {name}"),
	}
}
