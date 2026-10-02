//! Reading binary caches and substituting paths from them, following
//! CppNix 2.35.2's `BinaryCacheStore`, `LocalBinaryCacheStore` and
//! `PathSubstitutionGoal`.
//!
//! `file://`, `http://` and `https://` caches are read. Substitution checks signatures
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

/// Where a cache's files are.
#[derive(Clone)]
enum Transport {
	/// A local directory, a `file://` URL.
	File(PathBuf),
	/// An HTTP(S) server; the URL has no trailing slash.
	Http(String, ureq::Agent),
}

impl std::fmt::Debug for Transport {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Transport::File(dir) => write!(f, "File({})", dir.display()),
			Transport::Http(url, _) => write!(f, "Http({url})"),
		}
	}
}

/// A binary cache: `file://` (`LocalBinaryCacheStore`) or `http(s)://`
/// (`HttpBinaryCacheStore`).
#[derive(Clone, Debug)]
pub struct BinaryCache {
	uri: String,
	transport: Transport,
	info: CacheInfo,
	/// Whether paths from this cache need no signature (`?trusted=1`).
	pub trusted: bool,
}

impl BinaryCache {
	/// Open the cache at a URL, checking that it is for `/nix/store`
	/// (`BinaryCacheStore::init`).
	pub fn open(uri: &str) -> Result<Self> {
		let (base, query) = uri.split_once('?').unwrap_or((uri, ""));
		let mut trusted = false;
		for param in query.split('&').filter(|p| !p.is_empty()) {
			if let Some(value) = param.strip_prefix("trusted=") {
				trusted = value == "1" || value == "true";
			}
		}
		let transport = if let Some(path) = base.strip_prefix("file://") {
			Transport::File(PathBuf::from(path))
		} else if base.starts_with("http://") || base.starts_with("https://") {
			let agent = ureq::Agent::config_builder()
				.http_status_as_error(false)
				.user_agent(concat!("nix-rs/", env!("CARGO_PKG_VERSION")))
				.build()
				.into();
			Transport::Http(base.trim_end_matches('/').to_owned(), agent)
		} else {
			return Err(Error::Other(format!(
				"don't know how to open binary cache '{uri}'"
			)));
		};
		let mut cache = Self {
			uri: base.trim_end_matches('/').to_owned(),
			transport,
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

	/// A file of the cache, or `None` if it has none by that name.
	fn open_file(&self, name: &str) -> Result<Option<Box<dyn Read + Send>>> {
		match &self.transport {
			Transport::File(dir) => match fs::File::open(dir.join(name)) {
				Ok(file) => Ok(Some(Box::new(file))),
				Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
				Err(e) => Err(Error::io(
					format!("reading '{name}' from '{}'", self.uri),
					e,
				)),
			},
			Transport::Http(base, agent) => {
				let url = format!("{base}/{name}");
				let response = agent
					.get(&url)
					.call()
					.map_err(|e| Error::Other(format!("unable to download '{url}': {e}")))?;
				match response.status().as_u16() {
					200..=299 => Ok(Some(Box::new(response.into_body().into_reader()))),
					// S3 answers 403 for files that do not exist.
					404 | 403 => Ok(None),
					status => Err(Error::Other(format!(
						"unable to download '{url}': HTTP error {status}"
					))),
				}
			}
		}
	}

	fn get_file(&self, name: &str) -> Result<Option<Vec<u8>>> {
		let Some(mut reader) = self.open_file(name)? else {
			return Ok(None);
		};
		let mut bytes = Vec::new();
		reader
			.read_to_end(&mut bytes)
			.map_err(|e| Error::io(format!("reading '{name}' from '{}'", self.uri), e))?;
		Ok(Some(bytes))
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
		let file = self.open_file(&cached.url)?.ok_or_else(|| {
			Error::Other(format!(
				"file '{}' does not exist in binary cache '{}'",
				cached.url, self.uri
			))
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
	pub fn substitute(&self, caches: &[BinaryCache], path: &StorePath) -> Result<()> {
		let mut sorted: Vec<&BinaryCache> = caches.iter().collect();
		sorted.sort_by_key(|c| c.info.priority);
		let mut done = BTreeSet::new();
		self.substitute_inner(&sorted, path, &mut done)
	}

	fn substitute_inner(
		&self,
		caches: &[&BinaryCache],
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
				if reference != path
					&& let Err(e) = self.substitute_inner(caches, reference, done)
				{
					tracing::error!("{e}");
					return Err(Error::Other(format!(
						"some references of path '{}' could not be realised",
						print(path)
					)));
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
			// A failed download falls through to the next cache, as
			// `PathSubstitutionGoal::tryToRun` does.
			let result = cache.nar(&cached).and_then(|mut nar| {
				self.add_to_store(&cached.info, &mut nar, false, !cache.trusted)
			});
			match result {
				Ok(()) => {
					done.insert(path.clone());
					return Ok(());
				}
				Err(e) => tracing::error!("{e}"),
			}
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
