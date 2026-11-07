//! Git objects shared with CppNix's packfiles-only tarball-cache-v2.

use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;

use crate::mount::Mount;
use noxi_flakeref::error::{Error, Result};

pub(super) fn git(repo: &Path, args: &[&str], input: &[u8]) -> Result<String> {
	let mut child = Command::new("git")
		.arg("-C")
		.arg(repo)
		.args(args)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()?;
	child.stdin.take().unwrap().write_all(input)?;
	let output = child.wait_with_output()?;
	if !output.status.success() {
		return Err(Error::new(
			String::from_utf8_lossy(&output.stderr).trim().to_owned(),
		));
	}
	Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Objects streamed into one undeltified version 2 pack with its version 2
/// index, as `git pack-objects` would write them, without loose objects.
struct PackWriter {
	file: std::io::BufWriter<tempfile::NamedTempFile>,
	offset: u64,
	/// Object name, CRC-32 of its packed bytes and pack offset.
	entries: Vec<([u8; 20], u32, u64)>,
	written: std::collections::HashSet<[u8; 20]>,
}

impl PackWriter {
	fn new(directory: &Path) -> Result<Self> {
		let mut file = std::io::BufWriter::new(tempfile::NamedTempFile::new_in(directory)?);
		// The object count is filled in by `finish`.
		file.write_all(b"PACK\0\0\0\x02\0\0\0\0")?;
		Ok(Self {
			file,
			offset: 12,
			entries: Vec::new(),
			written: Default::default(),
		})
	}

	/// Add a blob or tree and return its object name.
	fn add(&mut self, kind: &str, content: &[u8]) -> Result<[u8; 20]> {
		use sha1::{Digest, Sha1};
		let mut hasher = Sha1::new();
		hasher.update(format!("{kind} {}\0", content.len()).as_bytes());
		hasher.update(content);
		let hash: [u8; 20] = hasher.finalize().into();
		if !self.written.insert(hash) {
			return Ok(hash);
		}
		let kind: u8 = if kind == "tree" { 2 } else { 3 };
		let mut size = content.len() as u64;
		let mut packed = vec![(kind << 4) | (size & 0x0f) as u8];
		size >>= 4;
		while size > 0 {
			*packed.last_mut().unwrap() |= 0x80;
			packed.push((size & 0x7f) as u8);
			size >>= 7;
		}
		let mut encoder = flate2::write::ZlibEncoder::new(packed, flate2::Compression::fast());
		encoder.write_all(content)?;
		let packed = encoder.finish()?;
		let mut crc = flate2::Crc::new();
		crc.update(&packed);
		self.file.write_all(&packed)?;
		self.entries.push((hash, crc.sum(), self.offset));
		self.offset += packed.len() as u64;
		Ok(hash)
	}

	/// Complete the pack and its index under `directory`.
	fn finish(self, directory: &Path) -> Result<()> {
		use sha1::{Digest, Sha1};
		use std::io::{Read, Seek, SeekFrom};
		let mut file = self.file.into_inner().map_err(|e| e.into_error())?;
		let count = u32::try_from(self.entries.len()).map_err(|e| Error::new(e.to_string()))?;
		file.seek(SeekFrom::Start(8))?;
		file.write_all(&count.to_be_bytes())?;
		file.seek(SeekFrom::Start(0))?;
		let mut hasher = Sha1::new();
		let mut buffer = vec![0; 1 << 16];
		loop {
			let read = file.read(&mut buffer)?;
			if read == 0 {
				break;
			}
			hasher.update(&buffer[..read]);
		}
		let checksum: [u8; 20] = hasher.finalize().into();
		file.write_all(&checksum)?;
		let mut entries = self.entries;
		entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
		let mut index = b"\xfftOc\0\0\0\x02".to_vec();
		for byte in 0..=255u8 {
			let below = entries.partition_point(|e| e.0[0] <= byte) as u32;
			index.extend_from_slice(&below.to_be_bytes());
		}
		for entry in &entries {
			index.extend_from_slice(&entry.0);
		}
		for entry in &entries {
			index.extend_from_slice(&entry.1.to_be_bytes());
		}
		let mut large = Vec::new();
		for entry in &entries {
			let offset = if entry.2 < 0x8000_0000 {
				entry.2 as u32
			} else {
				large.push(entry.2);
				0x8000_0000 | (large.len() as u32 - 1)
			};
			index.extend_from_slice(&offset.to_be_bytes());
		}
		for offset in large {
			index.extend_from_slice(&offset.to_be_bytes());
		}
		index.extend_from_slice(&checksum);
		let index_checksum: [u8; 20] = Sha1::digest(&index).into();
		index.extend_from_slice(&index_checksum);
		let name = format!("pack-{}", data_encoding::HEXLOWER.encode(&checksum));
		file.persist(directory.join(format!("{name}.pack")))
			.map_err(|e| e.error)?;
		// The index makes the pack visible, so it is written last.
		let mut index_file = tempfile::NamedTempFile::new_in(directory)?;
		index_file.write_all(&index)?;
		index_file
			.persist(directory.join(format!("{name}.idx")))
			.map_err(|e| e.error)?;
		Ok(())
	}
}

pub(super) struct TreeCache {
	repo: PathBuf,
}

impl TreeCache {
	pub fn open(directory: &Path) -> Result<Self> {
		let repo = directory.join("tarball-cache-v2");
		if !repo.join("HEAD").is_file() || !repo.join("objects").is_dir() {
			fs::create_dir_all(&repo)?;
			git(&repo, &["init", "--quiet", "--bare"], &[])?;
		}
		Ok(Self { repo })
	}

	fn write_tree(pack: &mut PackWriter, path: &Path) -> Result<[u8; 20]> {
		let mut entries = Vec::new();
		for entry in fs::read_dir(path)? {
			let path = entry?.path();
			let meta = fs::symlink_metadata(&path)?;
			let (mode, hash) = if meta.is_dir() {
				("40000", Self::write_tree(pack, &path)?)
			} else if meta.file_type().is_symlink() {
				let target = fs::read_link(&path)?;
				("120000", pack.add("blob", target.as_os_str().as_bytes())?)
			} else if meta.permissions().mode() & 0o100 != 0 {
				("100755", pack.add("blob", &fs::read(&path)?)?)
			} else {
				("100644", pack.add("blob", &fs::read(&path)?)?)
			};
			// Git orders entries by name, comparing directories as if they ended in '/'.
			let mut key = path.file_name().unwrap().as_bytes().to_vec();
			let name = key.len();
			if meta.is_dir() {
				key.push(b'/');
			}
			entries.push((key, name, mode, hash));
		}
		entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
		let mut tree = Vec::new();
		for (key, name, mode, hash) in entries {
			tree.extend_from_slice(mode.as_bytes());
			tree.push(b' ');
			tree.extend_from_slice(&key[..name]);
			tree.push(0);
			tree.extend_from_slice(&hash);
		}
		pack.add("tree", &tree)
	}

	/// Pack the tree at `path` into the cache, without a process per
	/// object or loose objects, and return its hexadecimal tree hash.
	pub fn insert(&self, path: &Path) -> Result<String> {
		let lock = fs::OpenOptions::new()
			.create(true)
			.truncate(false)
			.write(true)
			.open(self.repo.join("noxi-write.lock"))?;
		lock.lock()?;
		let directory = self.repo.join("objects/pack");
		fs::create_dir_all(&directory)?;
		let mut pack = PackWriter::new(&directory)?;
		let hash = Self::write_tree(&mut pack, path)?;
		pack.finish(&directory)?;
		Ok(data_encoding::HEXLOWER.encode(&hash))
	}

	pub fn get(&self, hash: &str) -> Result<Option<Mount>> {
		if git(
			&self.repo,
			&["cat-file", "-e", &format!("{hash}^{{tree}}")],
			&[],
		)
		.is_err()
		{
			return Ok(None);
		}
		let keep = Rc::new(tempfile::tempdir()?);
		super::git::export(&self.repo, hash, keep.path(), false, false)?;
		Ok(Some(Mount {
			root: keep.path().to_owned(),
			filter: None,
			keep: Some(keep),
		}))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn tree_hashes_match_git() {
		let source = tempfile::tempdir().unwrap();
		let root = source.path();
		for dir in ["b", "b/c", "x-y"] {
			fs::create_dir(root.join(dir)).unwrap();
		}
		for file in ["b.c", "b-c", "b/c/d", "b0", "x-y/z", "x"] {
			fs::write(root.join(file), file).unwrap();
		}
		fs::set_permissions(root.join("b0"), fs::Permissions::from_mode(0o755)).unwrap();
		std::os::unix::fs::symlink("b/c", root.join("link")).unwrap();
		let large: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
		fs::write(root.join("large"), &large).unwrap();
		let cache = tempfile::tempdir().unwrap();
		let trees = TreeCache::open(cache.path()).unwrap();
		let hash = trees.insert(root).unwrap();
		git(root, &["init", "--quiet"], &[]).unwrap();
		git(root, &["add", "-A", "--", ".", ":!.git"], &[]).unwrap();
		assert_eq!(hash, git(root, &["write-tree"], &[]).unwrap());
		let mount = trees.get(&hash).unwrap().unwrap();
		assert_eq!(fs::read(mount.root.join("b/c/d")).unwrap(), b"b/c/d");
		assert_eq!(fs::read(mount.root.join("large")).unwrap(), large);
		assert_eq!(
			fs::read_link(mount.root.join("link")).unwrap(),
			Path::new("b/c")
		);
		let mode = fs::metadata(mount.root.join("b0"))
			.unwrap()
			.permissions()
			.mode();
		assert_eq!(mode & 0o111, 0o111);
		assert!(fs::read_dir(trees.repo.join("objects")).unwrap().all(|e| {
			let name = e.unwrap().file_name();
			name == "pack" || name == "info"
		}));
		for entry in fs::read_dir(trees.repo.join("objects/pack")).unwrap() {
			let path = entry.unwrap().path();
			if path.extension().is_some_and(|e| e == "idx") {
				git(&trees.repo, &["verify-pack", path.to_str().unwrap()], &[]).unwrap();
			}
		}
	}
}
