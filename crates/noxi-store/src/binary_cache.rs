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
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use data_encoding::BASE64;
use nix_compat::narinfo::{Flags, NarInfo, Signature, SigningKey};
use nix_compat::nixcpp::narinfo as cppnix;
use nix_compat::nixhash::NixHash;
use nix_compat::store_path::{StorePath, store_dir};
use tempfile::NamedTempFile;

use crate::error::{Error, Result};
use crate::local::LocalStore;
use crate::lock::PathLocks;
use crate::nar::HashSink;
use crate::nar_listing::Listing;
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
	upload: UploadSettings,
	uri: String,
	transport: Transport,
	info: CacheInfo,
	/// Whether paths from this cache need no signature (`?trusted=1`).
	pub trusted: bool,
	path_info_cache: Arc<Mutex<BTreeMap<StorePath, (Instant, Option<CachedPath>)>>>,
	positive_ttl: Duration,
	negative_ttl: Duration,
	http10_close: Arc<AtomicBool>,
	/// Paths whose `query-path-info` activity was already reported.
	announced: Arc<Mutex<BTreeSet<StorePath>>>,
}

impl BinaryCache {
	/// Open the cache at a URL, checking that it is for `/nix/store`
	/// (`BinaryCacheStore::init`).
	pub fn open(uri: &str) -> Result<Self> {
		let (base, query) = uri.split_once('?').unwrap_or((uri, ""));
		let mut trusted = false;
		let mut upload = UploadSettings::default();
		let mut want_mass_query = None;
		let mut priority = None;
		let http = base.starts_with("http://") || base.starts_with("https://");
		for param in query.split('&').filter(|p| !p.is_empty()) {
			let (name, value) = param.split_once('=').unwrap_or((param, ""));
			let invalid = || Error::Other(format!("option '{name}' has invalid value '{value}'"));
			match name {
				"trusted" => trusted = parse_bool(value).ok_or_else(invalid)?,
				"compression" => {
					if !cppnix::COMPRESSION_ALGOS.contains(&value) {
						return Err(invalid());
					}
					upload.compression = value.to_owned();
				}
				"compression-level" => {
					upload.compression_level = value.parse().map_err(|_| invalid())?
				}
				"parallel-compression" => {
					upload.parallel_compression = Some(parse_bool(value).ok_or_else(invalid)?)
				}
				"write-nar-listing" => {
					upload.write_nar_listing = parse_bool(value).ok_or_else(invalid)?
				}
				"index-debug-info" => {
					upload.write_debug_info = parse_bool(value).ok_or_else(invalid)?
				}
				"secret-key" => upload.secret_key_files.insert(0, value.to_owned()),
				"secret-keys" => upload
					.secret_key_files
					.extend(value.split(',').map(str::to_owned)),
				"want-mass-query" => want_mass_query = Some(parse_bool(value).ok_or_else(invalid)?),
				"priority" => priority = Some(value.parse().map_err(|_| invalid())?),
				// Settings every binary cache store has that do not affect
				// what noxi reads or writes.
				"local-nar-cache" | "path-info-cache-size" | "system-features" | "store" => {}
				"narinfo-compression" | "ls-compression" | "log-compression" if http => {}
				_ => {
					// `Config::warnUnknownSettings`. The handle may be opened
					// more than once; warn once per setting.
					static WARNED: Mutex<Vec<String>> = Mutex::new(Vec::new());
					let mut warned = WARNED.lock().unwrap_or_else(|e| e.into_inner());
					if !warned.iter().any(|w| w == name) {
						warned.push(name.to_owned());
						nix_compat::log::logger::warn(&format!("unknown setting '{name}'"));
					}
				}
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
			upload,
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
			announced: Arc::default(),
		};
		if let Some(text) = cache.get_file("nix-cache-info")? {
			let text = String::from_utf8_lossy(&text).into_owned();
			for line in text.split('\n') {
				let Some((name, value)) = line.split_once(':') else {
					continue;
				};
				let value = value.trim();
				match name {
					"StoreDir" if value != store_dir() => {
						return Err(Error::Other(format!(
							"binary cache '{}' is for Nix stores with prefix '{value}', not '{}'",
							cache.uri,
							store_dir()
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
		// `nix-cache-info` only sets defaults; URL parameters win.
		if let Some(value) = want_mass_query {
			cache.info.want_mass_query = value;
		}
		if let Some(value) = priority {
			cache.info.priority = value;
		}
		Ok(cache)
	}

	/// The upload settings from the cache URL's parameters.
	pub fn upload_settings(&self) -> &UploadSettings {
		&self.upload
	}

	/// Prepare a `file://` cache for writing as `LocalBinaryCacheStore::init`
	/// does: create its directories and, if it has none, `nix-cache-info`.
	pub fn init_for_upload(&self) -> Result<()> {
		let Transport::File(dir) = &self.transport else {
			return Err(Error::Other(
				"binary cache uploads require a file:// cache".into(),
			));
		};
		let mut subdirs = vec!["nar", "build-trace-v2"];
		if self.upload.write_debug_info {
			subdirs.push("debuginfo");
		}
		subdirs.push("log");
		for sub in subdirs {
			fs::create_dir_all(dir.join(sub))
				.map_err(|e| Error::io(format!("creating binary cache '{}'", self.uri), e))?;
		}
		if self.get_file("nix-cache-info")?.is_none() {
			write_atomic(
				&dir.join("nix-cache-info"),
				format!("StoreDir: {}\n", store_dir()).as_bytes(),
			)?;
		}
		Ok(())
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

	/// Export `path` and its reference closure to a local cache, with the
	/// compression, listings and signing keys of the cache URL plus
	/// `signing_keys`. NAR bytes must match source metadata before signing.
	/// Dependencies are published first; a failed upload may leave completed
	/// dependencies behind. An existing narinfo keeps its signatures. The
	/// source must permit temporary GC roots, which remain until its last
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
		self.check_upload(source, signing_keys)?;
		self.init_for_upload()?;
		// Revalidate existing cache ownership even if it changed after open.
		BinaryCache::open(&self.uri)?;
		self.upload_path(
			source,
			path,
			signing_keys,
			&mut BTreeSet::new(),
			&mut BTreeSet::new(),
		)
	}

	/// Upload one path whose references are already in the cache, as
	/// `BinaryCacheStore::addMultipleToStore` does for each path `nix copy`
	/// sends. Call [`BinaryCache::init_for_upload`] first.
	pub fn upload<S>(
		&self,
		source: &LocalStore,
		path: &StorePath,
		signing_keys: &[SigningKey<S>],
	) -> Result<()>
	where
		S: ed25519::signature::Signer<ed25519::Signature>,
	{
		self.check_upload(source, signing_keys)?;
		source.add_temp_root(path)?;
		let info = source
			.query_path_info(path)?
			.ok_or_else(|| Error::InvalidPath(print(path)))?;
		// `uploadNarInfo`'s check, made before anything is written.
		for reference in &info.references {
			if reference != path && self.narinfo_text(reference)?.is_none() {
				return Err(Error::Other(format!(
					"cannot add '{}' to the binary cache because the reference '{}' is not valid",
					print(path),
					print(reference)
				)));
			}
		}
		self.publish(source, &info, signing_keys)
	}

	fn check_upload<S>(&self, source: &LocalStore, signing_keys: &[SigningKey<S>]) -> Result<()>
	where
		S: ed25519::signature::Signer<ed25519::Signature>,
	{
		if !matches!(
			self.upload.compression.as_str(),
			"none" | "xz" | "lzma" | "gzip" | "bzip2" | "zstd"
		) {
			return Err(Error::Other(format!(
				"unsupported cache upload compression '{}'",
				self.upload.compression
			)));
		}
		if !matches!(self.transport, Transport::File(_)) {
			return Err(Error::Other(
				"binary cache uploads require a file:// cache".into(),
			));
		}
		if source.config().read_only {
			return Err(Error::Other(
				"binary cache upload requires source GC protection".into(),
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
		Ok(())
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
		self.publish(source, &info, signing_keys)?;
		visiting.remove(path);
		done.insert(path.clone());
		Ok(())
	}

	/// `BinaryCacheStore::uploadData` and `uploadNarInfo` for one path:
	/// the optional listing and debug info index, the NAR, then the narinfo.
	fn publish<S>(
		&self,
		source: &LocalStore,
		info: &ValidPathInfo,
		signing_keys: &[SigningKey<S>],
	) -> Result<()>
	where
		S: ed25519::signature::Signer<ed25519::Signature>,
	{
		let path = &info.path;
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
		let listing = if self.upload.write_nar_listing || self.upload.write_debug_info {
			temporary.rewind()?;
			Some(
				crate::nar_listing::parse_listing(&mut BufReader::new(temporary.as_file_mut()))
					.map_err(Error::Other)?,
			)
		} else {
			None
		};
		let started = Instant::now();
		let compression = self.upload.compression.as_str();
		let (file_hash, file_size) = if compression == "none" {
			(hash, size)
		} else {
			temporary.rewind()?;
			let mut compressed = NamedTempFile::new_in(dir.join("nar"))?;
			{
				let mut writer = BufWriter::new(compressed.as_file_mut());
				compress_nar(
					&self.upload,
					&mut BufReader::new(temporary.as_file_mut()),
					&mut writer,
				)?;
				writer.flush()?;
			}
			compressed.rewind()?;
			let mut sink = HashSink::new();
			io::copy(compressed.as_file_mut(), &mut sink)?;
			temporary = compressed;
			sink.finish()
		};
		let suffix = match compression {
			"xz" => ".xz",
			"bzip2" => ".bz2",
			"zstd" => ".zst",
			"lzip" => ".lzip",
			"lz4" => ".lz4",
			"br" => ".br",
			_ => "",
		};
		let url = format!(
			"nar/{}.nar{suffix}",
			nix_compat::nixbase32::encode(&file_hash)
		);
		if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Talkative) {
			eprintln!(
				"copying path '{}' ({size} bytes, compressed {:.1}% in {} ms) to binary cache",
				print(path),
				(1.0 - file_size as f64 / size as f64) * 100.0,
				started.elapsed().as_millis()
			);
		}
		let hash_part = nix_compat::nixbase32::encode(path.digest());
		if let Some(listing) = &listing {
			if self.upload.write_nar_listing {
				let mut json = String::from("{\"root\":");
				listing_json(listing, &mut json);
				json.push_str(",\"version\":1}");
				write_atomic(
					&local_cache_path(dir, &format!("{hash_part}.ls"))?,
					json.as_bytes(),
				)?;
			}
			if self.upload.write_debug_info {
				self.index_debug_info(dir, listing, &url)?;
			}
		}
		let metadata_path = local_cache_path(dir, &format!("{hash_part}.narinfo"))?;
		// Serialize signature merging and publication between Rust exporters.
		let mut publication_lock = PathLocks::lock([metadata_path.clone()])?;
		// CppNix takes no lock, so leave no lock file in the cache.
		publication_lock.set_deletion(true);
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
		let keys = self
			.upload
			.secret_key_files
			.iter()
			.map(|file| {
				let text = fs::read_to_string(file)
					.map_err(|e| Error::io(format!("opening file \"{file}\""), e))?;
				nix_compat::narinfo::parse_keypair(text.trim())
					.map(|(key, _)| key)
					.map_err(|e| Error::Other(e.to_string()))
			})
			.collect::<Result<Vec<_>>>()?;
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
			compression: Some(compression),
			file_hash: Some(file_hash),
			file_size: Some(file_size),
		};
		for key in &keys {
			narinfo.add_signature(key);
		}
		for key in signing_keys {
			narinfo.add_signature(key);
		}
		narinfo
			.signatures
			.sort_by(|a, b| a.name().cmp(b.name()).then(a.bytes().cmp(b.bytes())));
		narinfo.signatures.dedup();
		let nar_path = local_cache_path(dir, &url)?;
		// `uploadData` keeps an existing NAR of the same file hash; anything
		// but a regular file there fails publication.
		if !nar_path.is_file() {
			persist_atomic(temporary, &nar_path)?;
		}
		write_atomic(&metadata_path, narinfo.to_string().as_bytes())?;
		Ok(())
	}

	/// `uploadData`'s `index-debug-info`: a `debuginfo/<build-id>` link for
	/// each `lib/debug/.build-id/xx/<38 hex>.debug` file of the NAR.
	fn index_debug_info(&self, dir: &Path, listing: &Listing, url: &str) -> Result<()> {
		let is_hex = |name: &[u8]| name.iter().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'));
		let Some(Listing::Directory(prefixes)) =
			listing.find(&[b"lib".as_slice(), b"debug", b".build-id"])
		else {
			return Ok(());
		};
		for (s1, entry) in prefixes {
			let Listing::Directory(files) = entry else {
				continue;
			};
			if s1.len() != 2 || !is_hex(s1) {
				continue;
			}
			for (s2, file) in files {
				let Some(stem) = s2.strip_suffix(b".debug") else {
					continue;
				};
				if !matches!(file, Listing::Regular { .. }) || stem.len() != 38 || !is_hex(stem) {
					continue;
				}
				let s1 = String::from_utf8_lossy(s1);
				let s2 = String::from_utf8_lossy(s2);
				let key = local_cache_path(dir, &format!("debuginfo/{s1}{s2}"))?;
				if key.exists() {
					continue;
				}
				let mut json = String::from("{\"archive\":");
				json_string(format!("../{url}").as_bytes(), &mut json);
				json.push_str(",\"member\":");
				json_string(
					format!("lib/debug/.build-id/{s1}/{s2}").as_bytes(),
					&mut json,
				);
				json.push('}');
				fs::create_dir_all(dir.join("debuginfo"))?;
				write_atomic(&key, json.as_bytes())?;
			}
		}
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
		// `queryPathInfoUncached`'s activity. CppNix keeps answers in
		// memory, so a path is only announced the first time it is asked
		// for.
		let _act = self
			.announced
			.lock()
			.unwrap()
			.insert(path.clone())
			.then(|| {
				let shown = print(path);
				nix_compat::log::logger::Activity::new(
					nix_compat::log::VerbosityLevel::Talkative,
					nix_compat::log::ActivityType::QueryPathInfo,
					format!("querying info about '{shown}' on '{}'", self.uri),
					vec![shown.into(), self.uri.clone().into()],
					0,
				)
			});
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

/// A NAR being copied, reporting the bytes read as the `copy-path`
/// activity's progress, as `copyStorePath`'s progress sink does. Reports are
/// made every 64 KiB and once at the end.
struct ProgressReader<'a> {
	inner: Box<dyn BufRead + Send>,
	act: Option<&'a nix_compat::log::logger::Activity>,
	total: u64,
	reported: u64,
	size: u64,
}

impl ProgressReader<'_> {
	fn advance(&mut self, n: usize) {
		self.total += n as u64;
		if self.total - self.reported >= 65536 {
			self.report();
		}
	}

	fn report(&mut self) {
		if self.total != self.reported || self.total == 0 {
			self.reported = self.total;
			if let Some(act) = self.act {
				act.progress(self.total, self.size, 0, 0);
			}
		}
	}
}

impl Read for ProgressReader<'_> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let n = self.inner.read(buf)?;
		self.advance(n);
		Ok(n)
	}
}

impl BufRead for ProgressReader<'_> {
	fn fill_buf(&mut self) -> io::Result<&[u8]> {
		self.inner.fill_buf()
	}

	fn consume(&mut self, amt: usize) {
		self.inner.consume(amt);
		self.advance(amt);
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

/// The `BinaryCacheStoreConfig` settings that shape uploads, from the
/// cache URL's parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadSettings {
	/// `compression`, `xz` by default.
	pub compression: String,
	/// `compression-level`; `-1` selects the method's default.
	pub compression_level: i32,
	/// `parallel-compression`; unset means on for zstd only.
	pub parallel_compression: Option<bool>,
	/// `write-nar-listing`: write `HASH.ls` beside each narinfo.
	pub write_nar_listing: bool,
	/// `index-debug-info`: index DWARF files under `debuginfo/`.
	pub write_debug_info: bool,
	/// `secret-key` followed by the `secret-keys` files.
	pub secret_key_files: Vec<String>,
}

impl Default for UploadSettings {
	fn default() -> Self {
		Self {
			compression: "xz".into(),
			compression_level: -1,
			parallel_compression: None,
			write_nar_listing: false,
			write_debug_info: false,
			secret_key_files: Vec::new(),
		}
	}
}

/// `BaseSetting<bool>::parse`.
fn parse_bool(value: &str) -> Option<bool> {
	match value {
		"true" | "yes" | "1" => Some(true),
		"false" | "no" | "0" => Some(false),
		_ => None,
	}
}

/// A string as nlohmann's `dump()` writes it.
fn json_string(bytes: &[u8], out: &mut String) {
	out.push('"');
	for c in String::from_utf8_lossy(bytes).chars() {
		match c {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			'\u{8}' => out.push_str("\\b"),
			'\u{c}' => out.push_str("\\f"),
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
			c => out.push(c),
		}
	}
	out.push('"');
}

/// `NarAccessor::getListing` as compact JSON with sorted keys.
fn listing_json(listing: &Listing, out: &mut String) {
	use std::fmt::Write as _;
	match listing {
		Listing::Regular {
			executable,
			size,
			offset,
		} => {
			let _ = write!(
				out,
				"{{\"executable\":{executable},\"narOffset\":{offset},\"size\":{size},\"type\":\"regular\"}}"
			);
		}
		Listing::Directory(entries) => {
			out.push_str("{\"entries\":{");
			for (i, (name, entry)) in entries.iter().enumerate() {
				if i > 0 {
					out.push(',');
				}
				json_string(name, out);
				out.push(':');
				listing_json(entry, out);
			}
			out.push_str("},\"type\":\"directory\"}");
		}
		Listing::Symlink(target) => {
			out.push_str("{\"target\":");
			json_string(target, out);
			out.push_str(",\"type\":\"symlink\"}");
		}
	}
}

/// `makeCompressionSink` with the reference's encoders and settings: xz and
/// lzma through liblzma and bzip2 at libarchive's default level 9, which
/// reproduce its bytes, and zstd in independent 16 MiB frames.
fn compress_nar(
	settings: &UploadSettings,
	reader: &mut dyn Read,
	writer: &mut dyn Write,
) -> io::Result<()> {
	let level = settings.compression_level;
	let parallel = settings
		.parallel_compression
		.unwrap_or(settings.compression == "zstd");
	match settings.compression.as_str() {
		"xz" => crate::xz::compress(crate::xz::Format::Xz, level, parallel, reader, writer)?,
		"lzma" => crate::xz::compress(crate::xz::Format::Lzma, level, parallel, reader, writer)?,
		"gzip" => {
			// libarchive's header records the time and a Unix origin.
			let now = std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.map_or(0, |d| d.as_secs() as u32);
			let level = if (0..=9).contains(&level) {
				level as u32
			} else {
				6
			};
			let mut encoder = flate2::GzBuilder::new()
				.mtime(now)
				.operating_system(3)
				.write(writer, flate2::Compression::new(level));
			io::copy(reader, &mut encoder)?;
			encoder.finish()?;
		}
		"bzip2" => {
			let level = if (1..=9).contains(&level) {
				level as u32
			} else {
				9
			};
			let mut encoder = bzip2::write::BzEncoder::new(writer, bzip2::Compression::new(level));
			io::copy(reader, &mut encoder)?;
			encoder.finish()?;
		}
		"zstd" => compress_zstd(level, parallel, reader, writer)?,
		_ => return Err(io::Error::other("unsupported cache upload compression")),
	}
	Ok(())
}

/// `ZstdMultiFrameCompressionSink`: each 16 MiB of input is one frame
/// pledged at its exact size; parallel compression uses up to four workers.
fn compress_zstd(
	level: i32,
	parallel: bool,
	reader: &mut dyn Read,
	writer: &mut dyn Write,
) -> io::Result<()> {
	use zstd::zstd_safe::{self, CCtx, CParameter, InBuffer, OutBuffer, ResetDirective};
	const FRAME: usize = 16 * 1024 * 1024;
	let error =
		|code: usize| io::Error::other(format!("zstd error: {}", zstd_safe::get_error_name(code)));
	let mut cctx = CCtx::create();
	if level != -1 {
		cctx.set_parameter(CParameter::CompressionLevel(level))
			.map_err(error)?;
	}
	if parallel {
		let cpus = std::thread::available_parallelism()
			.map_or(1, |n| n.get())
			.min(4);
		if cpus > 1 {
			// Ignored if libzstd lacks threads, as in the reference.
			let _ = cctx.set_parameter(CParameter::NbWorkers(cpus as u32));
		}
	}
	let mut output = vec![0u8; CCtx::out_size()];
	let mut frame = Vec::new();
	let mut emitted = false;
	let mut eof = false;
	while !eof {
		frame.clear();
		while frame.len() < FRAME {
			let start = frame.len();
			frame.resize((start + 65536).min(FRAME), 0);
			let n = loop {
				match reader.read(&mut frame[start..]) {
					Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
					result => break result?,
				}
			};
			frame.truncate(start + n);
			if n == 0 {
				eof = true;
				break;
			}
		}
		if frame.is_empty() && emitted {
			break;
		}
		cctx.reset(ResetDirective::SessionOnly).map_err(error)?;
		cctx.set_pledged_src_size(Some(frame.len() as u64))
			.map_err(error)?;
		let mut input = InBuffer::around(&frame);
		loop {
			let mut out = OutBuffer::around(&mut output[..]);
			let remaining = cctx
				.compress_stream2(
					&mut out,
					&mut input,
					zstd_safe::zstd_sys::ZSTD_EndDirective::ZSTD_e_end,
				)
				.map_err(error)?;
			let n = out.pos();
			writer.write_all(&output[..n])?;
			if remaining == 0 {
				break;
			}
		}
		emitted = true;
	}
	Ok(())
}

/// Wrap `reader` in the decompressor for a narinfo `Compression` value.
pub fn decompress(
	method: Option<&str>,
	reader: Box<dyn BufRead + Send>,
) -> Result<Box<dyn BufRead + Send>> {
	let reader: Box<dyn Read + Send> = match method {
		None | Some("none") | Some("") => return Ok(reader),
		Some("xz") => Box::new(lzma_rust2::XzReader::new(reader, true)),
		Some("lzma") => Box::new(
			lzma_rust2::LzmaReader::new_mem_limit(reader, u32::MAX, None)
				.map_err(|e| Error::io("starting lzma decompression", e))?,
		),
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
			// `PathSubstitutionGoal::tryToRun` does. The substitution goal's
			// counts and its `substitute` and `copy-path` activities are
			// reported to the running worker.
			use nix_compat::log::logger::{Activity, worker_update};
			use nix_compat::log::{ActivityType, VerbosityLevel};
			let nar_size = cached.info.nar_size;
			let file_size = cached.file_size.unwrap_or(0);
			worker_update(|c| {
				c.expected_substitutions += 1;
				c.expected_nar_size += nar_size;
				c.expected_download_size += file_size;
			});
			worker_update(|c| c.running_substitutions += 1);
			// Outside a worker, as for `nix copy`, the caller reports the
			// copy itself, as `copyPaths` does.
			let acts = nix_compat::log::logger::worker_active().then(|| {
				let shown = print(path);
				let substitute = Activity::new(
					VerbosityLevel::Error,
					ActivityType::Substitute,
					"",
					vec![shown.clone().into(), cache.uri.clone().into()],
					0,
				);
				let copy = Activity::new(
					VerbosityLevel::Info,
					ActivityType::CopyPath,
					format!("copying path '{shown}' from '{}'", cache.uri),
					vec![shown.into(), cache.uri.clone().into(), "local://".into()],
					substitute.id(),
				);
				(substitute, copy)
			});
			let result = cache.nar(&cached).and_then(|nar| {
				let mut nar = ProgressReader {
					inner: nar,
					act: acts.as_ref().map(|(_, copy)| copy),
					total: 0,
					reported: 0,
					size: nar_size,
				};
				let result = self.add_to_store(&cached.info, &mut nar, false, !cache.trusted);
				nar.report();
				result
			});
			// The copy activity stops before its parent.
			if let Some((substitute, copy)) = acts {
				drop(copy);
				drop(substitute);
			}
			let succeeded = result.is_ok();
			worker_update(|c| {
				c.running_substitutions = c.running_substitutions.saturating_sub(1);
				c.expected_substitutions = c.expected_substitutions.saturating_sub(1);
				c.expected_nar_size = c.expected_nar_size.saturating_sub(nar_size);
				c.expected_download_size = c.expected_download_size.saturating_sub(file_size);
				if succeeded {
					c.done_substitutions += 1;
					c.done_nar_size += nar_size;
					c.done_download_size += file_size;
				} else {
					c.failed_substitutions += 1;
				}
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
	#[test]
	fn standalone_lzma_cache_streams_decode_and_reject_truncation() {
		// Produced by xz --format=lzma, the standalone format accepted by
		// CppNix's compression.cc ArchiveDecompressionSource.
		const DATA: &[u8] = &[
			93, 0, 0, 128, 0, 255, 255, 255, 255, 255, 255, 255, 255, 0, 55, 27, 203, 119, 46, 24,
			29, 84, 35, 72, 116, 154, 75, 117, 195, 229, 249, 242, 172, 153, 171, 74, 213, 221, 46,
			62, 54, 107, 127, 255, 223, 184, 0, 0,
		];
		let mut decoded = Vec::new();
		super::decompress(Some("lzma"), Box::new(std::io::Cursor::new(DATA)))
			.unwrap()
			.read_to_end(&mut decoded)
			.unwrap();
		assert_eq!(decoded, b"noxi lzma cache fixture\n");
		assert!(
			super::decompress(Some("lzma"), Box::new(std::io::Cursor::new(&DATA[..5]))).is_err()
		);
	}
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
