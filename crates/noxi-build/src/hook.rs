//! The build hook, CppNix 2.35.2's `HookInstance` and the parts of
//! `DerivationBuildingGoal` that talk to one: an external program, `nix
//! __build-remote` by default, that offers to build derivations elsewhere.
//!
//! The hook is executed directly, never through a shell. Store paths,
//! systems, features and settings reach it only as length-prefixed strings
//! on its standard input.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use nix_compat::log::logger::{self, Activity, LogField};
use nix_compat::log::{ActivityType, ResultType, VerbosityLevel};
use nix_compat::nixcpp::machines::Machine;
use nix_compat::store_path::StorePath;
use serde_json::Value;

use crate::error::{BuildError, Result};
use crate::sandbox::ExitStatus;
use crate::settings::BuildSettings;

/// `HookReply`.
pub enum Reply {
	/// The hook builds the derivation.
	Accept(Box<HookInstance>),
	/// The hook does not build it.
	Decline,
	/// Every suitable machine is busy.
	Postpone,
}

/// The build hook of one worker: `Worker::hook`, `Worker::tryBuildHook` and
/// the count of running local builds the hook is told about.
pub struct Hooks {
	state: Mutex<State>,
	local_builds: Mutex<u64>,
	slot_freed: Condvar,
}

impl std::fmt::Debug for Hooks {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Hooks").finish_non_exhaustive()
	}
}

#[derive(Default)]
struct State {
	instance: Option<Box<HookInstance>>,
	/// The hook declined permanently.
	disabled: bool,
}

/// A running local build, counted against `max-jobs`.
pub struct Slot(Arc<Hooks>);

impl Drop for Slot {
	fn drop(&mut self) {
		let mut running = self
			.0
			.local_builds
			.lock()
			.unwrap_or_else(|e| e.into_inner());
		*running -= 1;
		self.0.slot_freed.notify_all();
	}
}

/// `builders`, `@CONF_DIR/machines` by default.
fn builders(settings: &BuildSettings) -> String {
	if let Some(builders) = settings.config.get("builders") {
		return builders.to_owned();
	}
	if let Ok(systems) = std::env::var("NIX_REMOTE_SYSTEMS") {
		return systems
			.split(':')
			.map(|path| format!("@{path}"))
			.collect::<Vec<_>>()
			.join("\n");
	}
	let conf = std::env::var("NIX_CONF_DIR").unwrap_or_else(|_| "/etc/nix".to_owned());
	format!("@{conf}/machines")
}

impl Hooks {
	/// The hook to try for builds with `settings`, if one can accept any.
	///
	/// The default hook declines permanently when no machine is configured.
	/// That is decided here without starting it.
	pub fn new(settings: &BuildSettings) -> Option<Arc<Self>> {
		if !settings.remote_builds {
			return None;
		}
		match settings.config.get("build-hook") {
			Some(hook) if hook.trim().is_empty() => return None,
			Some(_) => {}
			None => {
				let systems = BTreeSet::from([settings.system.clone()]);
				let cwd = std::env::current_dir().unwrap_or_default();
				// A malformed specification is the hook's error to report.
				if let Ok(machines) =
					Machine::parse_config(&systems, &builders(settings), &cwd, &mut Vec::new())
					&& machines.is_empty()
				{
					return None;
				}
			}
		}
		Some(Arc::new(Self {
			state: Mutex::default(),
			local_builds: Mutex::new(0),
			slot_freed: Condvar::new(),
		}))
	}

	/// `getNrLocalBuilds() < maxBuildJobs`.
	fn slot_free(&self, settings: &BuildSettings) -> bool {
		*self.local_builds.lock().unwrap_or_else(|e| e.into_inner()) < settings.max_build_jobs
	}

	/// Take a slot for a local build, if one is free.
	pub fn acquire_slot(self: &Arc<Self>, settings: &BuildSettings) -> Option<Slot> {
		let mut running = self.local_builds.lock().unwrap_or_else(|e| e.into_inner());
		if *running >= settings.max_build_jobs {
			return None;
		}
		*running += 1;
		Some(Slot(self.clone()))
	}

	/// Wait until a local build ends, or a short while.
	pub fn wait_for_slot(&self) {
		let running = self.local_builds.lock().unwrap_or_else(|e| e.into_inner());
		let _ = self
			.slot_freed
			.wait_timeout(running, Duration::from_millis(100));
	}

	/// `tryBuildHook`: offer the derivation to the hook.
	pub fn offer(
		&self,
		settings: &BuildSettings,
		system: &[u8],
		drv_path: &StorePath,
		features: &BTreeSet<String>,
		cancelled: &dyn Fn() -> bool,
	) -> Result<Reply> {
		let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
		if state.disabled {
			return Ok(Reply::Decline);
		}
		let mut hook = match state.instance.take() {
			Some(hook) => hook,
			None => Box::new(HookInstance::start(settings)?),
		};
		let mut request = Vec::new();
		put_string(&mut request, b"try");
		put_u64(&mut request, self.slot_free(settings).into());
		put_string(&mut request, system);
		put_string(&mut request, drv_path.to_absolute_path().as_bytes());
		put_strings(&mut request, features.iter().map(String::as_bytes));
		hook.out.extend(request);
		if let Err(error) = hook.flush() {
			if error.kind() != io::ErrorKind::BrokenPipe {
				return Err(error.into());
			}
			let text = hook.drain();
			logger::log(
				VerbosityLevel::Error,
				&format!(
					"build hook died unexpectedly: {}",
					String::from_utf8_lossy(text.trim_ascii_end())
				),
			);
			return Ok(Reply::Decline);
		}
		let reply = loop {
			let line = hook
				.read_line(cancelled)
				.map_err(|e| traced(e, "while reading the response from the build hook"))?;
			if hook.handle_json(&line, "the build hook").is_some() {
				continue;
			}
			if let Some(reply) = line.strip_prefix(b"# ") {
				break String::from_utf8_lossy(reply).into_owned();
			}
			let _ = io::stderr().lock().write_all(&[&line[..], b"\n"].concat());
		};
		match reply.as_str() {
			"accept" => Ok(Reply::Accept(hook)),
			"decline" => {
				state.instance = Some(hook);
				Ok(Reply::Decline)
			}
			"decline-permanently" => {
				state.disabled = true;
				Ok(Reply::Decline)
			}
			"postpone" => {
				state.instance = Some(hook);
				Ok(Reply::Postpone)
			}
			_ => Err(BuildError::Other(format!("bad hook reply '{reply}'"))),
		}
	}
}

fn traced(error: BuildError, trace: &str) -> BuildError {
	match error {
		BuildError::Other(message) => BuildError::Other(format!("{message}\n… {trace}")),
		error => error,
	}
}

fn put_u64(out: &mut Vec<u8>, n: u64) {
	out.extend(n.to_le_bytes());
}

fn put_string(out: &mut Vec<u8>, s: &[u8]) {
	put_u64(out, s.len() as u64);
	out.extend(s);
	out.resize(out.len().next_multiple_of(8), 0);
}

fn put_strings<'a>(out: &mut Vec<u8>, items: impl ExactSizeIterator<Item = &'a [u8]>) {
	put_u64(out, items.len() as u64);
	for item in items {
		put_string(out, item);
	}
}

/// What a hook reports while it builds.
pub enum HookEvent<'a> {
	/// Output of the builder, which is logged and shown.
	Output(&'a [u8]),
	/// A log line the hook sent as a result, which is only logged.
	LogLine(&'a [u8]),
}

/// A running hook process, `HookInstance`.
pub struct HookInstance {
	pid: libc::pid_t,
	to: Option<File>,
	/// Buffered writes to `to`.
	out: Vec<u8>,
	from: File,
	/// Bytes read from `from` and not consumed.
	pending: Vec<u8>,
	builder_out: File,
	activities: BTreeMap<u64, Activity>,
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
	let mut fds = [0; 2];
	// SAFETY: `fds` has room for two descriptors.
	if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
		return Err(io::Error::last_os_error());
	}
	// SAFETY: pipe2 returned two new descriptors.
	Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// `ExecutablePath::findPath`: a name without a slash is looked up in
/// `PATH`.
fn find_program(name: &str) -> Result<PathBuf> {
	if name.contains('/') {
		return Ok(PathBuf::from(name));
	}
	let path = std::env::var_os("PATH").unwrap_or_default();
	for dir in std::env::split_paths(&path) {
		let dir = if dir.as_os_str().is_empty() {
			PathBuf::from(".")
		} else {
			dir
		};
		let file = dir.join(name);
		let Ok(c_file) = CString::new(file.as_os_str().as_bytes()) else {
			continue;
		};
		// SAFETY: `c_file` is a valid NUL-terminated string.
		if file.is_file() && unsafe { libc::access(c_file.as_ptr(), libc::X_OK) } == 0 {
			return Ok(file);
		}
	}
	Err(BuildError::Other(format!(
		"Could not find executable '{name}'\n… while resolving the 'build-hook' setting'"
	)))
}

impl HookInstance {
	fn start(settings: &BuildSettings) -> Result<Self> {
		let table = settings.config_table().map_err(BuildError::Other)?;
		let command = table.get("build-hook").cloned().unwrap_or_default();
		let words: Vec<&str> = command.split_ascii_whitespace().collect();
		logger::log(
			VerbosityLevel::Debug,
			&format!("starting build hook '{}'", words.join(" ")),
		);
		let Some((program, args)) = words.split_first() else {
			return Err(BuildError::Other(
				"'build-hook' setting is empty".to_owned(),
			));
		};
		let program = find_program(program)?;
		let mut argv: Vec<OsString> = vec![program.file_name().unwrap_or_default().to_owned()];
		argv.extend(args.iter().map(OsString::from));
		argv.push(nix_compat::log::verbosity().to_string().into());
		let (from, from_write) = pipe()?;
		let (to_read, to) = pipe()?;
		let (builder_out, builder_write) = pipe()?;
		let pid = spawn(
			&program,
			&argv,
			to_read.as_raw_fd(),
			from_write.as_raw_fd(),
			builder_write.as_raw_fd(),
			builder_out.as_raw_fd(),
		)?;
		// The hook has its own copies of the far ends.
		drop((from_write, to_read, builder_write));
		let mut out = Vec::new();
		for (name, value) in &table {
			put_u64(&mut out, 1);
			put_string(&mut out, name.as_bytes());
			put_string(&mut out, value.as_bytes());
		}
		put_u64(&mut out, 0);
		Ok(Self {
			pid,
			to: Some(to.into()),
			out,
			from: from.into(),
			pending: Vec::new(),
			builder_out: builder_out.into(),
			activities: BTreeMap::new(),
		})
	}

	fn flush(&mut self) -> io::Result<()> {
		let out = std::mem::take(&mut self.out);
		match &mut self.to {
			Some(to) => to.write_all(&out),
			None => Ok(()),
		}
	}

	/// What the hook still says, until it closes its standard error.
	fn drain(&mut self) -> Vec<u8> {
		let mut text = std::mem::take(&mut self.pending);
		let _ = self.from.read_to_end(&mut text);
		text
	}

	/// `readLine` from the hook's standard error.
	fn read_line(&mut self, cancelled: &dyn Fn() -> bool) -> Result<Vec<u8>> {
		loop {
			if let Some(end) = self.pending.iter().position(|&c| c == b'\n') {
				let mut line: Vec<u8> = self.pending.drain(..=end).collect();
				line.pop();
				return Ok(line);
			}
			if cancelled() {
				return Err(BuildError::Cancelled);
			}
			if poll([self.from.as_raw_fd(), -1])?[0] {
				let mut chunk = [0; 4096];
				let n = self.from.read(&mut chunk)?;
				if n == 0 {
					return Err(BuildError::Other(
						"unexpected EOF reading a line".to_owned(),
					));
				}
				self.pending.extend(&chunk[..n]);
			}
		}
	}

	/// `handleJSONLogMessage` for a trusted source. `None` if the line is
	/// not a message; otherwise the message.
	fn handle_json(&mut self, line: &[u8], source: &str) -> Option<Value> {
		let text = line.strip_prefix(b"@nix ")?;
		let message: Value = match serde_json::from_slice(text) {
			Ok(message) => message,
			Err(error) => {
				logger::log(
					VerbosityLevel::Error,
					&format!("bad JSON log message from {source}: {error}"),
				);
				return None;
			}
		};
		if let Err(error) = self.handle_message(&message) {
			logger::warn(&format!(
				"Unable to handle a JSON message from {source}: {error}"
			));
			return None;
		}
		Some(message)
	}

	fn handle_message(&mut self, message: &Value) -> std::result::Result<(), String> {
		fn int(message: &Value, key: &str) -> std::result::Result<u64, String> {
			message[key]
				.as_u64()
				.ok_or_else(|| format!("'{key}' is not a number"))
		}
		fn string<'a>(message: &'a Value, key: &str) -> std::result::Result<&'a str, String> {
			message[key]
				.as_str()
				.ok_or_else(|| format!("'{key}' is not a string"))
		}
		fn fields(message: &Value) -> std::result::Result<Vec<LogField>, String> {
			message["fields"]
				.as_array()
				.into_iter()
				.flatten()
				.map(|field| match field {
					Value::String(s) => Ok(LogField::Str(s.clone())),
					field => field
						.as_u64()
						.map(LogField::Int)
						.ok_or_else(|| "unsupported JSON type".to_owned()),
				})
				.collect()
		}
		let level = |message: &Value| -> std::result::Result<VerbosityLevel, String> {
			Ok(VerbosityLevel::try_from(int(message, "level")?).unwrap_or(VerbosityLevel::Vomit))
		};
		match string(message, "action")? {
			"start" => {
				let kind = int(message, "type")?;
				let kind = u8::try_from(kind)
					.ok()
					.and_then(|kind| ActivityType::try_from(kind).ok())
					.unwrap_or(ActivityType::Unknown);
				let activity = Activity::new(
					level(message)?,
					kind,
					string(message, "text")?,
					fields(message)?,
					logger::worker_activity(),
				);
				self.activities.insert(int(message, "id")?, activity);
			}
			"stop" => {
				self.activities.remove(&int(message, "id")?);
			}
			"result" => {
				if let Some(activity) = self.activities.get(&int(message, "id")?)
					&& let Some(kind) = u8::try_from(int(message, "type")?)
						.ok()
						.and_then(|kind| ResultType::try_from(kind).ok())
				{
					activity.result(kind, &fields(message)?);
				}
			}
			"setPhase" => logger::result(
				logger::worker_activity(),
				ResultType::SetPhase,
				&[string(message, "phase")?.into()],
			),
			"msg" => logger::log(level(message)?, string(message, "msg")?),
			_ => {}
		}
		Ok(())
	}

	/// The machine the hook builds on.
	pub fn read_machine_name(&mut self, cancelled: &dyn Fn() -> bool) -> Result<String> {
		let line = self
			.read_line(cancelled)
			.map_err(|e| traced(e, "while reading the machine name from the build hook"))?;
		Ok(String::from_utf8_lossy(&line).into_owned())
	}

	/// Send the paths to copy to the machine and the outputs to copy back.
	pub fn send_paths(
		&mut self,
		input_paths: &BTreeSet<StorePath>,
		missing_outputs: &BTreeSet<String>,
	) -> Result<()> {
		let paths: Vec<String> = input_paths
			.iter()
			.map(StorePath::to_absolute_path)
			.collect();
		let sorted: BTreeSet<&[u8]> = paths.iter().map(String::as_bytes).collect();
		let mut out = Vec::new();
		put_strings(&mut out, sorted.into_iter());
		put_strings(&mut out, missing_outputs.iter().map(String::as_bytes));
		self.out.extend(out);
		self.flush()?;
		self.to = None;
		Ok(())
	}

	/// Relay what the hook reports until it closes one of its pipes, then
	/// end it and return how it exited.
	pub fn run(
		&mut self,
		cancelled: &dyn Fn() -> bool,
		events: &mut dyn FnMut(HookEvent<'_>) -> Result<()>,
	) -> Result<ExitStatus> {
		let mut line = std::mem::take(&mut self.pending);
		let mut eof = false;
		let mut chunk = [0; 4096];
		loop {
			while let Some(end) = line.iter().position(|&c| c == b'\n') {
				let rest = line.split_off(end + 1);
				line.pop();
				self.relay(&line, events)?;
				line = rest;
			}
			if eof {
				break;
			}
			if cancelled() {
				return Err(BuildError::Cancelled);
			}
			let ready = poll([self.builder_out.as_raw_fd(), self.from.as_raw_fd()])?;
			if ready[0] {
				let n = self.builder_out.read(&mut chunk)?;
				events(HookEvent::Output(&chunk[..n]))?;
				eof |= n == 0;
			}
			if ready[1] {
				let n = self.from.read(&mut chunk)?;
				line.extend(&chunk[..n]);
				eof |= n == 0;
			}
		}
		// Output that came with the end of the hook.
		while ready(self.builder_out.as_raw_fd())? {
			let n = self.builder_out.read(&mut chunk)?;
			if n == 0 {
				break;
			}
			events(HookEvent::Output(&chunk[..n]))?;
		}
		Ok(ExitStatus(self.kill()))
	}

	/// A line of the hook's standard error. Log lines of its activities,
	/// which `ssh-ng` and local machines send, also go to the build log, as
	/// do phase changes.
	fn relay(
		&mut self,
		line: &[u8],
		events: &mut dyn FnMut(HookEvent<'_>) -> Result<()>,
	) -> Result<()> {
		let Some(message) = self.handle_json(line, "the derivation builder") else {
			return Ok(());
		};
		let kind = message["type"].as_u64();
		let field = message["fields"].get(0);
		if kind == Some(u8::from(ResultType::BuildLogLine).into()) {
			let text = field.and_then(Value::as_str).unwrap_or_default();
			events(HookEvent::LogLine(format!("{text}\n").as_bytes()))?;
		} else if kind == Some(u8::from(ResultType::SetPhase).into())
			&& let Some(phase) = field.filter(|phase| !phase.is_null())
		{
			let text = serde_json::json!({"action": "setPhase", "phase": phase});
			events(HookEvent::LogLine(format!("@nix {text}\n").as_bytes()))?;
		}
		Ok(())
	}

	/// `Pid::kill`: a custom hook gets half a second to clean up.
	fn kill(&mut self) -> libc::c_int {
		let pid = std::mem::replace(&mut self.pid, -1);
		let mut status = 0;
		if pid == -1 {
			return status;
		}
		// SAFETY: the hook leads its own process group; signalling it and
		// reaping our own child.
		unsafe {
			libc::kill(-pid, libc::SIGTERM);
			let deadline = Instant::now() + Duration::from_millis(500);
			loop {
				match libc::waitpid(pid, &mut status, libc::WNOHANG) {
					0 if Instant::now() < deadline => {
						std::thread::sleep(Duration::from_millis(5));
					}
					0 => {
						libc::kill(-pid, libc::SIGKILL);
						while libc::waitpid(pid, &mut status, 0) == -1
							&& io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
						{
						}
						break;
					}
					-1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
					_ => break,
				}
			}
		}
		status
	}
}

impl Drop for HookInstance {
	fn drop(&mut self) {
		self.to = None;
		self.kill();
	}
}

/// Wait a short while for data or end of file on `fds`; -1 is ignored.
fn poll(fds: [RawFd; 2]) -> io::Result<[bool; 2]> {
	let mut pfds = fds.map(|fd| libc::pollfd {
		fd,
		events: libc::POLLIN,
		revents: 0,
	});
	// SAFETY: two valid pollfds.
	if unsafe { libc::poll(pfds.as_mut_ptr(), 2, 100) } == -1 {
		let error = io::Error::last_os_error();
		return match error.kind() {
			io::ErrorKind::Interrupted => Ok([false; 2]),
			_ => Err(error),
		};
	}
	Ok(pfds.map(|pfd| pfd.revents != 0))
}

/// Whether `fd` has data or is at its end.
fn ready(fd: RawFd) -> io::Result<bool> {
	let mut pfd = libc::pollfd {
		fd,
		events: libc::POLLIN,
		revents: 0,
	};
	// SAFETY: one valid pollfd.
	match unsafe { libc::poll(&mut pfd, 1, 0) } {
		-1 => Err(io::Error::last_os_error()),
		n => Ok(n > 0),
	}
}

/// Start the hook in its own session and process group, with its requests
/// on standard input, its replies on standard output and error, the
/// builder's output pipe as descriptor 4 and that pipe's read end as
/// descriptor 5.
fn spawn(
	program: &Path,
	argv: &[OsString],
	stdin: RawFd,
	stderr: RawFd,
	builder_write: RawFd,
	builder_read: RawFd,
) -> Result<libc::pid_t> {
	let c = |s: &std::ffi::OsStr| {
		CString::new(s.as_bytes())
			.map_err(|_| BuildError::Other("build hook argument contains a NUL byte".to_owned()))
	};
	let c_program = c(program.as_os_str())?;
	let c_args = argv
		.iter()
		.map(|arg| c(arg))
		.collect::<Result<Vec<CString>>>()?;
	let mut pointers: Vec<*const libc::c_char> = c_args.iter().map(|arg| arg.as_ptr()).collect();
	pointers.push(std::ptr::null());
	let error = format!("executing \"{}\"\n", program.display());
	// SAFETY: the child of a possibly threaded process makes only
	// async-signal-safe calls on memory prepared before the fork.
	unsafe {
		let pid = libc::fork();
		if pid == -1 {
			return Err(io::Error::last_os_error().into());
		}
		if pid > 0 {
			return Ok(pid);
		}
		let fail = || -> ! {
			libc::write(2, error.as_ptr().cast(), error.len());
			libc::_exit(1);
		};
		// Move the pipes out of the way of the descriptors to fill.
		let source = [stdin, stderr, stderr, builder_write, builder_read]
			.map(|fd| libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10));
		if source.contains(&-1) {
			fail();
		}
		for (fd, target) in source.into_iter().zip([0, 1, 2, 4, 5]) {
			if libc::dup2(fd, target) == -1 {
				fail();
			}
		}
		let mut set: libc::sigset_t = std::mem::zeroed();
		libc::sigemptyset(&mut set);
		libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
		libc::signal(libc::SIGPIPE, libc::SIG_DFL);
		if libc::setsid() == -1 || libc::chdir(c"/".as_ptr()) == -1 {
			fail();
		}
		libc::execv(c_program.as_ptr(), pointers.as_ptr());
		fail()
	}
}
