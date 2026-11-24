//! Temporary directories for tests that create Nix stores.
//!
//! Store objects are read-only, and nothing can be unlinked from a directory
//! without write permission, so `tempfile::TempDir` leaves them behind.
//! [`TempDir`] first makes its directories writable. It changes only real
//! directories the current user owns, on the mount the root is on, reached
//! without following a symlink, so nothing outside the root is changed.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A temporary directory removed on drop, also while unwinding from a panic,
/// together with any read-only directories below it.
#[derive(Debug)]
pub struct TempDir(Option<tempfile::TempDir>);

/// A [`TempDir`] in `TMPDIR`.
pub fn tempdir() -> io::Result<TempDir> {
	tempfile::tempdir().map(TempDir::from)
}

impl From<tempfile::TempDir> for TempDir {
	/// Take over a directory made by `tempfile::Builder`.
	fn from(dir: tempfile::TempDir) -> Self {
		TempDir(Some(dir))
	}
}

impl TempDir {
	pub fn path(&self) -> &Path {
		self.0.as_ref().expect("held until drop").path()
	}

	/// Leave the directory on disk.
	pub fn keep(mut self) -> PathBuf {
		self.0.take().expect("held until drop").keep()
	}
}

impl AsRef<Path> for TempDir {
	fn as_ref(&self) -> &Path {
		self.path()
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let Some(dir) = self.0.take() else {
			return;
		};
		// Never panic: the test may already be unwinding.
		match make_removable(dir.path()) {
			Ok(()) => {
				let path = dir.path().to_owned();
				if let Err(error) = dir.close() {
					eprintln!("cannot remove '{}': {error}", path.display());
				}
			}
			Err(error) => {
				let path = dir.keep();
				eprintln!("leaving '{}' behind: {error}", path.display());
			}
		}
	}
}

/// Keep `dir` for the whole test process, for state several tests of a binary
/// share, and remove it when the process exits.
pub fn until_exit(dir: tempfile::TempDir) -> PathBuf {
	static DIRS: Mutex<Vec<(u32, PathBuf)>> = Mutex::new(Vec::new());
	extern "C" fn remove() {
		// A forked child inherits the list but owns none of it.
		let pid = std::process::id();
		if let Ok(dirs) = DIRS.lock() {
			for (_, dir) in dirs.iter().filter(|(owner, _)| *owner == pid) {
				let _ = remove_tree(dir);
			}
		}
	}
	let path = dir.path().to_owned();
	let mut dirs = DIRS.lock().unwrap_or_else(|error| error.into_inner());
	if dirs.is_empty() {
		// SAFETY: `remove` is a plain function that does not unwind.
		unsafe { libc::atexit(remove) };
	}
	dirs.push((std::process::id(), dir.keep()));
	path
}

/// Remove the tree `root` of a store-creating test, which the caller created.
/// A missing tree is already removed.
pub fn remove_tree(root: &Path) -> io::Result<()> {
	match make_removable(root) {
		Ok(()) => fs::remove_dir_all(root),
		Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
		Err(error) => Err(error),
	}
}

/// Give the owner full access to the directories at and below `root`, so that
/// the tree can be read and removed. Fails without removing anything when
/// `root` is not a real directory or another mount is found below it, which a
/// recursive removal would cross.
pub fn make_removable(root: &Path) -> io::Result<()> {
	let dir = open(root)?;
	let mount = mount_id(&dir)?;
	visit(&dir, root, mount)
}

/// Pin the directory at `path` itself: `O_PATH` needs no permission on it and
/// `O_NOFOLLOW` refuses a symlink.
fn open(path: &Path) -> io::Result<File> {
	OpenOptions::new()
		.read(true)
		.custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_DIRECTORY)
		.open(path)
}

/// `st_dev` does not tell a bind mount of the same file system apart.
fn mount_id(dir: &File) -> io::Result<u64> {
	let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", dir.as_raw_fd()))?;
	info.lines()
		.find_map(|line| line.strip_prefix("mnt_id:")?.trim().parse().ok())
		.ok_or_else(|| io::Error::other("no mnt_id in /proc/self/fdinfo"))
}

fn visit(dir: &File, shown: &Path, mount: u64) -> io::Result<()> {
	if mount_id(dir)? != mount {
		return Err(io::Error::other(format!(
			"another file system is mounted at '{}'",
			shown.display()
		)));
	}
	// The open directory, whatever its name refers to by now.
	let pinned = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
	let meta = dir.metadata()?;
	// SAFETY: geteuid cannot fail.
	if meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o700 != 0o700 {
		fs::set_permissions(
			&pinned,
			Permissions::from_mode(meta.mode() & 0o7777 | 0o700),
		)?;
	}
	let entries = match fs::read_dir(&pinned) {
		Ok(entries) => entries,
		// Another user's directory; the removal reports it.
		Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(()),
		Err(error) => return Err(error),
	};
	for entry in entries {
		let entry = entry?;
		if !entry.file_type()?.is_dir() {
			continue;
		}
		let child = match open(&entry.path()) {
			Ok(child) => child,
			// Removed or replaced since it was listed.
			Err(error)
				if error.kind() == io::ErrorKind::NotFound
					|| matches!(error.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) =>
			{
				continue;
			}
			Err(error) => return Err(error),
		};
		visit(&child, &shown.join(entry.file_name()), mount)?;
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::os::unix::fs::symlink;

	fn mode(path: &Path) -> u32 {
		fs::symlink_metadata(path).unwrap().mode() & 0o7777
	}

	fn set_mode(path: &Path, mode: u32) {
		fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
	}

	/// A read-only and a closed directory, and symlinks out of the tree.
	fn fill(root: &Path, outside: &Path) {
		for (name, mode) in [("object", 0o555), ("closed", 0o000)] {
			let dir = root.join(name);
			fs::create_dir_all(dir.join("sub")).unwrap();
			fs::write(dir.join("sub/file"), "x").unwrap();
			symlink(outside, dir.join("sub/link")).unwrap();
			set_mode(&dir.join("sub/file"), 0o444);
			set_mode(&dir.join("sub"), mode);
			set_mode(&dir, mode);
		}
		symlink(outside, root.join("link")).unwrap();
	}

	#[test]
	fn removes_read_only_trees_and_nothing_outside() {
		let outside = tempdir().unwrap();
		let object = outside.path().join("object");
		fs::create_dir(&object).unwrap();
		fs::write(object.join("file"), "kept").unwrap();
		set_mode(&object.join("file"), 0o444);
		set_mode(&object, 0o555);

		let dir = tempdir().unwrap();
		let root = dir.path().to_owned();
		fill(&root, &object);
		drop(dir);
		assert!(!root.exists());

		let dir = tempdir().unwrap();
		let panicked = dir.path().to_owned();
		fill(&panicked, &object);
		let result = std::thread::spawn(move || {
			let _dir = dir;
			panic!("expected by this test");
		})
		.join();
		assert!(result.is_err());
		assert!(!panicked.exists());

		// A root replaced by a symlink is refused.
		let link = outside.path().join("root");
		symlink(&object, &link).unwrap();
		assert!(remove_tree(&link).is_err());

		assert_eq!(mode(&object), 0o555);
		assert_eq!(mode(&object.join("file")), 0o444);
		assert_eq!(fs::read(object.join("file")).unwrap(), b"kept");
	}

	#[test]
	fn removes_a_missing_tree() {
		let dir = tempdir().unwrap();
		remove_tree(&dir.path().join("missing")).unwrap();
	}
}
