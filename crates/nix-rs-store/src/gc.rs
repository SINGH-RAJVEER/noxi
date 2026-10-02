//! Garbage collection, following CppNix 2.35.2's `gc.cc` and
//! `local-gc.cc`.
//!
//! Roots are the symlinks under `STATE_DIR/gcroots` and
//! `STATE_DIR/profiles` (with indirect roots followed one level), the
//! `temproots/<pid>` files of live processes, and store paths that running
//! processes use (`/proc`). While collecting, the collector holds
//! `gc.lock` exclusively and accepts new temporary roots on
//! `gc-socket/socket`, as CppNix does, so clients of either implementation
//! can add roots during a collection.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use nix_compat::nixbase32;
use nix_compat::store_path::{STORE_DIR, STORE_DIR_WITH_SLASH, StorePath};

use crate::error::{Error, Result};
use crate::local::LocalStore;
use crate::lock::{LockType, lock_file};

fn print(path: &StorePath) -> String {
	path.to_absolute_path()
}

/// What a collection does, CppNix's `GCOptions::GCAction`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcAction {
	/// Return the live paths.
	ReturnLive,
	/// Return the dead paths.
	ReturnDead,
	/// Delete the dead paths.
	DeleteDead,
	/// Delete the given paths, failing if one is alive.
	DeleteSpecific,
}

#[derive(Clone, Debug)]
pub struct GcOptions {
	pub action: GcAction,
	/// The paths to consider; `None` means the whole store.
	pub paths: Option<BTreeSet<StorePath>>,
	/// With specific paths, also delete their referrers.
	pub delete_referrers: bool,
	/// Ignore roots (`--ignore-liveness`).
	pub ignore_liveness: bool,
	/// Stop after freeing more than this many bytes.
	pub max_freed: u64,
	/// `keep-outputs`.
	pub keep_outputs: bool,
	/// `keep-derivations`.
	pub keep_derivations: bool,
}

impl GcOptions {
	pub fn new(action: GcAction) -> Self {
		Self {
			action,
			paths: None,
			delete_referrers: false,
			ignore_liveness: false,
			max_freed: u64::MAX,
			keep_outputs: false,
			keep_derivations: true,
		}
	}
}

/// The paths a collection deleted or found, as absolute paths, and the
/// bytes it freed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcResults {
	pub paths: BTreeSet<String>,
	pub bytes_freed: u64,
}

/// Each root and the places that make it one.
pub type Roots = BTreeMap<StorePath, BTreeSet<String>>;

const CENSORED: &str = "{censored}";

/// CppNix's `StoreDirConfig::toStorePath`: the store path a path inside
/// the store belongs to.
fn to_store_path(target: &str) -> Option<StorePath> {
	let rest = target.strip_prefix(STORE_DIR_WITH_SLASH)?;
	let base = rest.split('/').next()?;
	StorePath::from_bytes(base.as_bytes()).ok()
}

/// `deletePath` with CppNix's freed-space estimate: the size of each file
/// with one or two links.
pub(crate) fn delete_counting(path: &Path) -> Result<u64> {
	let metadata = match fs::symlink_metadata(path) {
		Ok(m) => m,
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
		Err(e) => {
			return Err(Error::io(
				format!("getting status of '{}'", path.display()),
				e,
			));
		}
	};
	let mut freed = 0;
	if metadata.is_dir() {
		if metadata.mode() & 0o700 != 0o700 {
			use std::os::unix::fs::PermissionsExt;
			fs::set_permissions(path, fs::Permissions::from_mode(metadata.mode() | 0o700))?;
		}
		for entry in fs::read_dir(path)? {
			freed += delete_counting(&entry?.path())?;
		}
		fs::remove_dir(path).map_err(|e| Error::io(format!("removing '{}'", path.display()), e))?;
	} else {
		if matches!(metadata.nlink(), 1 | 2) {
			freed += metadata.size();
		}
		fs::remove_file(path)
			.map_err(|e| Error::io(format!("removing '{}'", path.display()), e))?;
	}
	Ok(freed)
}

/// `findRuntimeRootsUnchecked`: store paths that processes have open,
/// mapped, as their executable or working directory, or in their
/// environment.
fn runtime_roots() -> BTreeMap<String, BTreeSet<String>> {
	let mut unchecked: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
	let mut add = |target: String, source: String| {
		unchecked.entry(target).or_default().insert(source);
	};
	let read_link = |file: &str, add: &mut dyn FnMut(String, String)| {
		if let Ok(target) = fs::read_link(file)
			&& target.is_absolute()
		{
			add(target.to_string_lossy().into_owned(), file.to_owned());
		}
	};
	let Ok(proc) = fs::read_dir("/proc") else {
		return unchecked;
	};
	for entry in proc.flatten() {
		let name = entry.file_name().to_string_lossy().into_owned();
		if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
			continue;
		}
		read_link(&format!("/proc/{name}/exe"), &mut add);
		read_link(&format!("/proc/{name}/cwd"), &mut add);
		if let Ok(fds) = fs::read_dir(format!("/proc/{name}/fd")) {
			for fd in fds.flatten() {
				let fd = fd.file_name().to_string_lossy().into_owned();
				if !fd.starts_with('.') {
					read_link(&format!("/proc/{name}/fd/{fd}"), &mut add);
				}
			}
		}
		let maps = format!("/proc/{name}/maps");
		if let Ok(text) = fs::read_to_string(&maps) {
			for line in text.lines() {
				// The sixth field, when it is an absolute path.
				let fields: Vec<&str> = line.split_whitespace().collect();
				if fields.len() == 6 && fields[5].starts_with('/') {
					add(fields[5].to_owned(), maps.clone());
				}
			}
		}
		let environ = format!("/proc/{name}/environ");
		if let Ok(bytes) = fs::read(&environ) {
			for found in store_paths_in(&bytes) {
				add(found, environ.clone());
			}
		}
	}
	for file in [
		"/proc/sys/kernel/modprobe",
		"/proc/sys/kernel/fbsplash",
		"/proc/sys/kernel/poweroff_cmd",
	] {
		if let Ok(text) = fs::read_to_string(file) {
			add(text, file.to_owned());
		}
	}
	unchecked
}

/// Matches of `STORE_DIR/[0-9a-z]+[0-9a-zA-Z+\-._?=]*` in `bytes`.
fn store_paths_in(bytes: &[u8]) -> Vec<String> {
	let prefix = format!("{STORE_DIR}/");
	let prefix = prefix.as_bytes();
	let mut found = Vec::new();
	let mut i = 0;
	while let Some(pos) = bytes[i..].windows(prefix.len()).position(|w| w == prefix) {
		let start = i + pos;
		let mut end = start + prefix.len();
		if end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end].is_ascii_lowercase()) {
			end += 1;
			while end < bytes.len()
				&& (bytes[end].is_ascii_alphanumeric() || b"+-._?=".contains(&bytes[end]))
			{
				end += 1;
			}
			found.push(String::from_utf8_lossy(&bytes[start..end]).into_owned());
		}
		i = start + prefix.len();
	}
	found
}

/// Temporary roots received on the GC socket during a collection, by hash
/// part, and the hash part of the path being deleted.
#[derive(Default)]
struct Shared {
	temp_roots: BTreeSet<String>,
	pending: Option<String>,
}

/// The GC roots server: accepts connections on `gc-socket/socket` and
/// records each store path a client sends as a temporary root.
struct RootsServer {
	shared: Arc<(Mutex<Shared>, Condvar)>,
	path: PathBuf,
	stop: Arc<std::sync::atomic::AtomicBool>,
	thread: Option<std::thread::JoinHandle<()>>,
}

impl RootsServer {
	fn start(path: PathBuf, shared: Arc<(Mutex<Shared>, Condvar)>) -> Result<Self> {
		if let Some(parent) = path.parent() {
			fs::create_dir_all(parent)?;
		}
		let _ = fs::remove_file(&path);
		let listener = UnixListener::bind(&path)
			.map_err(|e| Error::io(format!("creating socket '{}'", path.display()), e))?;
		use std::os::unix::fs::PermissionsExt;
		fs::set_permissions(&path, fs::Permissions::from_mode(0o666))?;
		listener.set_nonblocking(true)?;
		let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
		let thread = {
			let shared = shared.clone();
			let stop = stop.clone();
			std::thread::spawn(move || {
				while !stop.load(std::sync::atomic::Ordering::Relaxed) {
					match listener.accept() {
						Ok((stream, _)) => {
							let shared = shared.clone();
							std::thread::spawn(move || serve_client(stream, shared));
						}
						Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
							std::thread::sleep(std::time::Duration::from_millis(10));
						}
						Err(_) => break,
					}
				}
			})
		};
		Ok(Self {
			shared,
			path,
			stop,
			thread: Some(thread),
		})
	}
}

impl Drop for RootsServer {
	fn drop(&mut self) {
		self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
		self.shared.1.notify_all();
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
		let _ = fs::remove_file(&self.path);
	}
}

fn serve_client(stream: std::os::unix::net::UnixStream, shared: Arc<(Mutex<Shared>, Condvar)>) {
	let _ = stream.set_nonblocking(false);
	let Ok(mut writer) = stream.try_clone() else {
		return;
	};
	let mut reader = BufReader::new(stream);
	loop {
		let mut line = String::new();
		match reader.read_line(&mut line) {
			Ok(0) | Err(_) => return,
			Ok(_) => {}
		}
		let line = line.trim_end_matches('\n');
		match StorePath::from_absolute_path(line.as_bytes()) {
			Ok(path) => {
				let hash = nixbase32::encode(path.digest());
				let (lock, wakeup) = &*shared;
				let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
				state.temp_roots.insert(hash.clone());
				// Wait until a deletion of this path finishes, so the client
				// cannot recreate it underneath the collector.
				while state.pending.as_deref() == Some(hash.as_str()) {
					state = wakeup.wait(state).unwrap_or_else(|e| e.into_inner());
				}
			}
			Err(_) => tracing::error!("received garbage instead of a root from client"),
		}
		if writer.write_all(b"1").is_err() {
			return;
		}
	}
}

impl LocalStore {
	/// `findTempRoots`: the roots in the `temproots` files of processes
	/// that still hold them. Stale files are removed.
	pub fn find_temp_roots(&self, censor: bool, log: &mut dyn FnMut(&str)) -> Result<Roots> {
		let dir = self.config().state_dir.join("temproots");
		let mut roots = Roots::new();
		let entries = match fs::read_dir(&dir) {
			Ok(entries) => entries,
			Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(roots),
			Err(e) => return Err(Error::io(format!("reading '{}'", dir.display()), e)),
		};
		for entry in entries {
			let entry = entry?;
			let name = entry.file_name().to_string_lossy().into_owned();
			if name.starts_with('.') {
				continue;
			}
			let path = entry.path();
			let mut file = match fs::OpenOptions::new().read(true).write(true).open(&path) {
				Ok(file) => file,
				Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
				Err(e) => {
					return Err(Error::io(
						format!("opening temporary roots file '{}'", path.display()),
						e,
					));
				}
			};
			if lock_file(&file, LockType::Write, false)? {
				log(&format!(
					"removing stale temporary roots file \"{}\"",
					path.display()
				));
				let _ = fs::remove_file(&path);
				let _ = file.write_all(b"d");
				continue;
			}
			let mut contents = Vec::new();
			file.read_to_end(&mut contents)?;
			for root in contents.split(|&b| b == 0) {
				if root.is_empty() {
					continue;
				}
				let path = StorePath::from_absolute_path(root).map_err(|e| {
					Error::Other(format!(
						"path '{}' is not a valid store path: {e}",
						String::from_utf8_lossy(root)
					))
				})?;
				roots.entry(path).or_default().insert(if censor {
					CENSORED.to_owned()
				} else {
					format!("{{temp:{name}}}")
				});
			}
		}
		Ok(roots)
	}

	/// `findRoots(path, type, roots)` for one entry under the roots
	/// directories.
	fn find_roots_in(
		&self,
		path: &Path,
		roots: &mut Roots,
		log: &mut dyn FnMut(&str),
	) -> Result<()> {
		let state_dir = self.config().state_dir.clone();
		let mut found = |link: &Path, target: &str, roots: &mut Roots| -> Result<()> {
			if let Some(store_path) = to_store_path(target) {
				if self.is_valid_path(&store_path)? {
					roots
						.entry(store_path)
						.or_default()
						.insert(link.to_string_lossy().into_owned());
				} else {
					log(&format!(
						"skipping invalid root from \"{}\" to \"{target}\"",
						link.display()
					));
				}
			}
			Ok(())
		};
		let metadata = match fs::symlink_metadata(path) {
			Ok(m) => m,
			Err(e)
				if matches!(
					e.kind(),
					io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
				) =>
			{
				log(&format!(
					"cannot read potential root \"{}\"",
					path.display()
				));
				return Ok(());
			}
			Err(e) => {
				return Err(Error::io(
					format!("finding GC roots in '{}'", path.display()),
					e,
				));
			}
		};
		if metadata.is_dir() {
			let entries = match fs::read_dir(path) {
				Ok(entries) => entries,
				Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
					log(&format!(
						"cannot read potential root \"{}\"",
						path.display()
					));
					return Ok(());
				}
				Err(e) => {
					return Err(Error::io(
						format!("finding GC roots in '{}'", path.display()),
						e,
					));
				}
			};
			let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
			children.sort();
			for child in children {
				self.find_roots_in(&child, roots, log)?;
			}
		} else if metadata.file_type().is_symlink() {
			let target = fs::read_link(path)?;
			let target_str = target.to_string_lossy().into_owned();
			if target_str.starts_with(STORE_DIR_WITH_SLASH) {
				found(path, &target_str, roots)?;
			} else {
				// An indirect root: a link to a link into the store.
				let target = path.parent().unwrap_or(Path::new("/")).join(&target);
				if fs::symlink_metadata(&target).is_err() {
					if path.starts_with(state_dir.join("gcroots/auto")) {
						log(&format!(
							"removing stale link from \"{}\" to \"{}\"",
							path.display(),
							target.display()
						));
						let _ = fs::remove_file(path);
					}
				} else if let Ok(target2) = fs::read_link(&target) {
					let target2 = target2.to_string_lossy().into_owned();
					if target2.starts_with(STORE_DIR_WITH_SLASH) {
						found(&target, &target2, roots)?;
					}
				}
			}
		} else if metadata.is_file() {
			let name = path
				.file_name()
				.map(|n| n.to_string_lossy().into_owned())
				.unwrap_or_default();
			if let Ok(store_path) = StorePath::from_bytes(name.as_bytes())
				&& self.is_valid_path(&store_path)?
			{
				roots
					.entry(store_path)
					.or_default()
					.insert(path.to_string_lossy().into_owned());
			}
		}
		Ok(())
	}

	/// `findRootsNoTemp`: the roots under `gcroots` and `profiles`, and
	/// valid paths that running processes use.
	pub fn find_roots_no_temp(&self, censor: bool, log: &mut dyn FnMut(&str)) -> Result<Roots> {
		let mut roots = Roots::new();
		let state_dir = self.config().state_dir.clone();
		self.find_roots_in(&state_dir.join("gcroots"), &mut roots, log)?;
		self.find_roots_in(&state_dir.join("profiles"), &mut roots, log)?;
		for (target, sources) in runtime_roots() {
			let Some(path) = to_store_path(&target) else {
				continue;
			};
			if !self.is_valid_path(&path)? {
				continue;
			}
			let entry = roots.entry(path).or_default();
			if censor {
				entry.insert(CENSORED.to_owned());
			} else {
				entry.extend(sources);
			}
		}
		Ok(roots)
	}

	/// `findRoots`: every root, as `nix-store --gc --print-roots` lists them.
	pub fn find_roots(&self, censor: bool, log: &mut dyn FnMut(&str)) -> Result<Roots> {
		let mut roots = self.find_roots_no_temp(censor, log)?;
		for (path, sources) in self.find_temp_roots(censor, log)? {
			roots.entry(path).or_default().extend(sources);
		}
		Ok(roots)
	}

	/// `computeFSClosure` of `path`, optionally with derivation outputs and
	/// derivers.
	fn gc_closure(
		&self,
		path: &StorePath,
		include_outputs: bool,
		include_derivers: bool,
		out: &mut BTreeSet<StorePath>,
	) -> Result<()> {
		let mut pending = vec![path.clone()];
		while let Some(path) = pending.pop() {
			if out.contains(&path) {
				continue;
			}
			let Some(info) = self.query_path_info(&path)? else {
				continue;
			};
			out.insert(path.clone());
			pending.extend(info.references.iter().cloned());
			if include_outputs && path.name().ends_with(".drv") {
				for output in self.query_derivation_outputs(&path)?.into_values() {
					if self.is_valid_path(&output)? {
						pending.push(output);
					}
				}
			}
			if include_derivers
				&& let Some(deriver) = &info.deriver
				&& self.is_valid_path(deriver)?
			{
				pending.push(deriver.clone());
			}
		}
		Ok(())
	}

	/// `topoSortPaths`: referrers before the paths they refer to.
	fn topo_sort_paths(&self, paths: &BTreeSet<StorePath>) -> Result<Vec<StorePath>> {
		let mut sorted = Vec::new();
		let mut visited = BTreeSet::new();
		let mut parents = BTreeSet::new();
		fn visit(
			store: &LocalStore,
			path: &StorePath,
			paths: &BTreeSet<StorePath>,
			visited: &mut BTreeSet<StorePath>,
			parents: &mut BTreeSet<StorePath>,
			sorted: &mut Vec<StorePath>,
		) -> Result<()> {
			if parents.contains(path) {
				return Err(Error::Other(format!(
					"cycle detected in the references of '{}'",
					print(path)
				)));
			}
			if !visited.insert(path.clone()) {
				return Ok(());
			}
			parents.insert(path.clone());
			let references = store
				.query_path_info(path)?
				.map(|i| i.references)
				.unwrap_or_default();
			for reference in &references {
				if reference != path && paths.contains(reference) {
					visit(store, reference, paths, visited, parents, sorted)?;
				}
			}
			sorted.push(path.clone());
			parents.remove(path);
			Ok(())
		}
		for path in paths {
			visit(self, path, paths, &mut visited, &mut parents, &mut sorted)?;
		}
		sorted.reverse();
		Ok(sorted)
	}

	/// `collectGarbage`. Progress messages CppNix prints at the default
	/// verbosity go to `log`.
	pub fn collect_garbage(
		&self,
		options: &GcOptions,
		log: &mut dyn FnMut(&str),
	) -> Result<GcResults> {
		let mut results = GcResults::default();
		if options.paths.as_ref().is_some_and(BTreeSet::is_empty) {
			return Ok(results);
		}
		let should_delete = matches!(
			options.action,
			GcAction::DeleteDead | GcAction::DeleteSpecific
		);
		let whole_store = options.paths.is_none();
		let state_dir = self.config().state_dir.clone();
		let real_store = self.config().real_store_dir.clone();
		if should_delete {
			delete_counting(&state_dir.join("db/reserved"))?;
		}

		let gc_lock = self.open_gc_lock()?;
		if !lock_file(&gc_lock, LockType::Write, false)? {
			log("waiting for the big garbage collector lock...");
			lock_file(&gc_lock, LockType::Write, true)?;
		}

		let shared = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
		let _server = RootsServer::start(state_dir.join("gc-socket/socket"), shared.clone())?;

		log("finding garbage collector roots...");
		let mut roots = BTreeSet::new();
		if !options.ignore_liveness {
			roots.extend(self.find_roots_no_temp(true, log)?.into_keys());
		}
		for path in self.find_temp_roots(true, log)?.into_keys() {
			shared
				.0
				.lock()
				.unwrap_or_else(|e| e.into_inner())
				.temp_roots
				.insert(nixbase32::encode(path.digest()));
			roots.insert(path);
		}

		let mut alive: BTreeSet<StorePath> = BTreeSet::new();
		let mut dead: BTreeSet<StorePath> = BTreeSet::new();
		let mut referrers_cache: BTreeMap<StorePath, BTreeSet<StorePath>> = BTreeMap::new();
		let is_temp_root = |path: &StorePath| -> bool {
			shared
				.0
				.lock()
				.unwrap_or_else(|e| e.into_inner())
				.temp_roots
				.contains(&nixbase32::encode(path.digest()))
		};
		let set_pending = |path: Option<&StorePath>| {
			let (lock, wakeup) = &*shared;
			lock.lock().unwrap_or_else(|e| e.into_inner()).pending =
				path.map(|p| nixbase32::encode(p.digest()));
			wakeup.notify_all();
		};

		struct LimitReached;
		let mut delete_from_store = |base_name: &str,
		                             results: &mut GcResults,
		                             log: &mut dyn FnMut(&str)|
		 -> Result<std::result::Result<(), LimitReached>> {
			let real = real_store.join(base_name);
			if base_name.starts_with("tmp-") {
				// A temporary directory another process still holds.
				match fs::File::open(&real) {
					Ok(file) if lock_file(&file, LockType::Write, false)? => {}
					_ => return Ok(Ok(())),
				}
			}
			let path = format!("{STORE_DIR}/{base_name}");
			log(&format!("deleting '{path}'"));
			results.paths.insert(path);
			results.bytes_freed += delete_counting(&real)?;
			if results.bytes_freed > options.max_freed {
				log(&format!(
					"deleted more than {} bytes; stopping",
					options.max_freed
				));
				return Ok(Err(LimitReached));
			}
			Ok(Ok(()))
		};

		// `maybeDeleteReferrersClosure`. Returns `Err(LimitReached)` once
		// `max_freed` is exceeded.
		let mut maybe_delete = |start: &StorePath,
		                        alive: &mut BTreeSet<StorePath>,
		                        dead: &mut BTreeSet<StorePath>,
		                        results: &mut GcResults,
		                        log: &mut dyn FnMut(&str)|
		 -> Result<std::result::Result<(), LimitReached>> {
			let mut visited = BTreeSet::new();
			let mut todo = VecDeque::new();
			visited.insert(start.clone());
			todo.push_back(start.clone());
			let mark_alive = |path: &StorePath, alive: &mut BTreeSet<StorePath>| -> Result<()> {
				alive.insert(path.clone());
				let mut closure = BTreeSet::new();
				self.gc_closure(
					path,
					whole_store && options.keep_outputs,
					whole_store && options.keep_derivations,
					&mut closure,
				)?;
				alive.extend(closure);
				Ok(())
			};
			while let Some(path) = todo.pop_front() {
				if alive.contains(&path) {
					alive.insert(start.clone());
					set_pending(None);
					return Ok(Ok(()));
				}
				if dead.contains(&path) {
					continue;
				}
				if roots.contains(&path) {
					alive.insert(start.clone());
					set_pending(None);
					mark_alive(&path, alive)?;
					return Ok(Ok(()));
				}
				if let Some(paths) = &options.paths
					&& !options.delete_referrers
					&& !paths.contains(&path)
				{
					set_pending(None);
					return Ok(Ok(()));
				}
				if is_temp_root(&path) {
					alive.insert(start.clone());
					set_pending(None);
					mark_alive(&path, alive)?;
					return Ok(Ok(()));
				}
				set_pending(Some(&path));
				if self.is_valid_path(&path)? {
					if !referrers_cache.contains_key(&path) {
						referrers_cache.insert(path.clone(), self.query_referrers(&path)?);
					}
					for referrer in &referrers_cache[&path] {
						if visited.insert(referrer.clone()) {
							todo.push_back(referrer.clone());
						}
					}
					if whole_store {
						let mut extra = Vec::new();
						if options.keep_derivations && path.name().ends_with(".drv") {
							for output in self.query_derivation_outputs(&path)?.into_values() {
								if let Some(info) = self.query_path_info(&output)?
									&& info.deriver.as_ref() == Some(&path)
								{
									extra.push(output);
								}
							}
						}
						if options.keep_outputs {
							extra.extend(self.query_valid_derivers(&path)?);
						}
						for p in extra {
							if visited.insert(p.clone()) {
								todo.push_back(p);
							}
						}
					}
				}
			}
			set_pending(None);
			for path in self.topo_sort_paths(&visited)? {
				if !dead.insert(path.clone()) {
					continue;
				}
				if should_delete {
					if is_temp_root(&path) {
						mark_alive(&path, alive)?;
						continue;
					}
					set_pending(Some(&path));
					let result = self.invalidate_path_checked(&path).and_then(|()| {
						referrers_cache.remove(&path);
						delete_from_store(&path.to_string(), results, log)
					});
					set_pending(None);
					match result {
						Ok(Ok(())) => {}
						Ok(Err(limit)) => return Ok(Err(limit)),
						Err(e @ Error::PathInUse { .. }) => tracing::error!("BUG: {e}"),
						Err(e) => return Err(e),
					}
				}
			}
			Ok(Ok(()))
		};

		match &options.paths {
			Some(paths) => {
				log(match options.action {
					GcAction::DeleteDead => "deleting garbage within specified paths...",
					GcAction::DeleteSpecific => "deleting specified paths...",
					_ => "determining live/dead paths...",
				});
				for path in paths {
					if maybe_delete(path, &mut alive, &mut dead, &mut results, log)?.is_err() {
						break;
					}
					if !dead.contains(path) && options.action == GcAction::DeleteSpecific {
						return Err(Error::Other(format!(
							"Cannot delete path '{}' since it is still alive. To find out why, use: nix-store --query --roots and nix-store --query --referrers",
							print(path)
						)));
					}
				}
			}
			None if options.max_freed == 0 => {}
			None => {
				match options.action {
					GcAction::DeleteDead => log("deleting garbage..."),
					GcAction::DeleteSpecific => {
						return Err(Error::Other("Cannot delete the entire store".to_owned()));
					}
					_ => log("determining live/dead paths..."),
				}
				let mut names: Vec<String> = fs::read_dir(&real_store)
					.map_err(|e| {
						Error::io(format!("opening directory '{}'", real_store.display()), e)
					})?
					.flatten()
					.map(|e| e.file_name().to_string_lossy().into_owned())
					.filter(|n| n != ".links")
					.collect();
				names.sort();
				for name in names {
					let step = match StorePath::from_bytes(name.as_bytes()) {
						Ok(path) => maybe_delete(&path, &mut alive, &mut dead, &mut results, log)?,
						Err(_) => delete_from_store(&name, &mut results, log)?,
					};
					if step.is_err() {
						break;
					}
				}
			}
		}

		match options.action {
			GcAction::ReturnLive => {
				results.paths = alive.iter().map(print).collect();
				return Ok(results);
			}
			GcAction::ReturnDead => {
				results.paths = dead.iter().map(print).collect();
				return Ok(results);
			}
			_ => {}
		}

		log("deleting unused links...");
		let links = real_store.join(".links");
		let (mut actual, mut unshared) = (0i64, 0i64);
		if let Ok(entries) = fs::read_dir(&links) {
			for entry in entries.flatten() {
				let metadata = entry.metadata()?;
				if metadata.nlink() != 1 {
					actual += metadata.size() as i64;
					unshared += (metadata.nlink() as i64 - 1) * metadata.size() as i64;
					continue;
				}
				let _ = fs::remove_file(entry.path());
			}
		}
		let overhead = fs::metadata(&links)
			.map(|m| m.blocks() as i64 * 512)
			.unwrap_or(0);
		log(&format!(
			"note: hard linking is currently saving {}",
			render_size(unshared - actual - overhead, false)
		));
		Ok(results)
	}
}

/// CppNix's `renderSize`: a size in KiB or the largest binary unit it
/// exceeds, with one decimal.
pub fn render_size(value: i64, align: bool) -> String {
	const SUFFIXES: [char; 9] = ['K', 'K', 'M', 'G', 'T', 'P', 'E', 'Z', 'Y'];
	let mut unit = 0;
	let mut abs = value.unsigned_abs();
	while abs > 1024 && unit < SUFFIXES.len() - 1 {
		unit += 1;
		abs /= 1024;
	}
	let result = value as f64 / 1024f64.powi(unit.max(1) as i32);
	if align {
		format!("{result:6.1} {}iB", SUFFIXES[unit])
	} else {
		format!("{result:.1} {}iB", SUFFIXES[unit])
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn sizes_render_like_cppnix() {
		assert_eq!(render_size(0, false), "0.0 KiB");
		assert_eq!(render_size(100, false), "0.1 KiB");
		assert_eq!(render_size(1024, false), "1.0 KiB");
		assert_eq!(render_size(1025, false), "1.0 KiB");
		assert_eq!(render_size(1 << 20, false), "1024.0 KiB");
		assert_eq!(render_size((1 << 20) + 1, false), "1024.0 KiB");
		assert_eq!(render_size(1 << 21, false), "2.0 MiB");
		assert_eq!(render_size(3 << 20, false), "3.0 MiB");
		assert_eq!(render_size(-4096, false), "-4.0 KiB");
		assert_eq!(render_size(1536, true), "   1.5 KiB");
	}
}
