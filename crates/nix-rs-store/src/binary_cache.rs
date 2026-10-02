//! Reading binary caches and substituting paths from them, following
//! CppNix 2.35.2's `BinaryCacheStore`, `LocalBinaryCacheStore` and
//! `PathSubstitutionGoal`.
//!
//! Only `file://` caches are read so far. Substitution checks signatures
//! against the store's trusted keys before downloading, substitutes
//! references first, and imports each NAR through
//! [`LocalStore::add_to_store`], which checks its hash and size.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::path::PathBuf;

use nix_compat::narinfo::NarInfo;
use nix_compat::store_path::{STORE_DIR, StorePath};

use crate::error::{Error, Result};
use crate::local::LocalStore;
use crate::path_info::ValidPathInfo;

fn print(path: &StorePath) -> String {
	path.to_absolute_path()
}

/// What a cache's `nix-cache-info` file declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheInfo {
	pub want_mass_query: bool,
	pub priority: u32,
}

/// A binary cache in a local directory, a `file://` URL.
#[derive(Clone, Debug)]
pub struct FileBinaryCache {
	uri: String,
	dir: PathBuf,
	info: CacheInfo,
	/// Whether paths from this cache need no signature (`?trusted=1`).
	pub trusted: bool,
}

impl FileBinaryCache {
	/// Open the cache at a `file://` URL, checking that it is for
	/// `/nix/store` (`BinaryCacheStore::init`).
	pub fn open(uri: &str) -> Result<Self> {
		let rest = uri
			.strip_prefix("file://")
			.ok_or_else(|| Error::Other(format!("'{uri}' is not a file:// binary cache")))?;
		let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
		let mut trusted = false;
		for param in query.split('&').filter(|p| !p.is_empty()) {
			if let Some(value) = param.strip_prefix("trusted=") {
				trusted = value == "1" || value == "true";
			}
		}
		let mut cache = Self {
			uri: format!("file://{path}"),
			dir: PathBuf::from(path),
			info: CacheInfo {
				want_mass_query: false,
				priority: 50,
			},
			trusted,
		};
		if let Some(text) = cache.get_file("nix-cache-info")? {
			let text = String::from_utf8_lossy(&text).into_owned();
			for line in text.split('\n') {
				let Some((name, value)) = line.split_once(':') else {
					continue;
				};
				let value = value.trim();
				match name {
					"StoreDir" if value != STORE_DIR => {
						return Err(Error::Other(format!(
							"binary cache '{}' is for Nix stores with prefix '{value}', not '{STORE_DIR}'",
							cache.uri
						)));
					}
					"WantMassQuery" => cache.info.want_mass_query = value == "1",
					"Priority" => {
						cache.info.priority = value.parse().unwrap_or(cache.info.priority)
					}
					_ => {}
				}
			}
		}
		Ok(cache)
	}

	pub fn uri(&self) -> &str {
		&self.uri
	}

	pub fn info(&self) -> &CacheInfo {
		&self.info
	}

	fn get_file(&self, name: &str) -> Result<Option<Vec<u8>>> {
		match fs::read(self.dir.join(name)) {
			Ok(bytes) => Ok(Some(bytes)),
			Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
			Err(e) => Err(Error::io(
				format!("reading '{name}' from '{}'", self.uri),
				e,
			)),
		}
	}

	/// The text of `HASH.narinfo` for `path`, if the cache has it.
	pub fn narinfo_text(&self, path: &StorePath) -> Result<Option<String>> {
		let name = format!("{}.narinfo", nix_compat::nixbase32::encode(path.digest()));
		Ok(self
			.get_file(&name)?
			.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
	}

	/// The metadata the cache records for `path`.
	pub fn query_path_info(&self, path: &StorePath) -> Result<Option<CachedPath>> {
		let Some(text) = self.narinfo_text(path)? else {
			return Ok(None);
		};
		let narinfo = NarInfo::parse(&text).map_err(|e| {
			Error::Other(format!(
				"cannot parse NAR info for '{}' from '{}': {e}",
				print(path),
				self.uri
			))
		})?;
		let mut info = ValidPathInfo::new(narinfo.store_path.to_owned(), narinfo.nar_hash);
		info.nar_size = narinfo.nar_size;
		info.references = narinfo.references.iter().map(|r| r.to_owned()).collect();
		info.sigs = narinfo.signatures.iter().map(|s| s.to_string()).collect();
		info.ca = narinfo.ca.clone();
		info.deriver = narinfo.deriver.map(|d| {
			StorePath::from_bytes(format!("{d}.drv").as_bytes())
				.expect("a deriver with .drv appended is a valid store path")
		});
		Ok(Some(CachedPath {
			info,
			url: narinfo.url.to_owned(),
			compression: narinfo.compression.map(str::to_owned),
		}))
	}

	/// The decompressed NAR of a cached path.
	pub fn nar(&self, cached: &CachedPath) -> Result<Box<dyn BufRead + Send>> {
		let path = self.dir.join(&cached.url);
		let file = fs::File::open(&path).map_err(|e| {
			Error::io(
				format!("opening '{}' from binary cache '{}'", cached.url, self.uri),
				e,
			)
		})?;
		decompress(
			cached.compression.as_deref(),
			Box::new(BufReader::new(file)),
		)
	}
}

/// A narinfo's path metadata, NAR location and compression.
#[derive(Clone, Debug)]
pub struct CachedPath {
	pub info: ValidPathInfo,
	pub url: String,
	/// `None` for `Compression: none`.
	pub compression: Option<String>,
}

/// Wrap `reader` in the decompressor for a narinfo `Compression` value.
pub fn decompress(
	method: Option<&str>,
	reader: Box<dyn BufRead + Send>,
) -> Result<Box<dyn BufRead + Send>> {
	let reader: Box<dyn Read + Send> = match method {
		None | Some("none") | Some("") => return Ok(reader),
		Some("xz") => Box::new(lzma_rust2::XzReader::new(reader, true)),
		Some("bzip2") => Box::new(bzip2::bufread::MultiBzDecoder::new(reader)),
		Some("zstd") => Box::new(
			zstd::stream::read::Decoder::with_buffer(reader)
				.map_err(|e| Error::io("starting zstd decompression", e))?,
		),
		Some("gzip") => Box::new(flate2::bufread::MultiGzDecoder::new(reader)),
		Some("br") => Box::new(brotli_decompressor::Decompressor::new(reader, 65536)),
		Some(other) => {
			return Err(Error::Other(format!(
				"unknown compression method '{other}'"
			)));
		}
	};
	Ok(Box::new(BufReader::new(reader)))
}

impl LocalStore {
	/// Substitute `path` and its closure from `caches`, tried in priority
	/// order (`PathSubstitutionGoal`). Fails if no cache has a trusted
	/// copy of `path`.
	pub fn substitute(&self, caches: &[FileBinaryCache], path: &StorePath) -> Result<()> {
		let mut sorted: Vec<&FileBinaryCache> = caches.iter().collect();
		sorted.sort_by_key(|c| c.info.priority);
		let mut done = BTreeSet::new();
		self.substitute_inner(&sorted, path, &mut done)
	}

	fn substitute_inner(
		&self,
		caches: &[&FileBinaryCache],
		path: &StorePath,
		done: &mut BTreeSet<StorePath>,
	) -> Result<()> {
		if done.contains(path) {
			return Ok(());
		}
		self.add_temp_root(path)?;
		if self.is_valid_path(path)? {
			done.insert(path.clone());
			return Ok(());
		}
		if self.config().read_only {
			return Err(Error::Other(format!(
				"cannot substitute path '{}' - no write access to the Nix store",
				print(path)
			)));
		}
		let mut last_error = None;
		for cache in caches {
			let cached = match cache.query_path_info(path) {
				Ok(Some(cached)) => cached,
				Ok(None) => continue,
				Err(e) => {
					last_error = Some(e);
					continue;
				}
			};
			if cached.info.path != *path {
				tracing::error!(
					"asked '{}' for '{}' but got '{}'",
					cache.uri,
					print(path),
					print(&cached.info.path)
				);
				continue;
			}
			if !cache.trusted && self.path_info_is_untrusted(&cached.info) {
				tracing::warn!(
					"ignoring substitute for '{}' from '{}', as it's not signed by any of the keys in 'trusted-public-keys'",
					print(path),
					cache.uri
				);
				continue;
			}
			for reference in &cached.info.references {
				if reference != path {
					self.substitute_inner(caches, reference, done)?;
				}
			}
			for reference in &cached.info.references {
				if reference != path && !self.is_valid_path(reference)? {
					return Err(Error::Other(format!(
						"reference '{}' of path '{}' is not a valid path",
						print(reference),
						print(path)
					)));
				}
			}
			let mut nar = cache.nar(&cached)?;
			self.add_to_store(&cached.info, &mut nar, false, !cache.trusted)?;
			done.insert(path.clone());
			return Ok(());
		}
		if let Some(e) = last_error {
			return Err(e);
		}
		Err(Error::Other(format!(
			"path '{}' is required, but there is no substituter that can build it",
			print(path)
		)))
	}
}
