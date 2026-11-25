//! Connections over `ssh`, CppNix 2.35.2's `SSHMaster` (`ssh.cc`).
//!
//! `ssh` is executed directly with an argument vector, so neither the host
//! nor the store's parameters pass through a local shell. A user or host
//! that `ssh` would read as an option is refused.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use nix_compat::nixcpp::url::{Authority, Query};
use noxi_store::{Error, Result};

/// The parameters of `CommonSSHStoreConfig`.
#[derive(Clone, Debug)]
pub struct SshConfig {
	pub authority: Authority,
	/// `ssh-key`.
	pub key: Option<PathBuf>,
	/// `base64-ssh-public-host-key`.
	pub public_host_key: String,
	/// `compress`.
	pub compress: bool,
	/// `max-connections`.
	pub max_connections: usize,
	/// `log-fd`: where `ssh` writes its standard error.
	pub log_fd: Option<RawFd>,
	/// `remote-program`.
	pub remote_program: Vec<String>,
}

impl SshConfig {
	/// The configuration of `ssh://AUTHORITY` or `ssh-ng://AUTHORITY`.
	pub fn new(authority: &str, params: &Query, default_program: &str) -> Result<Self> {
		let authority = Authority::parse(authority).map_err(|e| Error::Other(e.to_string()))?;
		check_authority(&authority)?;
		let get = |name: &str| params.get(name).filter(|value| !value.is_empty());
		Ok(Self {
			authority,
			key: get("ssh-key").map(PathBuf::from),
			public_host_key: get("base64-ssh-public-host-key")
				.cloned()
				.unwrap_or_default(),
			compress: match get("compress").map(String::as_str) {
				None | Some("false" | "no" | "0") => false,
				Some("true" | "yes" | "1") => true,
				Some(value) => {
					return Err(Error::Other(format!(
						"Boolean setting 'compress' has invalid value '{value}'"
					)));
				}
			},
			max_connections: integer(params, "max-connections")?.map_or(1, |n| n.max(1) as usize),
			log_fd: integer(params, "log-fd")?
				.filter(|fd| *fd >= 0)
				.map(|fd| fd as RawFd),
			remote_program: match get("remote-program") {
				Some(program) => program
					.split_ascii_whitespace()
					.map(str::to_owned)
					.collect(),
				None => vec![default_program.to_owned()],
			},
		})
	}
}

fn integer(params: &Query, name: &str) -> Result<Option<i32>> {
	params
		.get(name)
		.map(|value| {
			value
				.parse()
				.map_err(|_| Error::Other(format!("setting '{name}' has invalid value '{value}'")))
		})
		.transpose()
}

/// `checkValidAuthority`.
fn check_authority(authority: &Authority) -> Result<()> {
	let invalid = |reason: String| {
		Err(Error::Other(format!(
			"invalid SSH authority: '{authority}': {reason}"
		)))
	};
	if let Some(user) = &authority.user {
		if user.is_empty() {
			return invalid("user name must not be empty".to_owned());
		}
		if user.starts_with('-') {
			return invalid(format!("user name '{user}' must not start with '-'"));
		}
	}
	let host = &authority.host;
	if host.is_empty() {
		return invalid("host name must not be empty".to_owned());
	}
	if host.starts_with('-') {
		return invalid(format!("host name '{host}' must not start with '-'"));
	}
	Ok(())
}

/// `shellSplitString`: words as a shell splits them, with nothing expanded.
pub fn shell_split(s: &str) -> std::result::Result<Vec<String>, String> {
	let mut words = Vec::new();
	let mut word: Option<String> = None;
	let mut chars = s.chars();
	let mut escaping = false;
	while let Some(c) = chars.next() {
		match c {
			c if escaping => {
				word.get_or_insert_default().push(c);
				escaping = false;
			}
			'\\' => escaping = true,
			' ' | '\t' => words.extend(word.take()),
			'"' => {
				let word = word.get_or_insert_default();
				let mut escaping = false;
				loop {
					let c = chars.next().ok_or("unterminated double quote")?;
					match c {
						'$' | '`' | '"' | '\\' if escaping => {
							word.push(c);
							escaping = false;
						}
						c if escaping => {
							word.push('\\');
							word.push(c);
							escaping = false;
						}
						'\\' => escaping = true,
						'"' => break,
						c => word.push(c),
					}
				}
			}
			'\'' => {
				let word = word.get_or_insert_default();
				loop {
					match chars.next().ok_or("unterminated single quote")? {
						'\'' => break,
						c => word.push(c),
					}
				}
			}
			c => word.get_or_insert_default().push(c),
		}
	}
	words.extend(word);
	Ok(words)
}

/// A running `ssh`, or the remote program itself for `localhost`. It ends
/// when its connection does.
#[derive(Debug)]
pub struct Process(Child);

impl Drop for Process {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

/// Connections to one host.
pub struct SshMaster {
	config: SshConfig,
	/// `ssh` is not used for `localhost`, as in CppNix.
	fake: bool,
	/// The decoded host key.
	host_key: Vec<u8>,
	/// Holds the host key and the master's socket.
	dir: tempfile::TempDir,
	/// The master `ssh`, once started.
	master: Mutex<Option<Process>>,
}

fn io_error(what: String) -> impl FnOnce(io::Error) -> Error {
	move |e| Error::Other(format!("{what}: {e}"))
}

impl SshMaster {
	pub fn new(config: SshConfig) -> Result<Self> {
		let host_key = decode_base64(&config.public_host_key).map_err(|e| {
			Error::Other(format!(
				"{e}\n… while decoding ssh public host key for host '{}'",
				config.authority.host
			))
		})?;
		Ok(Self {
			fake: config.authority.to_string() == "localhost",
			host_key,
			dir: tempfile::Builder::new().prefix("nix-").tempdir()?,
			master: Mutex::default(),
			config,
		})
	}

	pub fn config(&self) -> &SshConfig {
		&self.config
	}

	/// More than one connection shares a master.
	fn use_master(&self) -> bool {
		self.config.max_connections > 1 && !self.fake
	}

	/// `USER@HOST`, which `checkValidAuthority` keeps from being an option.
	fn destination(&self) -> OsString {
		let authority = &self.config.authority;
		match &authority.user {
			Some(user) => format!("{user}@{}", authority.host).into(),
			None => authority.host.clone().into(),
		}
	}

	/// `addCommonSSHOpts`.
	fn common_options(&self, args: &mut Vec<OsString>) -> Result<()> {
		let options = std::env::var("NIX_SSHOPTS").unwrap_or_default();
		let words = shell_split(&options)
			.map_err(|e| Error::Other(format!("{e}\n… while splitting NIX_SSHOPTS '{options}'")))?;
		args.extend(words.into_iter().map(OsString::from));
		if let Some(key) = &self.config.key {
			args.push("-i".into());
			args.push(key.into());
		}
		if !self.host_key.is_empty() {
			let file = self.dir.path().join("host-key");
			let mut line = format!("{} ", self.config.authority.host).into_bytes();
			line.extend(&self.host_key);
			line.push(b'\n');
			std::fs::write(&file, line)?;
			let mut option = OsString::from("-oUserKnownHostsFile=");
			option.push(file);
			args.push(option);
		}
		if self.config.compress {
			args.push("-C".into());
		}
		if let Some(port) = self.config.authority.port {
			args.push(format!("-p{port}").into());
		}
		args.push("-oPermitLocalCommand=yes".into());
		args.push("-oLocalCommand=echo started".into());
		Ok(())
	}

	fn ssh(&self) -> Command {
		let mut command = Command::new("ssh");
		command.env("SHELL", "/bin/sh");
		command
	}

	fn master_is_running(&self) -> Result<bool> {
		let mut args = vec!["-O".into(), "check".into(), self.destination()];
		self.common_options(&mut args)?;
		let output = self
			.ssh()
			.args(args)
			.stdin(Stdio::null())
			.output()
			.map_err(io_error("executing 'ssh'".to_owned()))?;
		Ok(output.status.success())
	}

	/// `startMaster`.
	fn start_master(&self) -> Result<PathBuf> {
		let socket = self.dir.path().join("ssh.sock");
		let mut master = self.master.lock().unwrap_or_else(|e| e.into_inner());
		if master.is_some() || self.master_is_running()? {
			return Ok(socket);
		}
		let mut args = vec![
			self.destination(),
			"-M".into(),
			"-N".into(),
			"-S".into(),
			socket.clone().into(),
		];
		if chatty() {
			args.push("-v".into());
		}
		self.common_options(&mut args)?;
		let mut child = self
			.ssh()
			.args(args)
			.stdout(Stdio::piped())
			.spawn()
			.map_err(io_error("unable to execute 'ssh'".to_owned()))?;
		let mut stdout = child.stdout.take().expect("piped output");
		let process = Process(child);
		let reply = read_line(&mut stdout);
		if reply != b"started" {
			return Err(Error::Other(format!(
				"failed to start SSH master connection to '{}'",
				self.config.authority.host
			)));
		}
		*master = Some(process);
		Ok(socket)
	}

	/// `startCommand`: run `command` on the host. The result is its standard
	/// input and output.
	pub fn start_command(&self, command: &[OsString]) -> Result<(UnixStream, Process)> {
		let socket = match self.use_master() {
			true => Some(self.start_master()?),
			false => None,
		};
		let mut args: Vec<OsString> = Vec::new();
		let mut process = if self.fake {
			let (program, rest) = command
				.split_first()
				.ok_or_else(|| Error::Other("the remote program is empty".to_owned()))?;
			args.extend(rest.iter().cloned());
			let mut process = Command::new(program);
			process.env("SHELL", "/bin/sh");
			process
		} else {
			args.push(self.destination());
			args.push("-x".into());
			self.common_options(&mut args)?;
			if let Some(socket) = &socket {
				args.push("-S".into());
				args.push(socket.into());
			}
			if chatty() {
				args.push("-v".into());
			}
			args.push("--".into());
			args.extend(command.iter().cloned());
			self.ssh()
		};
		let (local, remote) = UnixStream::pair()?;
		process
			.args(args)
			.stdin(OwnedFd::from(remote.try_clone()?))
			.stdout(OwnedFd::from(remote));
		if let Some(fd) = self.config.log_fd {
			// SAFETY: duplicating a descriptor; the result is a new one.
			let fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
			if fd == -1 {
				return Err(Error::Other(format!(
					"duping over stderr: {}",
					io::Error::last_os_error()
				)));
			}
			// SAFETY: F_DUPFD returned a new descriptor.
			process.stderr(unsafe { OwnedFd::from_raw_fd(fd) });
		}
		let name = process.get_program().to_owned();
		let child = Process(process.spawn().map_err(|e| {
			let reason = e.to_string();
			Error::Other(format!(
				"unable to execute '{}': {}",
				OsStr::new(&name).to_string_lossy(),
				reason.split(" (os error").next().unwrap_or(&reason)
			))
		})?);
		// The command holds the only other end now.
		drop(process);
		// Wait for the connection, whose local command says so.
		if !self.fake && !self.use_master() && !self.master_is_running()? {
			let reply = read_line(&mut (&local));
			if reply != b"started" {
				nix_compat::log::logger::log(
					nix_compat::log::VerbosityLevel::Talkative,
					&format!("SSH stdout first line: {}", String::from_utf8_lossy(&reply)),
				);
				return Err(Error::Other(format!(
					"failed to start SSH connection to '{}'",
					self.config.authority.host
				)));
			}
		}
		Ok((local, child))
	}
}

fn chatty() -> bool {
	nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Chatty)
}

/// A line, read without taking anything past it. What was read before the
/// end of the stream is dropped, as `readLine` does.
fn read_line(from: &mut dyn Read) -> Vec<u8> {
	let mut line = Vec::new();
	let mut byte = [0];
	loop {
		match from.read(&mut byte) {
			Ok(1) if byte[0] == b'\n' => return line,
			Ok(1) => line.push(byte[0]),
			Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
			_ => return Vec::new(),
		}
	}
}

/// `base64::decode`: stops at the first `=` and skips newlines.
fn decode_base64(s: &str) -> std::result::Result<Vec<u8>, String> {
	let mut out = Vec::new();
	let (mut bits, mut acc) = (0u32, 0u32);
	for c in s.bytes() {
		let value = match c {
			b'=' => break,
			b'\n' => continue,
			b'A'..=b'Z' => c - b'A',
			b'a'..=b'z' => c - b'a' + 26,
			b'0'..=b'9' => c - b'0' + 52,
			b'+' => 62,
			b'/' => 63,
			_ => {
				return Err(format!(
					"invalid character in Base64 string: '{}'",
					c as char
				));
			}
		};
		acc = acc << 6 | u32::from(value);
		bits += 6;
		if bits >= 8 {
			bits -= 8;
			out.push((acc >> bits) as u8);
			acc &= (1 << bits) - 1;
		}
	}
	Ok(out)
}

/// Keep a descriptor from the programs this process starts.
pub fn close_on_exec(fd: RawFd) {
	// SAFETY: fcntl on a descriptor, which fails harmlessly if it is closed.
	unsafe {
		let flags = libc::fcntl(fd, libc::F_GETFD);
		if flags != -1 {
			libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
		}
	}
}

/// The descriptor of a stream, to shut its sending half down.
pub fn close_write(stream: &UnixStream) {
	// SAFETY: shutting down an open socket.
	unsafe { libc::shutdown(stream.as_raw_fd(), libc::SHUT_WR) };
}

#[cfg(test)]
mod tests {
	use super::*;

	fn config(authority: &str, params: &[(&str, &str)]) -> Result<SshConfig> {
		let params = params
			.iter()
			.map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
			.collect();
		SshConfig::new(authority, &params, "nix-store")
	}

	#[test]
	fn refuses_authorities_ssh_would_read_as_options() {
		for (authority, reason) in [
			(
				"-oProxyCommand=x",
				"host name '-oProxyCommand=x' must not start with '-'",
			),
			(
				"%2doProxyCommand=x",
				"host name '-oProxyCommand=x' must not start with '-'",
			),
			("-l@host", "user name '-l' must not start with '-'"),
			("%2Dl@host", "user name '-l' must not start with '-'"),
			("user@-host", "host name '-host' must not start with '-'"),
			("@host", "user name must not be empty"),
			("", "host name must not be empty"),
		] {
			let error = config(authority, &[]).unwrap_err().to_string();
			assert!(error.ends_with(reason), "{authority}: {error}");
		}
		assert!(config("user@host:22", &[]).is_ok());
	}

	#[test]
	fn splits_options_without_expanding_them() {
		assert_eq!(
			shell_split(r#"-p 22  -o "Foo=a b" 'x $y' \ z "q\"\$\n" """#).unwrap(),
			["-p", "22", "-o", "Foo=a b", "x $y", " z", "q\"$\\n", ""]
		);
		assert!(shell_split("'a").is_err());
		assert!(shell_split("\"a").is_err());
		assert_eq!(
			shell_split("$(touch x); `id`").unwrap(),
			["$(touch", "x);", "`id`"]
		);
	}

	#[test]
	fn passes_the_key_host_key_and_port_as_single_arguments() {
		let config = config(
			"user@host:2222",
			&[
				("ssh-key", "/tmp/a b"),
				("base64-ssh-public-host-key", "c3NoLWVkMjU1MTkgQUFBQQ=="),
				("compress", "true"),
			],
		)
		.unwrap();
		let master = SshMaster::new(config).unwrap();
		let mut args = Vec::new();
		master.common_options(&mut args).unwrap();
		let key = master.dir.path().join("host-key");
		assert_eq!(
			args,
			[
				"-i".into(),
				"/tmp/a b".into(),
				OsString::from(format!("-oUserKnownHostsFile={}", key.display())),
				"-C".into(),
				"-p2222".into(),
				"-oPermitLocalCommand=yes".into(),
				"-oLocalCommand=echo started".into(),
			]
		);
		assert_eq!(std::fs::read(key).unwrap(), b"host ssh-ed25519 AAAA\n");
		assert_eq!(master.destination(), "user@host");
	}
}
