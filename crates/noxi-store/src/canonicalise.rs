//! Normalising file metadata in the store, as CppNix's
//! `posix-fs-canonicalise.cc` does: modes 0444 or 0555, modification time
//! 1, ownership by the current user, and no extended attributes.

use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

/// The modification time of every store file: one second after the epoch.
pub const MTIME_STORE: i64 = 1;

/// Extended attributes CppNix leaves in place (`ignored-acls`).
pub const IGNORED_ACLS: &[&str] = &["security.selinux", "system.nfs4_acl", "security.csm"];

/// Canonicalise `path` and everything below it. With `uid_range`, files not
/// owned by a user in that range are rejected, unless they are hard links to
/// files already seen.
pub fn canonicalise_path_metadata(path: &Path, uid_range: Option<(u32, u32)>) -> io::Result<()> {
	let mut seen = HashSet::new();
	canonicalise(path, uid_range, &mut seen)
}

fn canonicalise(
	path: &Path,
	uid_range: Option<(u32, u32)>,
	seen: &mut HashSet<(u64, u64)>,
) -> io::Result<()> {
	let metadata = fs::symlink_metadata(path)?;
	let file_type = metadata.file_type();
	if !(file_type.is_file() || file_type.is_dir() || file_type.is_symlink()) {
		return Err(io::Error::other(format!(
			"file '{}' has an unsupported type",
			path.display()
		)));
	}

	remove_xattrs(path)?;

	if let Some((first, last)) = uid_range
		&& (metadata.uid() < first || metadata.uid() > last)
	{
		if file_type.is_dir() || !seen.contains(&(metadata.dev(), metadata.ino())) {
			return Err(io::Error::other(format!(
				"invalid ownership on file '{}'",
				path.display()
			)));
		}
		return Ok(());
	}

	seen.insert((metadata.dev(), metadata.ino()));

	if !file_type.is_symlink() {
		let mode = metadata.permissions().mode() & 0o7777;
		let is_dir = file_type.is_dir();
		if (mode != 0o444 || is_dir) && mode != 0o555 {
			let executable = metadata.permissions().mode() & 0o100 != 0 || is_dir;
			let mode = 0o444 | if executable { 0o111 } else { 0 };
			fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
		}
	}

	if metadata.mtime() != MTIME_STORE {
		set_mtime(path, &metadata)?;
	}

	// SAFETY: geteuid and getegid cannot fail.
	let (euid, egid) = unsafe { (libc::geteuid(), libc::getegid()) };
	if metadata.uid() != euid {
		let c_path = c_path(path)?;
		// SAFETY: `c_path` is a valid NUL-terminated path.
		if unsafe { libc::lchown(c_path.as_ptr(), euid, egid) } == -1 {
			return Err(io::Error::last_os_error());
		}
	}

	if file_type.is_dir() {
		for entry in fs::read_dir(path)? {
			canonicalise(&entry?.path(), uid_range, seen)?;
		}
	}
	Ok(())
}

/// Set the modification time to [MTIME_STORE], keeping the access time and
/// not following symlinks.
fn set_mtime(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
	let c_path = c_path(path)?;
	let times = [
		libc::timespec {
			tv_sec: metadata.atime(),
			tv_nsec: metadata.atime_nsec(),
		},
		libc::timespec {
			tv_sec: MTIME_STORE,
			tv_nsec: 0,
		},
	];
	// SAFETY: `c_path` is NUL-terminated and `times` has two entries.
	let result = unsafe {
		libc::utimensat(
			libc::AT_FDCWD,
			c_path.as_ptr(),
			times.as_ptr(),
			libc::AT_SYMLINK_NOFOLLOW,
		)
	};
	if result == -1 {
		return Err(io::Error::last_os_error());
	}
	Ok(())
}

fn remove_xattrs(path: &Path) -> io::Result<()> {
	let c_path = c_path(path)?;
	// SAFETY: a null buffer of size 0 asks for the required size.
	let size = unsafe { libc::llistxattr(c_path.as_ptr(), std::ptr::null_mut(), 0) };
	if size < 0 {
		let error = io::Error::last_os_error();
		return match error.raw_os_error() {
			Some(libc::ENOTSUP) | Some(libc::ENODATA) => Ok(()),
			_ => Err(error),
		};
	}
	if size == 0 {
		return Ok(());
	}
	let mut buf = vec![0u8; size as usize];
	// SAFETY: `buf` has `buf.len()` writable bytes.
	let size = unsafe { libc::llistxattr(c_path.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
	if size < 0 {
		return Err(io::Error::last_os_error());
	}
	buf.truncate(size as usize);
	for name in buf.split(|&b| b == 0).filter(|name| !name.is_empty()) {
		if IGNORED_ACLS
			.iter()
			.any(|ignored| ignored.as_bytes() == name)
		{
			continue;
		}
		let c_name = CString::new(name).expect("xattr names have no NUL");
		// SAFETY: both arguments are NUL-terminated.
		if unsafe { libc::lremovexattr(c_path.as_ptr(), c_name.as_ptr()) } == -1 {
			return Err(io::Error::last_os_error());
		}
	}
	Ok(())
}

pub(crate) fn c_path(path: &Path) -> io::Result<CString> {
	CString::new(path.as_os_str().as_bytes())
		.map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn canonicalises_modes_and_times() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("out");
		fs::create_dir(&root).unwrap();
		fs::write(root.join("file"), b"x").unwrap();
		fs::write(root.join("exe"), b"x").unwrap();
		fs::set_permissions(root.join("exe"), fs::Permissions::from_mode(0o700)).unwrap();
		std::os::unix::fs::symlink("file", root.join("link")).unwrap();

		canonicalise_path_metadata(&root, None).unwrap();

		let mode = |p: &str| {
			fs::symlink_metadata(root.join(p))
				.unwrap()
				.permissions()
				.mode()
				& 0o7777
		};
		assert_eq!(mode("file"), 0o444);
		assert_eq!(mode("exe"), 0o555);
		assert_eq!(
			fs::metadata(&root).unwrap().permissions().mode() & 0o7777,
			0o555
		);
		for p in ["file", "exe", "link"] {
			assert_eq!(
				fs::symlink_metadata(root.join(p)).unwrap().mtime(),
				MTIME_STORE
			);
		}
		// Make the tree removable again.
		fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
	}
}
