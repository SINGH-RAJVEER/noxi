//! Interrupting a command, CppNix's `_isInterrupted` and the signal handler
//! thread of `initNix`.
//!
//! SIGINT, SIGTERM and SIGHUP set a flag that running builds check, so they
//! stop their builders, and the build hooks this process started are
//! signalled in turn, which cancels their builds on other machines. The
//! command then ends with `error: interrupted by the user`. A command that
//! does not notice within a few seconds is ended with that error.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Duration;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
/// The write end of the pipe the handler wakes the watcher with.
static WAKE: AtomicI32 = AtomicI32::new(-1);
/// The process groups of the running build hooks.
static GROUPS: Mutex<Vec<libc::pid_t>> = Mutex::new(Vec::new());

/// The message CppNix ends an interrupted command with.
pub const MESSAGE: &str = "interrupted by the user";

/// Whether the command was interrupted.
pub fn interrupted() -> bool {
	INTERRUPTED.load(Ordering::Relaxed)
}

extern "C" fn handle(_signal: libc::c_int) {
	INTERRUPTED.store(true, Ordering::Relaxed);
	let fd = WAKE.load(Ordering::Relaxed);
	if fd != -1 {
		// SAFETY: write is async-signal-safe; the byte only wakes the watcher.
		unsafe { libc::write(fd, [0u8].as_ptr().cast(), 1) };
	}
}

/// Handle the signals that interrupt a command. Signals ignored by the
/// parent, as a background job's SIGINT is, are handled too, as in CppNix.
/// A handler is reset by `exec`, so builders and other programs this
/// process starts keep the default actions.
pub fn install() {
	let mut fds = [0; 2];
	// SAFETY: `fds` has room for both descriptors.
	if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
		return;
	}
	WAKE.store(fds[1], Ordering::Relaxed);
	let read = fds[0];
	let spawned = std::thread::Builder::new()
		.name("interrupt".into())
		.spawn(move || {
			let mut byte = [0u8];
			// SAFETY: reading the pipe this thread owns.
			while unsafe { libc::read(read, byte.as_mut_ptr().cast(), 1) } == -1 {}
			signal_hooks(libc::SIGTERM);
			std::thread::sleep(Duration::from_secs(5));
			// The command did not end by itself.
			signal_hooks(libc::SIGKILL);
			nix_compat::log::logger::stop();
			let _ = std::io::Write::write_all(
				&mut std::io::stderr(),
				format!("error: {MESSAGE}\n").as_bytes(),
			);
			std::process::exit(1);
		});
	if spawned.is_err() {
		return;
	}
	for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
		// SAFETY: the handler only stores to atomics and writes to a pipe.
		unsafe {
			let mut action: libc::sigaction = std::mem::zeroed();
			action.sa_sigaction = handle as extern "C" fn(libc::c_int) as usize;
			libc::sigemptyset(&mut action.sa_mask);
			libc::sigaction(signal, &action, std::ptr::null_mut());
		}
	}
}

fn signal_hooks(signal: libc::c_int) {
	for group in GROUPS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
		// SAFETY: signalling a process group this process started.
		unsafe { libc::kill(-group, signal) };
	}
}

/// A build hook's process group, signalled if the command is interrupted.
pub(crate) fn register(group: libc::pid_t) {
	GROUPS.lock().unwrap_or_else(|e| e.into_inner()).push(group);
	if interrupted() {
		// SAFETY: as in `signal_hooks`.
		unsafe { libc::kill(-group, libc::SIGTERM) };
	}
}

pub(crate) fn unregister(group: libc::pid_t) {
	GROUPS
		.lock()
		.unwrap_or_else(|e| e.into_inner())
		.retain(|other| *other != group);
}
