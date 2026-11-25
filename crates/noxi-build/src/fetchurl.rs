//! `builtin:fetchurl`, CppNix's `builtinFetchurl`.
//!
//! The reference runs this in the builder's process, as the build user and
//! inside the sandbox. This runs in the process that builds, which may be a
//! daemon running as root, so it must not read files for the derivation that
//! the builder could not: `file:` URLs are refused unless the builder would
//! run as the same user without a sandbox.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use nix_compat::nixhash::NixHash;
use noxi_store::binary_cache::{TransferSettings, download};
use noxi_store::nar;

use crate::buildenv::Error;

pub struct Fetch<'a> {
	pub env: &'a BTreeMap<String, String>,
	/// Where the output is written, as this process sees it.
	pub out: &'a Path,
	/// The expected hash of a flat fixed output, for `hashed-mirrors`.
	pub flat_hash: Option<&'a NixHash>,
	pub hashed_mirrors: &'a [String],
	pub transfer: TransferSettings,
	/// Whether the builder would see and be allowed to read this process's
	/// files.
	pub local_files: bool,
	pub cancelled: Option<&'a AtomicBool>,
}

/// A download that stops when the build is cancelled.
struct Cancellable<'a> {
	inner: Box<dyn Read + Send>,
	cancelled: Option<&'a AtomicBool>,
}

impl Read for Cancellable<'_> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		if self
			.cancelled
			.is_some_and(|flag| flag.load(Ordering::Relaxed))
		{
			return Err(io::Error::other("build cancelled"));
		}
		self.inner.read(buf)
	}
}

impl Fetch<'_> {
	pub fn run(&self) -> Result<(), Error> {
		let url = self
			.env
			.get("url")
			.ok_or_else(|| Error("attribute 'url' missing".to_owned()))?;
		if let Some(hash) = self.flat_hash {
			let path = hash.as_nix_lowerhex_string_fmt().to_string();
			let (algo, digest) = path.split_once(':').unwrap_or_default();
			for mirror in self.hashed_mirrors {
				let mirror = mirror.trim_end_matches('/');
				match self.fetch(&format!("{mirror}/{algo}/{digest}"), url) {
					Ok(()) => return Ok(()),
					Err(error) => tracing::debug!("{}", error.0),
				}
			}
		}
		self.fetch(url, url)
	}

	fn open(&self, url: &str) -> Result<Box<dyn Read + Send>, Error> {
		if let Some(path) = url.strip_prefix("file://") {
			if !self.local_files {
				return Err(Error(format!(
					"unable to download '{url}': 'file' URLs are not available to a sandboxed builder or a build user"
				)));
			}
			let path =
				nix_compat::nixcpp::url::percent_decode(path).unwrap_or_else(|_| path.to_owned());
			return match fs::File::open(&path) {
				Ok(file) => Ok(Box::new(file)),
				Err(_) => Err(Error(format!(
					"unable to download '{url}': Could not read a file:// file (37) Couldn't open file {path}"
				))),
			};
		}
		download(url, self.transfer).map_err(|error| Error(error.to_string()))
	}

	fn fetch(&self, url: &str, main_url: &str) -> Result<(), Error> {
		let mut source: Box<dyn Read + Send> = Box::new(Cancellable {
			inner: self.open(url)?,
			cancelled: self.cancelled,
		});
		let flag = |name: &str| self.env.get(name).is_some_and(|value| value == "1");
		if flag("unpack") {
			if main_url.ends_with(".xz") {
				source = Box::new(lzma_rust2::XzReader::new(BufReader::new(source), true));
			}
			nar::restore_path(self.out, &mut BufReader::new(source))?;
		} else {
			let mut file = fs::OpenOptions::new()
				.write(true)
				.create(true)
				.truncate(true)
				.custom_flags(libc::O_NOFOLLOW)
				.mode(0o644)
				.open(self.out)?;
			io::copy(&mut source, &mut file)?;
			file.flush()?;
		}
		if flag("executable") {
			fs::set_permissions(self.out, fs::Permissions::from_mode(0o755))?;
		}
		Ok(())
	}
}
