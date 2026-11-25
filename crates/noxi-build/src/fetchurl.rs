//! `builtin:fetchurl`, CppNix's `builtinFetchurl`.
//!
//! It runs in the builder's process: as the build user, with the sandbox's
//! files and network, and under the build's timeouts. Only the contents of
//! `netrc-file` and `ssl-cert-file` come from the process that builds.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use nix_compat::nixhash::NixHash;
use noxi_store::download::{Options, download};
use noxi_store::nar;

use crate::buildenv::Error;

pub struct Fetch {
	pub env: BTreeMap<String, String>,
	pub out: PathBuf,
	/// The expected hash of a flat fixed output, for `hashed-mirrors`.
	pub flat_hash: Option<NixHash>,
	pub hashed_mirrors: Vec<String>,
	pub options: Options,
	/// Where the reference leaves the certificates in the build directory.
	pub ca_copy: PathBuf,
}

/// A message for the process that builds, as the reference's JSON logger in
/// the builder writes it.
fn json_message(level: u64, text: &str) -> String {
	let message = serde_json::json!({ "action": "msg", "level": level, "msg": text });
	format!("@nix {message}\n")
}

/// Reports a download's progress once a second until it is dropped, also
/// while it waits, as curl's progress callback does. The build is therefore
/// not silent.
struct Progress {
	_stop: mpsc::Sender<()>,
}

impl Progress {
	fn start(done: Arc<AtomicU64>) -> Self {
		let (stop, stopped) = mpsc::channel::<()>();
		std::thread::spawn(move || {
			while let Err(mpsc::RecvTimeoutError::Timeout) =
				stopped.recv_timeout(Duration::from_secs(1))
			{
				let line = format!(
					"@nix {{\"action\":\"result\",\"fields\":[{},0,0,0],\"id\":1,\"type\":105}}\n",
					done.load(Ordering::Relaxed)
				);
				// SAFETY: a short write to our standard error, which is not
				// split from others.
				unsafe { libc::write(libc::STDERR_FILENO, line.as_ptr().cast(), line.len()) };
			}
		});
		Self { _stop: stop }
	}
}

/// A reader that counts what is read.
struct Counted<R> {
	inner: R,
	done: Arc<AtomicU64>,
	_progress: Progress,
}

impl<R: Read> Read for Counted<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let n = self.inner.read(buf)?;
		self.done.fetch_add(n as u64, Ordering::Relaxed);
		Ok(n)
	}
}

impl Fetch {
	pub fn run(&self, log: &mut dyn FnMut(&str)) -> Result<(), Error> {
		// The reference writes the `netrc` contents next to it, where a `file:`
		// URL can read them. They stay in memory here.
		fs::OpenOptions::new()
			.write(true)
			.create(true)
			.truncate(true)
			.mode(0o600)
			.open(&self.ca_copy)?
			.write_all(&self.options.ca_certificates)?;
		let url = self
			.env
			.get("url")
			.ok_or_else(|| Error("attribute 'url' missing".to_owned()))?;
		if let Some(hash) = &self.flat_hash {
			let path = hash.as_nix_lowerhex_string_fmt().to_string();
			let (algo, digest) = path.split_once(':').unwrap_or_default();
			for mirror in &self.hashed_mirrors {
				let slash = if mirror.ends_with('/') { "" } else { "/" };
				if self
					.fetch(&format!("{mirror}{slash}{algo}/{digest}"), url, log)
					.is_ok()
				{
					return Ok(());
				}
			}
		}
		self.fetch(url, url, log)
	}

	fn open(&self, url: &str, log: &mut dyn FnMut(&str)) -> Result<Box<dyn Read + Send>, Error> {
		if let Some(rest) = url.strip_prefix("file://") {
			let path = ["", "localhost", "127.0.0.1"]
				.into_iter()
				.find_map(|host| rest.strip_prefix(host).filter(|path| path.starts_with('/')))
				.ok_or_else(|| {
					Error(format!(
						"unable to download '{url}': URL using bad/illegal format or missing URL (3)"
					))
				})?;
			let path = path.split(['?', '#']).next().unwrap_or(path);
			let path =
				nix_compat::nixcpp::url::percent_decode(path).unwrap_or_else(|_| path.to_owned());
			return match fs::File::open(&path) {
				Ok(file) => Ok(Box::new(file)),
				Err(_) => Err(Error(format!(
					"unable to download '{url}': Could not read a file:// file (37) Couldn't open file {path}"
				))),
			};
		}
		let done = Arc::new(AtomicU64::new(0));
		let progress = Progress::start(done.clone());
		let body = download(url, &self.options, &mut |warning| {
			log(&json_message(
				1,
				&format!("\x1b[35;1mwarning:\x1b[0m {warning}"),
			));
		})
		.map_err(|error| Error(error.to_string()))?;
		Ok(Box::new(Counted {
			inner: body,
			done,
			_progress: progress,
		}))
	}

	fn fetch(&self, url: &str, main_url: &str, log: &mut dyn FnMut(&str)) -> Result<(), Error> {
		let mut source = self.open(url, log)?;
		let flag = |name: &str| self.env.get(name).is_some_and(|value| value == "1");
		if flag("unpack") {
			if main_url.ends_with(".xz") {
				source = Box::new(lzma_rust2::XzReader::new(BufReader::new(source), true));
			}
			nar::restore_path(&self.out, &mut BufReader::new(source))?;
		} else {
			let mut file = fs::OpenOptions::new()
				.write(true)
				.create(true)
				.truncate(true)
				.custom_flags(libc::O_NOFOLLOW)
				.mode(0o666)
				.open(&self.out)?;
			io::copy(&mut source, &mut file)?;
			file.flush()?;
		}
		if flag("executable") {
			fs::set_permissions(&self.out, fs::Permissions::from_mode(0o755))?;
		}
		Ok(())
	}
}
