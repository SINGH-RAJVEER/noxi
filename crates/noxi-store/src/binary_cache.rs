//! Reading binary caches and substituting paths from them, following
//! CppNix 2.35.2's `BinaryCacheStore`, `LocalBinaryCacheStore` and
//! `PathSubstitutionGoal`.
//!
//! `file://`, `http://` and `https://` caches are read. Substitution checks signatures
//! against the store's trusted keys before downloading, substitutes
//! references first, and imports each NAR through
//! [`LocalStore::add_to_store`], which checks its hash and size.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use data_encoding::BASE64;
use nix_compat::narinfo::{Flags, NarInfo, Signature, SigningKey};
use nix_compat::nixcpp::narinfo as cppnix;
use nix_compat::nixhash::NixHash;
use nix_compat::store_path::{STORE_DIR, StorePath};
use tempfile::NamedTempFile;

use crate::error::{Error, Result};
use crate::local::LocalStore;
use crate::lock::PathLocks;
use crate::nar::HashSink;
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
	path_info_cache: Arc<Mutex<BTreeMap<StorePath, (Instant, Option<CachedPath>)>>>,
	positive_ttl: Duration,
	negative_ttl: Duration,
	http10_close: Arc<AtomicBool>,
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
			let agent = crate::quick_ack::agent(
				ureq::Agent::config_builder()
					.http_status_as_error(false)
					.user_agent(concat!("noxi/", env!("CARGO_PKG_VERSION")))
					.build(),
			);
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
			path_info_cache: Arc::default(),
			positive_ttl: Duration::from_secs(30 * 24 * 60 * 60),
			negative_ttl: Duration::from_secs(60 * 60),
			http10_close: Arc::default(),
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

	/// Set the lifetime of HTTP metadata shared by this handle and its clones.
	/// Local cache metadata remains live so uploads are immediately visible.
	pub fn with_narinfo_cache_ttls(mut self, positive: u64, negative: u64) -> Self {
		self.positive_ttl = Duration::from_secs(positive);
		self.negative_ttl = Duration::from_secs(negative);
		self
	}

	/// Fetch independent HTTP metadata with a bounded number of workers.
	/// Callers query the results normally, preserving cache selection and errors.
	pub fn prefetch_path_info(&self, paths: &[StorePath], jobs: usize) {
		if !matches!(self.transport, Transport::Http(..))
			|| self.positive_ttl.is_zero()
			|| self.negative_ttl.is_zero()
			|| paths.len() < 2
			|| jobs < 2
		{
			return;
		}
		std::thread::scope(|scope| {
			for chunk in paths.chunks(paths.len().div_ceil(jobs.min(paths.len()))) {
				scope.spawn(move || {
					for path in chunk {
						let _ = self.query_path_info(path);
					}
				});
			}
		});
	}

	/// Export `path` and its reference closure to a local, uncompressed cache.
	/// NAR bytes must match source metadata before signing. Dependencies are
	/// published first; a failed upload may leave completed dependencies behind.
	/// The source must permit temporary GC roots, which remain until its last
	/// participating handle is dropped. Cache directories must be trusted.
	pub fn upload_closure<S>(
		&self,
		source: &LocalStore,
		path: &StorePath,
		signing_keys: &[SigningKey<S>],
	) -> Result<()>
	where
		S: ed25519::signature::Signer<ed25519::Signature>,
	{
		let Transport::File(dir) = &self.transport else {
			return Err(Error::Other(
				"binary cache uploads require a file:// cache".into(),
			));
		};
		if source.config().read_only {
			return Err(Error::Other(
				"binary cache upload requires source GC protection".into(),
			));
		}
		if signing_keys.is_empty() {
			return Err(Error::Other(
				"binary cache upload requires a signing key".into(),
			));
		}
		for key in signing_keys {
			if key.name().is_empty()
				|| !key
					.name()
					.chars()
					.all(|c| c.is_alphanumeric() || c == '-' || c == '.')
			{
				return Err(Error::Other("invalid binary cache signing key name".into()));
			}
		}
		fs::create_dir_all(dir.join("nar"))
			.map_err(|e| Error::io(format!("creating binary cache '{}'", self.uri), e))?;
		// Revalidate existing cache ownership even if it changed after open.
		BinaryCache::open(&self.uri)?;
		if self.get_file("nix-cache-info")?.is_none() {
			write_atomic(&dir.join("nix-cache-info"), b"StoreDir: /nix/store\n")?;
		}
		self.upload_path(
			source,
			path,
			signing_keys,
			&mut BTreeSet::new(),
			&mut BTreeSet::new(),
		)
	}

	fn upload_path<S>(
		&self,
		source: &LocalStore,
		path: &StorePath,
		signing_keys: &[SigningKey<S>],
		visiting: &mut BTreeSet<StorePath>,
		done: &mut BTreeSet<StorePath>,
	) -> Result<()>
	where
		S: ed25519::signature::Signer<ed25519::Signature>,
	{
		if done.contains(path) {
			return Ok(());
		}
		if !visiting.insert(path.clone()) {
			return Err(Error::Other(format!(
				"cycle in upload references of '{}'",
				print(path)
			)));
		}
		source.add_temp_root(path)?;
		let info = source
			.query_path_info(path)?
			.ok_or_else(|| Error::InvalidPath(print(path)))?;
		for reference in &info.references {
			if reference != path {
				self.upload_path(source, reference, signing_keys, visiting, done)?;
			}
		}
		let Transport::File(dir) = &self.transport else {
			unreachable!()
		};
		let mut temporary = NamedTempFile::new_in(dir.join("nar"))
			.map_err(|e| Error::io("staging binary cache NAR", e))?;
		let (hash, size) = {
			let mut sink = UploadSink {
				file: BufWriter::new(temporary.as_file_mut()),
				hash: HashSink::new(),
			};
			source.nar_from_path(path, &mut sink)?;
			sink.flush()
				.map_err(|e| Error::io("flushing binary cache NAR", e))?;
			sink.hash.finish()
		};
		if hash != info.nar_hash || size != info.nar_size || size == 0 {
			return Err(Error::Other(format!(
				"NAR hash or size mismatch exporting path '{}'",
				print(path)
			)));
		}
		let url = format!("nar/{}.nar", nix_compat::nixbase32::encode(&hash));
		let metadata_path = local_cache_path(
			dir,
			&format!("{}.narinfo", nix_compat::nixbase32::encode(path.digest())),
		)?;
		// Serialize signature merging and publication between Rust exporters.
		let _publication_lock = PathLocks::lock([metadata_path.clone()])?;
		let mut signatures = info
			.sigs
			.iter()
			.map(|sig| {
				Signature::<String>::parse(sig).map_err(|_| {
					Error::Other(format!("invalid source signature for '{}'", print(path)))
				})
			})
			.collect::<Result<Vec<_>>>()?;
		if let Some(text) = self.narinfo_text(path)? {
			let existing = NarInfo::parse(&text).map_err(|e| {
				Error::Other(format!("invalid cache metadata for '{}': {e}", print(path)))
			})?;
			if existing.store_path != info.path.as_ref()
				|| Some(existing.fingerprint()) != info.fingerprint()
			{
				return Err(Error::Other(format!(
					"conflicting cache metadata for '{}'",
					print(path)
				)));
			}
			signatures.extend(existing.signatures.iter().map(Signature::to_owned));
		}
		let deriver = info
			.deriver
			.as_ref()
			.map(|deriver| {
				let name = deriver.to_string();
				let base = name.strip_suffix(".drv").ok_or_else(|| {
					Error::Other(format!("invalid deriver for '{}'", print(path)))
				})?;
				StorePath::from_bytes(base.as_bytes())
					.map_err(|_| Error::Other(format!("invalid deriver for '{}'", print(path))))
			})
			.transpose()?;
		let mut narinfo = NarInfo {
			flags: Flags::empty(),
			store_path: info.path.as_ref(),
			nar_hash: hash,
			nar_size: size,
			references: info.references.iter().map(StorePath::as_ref).collect(),
			signatures: signatures.iter().map(Signature::as_ref).collect(),
			ca: info.ca.clone(),
			system: None,
			deriver: deriver.as_ref().map(StorePath::as_ref),
			url: &url,
			compression: None,
			file_hash: Some(hash),
			file_size: Some(size),
		};
		for key in signing_keys {
			narinfo.add_signature(key);
		}
		narinfo
			.signatures
			.sort_by(|a, b| a.name().cmp(b.name()).then(a.bytes().cmp(b.bytes())));
		narinfo.signatures.dedup();
		persist_atomic(temporary, &local_cache_path(dir, &url)?)?;
		write_atomic(&metadata_path, narinfo.to_string().as_bytes())?;
		visiting.remove(path);
		done.insert(path.clone());
		Ok(())
	}

	/// A file of the cache, or `None` if it has none by that name.
	fn open_file(&self, name: &str) -> Result<Option<Box<dyn Read + Send>>> {
		match &self.transport {
			Transport::File(dir) => match fs::File::open(local_cache_path(dir, name)?) {
				Ok(file) => Ok(Some(Box::new(file))),
				Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
				Err(e) => Err(Error::io(
					format!("reading '{name}' from '{}'", self.uri),
					e,
				)),
			},
			Transport::Http(base, agent) => {
				let url = format!("{base}/{name}");
				let mut request = agent.get(&url);
				if self.http10_close.load(Ordering::Relaxed) {
					// ureq 3.4 does not close a length-delimited HTTP/1.0
					// response unless Connection: close is explicit. Do not
					// race the server's FIN on the next request.
					request = request
						.header("Connection", "close")
						.config()
						.max_idle_age(Duration::ZERO)
						.build();
				}
				let response = request
					.call()
					.map_err(|e| Error::Other(format!("unable to download '{url}': {e}")))?;
				if response.version() == ureq::http::Version::HTTP_10
					&& !response
						.headers()
						.get_all("connection")
						.iter()
						.any(|value| {
							value.to_str().is_ok_and(|value| {
								value
									.split(',')
									.any(|token| token.trim().eq_ignore_ascii_case("keep-alive"))
							})
						}) {
					self.http10_close.store(true, Ordering::Relaxed);
				}
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

	/// The metadata the cache records for `path`, read as the reference
	/// reads it, with its errors for malformed files.
	pub fn query_path_info(&self, path: &StorePath) -> Result<Option<CachedPath>> {
		if !matches!(self.transport, Transport::Http(..)) {
			return self.query_path_info_uncached(path);
		}
		{
			let mut cache = self.path_info_cache.lock().unwrap();
			if let Some((when, value)) = cache.get(path) {
				let ttl = if value.is_some() {
					self.positive_ttl
				} else {
					self.negative_ttl
				};
				if when.elapsed() < ttl {
					return Ok(value.clone());
				}
				cache.remove(path);
			}
		}
		// Download outside the mutex so independent requests can overlap.
		// Transport and parse errors are retried by the next caller.
		let result = self.query_path_info_uncached(path)?;
		let ttl = if result.is_some() {
			self.positive_ttl
		} else {
			self.negative_ttl
		};
		if !ttl.is_zero() {
			self.path_info_cache
				.lock()
				.unwrap()
				.insert(path.clone(), (Instant::now(), result.clone()));
		}
		Ok(result)
	}

	fn query_path_info_uncached(&self, path: &StorePath) -> Result<Option<CachedPath>> {
		let name = format!("{}.narinfo", nix_compat::nixbase32::encode(path.digest()));
		let Some(bytes) = self.get_file(&name)? else {
			return Ok(None);
		};
		let narinfo =
			cppnix::NarInfo::parse(&bytes, &name).map_err(|e| Error::Other(e.to_string()))?;
		let NixHash::Sha256(nar_hash) = narinfo.nar_hash else {
			return Err(Error::Other(format!(
				"NAR info file '{name}' has a {} NAR hash; only SHA-256 is supported",
				narinfo.nar_hash.algo()
			)));
		};
		let mut info = ValidPathInfo::new(narinfo.path, nar_hash);
		info.nar_size = narinfo.nar_size;
		info.references = narinfo.references;
		info.sigs = narinfo
			.sigs
			.iter()
			.map(|sig| format!("{}:{}", sig.key_name, BASE64.encode(&sig.sig)))
			.collect();
		info.ca = narinfo.ca;
		info.deriver = narinfo.deriver;
		Ok(Some(CachedPath {
			info,
			url: narinfo.url,
			compression: (narinfo.compression != "none").then_some(narinfo.compression),
			file_size: (narinfo.file_size != 0).then_some(narinfo.file_size),
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

fn local_cache_path(root: &Path, name: &str) -> Result<PathBuf> {
	if name.is_empty() {
		return Err(Error::Other(
			"local binary cache path must not be empty".into(),
		));
	}
	if name.contains('\0') {
		return Err(Error::Other(
			"local binary cache path must not contain NUL bytes".into(),
		));
	}
	if Path::new(name).is_absolute() {
		return Err(Error::Other(format!(
			"local binary cache path '{name}' must not be absolute"
		)));
	}
	// Path::components normalizes interior '.', so inspect the raw segments.
	if name.split('/').any(|part| part == "." || part == "..") {
		return Err(Error::Other(format!(
			"local binary cache path '{name}' must not contain '..' or '.' segments"
		)));
	}
	Ok(root.join(name))
}

/// Hash exactly the bytes accepted by the staging writer.
struct UploadSink<W> {
	file: W,
	hash: HashSink,
}

impl<W: Write> Write for UploadSink<W> {
	fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
		let count = self.file.write(bytes)?;
		self.hash.write_all(&bytes[..count])?;
		Ok(count)
	}

	fn flush(&mut self) -> io::Result<()> {
		self.file.flush()
	}
}

fn persist_atomic(temporary: NamedTempFile, destination: &Path) -> Result<()> {
	temporary
		.as_file()
		.set_permissions(fs::Permissions::from_mode(0o644))?;
	temporary.as_file().sync_all()?;
	temporary
		.persist(destination)
		.map_err(|e| Error::io(format!("publishing '{}'", destination.display()), e.error))?;
	File::open(destination.parent().expect("cache file has a parent"))?.sync_all()?;
	Ok(())
}

fn write_atomic(destination: &Path, bytes: &[u8]) -> Result<()> {
	let mut temporary =
		NamedTempFile::new_in(destination.parent().expect("cache file has a parent"))?;
	temporary.write_all(bytes)?;
	persist_atomic(temporary, destination)
}

/// A narinfo's path metadata, NAR location and compression.
#[derive(Clone, Debug)]
pub struct CachedPath {
	pub info: ValidPathInfo,
	pub url: String,
	/// `None` for `Compression: none`.
	pub compression: Option<String>,
	/// The size of the compressed NAR, `FileSize`.
	pub file_size: Option<u64>,
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
				// No tracing subscriber prints warnings; CppNix's `warn`
				// reaches standard error.
				if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn) {
					eprintln!(
						"warning: ignoring substitute for '{}' from '{}', as it's not signed by any of the keys in 'trusted-public-keys'",
						print(path),
						cache.uri
					);
				}
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

#[cfg(test)]
mod tests {
	use super::*;
	use std::net::TcpListener;
	use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

	#[test]
	fn http_metadata_is_shared_expires_and_can_be_disabled() {
		check_metadata_cache("HTTP/1.1");
		check_metadata_cache("HTTP/1.0");
	}

	fn check_metadata_cache(protocol: &str) {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		listener.set_nonblocking(true).unwrap();
		let uri = format!("http://{}", listener.local_addr().unwrap());
		let requests = AtomicUsize::new(0);
		let stopped = AtomicBool::new(false);
		std::thread::scope(|scope| {
			scope.spawn(|| {
				while !stopped.load(Ordering::Relaxed) {
					let (stream, _) = match listener.accept() {
						Ok(connection) => connection,
						Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
							std::thread::sleep(Duration::from_millis(1));
							continue;
						}
						Err(error) => panic!("{error}"),
					};
					stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
					let mut reader = BufReader::new(stream);
					let mut request = String::new();
					reader.read_line(&mut request).unwrap();
					loop {
						let mut line = String::new();
						reader.read_line(&mut line).unwrap();
						if line == "\r\n" || line.is_empty() { break; }
					}
					requests.fetch_add(1, Ordering::Relaxed);
					let (status, body) = if request.contains("nix-cache-info") {
						("200 OK", "StoreDir: /nix/store\n")
					} else if request.contains("/nar/test.nar ") {
						("200 OK", "archive bytes")
					} else if request.contains("/00000000000000000000000000000000.narinfo") {
						("200 OK", "StorePath: /nix/store/00000000000000000000000000000000-test\nURL: nar/test.nar\nCompression: none\nNarHash: sha256:0000000000000000000000000000000000000000000000000000\nNarSize: 1\nReferences: \n")
					} else {
						("404 Not Found", "missing")
					};
					let connection = if protocol == "HTTP/1.1" { "Connection: close\r\n" } else { "" };
					write!(reader.get_mut(), "{protocol} {status}\r\nContent-Length: {}\r\n{connection}\r\n{body}", body.len()).unwrap();
				}
			});
			// Stop the server even if an assertion unwinds the client.
			struct Stop<'a>(&'a AtomicBool);
			impl Drop for Stop<'_> {
				fn drop(&mut self) {
					self.0.store(true, Ordering::Relaxed);
				}
			}
			let _stop = Stop(&stopped);
			let cache = BinaryCache::open(&uri).unwrap();
			assert_eq!(
				cache.http10_close.load(Ordering::Relaxed),
				protocol == "HTTP/1.0"
			);
			let path = StorePath::from_bytes(b"00000000000000000000000000000000-test").unwrap();
			let absent = StorePath::from_bytes(b"11111111111111111111111111111111-test").unwrap();
			assert!(cache.query_path_info(&path).unwrap().is_some());
			assert!(cache.clone().query_path_info(&path).unwrap().is_some());
			assert!(cache.query_path_info(&absent).unwrap().is_none());
			assert!(cache.clone().query_path_info(&absent).unwrap().is_none());
			assert_eq!(requests.load(Ordering::Relaxed), 3);
			for (when, _) in cache.path_info_cache.lock().unwrap().values_mut() {
				*when -= Duration::from_secs(31 * 24 * 60 * 60);
			}
			assert!(cache.query_path_info(&path).unwrap().is_some());
			assert!(cache.query_path_info(&absent).unwrap().is_none());
			assert_eq!(requests.load(Ordering::Relaxed), 5);
			let uncached = cache.with_narinfo_cache_ttls(0, 0);
			for _ in 0..2 {
				assert!(uncached.query_path_info(&path).unwrap().is_some());
				assert!(uncached.query_path_info(&absent).unwrap().is_none());
			}
			assert_eq!(requests.load(Ordering::Relaxed), 9);
			let cached = uncached.query_path_info(&path).unwrap().unwrap();
			let mut bytes = Vec::new();
			uncached
				.nar(&cached)
				.unwrap()
				.read_to_end(&mut bytes)
				.unwrap();
			assert_eq!(bytes, b"archive bytes");
			assert_eq!(requests.load(Ordering::Relaxed), 11);
		});
	}
}
