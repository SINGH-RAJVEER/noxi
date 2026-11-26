//! `LocalStore::autoGC`: collect garbage when the free space of the store's
//! file system falls below `min-free`, as CppNix 2.35.2 does during builds
//! and before paths are added.

use std::time::{Duration, Instant};

use nix_compat::log::{VerbosityLevel, log_enabled};

use crate::error::{Error, Result};
use crate::gc::{GcAction, GcOptions};
use crate::local::LocalStore;

/// What CppNix keeps in `LocalStore::State` for `autoGC`.
#[derive(Debug)]
pub(crate) struct AutoGcState {
	/// When the free space was last checked; `None` before the first check.
	last_check: Option<Instant>,
	/// The free space after the last collection. A collection is not
	/// started again unless the free space has fallen by more than 3% since.
	avail_after_gc: u64,
}

impl Default for AutoGcState {
	fn default() -> Self {
		Self {
			last_check: None,
			avail_after_gc: u64::MAX,
		}
	}
}

/// `std::stoll`: optional leading white space and sign, then the digits
/// up to the first other character.
fn stoll(text: &str) -> Option<u64> {
	let text = text.trim_start();
	let digits = text.strip_prefix('+').unwrap_or(text);
	let end = digits
		.find(|c: char| !c.is_ascii_digit())
		.unwrap_or(digits.len());
	if text.starts_with('-') {
		// A negative count converts to a huge unsigned one in CppNix.
		let digits = &text[1..];
		let end = digits
			.find(|c: char| !c.is_ascii_digit())
			.unwrap_or(digits.len());
		return digits[..end]
			.parse::<i64>()
			.ok()
			.map(|n| n.wrapping_neg() as u64);
	}
	digits[..end].parse::<i64>().ok().map(|n| n as u64)
}

impl LocalStore {
	/// The free space of the store's file system, or the number in
	/// `_NIX_TEST_FREE_SPACE_FILE` when that is set.
	fn available_space(&self) -> Result<u64> {
		if let Some(file) = std::env::var_os("_NIX_TEST_FREE_SPACE_FILE") {
			// `readFile`'s messages, then `std::stoll`'s exception.
			let path = std::path::Path::new(&file);
			let strerror = |e: std::io::Error| {
				let text = e.to_string();
				match text.find(" (os error") {
					Some(end) => text[..end].to_owned(),
					None => text,
				}
			};
			let mut handle = std::fs::File::open(path).map_err(|e| {
				Error::Other(format!(
					"opening file \"{}\": {}",
					path.display(),
					strerror(e)
				))
			})?;
			let mut text = Vec::new();
			std::io::Read::read_to_end(&mut handle, &mut text).map_err(|e| {
				Error::Other(format!(
					"reading file \"{}\": {}",
					path.display(),
					strerror(e)
				))
			})?;
			return stoll(&String::from_utf8_lossy(&text))
				.ok_or_else(|| Error::Other("stoll".to_owned()));
		}
		let dir = &self.config().real_store_dir;
		let path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes())
			.map_err(|_| Error::Other(format!("invalid path '{}'", dir.display())))?;
		// SAFETY: `statvfs` writes only to `st`; `path` is NUL-terminated.
		let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
		if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
			return Err(Error::Other(format!(
				"getting filesystem info about '{}': {}",
				dir.display(),
				std::io::Error::last_os_error()
			)));
		}
		Ok((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
	}

	/// `autoGC`: when at least `min-free-check-interval` seconds have passed
	/// since the last check and the free space is below both `min-free` and
	/// `max-free`, and has fallen by more than 3% since the last such
	/// collection, collect garbage until `max-free` bytes are free.
	///
	/// While another thread collects, this returns at once, or with `sync`
	/// waits for that collection to finish. The collection itself runs on
	/// the calling thread; CppNix runs it on its own thread and lets the
	/// worker loop go on. Its errors are ignored, as CppNix's are.
	pub fn auto_gc(&self, sync: bool) -> Result<()> {
		let config = self.config();
		let running = match self.auto_gc_running.try_lock() {
			Ok(guard) => guard,
			Err(std::sync::TryLockError::WouldBlock) => {
				if sync {
					drop(self.auto_gc_running.lock());
				}
				return Ok(());
			}
			Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
		};
		let avail = {
			let mut state = self.auto_gc.lock().unwrap_or_else(|e| e.into_inner());
			let now = Instant::now();
			if state.last_check.is_some_and(|last| {
				now < last + Duration::from_secs(config.min_free_check_interval)
			}) {
				return Ok(());
			}
			let avail = self.available_space()?;
			state.last_check = Some(now);
			if avail >= config.min_free || avail >= config.max_free {
				return Ok(());
			}
			if avail as f64 > state.avail_after_gc as f64 * 0.97 {
				return Ok(());
			}
			avail
		};
		let mut options = GcOptions::new(GcAction::DeleteDead);
		options.max_freed = config.max_free - avail;
		options.keep_outputs = config.gc_keep_outputs;
		options.keep_derivations = config.gc_keep_derivations;
		if log_enabled(VerbosityLevel::Info) {
			eprintln!("running auto-GC to free {} bytes", options.max_freed);
		}
		let result = self.collect_garbage(&options, &mut |message| {
			if log_enabled(VerbosityLevel::Info) {
				eprintln!("{message}");
			}
		});
		let after = result.and_then(|_| self.available_space());
		let mut state = self.auto_gc.lock().unwrap_or_else(|e| e.into_inner());
		if let Ok(after) = after {
			state.avail_after_gc = after;
		}
		state.last_check = Some(Instant::now());
		drop(running);
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::stoll;

	#[test]
	fn free_space_files_are_read_as_stoll_does() {
		assert_eq!(stoll("1100\n"), Some(1100));
		assert_eq!(stoll("  +100 bytes"), Some(100));
		assert_eq!(stoll("-1"), Some(u64::MAX));
		assert_eq!(stoll("x"), None);
		assert_eq!(stoll(""), None);
	}
}
