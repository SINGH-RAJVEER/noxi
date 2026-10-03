//! NAR format boundaries and resource limits checked against the pinned
//! CppNix 2.35.2 reference.
//!
//! Each archive is given to the reference's `nix-store --restore` and to
//! nix-rs's sync and async readers and `nix_rs_store::nar::restore_path`.
//! All must accept or reject it alike. The nesting limit's error message
//! must also match; nix-rs's other reader errors carry no CppNix message.
//! Dumping a tree that is nested too deep is compared with
//! `nix-store --dump`.

use std::fs;
use std::io::{self, Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use nix_compat::nar;
use tokio::io::AsyncReadExt;

fn from_env_or_target(var: &str, target: &str, probe: &str) -> PathBuf {
	if let Some(path) = std::env::var_os(var) {
		return PathBuf::from(path);
	}
	let path = Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../target")
		.join(target);
	assert!(
		path.join(probe).exists(),
		"{target} is missing; run `just reference` or set {var}"
	);
	path
}

fn reference(dir: &Path, args: &[&str], input: &[u8]) -> Output {
	let bin = from_env_or_target("NIXRS_REFERENCE_BIN", "nix-reference/bin", "nix-store");
	let mut child = Command::new(bin.join("nix-store"))
		.args(args)
		.env_clear()
		.env("HOME", dir)
		.env("NIX_CONF_DIR", dir)
		.env("XDG_CONFIG_HOME", dir)
		.env("NIX_REMOTE", "")
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	let mut stdin = child.stdin.take().unwrap();
	// The reference may stop reading early; a broken pipe is expected then.
	let _ = stdin.write_all(input);
	drop(stdin);
	child.wait_with_output().unwrap()
}

/// A tree written without any validation, so that invalid archives can be
/// built too.
enum Tree {
	File(Vec<u8>),
	Executable(Vec<u8>),
	Symlink(Vec<u8>),
	Directory(Vec<(Vec<u8>, Tree)>),
}

#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
	fn number(&mut self, n: u64) -> &mut Self {
		self.0.extend_from_slice(&n.to_le_bytes());
		self
	}

	fn string(&mut self, s: &[u8]) -> &mut Self {
		self.number(s.len() as u64);
		self.0.extend_from_slice(s);
		self.0.resize(self.0.len().next_multiple_of(8), 0);
		self
	}

	fn strings(&mut self, ss: &[&str]) -> &mut Self {
		for s in ss {
			self.string(s.as_bytes());
		}
		self
	}

	fn tree(&mut self, tree: &Tree) -> &mut Self {
		self.strings(&["(", "type"]);
		match tree {
			Tree::File(contents) => {
				self.strings(&["regular", "contents"]).string(contents);
			}
			Tree::Executable(contents) => {
				self.strings(&["regular", "executable", "", "contents"])
					.string(contents);
			}
			Tree::Symlink(target) => {
				self.strings(&["symlink", "target"]).string(target);
			}
			Tree::Directory(entries) => {
				self.string(b"directory");
				for (name, node) in entries {
					self.strings(&["entry", "(", "name"])
						.string(name)
						.string(b"node")
						.tree(node)
						.string(b")");
				}
			}
		}
		self.string(b")")
	}
}

fn archive(tree: &Tree) -> Vec<u8> {
	let mut w = Writer::default();
	w.string(b"nix-archive-1").tree(tree);
	w.0
}

fn nested(depth: usize, leaf: Tree) -> Tree {
	(0..depth).fold(leaf, |node, _| Tree::Directory(vec![(b"d".to_vec(), node)]))
}

fn dir(entries: Vec<(&[u8], Tree)>) -> Tree {
	Tree::Directory(
		entries
			.into_iter()
			.map(|(name, node)| (name.to_vec(), node))
			.collect(),
	)
}

fn drain_sync(node: nar::reader::Node<'_, '_>) -> io::Result<()> {
	match node {
		nar::reader::Node::File { mut reader, .. } => {
			io::copy(&mut reader, &mut io::sink())?;
		}
		nar::reader::Node::Symlink { .. } => {}
		nar::reader::Node::Directory(mut directory) => {
			while let Some(entry) = directory.next()? {
				drain_sync(entry.node)?;
			}
		}
	}
	Ok(())
}

async fn drain_async(node: nar::reader::r#async::Node<'_, '_>) -> io::Result<()> {
	match node {
		nar::reader::r#async::Node::File { mut reader, .. } => {
			let mut contents = Vec::new();
			reader.read_to_end(&mut contents).await?;
		}
		nar::reader::r#async::Node::Symlink { .. } => {}
		nar::reader::r#async::Node::Directory(mut directory) => {
			while let Some(entry) = directory.next().await? {
				Box::pin(drain_async(entry.node)).await?;
			}
		}
	}
	Ok(())
}

/// The outcome of each nix-rs reader: `Ok`, or the error's message.
fn nix_rs_outcomes(bytes: &[u8], dir: &Path) -> [Result<(), String>; 3] {
	let sync = nar::reader::open(&mut Cursor::new(bytes)).and_then(drain_sync);
	let runtime = tokio::runtime::Builder::new_current_thread()
		.build()
		.unwrap();
	let r#async = runtime.block_on(async {
		let mut reader = Cursor::new(bytes);
		let node = nar::reader::r#async::open(&mut reader).await?;
		drain_async(node).await
	});
	let restored = nix_rs_store::nar::restore_path(&dir.join("nix-rs"), &mut Cursor::new(bytes));
	[sync, r#async, restored].map(|result| result.map_err(|e| e.to_string()))
}

fn case(name: &str, bytes: Vec<u8>) -> (String, Vec<u8>) {
	(name.to_owned(), bytes)
}

fn file(contents: &[u8]) -> Tree {
	Tree::File(contents.to_vec())
}

const DEPTH_ERROR: &str = "bad archive: NAR directory nesting exceeds maximum depth of 64";

#[test]
fn archives_at_the_format_limits_are_read_like_the_reference() {
	let long_name = vec![b'n'; 255];
	let too_long_name = vec![b'n'; 256];
	let mut cases = vec![
		(
			"a name of 255 bytes",
			archive(&dir(vec![(&long_name, file(b"x"))])),
			true,
		),
		(
			"a name of 256 bytes",
			archive(&dir(vec![(&too_long_name, file(b"x"))])),
			false,
		),
		(
			"a symlink target of 4095 bytes",
			archive(&Tree::Symlink(vec![b't'; 4095])),
			true,
		),
		(
			"a symlink target of 4096 bytes",
			archive(&Tree::Symlink(vec![b't'; 4096])),
			false,
		),
		(
			"an empty symlink target",
			archive(&Tree::Symlink(vec![])),
			false,
		),
		(
			"a NUL in a symlink target",
			archive(&Tree::Symlink(b"a\0b".to_vec())),
			false,
		),
		(
			"63 nested directories",
			archive(&nested(63, file(b"x"))),
			true,
		),
		(
			"a file at depth 64",
			archive(&nested(64, file(b"x"))),
			false,
		),
		(
			"64 nested directories",
			archive(&nested(64, Tree::Directory(vec![]))),
			false,
		),
		("an empty file", archive(&file(b"")), true),
		(
			"an executable of 9 bytes",
			archive(&Tree::Executable(b"123456789".to_vec())),
			true,
		),
		(
			"names that differ only in case",
			archive(&dir(vec![(b"A", file(b"1")), (b"a", file(b"2"))])),
			true,
		),
		(
			"unsorted names",
			archive(&dir(vec![(b"b", file(b"")), (b"a", file(b""))])),
			false,
		),
		(
			"a repeated name",
			archive(&dir(vec![(b"a", file(b"")), (b"a", file(b""))])),
			false,
		),
		(
			"a non-UTF-8 name",
			archive(&dir(vec![(b"\xff", file(b""))])),
			true,
		),
	]
	.into_iter()
	.map(|(name, bytes, ok)| (case(name, bytes), ok))
	.collect::<Vec<_>>();

	let one_byte = archive(&file(b"x"));
	// The byte after the contents, before the closing `)` string.
	let padding = one_byte.len() - 16 - 7;
	let mut nonzero_padding = one_byte.clone();
	nonzero_padding[padding] = 1;
	let mut executable_value = Writer::default();
	executable_value
		.string(b"nix-archive-1")
		.strings(&["(", "type", "regular", "executable", "x", "contents"])
		.string(b"")
		.string(b")");
	let mut long_tag = Writer::default();
	long_tag
		.string(b"nix-archive-1")
		.strings(&["(", "type"])
		.string(&[b'r'; 33]);
	let mut huge_file = Writer::default();
	huge_file
		.string(b"nix-archive-1")
		.strings(&["(", "type", "regular", "contents"])
		.number(u64::MAX);
	let mut huge_name = Writer::default();
	huge_name
		.string(b"nix-archive-1")
		.strings(&["(", "type", "directory", "entry", "(", "name"])
		.number(1 << 62);
	let mut trailing = one_byte.clone();
	trailing.extend_from_slice(b"trailing");
	cases.extend(
		[
			(case("non-zero padding", nonzero_padding), false),
			(
				case("a non-empty executable marker", executable_value.0),
				false,
			),
			(case("a 33-byte type tag", long_tag.0), false),
			(case("a file of 2^64 - 1 bytes", huge_file.0), false),
			(case("a name length of 2^62", huge_name.0), false),
			(
				case(
					"a truncated archive",
					one_byte[..one_byte.len() - 8].to_vec(),
				),
				false,
			),
			// Neither engine reads past the archive.
			(case("bytes after the archive", trailing), true),
		]
		.into_iter(),
	);

	for (index, ((name, bytes), ok)) in cases.iter().enumerate() {
		let dir = tempfile::tempdir().unwrap();
		let out = dir.path().join("reference");
		let output = reference(dir.path(), &["--restore", out.to_str().unwrap()], bytes);
		let stderr = String::from_utf8_lossy(&output.stderr);
		assert_eq!(output.status.success(), *ok, "{index} {name}: {stderr}");
		for (reader, outcome) in ["sync", "async", "restore"]
			.iter()
			.zip(nix_rs_outcomes(bytes, dir.path()))
		{
			assert_eq!(outcome.is_ok(), *ok, "{name}, {reader}: {outcome:?}");
			if stderr.contains(DEPTH_ERROR) {
				assert_eq!(outcome, Err(DEPTH_ERROR.to_owned()), "{name}, {reader}");
			}
		}
		if name.contains("depth 64") || name.starts_with("64") {
			assert!(stderr.contains(DEPTH_ERROR), "{name}: {stderr}");
		}
		if *ok {
			// The restored trees are the same.
			let mut reference_nar = Vec::new();
			nix_rs_store::nar::dump_path(&mut reference_nar, &out).unwrap();
			let mut nix_rs_nar = Vec::new();
			nix_rs_store::nar::dump_path(&mut nix_rs_nar, &dir.path().join("nix-rs")).unwrap();
			assert_eq!(nix_rs_nar, reference_nar, "{name}");
		}
	}
}

#[test]
fn dumping_a_tree_nested_too_deep_fails_like_the_reference() {
	let dir = tempfile::tempdir().unwrap();
	for (depth, leaf) in [(63, false), (63, true), (64, false)] {
		let root = dir.path().join(format!("tree-{depth}-{leaf}"));
		let mut deepest = root.clone();
		for _ in 0..depth {
			deepest.push("d");
		}
		fs::create_dir_all(&deepest).unwrap();
		if leaf {
			fs::write(deepest.join("f"), b"x").unwrap();
		}
		let output = reference(dir.path(), &["--dump", root.to_str().unwrap()], b"");
		let mut nar = Vec::new();
		let result = nix_rs_store::nar::dump_path(&mut nar, &root);
		let ok = depth == 63 && !leaf;
		assert_eq!(output.status.success(), ok, "{}", root.display());
		match result {
			Ok(()) => {
				assert!(ok);
				assert_eq!(nar, output.stdout);
			}
			Err(error) => {
				assert!(!ok);
				let stderr = String::from_utf8_lossy(&output.stderr);
				assert_eq!(stderr, format!("error: {error}\n"));
			}
		}
	}
}
