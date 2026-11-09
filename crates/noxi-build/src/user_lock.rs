//! Build users, following CppNix 2.35.2's `SimpleUserLock`
//! (`unix/user-lock.cc`) and `killUser` (`unix/processes.cc`).
//!
//! A daemon running as root builds as one member of `build-users-group`
//! at a time, holding `STATE_DIR/userpool/UID` locked for the build, so
//! that no builder runs with the daemon's own privileges and concurrent
//! builds cannot interfere with each other's processes or files.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::path::Path;

use noxi_store::lock::{LockType, lock_file, open_lock_file};

use crate::error::{BuildError, Result};

/// A build user locked for one build.
#[derive(Debug)]
pub struct BuildUser {
	pub uid: u32,
	/// The gid of `build-users-group`.
	pub gid: u32,
	_lock: File,
}

/// `useBuildUsers`: build users are used when `build-users-group` is set
/// and the process runs as root.
pub fn use_build_users(build_users_group: &str) -> bool {
	// SAFETY: getuid cannot fail.
	!build_users_group.is_empty() && unsafe { libc::getuid() } == 0
}

/// `acquireUserLock`: lock a free member of `group`. `Ok(None)` means
/// every member is busy with another build.
pub fn acquire(state_dir: &Path, group: &str) -> Result<Option<BuildUser>> {
	let pool = state_dir.join("userpool");
	std::fs::create_dir_all(&pool)?;
	let (gid, members) = group_members(group)?.ok_or_else(|| {
		BuildError::Other(format!(
			"the group '{group}' specified in 'build-users-group' does not exist"
		))
	})?;
	if members.is_empty() {
		return Err(BuildError::Other(format!(
			"the build users group '{group}' has no members"
		)));
	}
	for member in &members {
		let uid = user_uid(member)?.ok_or_else(|| {
			BuildError::Other(format!(
				"the user '{member}' in the group '{group}' does not exist"
			))
		})?;
		let path = pool.join(uid.to_string());
		let file = open_lock_file(&path, true)
			.map_err(|e| {
				BuildError::Other(format!("opening user lock \"{}\": {e}", path.display()))
			})?
			.expect("created");
		if lock_file(&file, LockType::Write, false)? {
			// SAFETY: getuid and geteuid cannot fail.
			if uid == unsafe { libc::getuid() } || uid == unsafe { libc::geteuid() } {
				return Err(BuildError::Other(format!(
					"the Nix user should not be a member of '{group}'"
				)));
			}
			return Ok(Some(BuildUser {
				uid,
				gid,
				_lock: file,
			}));
		}
	}
	Ok(None)
}

impl BuildUser {
	/// `chownToBuilder`.
	pub fn chown(&self, path: &Path) -> io::Result<()> {
		std::os::unix::fs::lchown(path, Some(self.uid), Some(self.gid)).map_err(|e| {
			io::Error::new(
				e.kind(),
				format!("cannot change ownership of '{}': {e}", path.display()),
			)
		})
	}

	/// `killUser`: kill every process running as this user, from a child
	/// that switches to the user and sends `kill(-1, SIGKILL)`.
	pub fn kill(&self) -> Result<()> {
		assert_ne!(self.uid, 0);
		// SAFETY: the child only makes async-signal-safe calls before
		// `_exit`.
		let pid = unsafe { libc::fork() };
		if pid == -1 {
			return Err(io::Error::last_os_error().into());
		}
		if pid == 0 {
			// SAFETY: plain process-state syscalls in the forked child.
			unsafe {
				if libc::setuid(self.uid) == -1 {
					libc::_exit(1);
				}
				loop {
					if libc::kill(-1, libc::SIGKILL) == 0 {
						break;
					}
					let errno = *libc::__errno_location();
					if errno == libc::ESRCH || errno == libc::EPERM {
						break;
					}
					if errno != libc::EINTR {
						libc::_exit(1);
					}
				}
				libc::_exit(0);
			}
		}
		let mut status = 0;
		loop {
			// SAFETY: waiting for our own child.
			if unsafe { libc::waitpid(pid, &mut status, 0) } != -1 {
				break;
			}
			let e = io::Error::last_os_error();
			if e.kind() != io::ErrorKind::Interrupted {
				return Err(e.into());
			}
		}
		if !(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0) {
			return Err(BuildError::Other(format!(
				"cannot kill processes for uid '{}'",
				self.uid
			)));
		}
		Ok(())
	}
}

/// The gid and member names of `group`, or `None` if it does not exist.
fn group_members(group: &str) -> Result<Option<(u32, Vec<String>)>> {
	let name = CString::new(group).map_err(|e| BuildError::Other(e.to_string()))?;
	let mut buf = vec![0u8; 4096];
	loop {
		// SAFETY: an all-zero `group` is a valid out-parameter.
		let mut grp: libc::group = unsafe { std::mem::zeroed() };
		let mut result = std::ptr::null_mut();
		// SAFETY: every pointer is valid for the call, with `buf`'s length.
		let rc = unsafe {
			libc::getgrnam_r(
				name.as_ptr(),
				&mut grp,
				buf.as_mut_ptr().cast(),
				buf.len(),
				&mut result,
			)
		};
		if rc == libc::ERANGE {
			buf.resize(buf.len() * 2, 0);
			continue;
		}
		if rc != 0 {
			return Err(io::Error::from_raw_os_error(rc).into());
		}
		if result.is_null() {
			return Ok(None);
		}
		let mut members = Vec::new();
		let mut member = grp.gr_mem;
		// SAFETY: `gr_mem` is a NULL-terminated array of C strings in `buf`.
		unsafe {
			while !(*member).is_null() {
				members.push(CStr::from_ptr(*member).to_string_lossy().into_owned());
				member = member.add(1);
			}
		}
		return Ok(Some((grp.gr_gid, members)));
	}
}

/// The uid of user `name`, or `None` if it does not exist.
fn user_uid(name: &str) -> Result<Option<u32>> {
	let cname = CString::new(name).map_err(|e| BuildError::Other(e.to_string()))?;
	let mut buf = vec![0u8; 4096];
	loop {
		// SAFETY: an all-zero `passwd` is a valid out-parameter.
		let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
		let mut result = std::ptr::null_mut();
		// SAFETY: every pointer is valid for the call, with `buf`'s length.
		let rc = unsafe {
			libc::getpwnam_r(
				cname.as_ptr(),
				&mut pwd,
				buf.as_mut_ptr().cast(),
				buf.len(),
				&mut result,
			)
		};
		if rc == libc::ERANGE {
			buf.resize(buf.len() * 2, 0);
			continue;
		}
		if rc != 0 {
			return Err(io::Error::from_raw_os_error(rc).into());
		}
		return Ok((!result.is_null()).then_some(pwd.pw_uid));
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn missing_group_is_reported() {
		let dir = tempfile::tempdir().unwrap();
		let err = acquire(dir.path(), "noxi-no-such-build-group").unwrap_err();
		assert_eq!(
			err.to_string(),
			"the group 'noxi-no-such-build-group' specified in 'build-users-group' does not exist"
		);
	}

	#[test]
	fn build_users_need_root_and_a_group() {
		assert!(!use_build_users(""));
		// SAFETY: getuid cannot fail.
		if unsafe { libc::getuid() } != 0 {
			assert!(!use_build_users("nixbld"));
		}
	}
}
