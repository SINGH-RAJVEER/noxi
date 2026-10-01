//! Running a builder in a Linux sandbox, following CppNix's
//! `ChrootLinuxDerivationBuilder`: private user, PID, mount, IPC, UTS and
//! (except for fixed-output derivations) network namespaces, a chroot with
//! only the input closure, and output read from a pseudoterminal.

use std::ffi::{CStr, CString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{BuildError, FailureStatus, Result};

/// The uid and gid the builder runs as inside its user namespace.
pub const SANDBOX_UID: u32 = 1000;
pub const SANDBOX_GID: u32 = 100;

/// One host path made visible in the chroot.
#[derive(Clone, Debug)]
pub struct Bind {
	/// Absolute path inside the sandbox.
	pub target: PathBuf,
	/// Path on the host.
	pub source: PathBuf,
	pub optional: bool,
}

/// Everything the sandboxed child needs, prepared before forking.
pub struct SandboxSpec {
	pub chroot_root: PathBuf,
	pub store_dir: PathBuf,
	pub binds: Vec<Bind>,
	/// Whether the build is cut off from the network (not a fixed-output
	/// derivation).
	pub sandboxed: bool,
	pub kvm: bool,
	pub shm_size: String,
	pub allow_new_privileges: bool,
	pub filter_syscalls: bool,
	pub build_dir_in_sandbox: PathBuf,
	pub builder: Vec<u8>,
	pub args: Vec<Vec<u8>>,
	pub env: Vec<Vec<u8>>,
	pub max_silent_time: u64,
	pub timeout: u64,
}

/// How the builder process ended.
pub struct ExitStatus(pub libc::c_int);

impl ExitStatus {
	pub fn success(&self) -> bool {
		libc::WIFEXITED(self.0) && libc::WEXITSTATUS(self.0) == 0
	}

	/// CppNix's `statusToString`.
	pub fn describe(&self) -> String {
		let status = self.0;
		if libc::WIFEXITED(status) {
			if libc::WEXITSTATUS(status) == 0 {
				"succeeded".to_owned()
			} else {
				format!("failed with exit code {}", libc::WEXITSTATUS(status))
			}
		} else if libc::WIFSIGNALED(status) {
			let sig = libc::WTERMSIG(status);
			// SAFETY: strsignal returns a static or thread-local string.
			let description = unsafe { CStr::from_ptr(libc::strsignal(sig)) };
			format!(
				"failed due to signal {sig} ({})",
				description.to_string_lossy()
			)
		} else {
			"died abnormally".to_owned()
		}
	}
}

fn cstring(bytes: &[u8]) -> io::Result<CString> {
	CString::new(bytes)
		.map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "string contains a NUL byte"))
}

fn cpath(path: &Path) -> io::Result<CString> {
	cstring(path.as_os_str().as_bytes())
}

fn check(ret: libc::c_int, what: impl FnOnce() -> String) -> io::Result<()> {
	if ret == -1 {
		let error = io::Error::last_os_error();
		return Err(io::Error::new(error.kind(), format!("{}: {error}", what())));
	}
	Ok(())
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
	let mut fds = [0; 2];
	// SAFETY: `fds` has room for two descriptors.
	check(
		unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) },
		|| "creating a pipe".to_owned(),
	)?;
	// SAFETY: pipe2 returned two new descriptors.
	Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Create the chroot skeleton: `root/tmp`, `root/etc`, and the store
/// directory, as CppNix's `setupBuildChroot` does.
pub fn setup_chroot(
	chroot_parent: &Path,
	store_dir: &Path,
	sandboxed: bool,
) -> io::Result<PathBuf> {
	nix_rs_store::delete_path(chroot_parent).map_err(io::Error::other)?;
	fs::DirBuilder::new().mode(0o700).create(chroot_parent)?;
	let root = chroot_parent.join("root");
	fs::DirBuilder::new().mode(0o750).create(&root)?;
	fs::set_permissions(&root, fs::Permissions::from_mode(0o750))?;
	let tmp = root.join("tmp");
	fs::create_dir_all(&tmp)?;
	fs::set_permissions(&tmp, fs::Permissions::from_mode(0o1777))?;
	fs::create_dir_all(root.join("etc"))?;
	if sandboxed {
		fs::write(
			root.join("etc/hosts"),
			"127.0.0.1 localhost\n::1 localhost\n",
		)?;
	}
	let chroot_store = root.join(store_dir.strip_prefix("/").unwrap_or(store_dir));
	fs::create_dir_all(&chroot_store)?;
	fs::set_permissions(&chroot_store, fs::Permissions::from_mode(0o1775))?;
	Ok(root)
}

/// Run the builder described by `spec`, passing its output to `log`.
/// Returns how it exited.
pub fn run(spec: &SandboxSpec, log: &mut dyn FnMut(&[u8])) -> Result<ExitStatus> {
	// The pseudoterminal that carries the builder's output.
	// SAFETY: posix_openpt has no memory-safety preconditions.
	let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
	check(master, || "opening pseudoterminal master".to_owned())?;
	// SAFETY: posix_openpt returned a new descriptor.
	let master = unsafe { OwnedFd::from_raw_fd(master) };
	// SAFETY: `master` is a pseudoterminal master.
	check(unsafe { libc::unlockpt(master.as_raw_fd()) }, || {
		"unlocking pseudoterminal".to_owned()
	})?;
	let mut name = [0 as libc::c_char; 128];
	// SAFETY: `name` has the length passed.
	if unsafe { libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) } != 0 {
		return Err(io::Error::last_os_error().into());
	}
	// SAFETY: ptsname_r wrote a NUL-terminated string.
	let slave_name = unsafe { CStr::from_ptr(name.as_ptr()) }.to_owned();

	let (sync_read, sync_write) = pipe()?;
	let (pid_read, pid_write) = pipe()?;
	let child_plan = ChildPlan::new(spec)?;

	// SAFETY: the helper only runs code that does not rely on other threads.
	let helper = unsafe { libc::fork() };
	check(helper, || "forking the sandbox helper".to_owned())?;
	if helper == 0 {
		drop(pid_read);
		drop(sync_write);
		let code = helper_main(
			&slave_name,
			spec,
			sync_read.as_raw_fd(),
			pid_write.as_raw_fd(),
			&child_plan,
		);
		// SAFETY: exiting the forked helper without running destructors.
		unsafe { libc::_exit(code) };
	}
	drop(pid_write);
	drop(sync_read);

	let helper_status = wait_pid(helper)?;
	let mut pid_text = String::new();
	File::from(pid_read).read_to_string(&mut pid_text)?;
	let pid: libc::pid_t = match pid_text.trim().parse() {
		Ok(pid) if libc::WIFEXITED(helper_status) && libc::WEXITSTATUS(helper_status) == 0 => pid,
		_ => {
			let mut output = Vec::new();
			let _ = read_available(&master, &mut output);
			return Err(BuildError::Other(format!(
				"unable to start build process: {}{}",
				ExitStatus(helper_status).describe(),
				setup_error(&output)
			)));
		}
	};

	let builder = Builder(Some(pid));
	let mut sync_write = File::from(sync_write);
	let setup = (|| -> io::Result<()> {
		let proc = PathBuf::from(format!("/proc/{pid}"));
		// SAFETY: getuid and getgid cannot fail.
		let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
		fs::write(proc.join("uid_map"), format!("{SANDBOX_UID} {uid} 1"))?;
		fs::write(proc.join("setgroups"), "deny")?;
		fs::write(proc.join("gid_map"), format!("{SANDBOX_GID} {gid} 1"))?;
		fs::write(
			spec.chroot_root.join("etc/passwd"),
			format!(
				"root:x:0:0:Nix build user:{0}:/noshell\nnixbld:x:{SANDBOX_UID}:{SANDBOX_GID}:Nix build user:{0}:/noshell\nnobody:x:65534:65534:Nobody:/:/noshell\n",
				spec.build_dir_in_sandbox.display()
			),
		)?;
		fs::write(
			spec.chroot_root.join("etc/group"),
			format!("root:x:0:\nnixbld:!:{SANDBOX_GID}:\nnogroup:x:65534:\n"),
		)?;
		Ok(())
	})();
	let ok = setup.is_ok();
	let _ = sync_write.write_all(if ok { b"1\n" } else { b"0\n" });
	drop(sync_write);
	if let Err(e) = setup {
		return Err(BuildError::Other(format!(
			"setting up the build user namespace: {e}"
		)));
	}

	// Read setup messages until the child reports it is ready.
	let mut reader = PtyReader::new(master);
	loop {
		match reader.read_line()? {
			Some(line) if line.first() == Some(&2) => break,
			Some(line) if line.first() == Some(&1) => {
				let rest = reader.read_rest()?;
				let status = builder.wait()?;
				return Err(BuildError::Other(format!(
					"while setting up the build environment: {}{}",
					String::from_utf8_lossy(&rest).trim_end(),
					if libc::WIFEXITED(status) {
						String::new()
					} else {
						format!(" ({})", ExitStatus(status).describe())
					}
				)));
			}
			Some(line) => tracing::debug!("sandbox setup: {}", String::from_utf8_lossy(&line)),
			None => {
				let status = builder.wait()?;
				return Err(BuildError::Other(format!(
					"while waiting for the build environment to initialize ({})",
					ExitStatus(status).describe()
				)));
			}
		}
	}

	// Forward the builder's output until it closes the terminal.
	let start = Instant::now();
	let mut last_output = Instant::now();
	let mut timed_out = None;
	loop {
		let wait = poll_timeout(spec, start, last_output);
		match reader.poll(wait)? {
			Poll::Data(data) => {
				last_output = Instant::now();
				log(&data);
			}
			Poll::Eof => break,
			Poll::Timeout => {
				if spec.timeout != 0 && start.elapsed() >= Duration::from_secs(spec.timeout) {
					timed_out = Some(format!("timed out after {} seconds", spec.timeout));
				} else {
					timed_out = Some(format!(
						"timed out after {} seconds of silence",
						spec.max_silent_time
					));
				}
				break;
			}
		}
	}
	if let Some(message) = timed_out {
		drop(builder);
		return Err(BuildError::failed(FailureStatus::TimedOut, message));
	}
	// The builder may have closed the terminal before exiting.
	let status = builder.wait()?;
	Ok(ExitStatus(status))
}

/// The started builder. Dropping it before [`Builder::wait`] kills and
/// reaps it, so no error return leaves it running.
struct Builder(Option<libc::pid_t>);

impl Builder {
	fn wait(mut self) -> io::Result<libc::c_int> {
		wait_pid(self.0.take().expect("not yet waited for"))
	}
}

impl Drop for Builder {
	fn drop(&mut self) {
		if let Some(pid) = self.0.take() {
			kill_and_wait(pid);
		}
	}
}

fn poll_timeout(spec: &SandboxSpec, start: Instant, last_output: Instant) -> Option<Duration> {
	let mut timeout: Option<Duration> = None;
	if spec.max_silent_time != 0 {
		timeout =
			Some(Duration::from_secs(spec.max_silent_time).saturating_sub(last_output.elapsed()));
	}
	if spec.timeout != 0 {
		let left = Duration::from_secs(spec.timeout).saturating_sub(start.elapsed());
		timeout = Some(timeout.map_or(left, |t| t.min(left)));
	}
	timeout
}

fn setup_error(output: &[u8]) -> String {
	match output.iter().position(|&b| b == 1) {
		Some(pos) => format!(": {}", String::from_utf8_lossy(&output[pos + 1..]).trim()),
		None => String::new(),
	}
}

fn read_available(fd: &OwnedFd, out: &mut Vec<u8>) -> io::Result<()> {
	let mut reader = PtyReader::new(fd.try_clone()?);
	while let Poll::Data(data) = reader.poll(Some(Duration::from_millis(10)))? {
		out.extend_from_slice(&data);
	}
	Ok(())
}

fn kill_and_wait(pid: libc::pid_t) {
	// SAFETY: signalling and reaping our own child.
	unsafe {
		libc::kill(pid, libc::SIGKILL);
	}
	let _ = wait_pid(pid);
}

fn wait_pid(pid: libc::pid_t) -> io::Result<libc::c_int> {
	let mut status = 0;
	loop {
		// SAFETY: `status` is writable.
		let ret = unsafe { libc::waitpid(pid, &mut status, libc::__WALL) };
		if ret == pid {
			return Ok(status);
		}
		let error = io::Error::last_os_error();
		if error.raw_os_error() != Some(libc::EINTR) {
			return Err(error);
		}
	}
}

enum Poll {
	Data(Vec<u8>),
	Eof,
	Timeout,
}

struct PtyReader {
	fd: OwnedFd,
	pending: Vec<u8>,
}

impl PtyReader {
	fn new(fd: OwnedFd) -> Self {
		Self {
			fd,
			pending: Vec::new(),
		}
	}

	fn poll(&mut self, timeout: Option<Duration>) -> io::Result<Poll> {
		if !self.pending.is_empty() {
			return Ok(Poll::Data(std::mem::take(&mut self.pending)));
		}
		let mut pfd = libc::pollfd {
			fd: self.fd.as_raw_fd(),
			events: libc::POLLIN,
			revents: 0,
		};
		let ms = timeout
			.map(|t| t.as_millis().min(i32::MAX as u128) as i32)
			.unwrap_or(-1);
		loop {
			// SAFETY: one valid pollfd.
			let ret = unsafe { libc::poll(&mut pfd, 1, ms) };
			if ret == 0 {
				return Ok(Poll::Timeout);
			}
			if ret > 0 {
				break;
			}
			let error = io::Error::last_os_error();
			if error.raw_os_error() != Some(libc::EINTR) {
				return Err(error);
			}
		}
		let mut buf = vec![0u8; 65536];
		loop {
			// SAFETY: `buf` has `buf.len()` writable bytes.
			let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
			if n > 0 {
				buf.truncate(n as usize);
				return Ok(Poll::Data(buf));
			}
			if n == 0 {
				return Ok(Poll::Eof);
			}
			let error = io::Error::last_os_error();
			match error.raw_os_error() {
				Some(libc::EINTR) => continue,
				// Linux reports EIO once every slave descriptor is closed.
				Some(libc::EIO) => return Ok(Poll::Eof),
				_ => return Err(error),
			}
		}
	}

	fn read_line(&mut self) -> io::Result<Option<Vec<u8>>> {
		loop {
			if let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
				let line: Vec<u8> = self.pending.drain(..=pos).collect();
				return Ok(Some(line[..line.len() - 1].to_vec()));
			}
			let mut pending = std::mem::take(&mut self.pending);
			match self.poll(None)? {
				Poll::Data(data) => {
					pending.extend_from_slice(&data);
					self.pending = pending;
				}
				Poll::Eof | Poll::Timeout => {
					return Ok(if pending.is_empty() {
						None
					} else {
						Some(pending)
					});
				}
			}
		}
	}

	fn read_rest(&mut self) -> io::Result<Vec<u8>> {
		let mut out = std::mem::take(&mut self.pending);
		while let Poll::Data(data) = self.poll(Some(Duration::from_secs(5)))? {
			out.extend_from_slice(&data);
		}
		Ok(out)
	}
}

/// Strings for the child, converted before forking.
struct ChildPlan {
	root: CString,
	store_in_root: CString,
	build_dir: CString,
	builder: CString,
	argv: Vec<CString>,
	envp: Vec<CString>,
}

impl ChildPlan {
	fn new(spec: &SandboxSpec) -> io::Result<Self> {
		let store_rel = spec.store_dir.strip_prefix("/").unwrap_or(&spec.store_dir);
		let base_name = spec
			.builder
			.rsplit(|&b| b == b'/')
			.next()
			.unwrap_or(&spec.builder)
			.to_vec();
		let mut argv = vec![cstring(&base_name)?];
		for arg in &spec.args {
			argv.push(cstring(arg)?);
		}
		Ok(Self {
			root: cpath(&spec.chroot_root)?,
			store_in_root: cpath(&spec.chroot_root.join(store_rel))?,
			build_dir: cpath(&spec.build_dir_in_sandbox)?,
			builder: cstring(&spec.builder)?,
			argv,
			envp: spec
				.env
				.iter()
				.map(|e| cstring(e))
				.collect::<io::Result<_>>()?,
		})
	}
}

/// The helper process: take the terminal as stderr, then create the
/// builder in new namespaces as a sibling (CLONE_PARENT).
fn helper_main(
	slave: &CStr,
	spec: &SandboxSpec,
	sync_read: RawFd,
	pid_write: RawFd,
	plan: &ChildPlan,
) -> i32 {
	let result = (|| -> io::Result<libc::pid_t> {
		open_slave(slave)?;
		// SAFETY: dropping supplementary groups; EPERM is tolerated.
		if unsafe { libc::setgroups(0, std::ptr::null()) } == -1
			&& io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
		{
			return Err(io::Error::last_os_error());
		}
		let mut flags = libc::CLONE_NEWPID
			| libc::CLONE_NEWNS
			| libc::CLONE_NEWIPC
			| libc::CLONE_NEWUTS
			| libc::CLONE_PARENT
			| libc::CLONE_NEWUSER
			| libc::SIGCHLD;
		if spec.sandboxed {
			flags |= libc::CLONE_NEWNET;
		}
		// SAFETY: a raw clone without a new stack behaves like fork; the
		// child only runs `child_main` and then exits or execs.
		let pid = unsafe {
			libc::syscall(
				libc::SYS_clone,
				flags as libc::c_ulong,
				0usize,
				0usize,
				0usize,
				0usize,
			)
		};
		if pid == -1 {
			return Err(io::Error::last_os_error());
		}
		if pid == 0 {
			// CLOEXEC keeps this open until exec, and the parent reads the
			// PID pipe to EOF before letting the builder exec.
			// SAFETY: the builder never uses the helper's end of the pipe.
			unsafe { libc::close(pid_write) };
			let code = child_main(sync_read, spec, plan);
			// SAFETY: the builder failed before exec.
			unsafe { libc::_exit(code) };
		}
		Ok(pid as libc::pid_t)
	})();
	match result {
		Ok(pid) => {
			let text = format!("{pid}\n");
			// SAFETY: writing to our pipe.
			unsafe { libc::write(pid_write, text.as_ptr().cast(), text.len()) };
			0
		}
		Err(e) => {
			report_child_error(&e.to_string());
			1
		}
	}
}

fn open_slave(slave: &CStr) -> io::Result<()> {
	// SAFETY: `slave` is NUL-terminated.
	let fd = unsafe {
		libc::open(
			slave.as_ptr(),
			libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
		)
	};
	check(fd, || "opening pseudoterminal slave".to_owned())?;
	// SAFETY: `term` is filled by tcgetattr.
	let mut term: libc::termios = unsafe { std::mem::zeroed() };
	// SAFETY: `fd` is a terminal and `term` is writable.
	check(unsafe { libc::tcgetattr(fd, &mut term) }, || {
		"getting pseudoterminal attributes".to_owned()
	})?;
	// SAFETY: `term` is a valid termios.
	unsafe { libc::cfmakeraw(&mut term) };
	// SAFETY: `fd` is a terminal.
	check(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) }, || {
		"putting pseudoterminal into raw mode".to_owned()
	})?;
	// SAFETY: both descriptors are open.
	check(unsafe { libc::dup2(fd, libc::STDERR_FILENO) }, || {
		"cannot pipe standard error into log file".to_owned()
	})?;
	// SAFETY: `fd` is ours and stderr keeps a duplicate.
	unsafe { libc::close(fd) };
	Ok(())
}

/// Send a setup failure to the parent: `\1`, then the message.
fn report_child_error(message: &str) {
	let text = format!("\x01\n{message}\n");
	// SAFETY: writing to stderr.
	unsafe { libc::write(libc::STDERR_FILENO, text.as_ptr().cast(), text.len()) };
}

fn child_main(sync_read: RawFd, spec: &SandboxSpec, plan: &ChildPlan) -> i32 {
	match child_setup(sync_read, spec, plan) {
		Ok(()) => {
			// SAFETY: the argument and environment arrays are NUL-terminated
			// pointer lists that outlive the call.
			unsafe {
				let mut argv: Vec<*const libc::c_char> =
					plan.argv.iter().map(|a| a.as_ptr()).collect();
				argv.push(std::ptr::null());
				let mut envp: Vec<*const libc::c_char> =
					plan.envp.iter().map(|e| e.as_ptr()).collect();
				envp.push(std::ptr::null());
				libc::execve(plan.builder.as_ptr(), argv.as_ptr(), envp.as_ptr());
			}
			let error = io::Error::last_os_error();
			let text = format!("executing '{}': {error}\n", plan.builder.to_string_lossy());
			// SAFETY: writing to stderr.
			unsafe { libc::write(libc::STDERR_FILENO, text.as_ptr().cast(), text.len()) };
			1
		}
		Err(e) => {
			report_child_error(&e.to_string());
			1
		}
	}
}

fn child_setup(sync_read: RawFd, spec: &SandboxSpec, plan: &ChildPlan) -> io::Result<()> {
	// commonChildInit: a new session, stdout to the terminal, stdin from
	// /dev/null.
	// SAFETY: plain process-state syscalls.
	unsafe {
		check(libc::setsid(), || "creating a new session".to_owned())?;
		check(libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO), || {
			"cannot dup stderr into stdout".to_owned()
		})?;
		let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
		check(null, || "cannot open '/dev/null'".to_owned())?;
		check(libc::dup2(null, libc::STDIN_FILENO), || {
			"cannot dup null device into stdin".to_owned()
		})?;
		libc::close(null);
	}

	// Wait for the parent to write our uid and gid maps.
	let mut buf = [0u8; 2];
	// SAFETY: `buf` is writable.
	let n = unsafe { libc::read(sync_read, buf.as_mut_ptr().cast(), 2) };
	if n < 1 || buf[0] != b'1' {
		return Err(io::Error::other("user namespace initialisation failed"));
	}
	// SAFETY: closing our copy of the pipe.
	unsafe { libc::close(sync_read) };

	enter_chroot(spec, plan)?;

	// SAFETY: plain process-state syscalls.
	unsafe {
		check(libc::chdir(plan.build_dir.as_ptr()), || {
			format!("changing into '{}'", plan.build_dir.to_string_lossy())
		})?;
		close_extra_fds();
		let limit = libc::rlimit {
			rlim_cur: 0,
			rlim_max: libc::RLIM_INFINITY,
		};
		libc::setrlimit(libc::RLIMIT_CORE, &limit);
		libc::umask(0o022);
		check(libc::setgid(SANDBOX_GID), || "setgid failed".to_owned())?;
		check(libc::setuid(SANDBOX_UID), || "setuid failed".to_owned())?;
		libc::write(libc::STDERR_FILENO, b"\x02\n".as_ptr().cast(), 2);
	}
	Ok(())
}

fn close_extra_fds() {
	// SAFETY: closing every descriptor above stderr.
	unsafe {
		if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) == 0 {
			return;
		}
		for fd in 3..1024 {
			libc::close(fd);
		}
	}
}

fn mount(
	source: Option<&CStr>,
	target: &CStr,
	fstype: Option<&CStr>,
	flags: libc::c_ulong,
	data: Option<&CStr>,
) -> io::Result<()> {
	// SAFETY: all pointers are NUL-terminated strings or null.
	let ret = unsafe {
		libc::mount(
			source.map_or(std::ptr::null(), CStr::as_ptr),
			target.as_ptr(),
			fstype.map_or(std::ptr::null(), CStr::as_ptr),
			flags,
			data.map_or(std::ptr::null(), |d| d.as_ptr().cast()),
		)
	};
	check(ret, || format!("mounting '{}'", target.to_string_lossy()))
}

fn enter_chroot(spec: &SandboxSpec, plan: &ChildPlan) -> io::Result<()> {
	if spec.sandboxed {
		bring_up_loopback()?;
	}
	// SAFETY: fixed strings with their lengths.
	unsafe {
		check(libc::sethostname(c"localhost".as_ptr(), 10), || {
			"cannot set host name".to_owned()
		})?;
		check(libc::setdomainname(c"(none)".as_ptr(), 7), || {
			"cannot set domain name".to_owned()
		})?;
	}

	mount(None, c"/", None, libc::MS_PRIVATE | libc::MS_REC, None)
		.map_err(|e| io::Error::other(format!("unable to make '/' private: {e}")))?;
	mount(Some(&plan.root), &plan.root, None, libc::MS_BIND, None)?;
	mount(
		Some(&plan.store_in_root),
		&plan.store_in_root,
		None,
		libc::MS_BIND,
		None,
	)?;
	mount(None, &plan.store_in_root, None, libc::MS_SHARED, None)?;

	let root = &spec.chroot_root;
	let mut binds = spec.binds.clone();
	if !binds.iter().any(|b| b.target == Path::new("/dev")) {
		fs::create_dir_all(root.join("dev/shm"))?;
		fs::create_dir_all(root.join("dev/pts"))?;
		let mut devices = vec!["/dev/full"];
		if spec.kvm && Path::new("/dev/kvm").exists() {
			devices.push("/dev/kvm");
		}
		devices.extend([
			"/dev/null",
			"/dev/random",
			"/dev/tty",
			"/dev/urandom",
			"/dev/zero",
		]);
		for device in devices {
			binds.push(Bind {
				target: PathBuf::from(device),
				source: fs::canonicalize(device).unwrap_or_else(|_| PathBuf::from(device)),
				optional: false,
			});
		}
		std::os::unix::fs::symlink("/proc/self/fd", root.join("dev/fd"))?;
		std::os::unix::fs::symlink("/proc/self/fd/0", root.join("dev/stdin"))?;
		std::os::unix::fs::symlink("/proc/self/fd/1", root.join("dev/stdout"))?;
		std::os::unix::fs::symlink("/proc/self/fd/2", root.join("dev/stderr"))?;
	}

	if !spec.sandboxed {
		fs::write(
			root.join("etc/nsswitch.conf"),
			"hosts: files dns\nservices: files\n",
		)?;
		for path in ["/etc/resolv.conf", "/etc/services", "/etc/hosts"] {
			if Path::new(path).exists() {
				binds.push(Bind {
					target: PathBuf::from(path),
					source: fs::canonicalize(path)?,
					optional: false,
				});
			}
		}
	}

	for bind in &binds {
		if bind.source == Path::new("/proc") {
			continue;
		}
		do_bind(root, bind)?;
	}

	let proc = cpath(&root.join("proc"))?;
	fs::create_dir_all(root.join("proc"))?;
	mount(Some(c"none"), &proc, Some(c"proc"), 0, None)?;

	if Path::new("/dev/shm").exists() {
		let shm = cpath(&root.join("dev/shm"))?;
		let data = cstring(format!("size={}", spec.shm_size).as_bytes())?;
		mount(Some(c"none"), &shm, Some(c"tmpfs"), 0, Some(&data))?;
	}

	if Path::new("/dev/pts/ptmx").exists() && !root.join("dev/ptmx").exists() {
		let pts = cpath(&root.join("dev/pts"))?;
		match mount(
			Some(c"none"),
			&pts,
			Some(c"devpts"),
			0,
			Some(c"newinstance,mode=0620"),
		) {
			Ok(()) => {
				std::os::unix::fs::symlink("/dev/pts/ptmx", root.join("dev/ptmx"))?;
				let _ = fs::set_permissions(
					root.join("dev/pts/ptmx"),
					fs::Permissions::from_mode(0o666),
				);
			}
			Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
				do_bind(
					root,
					&Bind {
						target: "/dev/pts".into(),
						source: "/dev/pts".into(),
						optional: false,
					},
				)?;
				do_bind(
					root,
					&Bind {
						target: "/dev/ptmx".into(),
						source: "/dev/ptmx".into(),
						optional: false,
					},
				)?;
			}
			Err(e) => return Err(e),
		}
	}

	fs::set_permissions(root.join("etc"), fs::Permissions::from_mode(0o555))?;

	// SAFETY: namespace and root changes for this process only.
	unsafe {
		check(libc::unshare(libc::CLONE_NEWNS), || {
			"unsharing mount namespace".to_owned()
		})?;
		check(libc::chdir(plan.root.as_ptr()), || {
			"cannot change directory to the chroot".to_owned()
		})?;
		check(libc::mkdir(c"real-root".as_ptr(), 0o500), || {
			"cannot create real-root directory".to_owned()
		})?;
		check(
			libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c"real-root".as_ptr())
				as libc::c_int,
			|| "cannot pivot old root directory onto the chroot".to_owned(),
		)?;
		check(libc::chroot(c".".as_ptr()), || {
			"cannot change root directory".to_owned()
		})?;
		check(
			libc::umount2(c"real-root".as_ptr(), libc::MNT_DETACH),
			|| "cannot unmount real root filesystem".to_owned(),
		)?;
		check(libc::rmdir(c"real-root".as_ptr()), || {
			"cannot remove real-root directory".to_owned()
		})?;
		if !spec.allow_new_privileges {
			check(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), || {
				"failed to set PR_SET_NO_NEW_PRIVS".to_owned()
			})?;
		}
	}
	if spec.filter_syscalls {
		crate::seccomp::install()?;
	}
	Ok(())
}

fn bring_up_loopback() -> io::Result<()> {
	// SAFETY: a socket used only for the interface ioctl.
	unsafe {
		let fd = libc::socket(libc::PF_INET, libc::SOCK_DGRAM, libc::IPPROTO_IP);
		check(fd, || "cannot open IP socket".to_owned())?;
		let mut ifr: libc::ifreq = std::mem::zeroed();
		for (i, b) in b"lo".iter().enumerate() {
			ifr.ifr_name[i] = *b as libc::c_char;
		}
		ifr.ifr_ifru.ifru_flags =
			(libc::IFF_UP | libc::IFF_LOOPBACK | libc::IFF_RUNNING) as libc::c_short;
		let ret = libc::ioctl(fd, libc::SIOCSIFFLAGS, &ifr);
		libc::close(fd);
		check(ret, || "cannot set loopback interface flags".to_owned())
	}
}

/// Make `bind.source` appear at `bind.target` in the chroot: bind-mount
/// directories and files, and copy symlinks.
fn do_bind(root: &Path, bind: &Bind) -> io::Result<()> {
	let rel = bind.target.strip_prefix("/").unwrap_or(&bind.target);
	if rel.as_os_str().is_empty()
		|| rel
			.components()
			.any(|c| matches!(c, std::path::Component::ParentDir))
	{
		return Err(io::Error::other(format!(
			"invalid path to bind mount in the chroot: '{}'",
			bind.target.display()
		)));
	}
	let target = root.join(rel);
	let metadata = match fs::symlink_metadata(&bind.source) {
		Ok(m) => m,
		Err(e)
			if bind.optional
				&& matches!(
					e.kind(),
					io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
				) =>
		{
			return Ok(());
		}
		Err(e) => {
			return Err(io::Error::new(
				e.kind(),
				format!(
					"getting attributes of path '{}': {e}",
					bind.source.display()
				),
			));
		}
	};
	if let Some(parent) = target.parent() {
		fs::create_dir_all(parent)?;
	}
	let source = cpath(&bind.source)?;
	let ctarget = cpath(&target)?;
	if metadata.is_dir() {
		fs::create_dir_all(&target)?;
		mount(
			Some(&source),
			&ctarget,
			Some(c""),
			libc::MS_BIND | libc::MS_REC,
			None,
		)
		.map_err(|e| {
			io::Error::other(format!(
				"bind mount from '{}' to '{}' failed: {e}",
				bind.source.display(),
				target.display()
			))
		})
	} else if metadata.file_type().is_symlink() {
		std::os::unix::fs::symlink(fs::read_link(&bind.source)?, &target)
	} else {
		File::options().write(true).create_new(true).open(&target)?;
		mount(
			Some(&source),
			&ctarget,
			Some(c""),
			libc::MS_BIND | libc::MS_REC,
			None,
		)
		.map_err(|e| {
			io::Error::other(format!(
				"bind mount from '{}' to '{}' failed: {e}",
				bind.source.display(),
				target.display()
			))
		})
	}
}
