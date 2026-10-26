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
		fs::create_dir_all(&repo)?;
		git(&repo, &["init", "--quiet", "--bare"], &[])?;
		Ok(Self { repo })
	}

	fn write_tree(&self, path: &Path) -> Result<String> {
		let mut entries = Vec::new();
		let mut names = fs::read_dir(path)?
			.map(|e| e.map(|e| e.path()))
			.collect::<std::io::Result<Vec<_>>>()?;
		names.sort();
		for path in names {
			let meta = fs::symlink_metadata(&path)?;
			let (mode, kind, hash) = if meta.is_dir() {
				("040000", "tree", self.write_tree(&path)?)
			} else {
				let bytes = if meta.file_type().is_symlink() {
					fs::read_link(&path)?.as_os_str().as_bytes().to_vec()
				} else {
					fs::read(&path)?
				};
				let hash = git(&self.repo, &["hash-object", "-w", "--stdin"], &bytes)?;
				(
					if meta.file_type().is_symlink() {
						"120000"
					} else if meta.permissions().mode() & 0o100 != 0 {
						"100755"
					} else {
						"100644"
					},
					"blob",
					hash,
				)
			};
			entries.extend_from_slice(format!("{mode} {kind} {hash}\t").as_bytes());
			entries.extend_from_slice(path.file_name().unwrap().as_bytes());
			entries.push(0);
		}
		git(&self.repo, &["mktree", "-z"], &entries)
	}

	pub fn insert(&self, path: &Path) -> Result<String> {
		let lock = fs::OpenOptions::new()
			.create(true)
			.truncate(false)
			.write(true)
			.open(self.repo.join("nixrs-write.lock"))?;
		lock.lock()?;
		let hash = self.write_tree(path)?;
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
