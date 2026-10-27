//! Git objects shared with CppNix's packfiles-only tarball-cache-v2.

use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;

use crate::mount::Mount;
use nix_rs_flakeref::error::{Error, Result};

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

	/// Write a loose object as `git hash-object -w` and `git mktree` do,
	/// without a process per object, and return its hexadecimal SHA-1.
	fn write_object(&self, kind: &str, content: &[u8]) -> Result<[u8; 20]> {
		use sha1::{Digest, Sha1};
		let header = format!("{kind} {}\0", content.len());
		let mut hasher = Sha1::new();
		hasher.update(header.as_bytes());
		hasher.update(content);
		let hash: [u8; 20] = hasher.finalize().into();
		let hex = data_encoding::HEXLOWER.encode(&hash);
		let directory = self.repo.join("objects").join(&hex[..2]);
		let path = directory.join(&hex[2..]);
		if path.exists() {
			return Ok(hash);
		}
		fs::create_dir_all(&directory)?;
		let mut file = tempfile::NamedTempFile::new_in(&directory)?;
		let mut encoder = flate2::write::ZlibEncoder::new(&mut file, flate2::Compression::fast());
		encoder.write_all(header.as_bytes())?;
		encoder.write_all(content)?;
		encoder.finish()?;
		file.persist(&path).map_err(|e| e.error)?;
		Ok(hash)
	}

	fn write_tree(&self, path: &Path) -> Result<[u8; 20]> {
		let mut entries = Vec::new();
		for entry in fs::read_dir(path)? {
			let path = entry?.path();
			let meta = fs::symlink_metadata(&path)?;
			let (mode, hash) = if meta.is_dir() {
				("40000", self.write_tree(&path)?)
			} else if meta.file_type().is_symlink() {
				let target = fs::read_link(&path)?;
				(
					"120000",
					self.write_object("blob", target.as_os_str().as_bytes())?,
				)
			} else if meta.permissions().mode() & 0o100 != 0 {
				("100755", self.write_object("blob", &fs::read(&path)?)?)
			} else {
				("100644", self.write_object("blob", &fs::read(&path)?)?)
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
		self.write_object("tree", &tree)
	}

	pub fn insert(&self, path: &Path) -> Result<String> {
		let lock = fs::OpenOptions::new()
			.create(true)
			.truncate(false)
			.write(true)
			.open(self.repo.join("nixrs-write.lock"))?;
		lock.lock()?;
		let hash = data_encoding::HEXLOWER.encode(&self.write_tree(path)?);
		let prefix = self.repo.join("objects/pack/pack");
		git(
			&self.repo,
			&[
				"pack-objects",
				"--revs",
				prefix
					.to_str()
					.ok_or_else(|| Error::new("non-UTF-8 cache path"))?,
			],
			format!("{hash}\n").as_bytes(),
		)?;
		git(&self.repo, &["prune-packed"], &[])?;
		Ok(hash)
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
		let cache = tempfile::tempdir().unwrap();
		let trees = TreeCache::open(cache.path()).unwrap();
		let hash = trees.insert(root).unwrap();
		git(root, &["init", "--quiet"], &[]).unwrap();
		git(root, &["add", "-A", "--", ".", ":!.git"], &[]).unwrap();
		assert_eq!(hash, git(root, &["write-tree"], &[]).unwrap());
		let mount = trees.get(&hash).unwrap().unwrap();
		assert_eq!(fs::read(mount.root.join("b/c/d")).unwrap(), b"b/c/d");
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
	}
}
