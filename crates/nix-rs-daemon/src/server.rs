//! Accepting connections, following CppNix 2.35.2's `daemonLoop` and
//! `authPeer`: each connection is authorised by its peer credentials and
//! handled in a forked child process, so a client's temporary roots and
//! store handle end with its connection.

use std::ffi::CStr;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use nix_rs_store::LocalStore;

use crate::connection::{Trust, process_connection};
use crate::error::{Error, Result};
use crate::settings::DaemonSettings;

/// The client's credentials, from `SO_PEERCRED`.
#[derive(Clone, Copy, Debug)]
pub struct PeerInfo {
	pub pid: libc::pid_t,
	pub uid: libc::uid_t,
	pub gid: libc::gid_t,
}

pub fn peer_info(stream: &UnixStream) -> Result<PeerInfo> {
	let mut cred = libc::ucred {
		pid: 0,
		uid: 0,
		gid: 0,
	};
	let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
	// SAFETY: `cred` has room for a `ucred` and `len` says so.
	let ret = unsafe {
		libc::getsockopt(
			stream.as_raw_fd(),
			libc::SOL_SOCKET,
			libc::SO_PEERCRED,
			(&raw mut cred).cast(),
			&mut len,
		)
	};
	if ret == -1 {
		return Err(std::io::Error::last_os_error().into());
	}
	Ok(PeerInfo {
		pid: cred.pid,
		uid: cred.uid,
		gid: cred.gid,
	})
}

fn user_name(uid: libc::uid_t) -> Option<String> {
	let mut buf = vec![0 as libc::c_char; 16384];
	let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
	let mut result = std::ptr::null_mut();
	// SAFETY: the buffers outlive the call.
	unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
	(!result.is_null()).then(|| {
		unsafe { CStr::from_ptr(pwd.pw_name) }
			.to_string_lossy()
			.into_owned()
	})
}

/// A group's name and members.
fn group(gid: Option<libc::gid_t>, name: Option<&str>) -> Option<(String, Vec<String>)> {
	let mut buf = vec![0 as libc::c_char; 65536];
	let mut grp: libc::group = unsafe { std::mem::zeroed() };
	let mut result = std::ptr::null_mut();
	// SAFETY: the buffers outlive the call.
	unsafe {
		match (gid, name) {
			(Some(gid), _) => {
				libc::getgrgid_r(gid, &mut grp, buf.as_mut_ptr(), buf.len(), &mut result)
			}
			(None, Some(name)) => {
				let name = std::ffi::CString::new(name).ok()?;
				libc::getgrnam_r(
					name.as_ptr(),
					&mut grp,
					buf.as_mut_ptr(),
					buf.len(),
					&mut result,
				)
			}
			(None, None) => return None,
		};
	}
	if result.is_null() {
		return None;
	}
	let group_name = unsafe { CStr::from_ptr(grp.gr_name) }
		.to_string_lossy()
		.into_owned();
	let mut members = Vec::new();
	let mut member = grp.gr_mem;
	// SAFETY: `gr_mem` is a NULL-terminated array of strings in `buf`.
	unsafe {
		while !member.is_null() && !(*member).is_null() {
			members.push(CStr::from_ptr(*member).to_string_lossy().into_owned());
			member = member.add(1);
		}
	}
	Some((group_name, members))
}

/// `matchUser`: `*`, the user's name, or `@group` for the user's primary
/// group or a group that lists the user.
fn match_user(user: Option<&str>, group_name: Option<&str>, entries: &[String]) -> bool {
	for entry in entries {
		if entry == "*" || Some(entry.as_str()) == user {
			return true;
		}
		if let Some(wanted) = entry.strip_prefix('@') {
			if Some(wanted) == group_name {
				return true;
			}
			if let (Some(user), Some((_, members))) = (user, group(None, Some(wanted)))
				&& members.iter().any(|m| m == user)
			{
				return true;
			}
		}
	}
	false
}

/// `authPeer`: whether the peer is trusted, and its user name.
pub fn auth_peer(peer: &PeerInfo, settings: &DaemonSettings) -> Result<(Trust, String)> {
	let user = user_name(peer.uid).unwrap_or_else(|| peer.uid.to_string());
	let group_name = group(Some(peer.gid), None)
		.map(|(name, _)| name)
		.unwrap_or_else(|| peer.gid.to_string());
	let trusted = match_user(Some(&user), Some(&group_name), &settings.trusted_users);
	if (!trusted && !match_user(Some(&user), Some(&group_name), &settings.allowed_users))
		|| settings.build_users_group.as_deref() == Some(group_name.as_str())
	{
		return Err(Error::Nix(format!(
			"user '{user}' is not allowed to connect to the Nix daemon"
		)));
	}
	Ok((
		if trusted {
			Trust::Trusted
		} else {
			Trust::NotTrusted
		},
		user,
	))
}

/// Handle one connection in this process.
pub fn handle_connection(
	stream: UnixStream,
	settings: &DaemonSettings,
	trust: Trust,
) -> Result<()> {
	let store = LocalStore::open(settings.store.clone())?;
	let read = stream.try_clone()?;
	process_connection(read, stream, &store, settings, trust)
}

/// Socket activation takes ownership of the matching descriptors starting at 3.
/// A present LISTEN_FDS requires LISTEN_PID to identify this process, as the
/// reference's `serveUnixSocket` requires.
pub fn listeners(socket: &Path) -> Result<Vec<UnixListener>> {
	if let Some(count) = std::env::var_os("LISTEN_FDS") {
		if std::env::var("LISTEN_PID").ok().as_deref()
			!= Some(std::process::id().to_string().as_str())
		{
			return Err(Error::Nix(
				"unexpected systemd environment variables".into(),
			));
		}
		let count: i32 = count
			.to_str()
			.and_then(|n| n.parse().ok())
			.filter(|n| *n >= 0 && *n <= i32::MAX - 3)
			.ok_or_else(|| Error::Nix("invalid LISTEN_FDS".into()))?;
		let names = std::env::var("LISTEN_FDNAMES").unwrap_or_default();
		let names: Vec<_> = names.split(':').filter(|s| !s.is_empty()).collect();
		let mut listeners = Vec::new();
		for i in 0..count {
			if names
				.get(i as usize)
				.is_some_and(|name| *name != "nix-daemon.socket")
			{
				continue;
			}
			let fd = 3 + i;
			if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
				return Err(std::io::Error::last_os_error().into());
			}
			// SAFETY: fcntl confirmed an open descriptor; the activation contract
			// transfers ownership of the matching listening socket to us.
			let listener = unsafe { UnixListener::from_raw_fd(fd) };
			listener.local_addr()?;
			listeners.push(listener);
		}
		return Ok(listeners);
	}
	if !socket.is_absolute() {
		return Err(Error::Nix("daemon socket must be an absolute path".into()));
	}
	if let Some(parent) = socket.parent() {
		std::fs::create_dir_all(parent)?;
	}
	// Replace stale socket files on restart, but never remove a regular file.
	if let Ok(metadata) = std::fs::symlink_metadata(socket) {
		use std::os::unix::fs::FileTypeExt;
		if metadata.file_type().is_socket() {
			std::fs::remove_file(socket)?;
		}
	}
	let listener = UnixListener::bind(socket)?;
	std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o666))?;
	Ok(vec![listener])
}

/// Accept connections on `listener` forever, forking a child for each.
pub fn serve(listener: UnixListener, settings: &DaemonSettings) -> Result<()> {
	serve_listeners(vec![listener], settings)
}

/// Serve all descriptors supplied by the service manager.
pub fn serve_listeners(listeners: Vec<UnixListener>, settings: &DaemonSettings) -> Result<()> {
	let mut fds: Vec<libc::pollfd> = listeners
		.iter()
		.map(|listener| libc::pollfd {
			fd: listener.as_raw_fd(),
			events: libc::POLLIN,
			revents: 0,
		})
		.collect();
	loop {
		// Reap finished children so they don't become zombies.
		// SAFETY: waiting for our own children without blocking.
		while unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } > 0 {}
		// A bounded poll also reaps workers when no new clients arrive.
		let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 1000) };
		if ready < 0 {
			let error = std::io::Error::last_os_error();
			if error.kind() == std::io::ErrorKind::Interrupted {
				continue;
			}
			return Err(error.into());
		}
		let Some(index) = fds.iter().position(|fd| fd.revents != 0) else {
			continue;
		};
		let (stream, _) = match listeners[index].accept() {
			Ok(conn) => conn,
			Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
			Err(e) => return Err(e.into()),
		};
		let peer = peer_info(&stream)?;
		let (trust, user) = match settings
			.force_trust
			.map(|trust| (trust, "<unknown>".into()))
			.map(Ok)
			.unwrap_or_else(|| auth_peer(&peer, settings))
		{
			Ok(auth) => auth,
			Err(e) => {
				tracing::error!("error processing connection: {e}");
				continue;
			}
		};
		tracing::info!(
			"accepted connection from pid {}, user {user}{}",
			peer.pid,
			if trust == Trust::Trusted {
				" (trusted)"
			} else {
				""
			}
		);
		// SAFETY: the child only handles the connection and exits.
		let pid = unsafe { libc::fork() };
		if pid == -1 {
			return Err(std::io::Error::last_os_error().into());
		}
		if pid == 0 {
			drop(listeners);
			// Each connection is an independent daemon worker, as in daemonLoop.
			if unsafe { libc::setsid() } == -1 {
				unsafe {
					libc::_exit(1);
				}
			}
			let code = match handle_connection(stream, settings, trust) {
				Ok(()) => 0,
				Err(e) => {
					tracing::error!("unexpected Nix daemon error: {e}");
					1
				}
			};
			// SAFETY: leave the forked child without unwinding into the
			// parent's state.
			unsafe { libc::_exit(code) };
		}
	}
}
