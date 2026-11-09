//! Unix domain sockets at any path length, following CppNix 2.35.2's
//! `bindConnectProcHelper` (`unix-domain-socket.cc`).
//!
//! `sockaddr_un` holds at most 107 path bytes. For a longer path, a forked
//! child changes into the socket's directory and binds or connects the
//! shared descriptor by its base name, so state and store directories deep
//! in the filesystem still work.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// `sizeof(sockaddr_un::sun_path)`.
const SUN_PATH: usize = 108;

#[derive(Clone, Copy)]
enum Operation {
	Bind,
	Connect,
}

/// Connect to the socket at `path`. Errors are the system error, as for a
/// short path; callers add CppNix's `cannot connect to socket at` prefix.
pub fn connect(path: &Path) -> io::Result<UnixStream> {
	if path.as_os_str().len() + 1 < SUN_PATH {
		return UnixStream::connect(path);
	}
	let fd = socket()?;
	long_path(Operation::Connect, &fd, path)?;
	Ok(UnixStream::from(fd))
}

/// Bind and listen on a socket at `path`, with CppNix's backlog of 100.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
	if path.as_os_str().len() + 1 < SUN_PATH {
		return UnixListener::bind(path);
	}
	let fd = socket()?;
	long_path(Operation::Bind, &fd, path)?;
	// SAFETY: `fd` is a bound socket.
	if unsafe { libc::listen(fd.as_raw_fd(), 100) } == -1 {
		let e = io::Error::last_os_error();
		return Err(io::Error::new(
			e.kind(),
			format!("cannot listen on socket '{}': {e}", path.display()),
		));
	}
	Ok(UnixListener::from(fd))
}

fn socket() -> io::Result<OwnedFd> {
	// SAFETY: socket has no memory-safety preconditions.
	let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
	if fd == -1 {
		return Err(io::Error::last_os_error());
	}
	// SAFETY: socket returned a new descriptor.
	Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Bind or connect `fd` to the over-long `path` from a child that changes
/// into its directory. The child reports 0, an errno, or -1 for a base name
/// that is itself too long.
fn long_path(operation: Operation, fd: &OwnedFd, path: &Path) -> io::Result<()> {
	let dir = path.parent().unwrap_or(Path::new("/"));
	let dir = if dir.as_os_str().is_empty() {
		Path::new(".")
	} else {
		dir
	};
	let dir = CString::new(dir.as_os_str().as_bytes()).map_err(io::Error::other)?;
	let base = path.file_name().map_or(&[][..], |b| b.as_bytes());
	// Prepared before forking: the child only makes async-signal-safe calls.
	let too_long = base.len() + 1 >= SUN_PATH;
	// SAFETY: an all-zero sockaddr_un is valid.
	let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
	addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
	if !too_long {
		for (dst, src) in addr.sun_path.iter_mut().zip(base) {
			*dst = *src as libc::c_char;
		}
	}
	let mut pipe = [0; 2];
	// SAFETY: `pipe` has room for two descriptors.
	if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
		return Err(io::Error::last_os_error());
	}
	// SAFETY: pipe2 returned two new descriptors.
	let (read, write) = unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
	// SAFETY: the child only calls chdir, bind or connect, write and _exit.
	let pid = unsafe { libc::fork() };
	if pid == -1 {
		return Err(io::Error::last_os_error());
	}
	if pid == 0 {
		// SAFETY: async-signal-safe calls in the forked child.
		unsafe {
			let report = |code: i32| {
				let mut buf = [0u8; 12];
				let mut n = buf.len();
				let negative = code < 0;
				let mut value = code.unsigned_abs();
				loop {
					n -= 1;
					buf[n] = b'0' + (value % 10) as u8;
					value /= 10;
					if value == 0 {
						break;
					}
				}
				if negative {
					n -= 1;
					buf[n] = b'-';
				}
				libc::write(write.as_raw_fd(), buf[n..].as_ptr().cast(), buf.len() - n);
				libc::_exit(0);
			};
			if libc::chdir(dir.as_ptr()) == -1 {
				report(*libc::__errno_location());
			}
			if too_long {
				report(-1);
			}
			let addr_ptr = (&raw const addr).cast::<libc::sockaddr>();
			let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
			let rc = match operation {
				Operation::Bind => libc::bind(fd.as_raw_fd(), addr_ptr, len),
				Operation::Connect => libc::connect(fd.as_raw_fd(), addr_ptr, len),
			};
			report(if rc == -1 {
				*libc::__errno_location()
			} else {
				0
			});
		}
	}
	drop(write);
	let mut output = Vec::new();
	io::Read::read_to_end(&mut std::fs::File::from(read), &mut output)?;
	let mut status = 0;
	// SAFETY: waiting for our own child.
	while unsafe { libc::waitpid(pid, &mut status, 0) } == -1 {
		let e = io::Error::last_os_error();
		if e.kind() != io::ErrorKind::Interrupted {
			return Err(e);
		}
	}
	match std::str::from_utf8(&output)
		.ok()
		.and_then(|s| s.trim().parse::<i32>().ok())
	{
		Some(0) => Ok(()),
		Some(errno) if errno > 0 => Err(io::Error::from_raw_os_error(errno)),
		// CppNix reports only that the operation failed; keep the reason.
		_ => Err(io::Error::other(format!(
			"socket path '{}' is too long",
			String::from_utf8_lossy(base)
		))),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::{Read, Write};

	#[test]
	fn long_paths_bind_and_connect() {
		let dir = tempfile::tempdir().unwrap();
		let deep = dir.path().join("d".repeat(60)).join("e".repeat(60));
		std::fs::create_dir_all(&deep).unwrap();
		let path = deep.join("socket");
		assert!(path.as_os_str().len() >= SUN_PATH);
		let listener = bind(&path).unwrap();
		let mut client = connect(&path).unwrap();
		let (mut server, _) = listener.accept().unwrap();
		client.write_all(b"ping").unwrap();
		let mut buf = [0u8; 4];
		server.read_exact(&mut buf).unwrap();
		assert_eq!(&buf, b"ping");
	}

	#[test]
	fn long_missing_paths_fail_like_cppnix() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("x".repeat(120)).join("socket");
		let err = connect(&path).unwrap_err();
		assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
		let base = dir.path().join("y".repeat(120));
		let err = connect(&base).unwrap_err();
		assert_eq!(
			err.to_string(),
			format!("socket path '{}' is too long", "y".repeat(120))
		);
	}
}
