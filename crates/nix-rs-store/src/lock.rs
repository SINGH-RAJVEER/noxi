//! `flock(2)` locks, compatible with CppNix's `pathlocks.cc`, so both
//! implementations exclude each other.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockType {
	Read,
	Write,
	None,
}

/// Open (and with `create`, create) a lock file with mode 0600. Returns
/// `None` if it does not exist and `create` is false.
pub fn open_lock_file(path: &Path, create: bool) -> io::Result<Option<File>> {
	let result = OpenOptions::new()
		.read(true)
		.write(true)
		.create(create)
		.mode(0o600)
		.custom_flags(libc::O_CLOEXEC)
		.open(path);
	match result {
		Ok(file) => Ok(Some(file)),
		Err(e) if !create && e.kind() == io::ErrorKind::NotFound => Ok(None),
		Err(e) => Err(e),
	}
}

/// Acquire, change or release a lock. Without `wait`, returns `false` if
/// another process holds a conflicting lock.
pub fn lock_file(file: &File, lock_type: LockType, wait: bool) -> io::Result<bool> {
	let operation = match lock_type {
		LockType::Read => libc::LOCK_SH,
		LockType::Write => libc::LOCK_EX,
		LockType::None => libc::LOCK_UN,
	};
	let operation = if wait {
		operation
	} else {
		operation | libc::LOCK_NB
	};
	loop {
		// SAFETY: flock only reads the descriptor, which `file` keeps open.
		if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
			return Ok(true);
		}
		let error = io::Error::last_os_error();
		match error.raw_os_error() {
			Some(libc::EINTR) => continue,
			Some(libc::EWOULDBLOCK) if !wait => return Ok(false),
			_ => return Err(error),
		}
	}
}

/// Remove a lock file while holding its lock, and mark the open file as
/// stale so a process waiting on it retries with a new file.
pub fn delete_lock_file(path: &Path, mut file: &File) {
	let _ = std::fs::remove_file(path);
	let _ = file.write_all(b"d");
}

/// Exclusive locks on `PATH.lock` for a set of paths, as CppNix's
/// `PathLocks` takes them for store paths being written.
#[derive(Debug, Default)]
pub struct PathLocks {
	locks: Vec<(File, PathBuf)>,
	delete_on_unlock: bool,
}

impl PathLocks {
	/// Lock every path, in sorted order so that concurrent lockers cannot
	/// deadlock. Waits for each lock.
	pub fn lock(paths: impl IntoIterator<Item = PathBuf>) -> io::Result<Self> {
		let mut paths: Vec<PathBuf> = paths.into_iter().collect();
		paths.sort();
		paths.dedup();
		let mut locks = PathLocks::default();
		for path in paths {
			let mut lock_path = path.into_os_string();
			lock_path.push(".lock");
			let lock_path = PathBuf::from(lock_path);
			loop {
				let file = open_lock_file(&lock_path, true)?.expect("created lock file");
				lock_file(&file, LockType::Write, true)?;
				// A non-empty lock file was deleted by its last holder after
				// we opened it; another process may already hold a new one.
				if file.metadata()?.len() == 0 {
					locks.locks.push((file, lock_path));
					break;
				}
			}
		}
		Ok(locks)
	}

	/// Remove the lock files when unlocking.
	pub fn set_deletion(&mut self, delete: bool) {
		self.delete_on_unlock = delete;
	}

	pub fn unlock(&mut self) {
		for (file, path) in self.locks.drain(..) {
			if self.delete_on_unlock {
				delete_lock_file(&path, &file);
			}
		}
	}
}

impl Drop for PathLocks {
	fn drop(&mut self) {
		self.unlock();
	}
}
