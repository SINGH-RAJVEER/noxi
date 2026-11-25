//! Git object hashes of file system objects, CppNix's `git::dumpHash`.

use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use nix_compat::git::{self, ObjectKind, TreeEntry};
use nix_compat::nixhash::{HashAlgo, NixHash, NixHashDigester};

use crate::{Error, Result};

/// The Git hash of the file, symlink or directory at `path`.
pub fn hash_path(path: &Path, algo: HashAlgo) -> Result<NixHash> {
	Ok(hash_entry(path, algo)?.1)
}

/// The mode and hash a tree lists for the object at `path`.
fn hash_entry(path: &Path, algo: HashAlgo) -> Result<(u32, NixHash)> {
	let metadata = fs::symlink_metadata(path)?;
	let kind = metadata.file_type();
	if kind.is_file() {
		let mut digester = NixHashDigester::new(algo);
		digester.update(format!("blob {}\0", metadata.len()).as_bytes());
		let mut file = File::open(path)?;
		let mut buffer = [0; 65536];
		loop {
			let count = io::Read::read(&mut file, &mut buffer)?;
			if count == 0 {
				break;
			}
			digester.update(&buffer[..count]);
		}
		let mode = if metadata.mode() & 0o100 != 0 {
			0o100755
		} else {
			0o100644
		};
		return Ok((mode, digester.finalize()));
	}
	let (mode, kind, body) = if kind.is_symlink() {
		let target = fs::read_link(path)?;
		(
			0o120000,
			ObjectKind::Blob,
			target.as_os_str().as_bytes().to_vec(),
		)
	} else if kind.is_dir() {
		let mut entries = Vec::new();
		for entry in fs::read_dir(path)? {
			let entry = entry?;
			let (mode, hash) = hash_entry(&entry.path(), algo)?;
			entries.push(TreeEntry {
				mode,
				name: entry.file_name().as_bytes().to_vec(),
				hash: hash.digest_as_bytes().to_vec(),
			});
		}
		// A directory sorts as if its name ended in a slash.
		entries.sort_by_cached_key(|entry| {
			let mut name = entry.name.clone();
			if entry.mode == 0o40000 {
				name.push(b'/');
			}
			name
		});
		let body = git::encode_tree(&entries, algo.digest_length())
			.map_err(|e| Error::Other(e.to_string()))?;
		(0o40000, ObjectKind::Tree, body)
	} else {
		return Err(Error::Other(format!(
			"file '{}' has an unsupported type",
			path.display()
		)));
	};
	let mut digester = NixHashDigester::new(algo);
	digester.update(git::encode_object(kind, &body));
	Ok((mode, digester.finalize()))
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::os::unix::fs::{PermissionsExt, symlink};

	/// Values from `git rev-parse` in the upstream `git-hashing` tests, and
	/// the sorting of a directory next to `a.b` and `a0`.
	#[test]
	fn hashes_match_git() {
		let dir = noxi_test_support::tempdir().unwrap();
		let root = dir.path().join("tree");
		fs::create_dir(&root).unwrap();
		fs::write(root.join("hello"), "Hello World\n").unwrap();
		fs::write(root.join("executable"), "Run Hello World\n").unwrap();
		fs::set_permissions(root.join("executable"), fs::Permissions::from_mode(0o755)).unwrap();
		let hex = |path: &Path, algo| {
			hash_path(path, algo)
				.unwrap()
				.as_nix_lowerhex_string_fmt()
				.to_string()
		};
		assert_eq!(
			hex(&root, HashAlgo::Sha1),
			"sha1:e5c0a11a556801a5c9dcf330ca9d7e2c572697f4"
		);
		assert_eq!(
			hex(&root, HashAlgo::Sha256),
			"sha256:cd79952f42462467d0ea574b0283bb6eb77e15b2b86891e29f2b981650365474"
		);
		assert_eq!(
			hex(&root.join("hello"), HashAlgo::Sha1),
			"sha1:557db03de997c86a4a028e1ebd3a1ceb225be238"
		);
		symlink("hello", root.join("link")).unwrap();
		fs::create_dir(root.join("a")).unwrap();
		for name in ["a.b", "a0", "a/x"] {
			fs::write(root.join(name), "").unwrap();
		}
		let Ok(output) = std::process::Command::new("bash")
			.args(["-c", "git init -q . && git add -A && git write-tree"])
			.current_dir(&root)
			.env("GIT_CONFIG_GLOBAL", "/dev/null")
			.output()
		else {
			return;
		};
		if !output.status.success() {
			return;
		}
		fs::remove_dir_all(root.join(".git")).unwrap();
		assert_eq!(
			hex(&root, HashAlgo::Sha1),
			format!("sha1:{}", String::from_utf8_lossy(&output.stdout).trim())
		);
	}
}
