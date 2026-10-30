//! Evaluator store state, read-only by default. Writable mode registers
//! text, imported paths and derivations in an explicitly diverted local
//! store, while retaining CppNix 2.35.2's logical `/nix/store` paths.

use std::cell::{RefCell, RefMut};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bstr::ByteSlice;
use nix_compat::derivation::{Derivation, UnverifiedDerivation};
use nix_compat::nixhash::{HashAlgo, NixHash};
use nix_compat::store_path::{self, STORE_DIR, StorePath};
use nix_rs_eval::{
	EvalIO, FileType, NixAttrs, NixContext, NixContextElement, NixString, StdIO, Value,
};
use nix_rs_store::{
	CapturedSource, DerivedPath, FileIngestionMethod, LocalStore, LocalStoreConfig, SourceCapture,
	Store,
};
use sha2::{Digest, Sha256};

use crate::errors::ImportError;
use crate::known_paths::KnownPaths;
use crate::mount::Mount;
use crate::nar::{self, EntryType, Filter};

/// State shared by the store-aware builtins of one evaluation.
#[derive(Default)]
pub struct EvaluatorStore {
	known_paths: RefCell<KnownPaths>,
	/// The store that registers text, imports and derivations; `None`
	/// evaluates read-only, computing paths without writing them.
	store: Option<Rc<dyn Store>>,
	real_store_dir: Option<PathBuf>,
	read_access: RefCell<Option<Rc<dyn Store>>>,
	opener: Option<Box<dyn Fn() -> io::Result<Option<Rc<dyn Store>>>>>,
	allow_ifd: std::cell::Cell<Option<bool>>,
	trace_ifd: std::cell::Cell<bool>,
	/// Fetched trees served at logical store paths, keyed by store path.
	mounts: RefCell<BTreeMap<PathBuf, Mount>>,
	pub(crate) fetch_config: RefCell<crate::fetch::FetchConfig>,
	pub(crate) fetch_state: RefCell<crate::fetch::FetchState>,
}

impl std::fmt::Debug for EvaluatorStore {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("EvaluatorStore")
			.field("known_paths", &self.known_paths)
			.field("writable", &self.store.is_some())
			.field("real_store_dir", &self.real_store_dir)
			.finish()
	}
}

/// A source captured once, before any store registration.
pub(crate) struct CapturedImport {
	name: String,
	bytes: CapturedSource,
	recursive: bool,
	hash: NixHash,
	path: StorePath,
}

/// Bound the resident capture while preserving exactly the bytes that were hashed.
const IMPORT_MEMORY_LIMIT: usize = 8 * 1024 * 1024;

/// Compatibility name for the default, read-only evaluator store.
pub type ReadOnlyStore = EvaluatorStore;

impl EvaluatorStore {
	pub fn new() -> Rc<Self> {
		Rc::new(Self::default())
	}

	fn diverted_config(root: &Path) -> io::Result<LocalStoreConfig> {
		LocalStoreConfig::diverted(root)
	}

	/// Map a diverted store without opening its database or creating files.
	pub fn read_only(root: &Path) -> io::Result<Rc<Self>> {
		let config = Self::diverted_config(root)?;
		Ok(Self::with_read_access(
			Some(config.real_store_dir.clone()),
			move || {
				LocalStore::open(config.clone())
					.map(|store| Some(Rc::new(store) as Rc<dyn Store>))
					.map_err(io::Error::other)
			},
		))
	}

	/// Compute values without registration while querying an already opened
	/// owner. Legacy commands open their owner before evaluation.
	pub fn with_read_store(store: Rc<dyn Store>, real_store_dir: Option<PathBuf>) -> Rc<Self> {
		Rc::new(Self {
			real_store_dir,
			read_access: RefCell::new(Some(store)),
			..Self::default()
		})
	}

	/// Compute store paths without registration, opening the selected owner
	/// only when context realization or derivation inspection needs it.
	pub fn with_read_access(
		real_store_dir: Option<PathBuf>,
		opener: impl Fn() -> io::Result<Option<Rc<dyn Store>>> + 'static,
	) -> Rc<Self> {
		Rc::new(Self {
			real_store_dir,
			opener: Some(Box::new(opener)),
			..Self::default()
		})
	}

	fn access_store(&self) -> io::Result<Option<Rc<dyn Store>>> {
		if let Some(store) = &self.store {
			return Ok(Some(store.clone()));
		}
		if let Some(store) = self.read_access.borrow().as_ref() {
			return Ok(Some(store.clone()));
		}
		let store = match &self.opener {
			Some(open) => open()?,
			None => None,
		};
		*self.read_access.borrow_mut() = store.clone();
		Ok(store)
	}

	/// Whether output-context access may build during evaluation.
	pub fn allow_import_from_derivation(&self, enabled: bool) {
		self.allow_ifd.set(Some(enabled));
	}

	/// Log the reference's warning for every output-context realization.
	pub fn trace_import_from_derivation(&self, enabled: bool) {
		self.trace_ifd.set(enabled);
	}

	/// Open a writable `local?root=ROOT` store. ROOT must already exist.
	/// Logical paths remain `/nix/store`; system stores are never opened.
	pub fn writable(root: &Path) -> io::Result<Rc<Self>> {
		Self::writable_with_settings(root, &nix_compat::nixcpp::settings::Settings::default())
	}

	pub fn writable_with_settings(
		root: &Path,
		settings: &nix_compat::nixcpp::settings::Settings,
	) -> io::Result<Rc<Self>> {
		let mut config = Self::diverted_config(root)?;
		config.apply_config(settings);
		let real_store_dir = Some(config.real_store_dir.clone());
		let local = LocalStore::open(config).map_err(io::Error::other)?;
		Ok(Self::with_store(Rc::new(local), real_store_dir))
	}

	/// Register paths in `store`, whose paths appear under
	/// `real_store_dir` when it is not `/nix/store`.
	pub fn with_store(store: Rc<dyn Store>, real_store_dir: Option<PathBuf>) -> Rc<Self> {
		Rc::new(Self {
			known_paths: RefCell::default(),
			store: Some(store),
			real_store_dir,
			..Self::default()
		})
	}

	/// The store paths are registered in, if evaluation is writable.
	pub fn store(&self) -> Option<&dyn Store> {
		self.store.as_deref()
	}

	pub fn add_text(
		&self,
		name: &str,
		contents: &[u8],
		references: &BTreeSet<StorePath>,
	) -> io::Result<StorePath> {
		if let Some(store) = &self.store {
			return store
				.add_text_to_store(name, contents, references)
				.map_err(io::Error::other);
		}
		store_path::build_text_path(name, contents, references.iter().map(StorePath::as_ref))
			.map(|p| p.to_owned())
			.map_err(io::Error::other)
	}

	/// Load dependency hashes from the selected owner before constructing a
	/// derivation from output contexts created in an earlier evaluation.
	pub(crate) fn load_context(&self, context: &NixContext) -> io::Result<()> {
		for element in context.iter() {
			let (path, deep) = match element {
				NixContextElement::Plain(_) => continue,
				NixContextElement::Single { derivation, .. } => (derivation, false),
				NixContextElement::Derivation(path) => (path, true),
			};
			let path = StorePath::from_absolute_path(path.as_bytes()).map_err(io::Error::other)?;
			self.load_derivation(&path)?;
			if deep && let Some(store) = self.access_store()? {
				let mut pending = vec![path];
				let mut seen = BTreeSet::new();
				while let Some(path) = pending.pop() {
					if !seen.insert(path.clone()) {
						continue;
					}
					let info = store
						.query_path_info(&path)
						.map_err(io::Error::other)?
						.ok_or_else(|| {
							io::Error::other(format!(
								"path '{}' is not valid",
								path.to_absolute_path()
							))
						})?;
					pending.extend(info.references.iter().cloned());
					self.known_paths
						.borrow_mut()
						.add_text(path.clone(), info.references);
					if path.name().ends_with(".drv") {
						self.load_derivation(&path)?;
					}
				}
			}
		}
		Ok(())
	}

	fn load_derivation(&self, path: &StorePath) -> io::Result<()> {
		if self
			.known_paths
			.borrow()
			.get_unverified_drv(&path.as_ref())
			.is_some()
		{
			return Ok(());
		}
		let store = self
			.access_store()?
			.ok_or_else(|| io::Error::other("cannot read a derivation without a store"))?;
		let mut pending: Vec<(StorePath, Option<UnverifiedDerivation>)> =
			vec![(path.clone(), None)];
		let mut active = BTreeSet::new();
		while let Some((path, drv)) = pending.pop() {
			if self
				.known_paths
				.borrow()
				.get_unverified_drv(&path.as_ref())
				.is_some()
			{
				continue;
			}
			if let Some(drv) = drv {
				active.remove(&path);
				self.known_paths
					.borrow_mut()
					.add_imported_derivation(path, drv)
					.map_err(io::Error::other)?;
				continue;
			}
			if !active.insert(path.clone()) {
				return Err(io::Error::other("cycle in derivation dependencies"));
			}
			let drv = store.read_derivation(&path).map_err(io::Error::other)?;
			let inputs: Vec<_> = drv.input_derivations().keys().cloned().collect();
			pending.push((path, Some(drv)));
			pending.extend(inputs.into_iter().rev().map(|path| (path, None)));
		}
		Ok(())
	}

	pub(crate) fn register_derivation(&self, drv: &Derivation) -> io::Result<()> {
		if self.store.is_some() {
			let references = drv
				.input_sources()
				.iter()
				.chain(drv.input_derivations().keys())
				.cloned()
				.collect();
			let path = self.add_text(drv.drv_path().name(), &drv.to_aterm_bytes(), &references)?;
			if &path != drv.drv_path() {
				return Err(io::Error::other(
					"registered derivation path differs from evaluator path",
				));
			}
		}
		Ok(())
	}

	/// The mount serving `path` and the path relative to its root.
	pub(crate) fn mount_of(&self, path: &Path) -> Option<(Mount, PathBuf)> {
		let mounts = self.mounts.borrow();
		if mounts.is_empty() {
			return None;
		}
		let (store_path, rest) = StorePath::from_absolute_path_full(path).ok()?;
		let mount = mounts.get(Path::new(&store_path.to_absolute_path()))?;
		Some((mount.clone(), rest.to_owned()))
	}

	/// Where `path` can be read, if it is mounted or valid in the store.
	pub(crate) fn valid_store_path(&self, path: &StorePath) -> Option<PathBuf> {
		if self.is_mounted(path) {
			return Some(self.physical_path(Path::new(&path.to_absolute_path())));
		}
		let store = self.access_store().ok()??;
		store
			.is_valid_path(path)
			.ok()?
			.then(|| store.real_path(path))
	}

	/// Hash a fetched tree as a NAR and make it available at its store
	/// path: copied into a writable store, otherwise served from `mount`.
	pub(crate) fn add_tree(&self, name: &str, mount: &Mount) -> io::Result<(StorePath, NixHash)> {
		let mut keep = |p: &Path, _| Ok(mount.allows_physical(p));
		let filter: Option<&mut Filter<'_>> = if mount.filter.is_some() {
			Some(&mut keep)
		} else {
			None
		};
		if let Some(store) = &self.store {
			let mut capture = SourceCapture::new(IMPORT_MEMORY_LIMIT);
			nar::dump_path(&mut capture, &mount.root, filter)?;
			let captured = capture.finish()?;
			let hash = NixHash::Sha256(captured.sha256());
			let path = ca_path(name, true, &hash, &mount.root)?;
			if !store.is_valid_path(&path).map_err(io::Error::other)? {
				store
					.add_to_store_from_capture(
						captured,
						name,
						FileIngestionMethod::NixArchive,
						&BTreeSet::new(),
					)
					.map_err(io::Error::other)?;
			}
			return Ok((path, hash));
		}
		let mut hasher = Sha256::new();
		nar::dump_path(&mut hasher, &mount.root, filter)?;
		let hash = NixHash::Sha256(hasher.finalize().into());
		let path = ca_path(name, true, &hash, &mount.root)?;
		self.mount(&path, mount.clone());
		Ok((path, hash))
	}

	/// Whether `path` is the root of, or inside, a mounted tree.
	pub fn is_mounted(&self, path: &StorePath) -> bool {
		self.mounts
			.borrow()
			.contains_key(Path::new(&path.to_absolute_path()))
	}

	/// Serve `root` at `path` for the rest of the evaluation.
	pub(crate) fn mount(&self, path: &StorePath, mount: Mount) {
		self.mounts
			.borrow_mut()
			.insert(PathBuf::from(path.to_absolute_path()), mount);
	}

	/// The mount whose physical root contains `physical`.
	fn mount_at_physical(&self, physical: &Path) -> Option<(PathBuf, Mount)> {
		self.mounts
			.borrow()
			.iter()
			.filter(|(_, mount)| physical.starts_with(&mount.root))
			.max_by_key(|(_, mount)| mount.root.as_os_str().len())
			.map(|(logical, mount)| (logical.clone(), mount.clone()))
	}

	/// Check that a mounted `path` is visible through its tree's filter.
	pub(crate) fn check_visible(&self, path: &Path) -> io::Result<()> {
		if let Some((mount, rel)) = self.mount_of(path)
			&& let Some(filter) = &mount.filter
			&& !filter.allows(&rel)
		{
			return Err(filter.not_allowed(&rel));
		}
		Ok(())
	}

	/// Dump `physical` as a NAR, hiding what its mount's filter hides.
	pub(crate) fn dump_filtered<W: io::Write>(
		&self,
		writer: &mut W,
		physical: &Path,
		mut filter: Option<&mut Filter<'_>>,
	) -> io::Result<()> {
		let mount = self
			.mount_at_physical(physical)
			.map(|(_, mount)| mount)
			.filter(|mount| mount.filter.is_some());
		match mount {
			None => nar::dump_path(writer, physical, filter),
			Some(mount) => {
				let mut combined = |p: &Path, ty: EntryType| -> io::Result<bool> {
					if !mount.allows_physical(p) {
						return Ok(false);
					}
					match filter.as_deref_mut() {
						Some(filter) => filter(p, ty),
						None => Ok(true),
					}
				};
				nar::dump_path(writer, physical, Some(&mut combined))
			}
		}
	}

	pub(crate) fn physical_path(&self, path: &Path) -> PathBuf {
		if let Some((mount, rest)) = self.mount_of(path) {
			return if rest.as_os_str().is_empty() {
				mount.root
			} else {
				mount.root.join(rest)
			};
		}
		if let Some(real) = &self.real_store_dir
			&& let Ok(rest) = path.strip_prefix(STORE_DIR)
		{
			return if rest.as_os_str().is_empty() {
				real.clone()
			} else {
				real.join(rest)
			};
		}
		path.to_owned()
	}

	pub(crate) fn logical_path(&self, path: &Path) -> PathBuf {
		if let Some((logical, mount)) = self.mount_at_physical(path)
			&& let Ok(rest) = path.strip_prefix(&mount.root)
		{
			return if rest.as_os_str().is_empty() {
				logical
			} else {
				logical.join(rest)
			};
		}
		if let Some(real) = &self.real_store_dir
			&& let Ok(rest) = path.strip_prefix(real)
		{
			return Path::new(STORE_DIR).join(rest);
		}
		path.to_owned()
	}

	pub(crate) fn capture_import(
		&self,
		path: &Path,
		name: &str,
		recursive: bool,
		filter: Option<&mut Filter<'_>>,
	) -> io::Result<CapturedImport> {
		let physical = self.physical_path(path);
		let mut capture = SourceCapture::new(IMPORT_MEMORY_LIMIT);
		if recursive {
			self.dump_filtered(&mut capture, &physical, filter)?;
		} else {
			io::copy(&mut std::fs::File::open(physical)?, &mut capture)?;
		};
		let bytes = capture.finish()?;
		let hash = NixHash::Sha256(bytes.sha256());
		let store_path = ca_path(name, recursive, &hash, path)?;
		Ok(CapturedImport {
			name: name.to_owned(),
			bytes,
			recursive,
			hash,
			path: store_path,
		})
	}

	pub(crate) fn import_captured(
		&self,
		captured: CapturedImport,
		expected: Option<&NixHash>,
	) -> io::Result<StorePath> {
		// Both hashing and ingestion use these bytes. No source filesystem
		// reads, or store writes, occur between verification and ingestion.
		if expected.is_some_and(|hash| hash != &captured.hash) {
			return Err(io::Error::other(
				"store path mismatch in (possibly filtered) path",
			));
		}
		if let Some(store) = &self.store {
			let method = if captured.recursive {
				FileIngestionMethod::NixArchive
			} else {
				FileIngestionMethod::Flat
			};
			return store
				.add_to_store_from_capture(captured.bytes, &captured.name, method, &BTreeSet::new())
				.map_err(io::Error::other);
		}
		Ok(captured.path)
	}

	pub fn known_paths(&self) -> RefMut<'_, KnownPaths> {
		self.known_paths.borrow_mut()
	}

	/// The store path a recursive (NAR) copy of `path` named `name` gets.
	pub fn import_nar(
		&self,
		path: &Path,
		name: &str,
		filter: Option<&mut Filter<'_>>,
	) -> io::Result<StorePath> {
		if self.store.is_some() {
			let captured = self.capture_import(path, name, true, filter)?;
			return self.import_captured(captured, None);
		}
		let mut hasher = Sha256::new();
		self.dump_filtered(&mut hasher, &self.physical_path(path), filter)?;
		let digest: [u8; 32] = hasher.finalize().into();
		ca_path(name, true, &NixHash::Sha256(digest), path)
	}

	/// The store path a flat copy of the regular file `path` gets, hashed
	/// with SHA-256.
	pub fn import_flat(&self, path: &Path, name: &str) -> io::Result<StorePath> {
		if self.store.is_some() {
			let captured = self.capture_import(path, name, false, None)?;
			return self.import_captured(captured, None);
		}
		let digest = nar::file_sha256(&self.physical_path(path))?;
		ca_path(name, false, &NixHash::Sha256(digest), path)
	}
}

pub(crate) fn ca_path(
	name: &str,
	recursive: bool,
	hash: &NixHash,
	path: &Path,
) -> io::Result<StorePath> {
	store_path::build_ca_path(name, recursive, hash, std::iter::empty(), false)
		.map(|path| path.to_owned())
		.map_err(|_| io_error(ImportError::InvalidName(path.to_owned())))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn expected_hash_is_checked_against_captured_bytes_before_registration() {
		for size in [8, IMPORT_MEMORY_LIMIT + 1] {
			check_capture_survives_replacement(size);
		}
	}

	fn check_capture_survives_replacement(size: usize) {
		for recursive in [false, true] {
			let root = tempfile::tempdir().unwrap();
			let source = tempfile::tempdir().unwrap();
			let file = source.path().join("source");
			let original = vec![b'o'; size];
			std::fs::write(&file, &original).unwrap();
			let store = EvaluatorStore::writable(root.path()).unwrap();
			let captured = store
				.capture_import(&file, "source", recursive, None)
				.unwrap();
			assert_eq!(captured.bytes.is_rolled(), size > IMPORT_MEMORY_LIMIT);
			let expected_hash = captured.hash.clone();
			let expected_path = captured.path.clone();
			// Deterministically replace the source between capture and ingestion.
			std::fs::write(&file, "replacement").unwrap();
			let wrong_hash = if recursive {
				nar::nar_sha256(&file, None).unwrap()
			} else {
				nar::file_sha256(&file).unwrap()
			};
			assert_ne!(expected_hash, NixHash::Sha256(wrong_hash));
			let rejected = store
				.capture_import(&file, "rejected", recursive, None)
				.unwrap();
			let rejected_path = rejected.path.clone();
			assert!(
				store
					.import_captured(rejected, Some(&expected_hash))
					.is_err()
			);
			assert!(
				!store
					.store()
					.unwrap()
					.is_valid_path(&rejected_path)
					.unwrap()
			);
			let imported = store
				.import_captured(captured, Some(&expected_hash))
				.unwrap();
			assert_eq!(imported, expected_path);
			let local = store.store().unwrap();
			assert_eq!(std::fs::read(local.real_path(&imported)).unwrap(), original);
			assert!(local.is_valid_path(&expected_path).unwrap());
			assert!(!local.is_valid_path(&rejected_path).unwrap());
		}
	}
}

pub(crate) fn io_error(error: ImportError) -> io::Error {
	io::Error::other(error)
}

/// The name CppNix gives a copy of `path`: its last component.
pub(crate) fn import_name(path: &Path) -> io::Result<&str> {
	let name = path
		.file_name()
		.ok_or_else(|| io_error(ImportError::InvalidName(path.to_owned())))?;
	if name.as_bytes().ends_with(b".drv") {
		return Err(io_error(ImportError::DerivationFileName(path.to_owned())));
	}
	store_path::validate_name(name.as_bytes())
		.map_err(|_| io_error(ImportError::InvalidName(path.to_owned())))
}

/// Where CppNix's built-in `<nix/...>` files appear. CppNix serves them from
/// an in-memory filesystem after the search path finds nothing.
pub const COREPKGS_DIR: &str = "/__corepkgs__";

const COREPKGS: &[(&str, &str)] = &[
	("fetchurl.nix", include_str!("corepkgs/fetchurl.nix")),
	("call-flake.nix", include_str!("corepkgs/call-flake.nix")),
];

enum Corepkg {
	Root,
	File(&'static str),
}

fn corepkg(path: &Path) -> Option<Corepkg> {
	let rest = path.strip_prefix(COREPKGS_DIR).ok()?;
	if rest.as_os_str().is_empty() {
		return Some(Corepkg::Root);
	}
	COREPKGS
		.iter()
		.find(|(name, _)| rest == Path::new(name))
		.map(|(_, contents)| Corepkg::File(contents))
}

/// Local file access and imports through an [`EvaluatorStore`].
pub struct StoreIO {
	store: Rc<ReadOnlyStore>,
	ca_derivations: bool,
}

impl StoreIO {
	pub fn new(store: Rc<ReadOnlyStore>) -> Self {
		Self {
			store,
			ca_derivations: false,
		}
	}

	/// Enable the reference's placeholder gate for deferred outputs.
	pub fn with_ca_derivations(mut self, enabled: bool) -> Self {
		self.ca_derivations = enabled;
		self
	}
}

impl EvalIO for StoreIO {
	fn path_exists(&self, path: &Path) -> io::Result<bool> {
		if self.store.check_visible(path).is_err() {
			return Ok(false);
		}
		if path.starts_with(COREPKGS_DIR) {
			return Ok(corepkg(path).is_some());
		}
		StdIO.path_exists(&self.store.physical_path(path))
	}

	fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
		if path.starts_with(COREPKGS_DIR) {
			return match corepkg(path) {
				Some(_) => Ok(path.to_owned()),
				None => Err(io::ErrorKind::NotFound.into()),
			};
		}
		let real = StdIO.canonicalize(&self.store.physical_path(path))?;
		Ok(self.store.logical_path(&real))
	}

	fn realise_path(&self, string: &NixString) -> io::Result<PathBuf> {
		let path = string.to_path().map_err(io::Error::other)?.to_owned();
		let elements: Vec<_> = string
			.iter_context()
			.flat_map(|context| context.iter())
			.filter(|element| match element {
				NixContextElement::Plain(path) => !StorePath::from_absolute_path(path.as_bytes())
					.is_ok_and(|path| self.store.is_mounted(&path)),
				_ => true,
			})
			.collect();
		if elements.is_empty() {
			return Ok(path);
		}
		let store = self
			.store
			.access_store()?
			.ok_or_else(|| io::Error::other("cannot realize a path context without a store"))?;
		let mut requests: BTreeMap<StorePath, BTreeSet<String>> = BTreeMap::new();
		for element in &elements {
			let raw = match element {
				NixContextElement::Plain(path) | NixContextElement::Derivation(path) => path,
				NixContextElement::Single { name, derivation } => {
					let drv = StorePath::from_absolute_path(derivation.as_bytes())
						.map_err(io::Error::other)?;
					requests.entry(drv).or_default().insert(name.clone());
					derivation
				}
			};
			let path = StorePath::from_absolute_path(raw.as_bytes()).map_err(io::Error::other)?;
			if !store.is_valid_path(&path).map_err(io::Error::other)? {
				return Err(io::Error::other(format!(
					"path '{}' is not valid",
					path.to_absolute_path()
				)));
			}
		}
		if requests.is_empty() {
			return Ok(path);
		}
		if !self.store.allow_ifd.get().unwrap_or(true) {
			let (drv, outputs) = requests.first_key_value().unwrap();
			return Err(io::Error::other(format!(
				"cannot build '{}^{}' during evaluation because the option 'allow-import-from-derivation' is disabled",
				drv.to_absolute_path(),
				outputs
					.first()
					.expect("context requests at least one output")
			)));
		}
		if self.store.trace_ifd.get()
			&& nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn)
		{
			let (drv, outputs) = requests.first_key_value().unwrap();
			eprintln!(
				"warning: built '{}^{}' during evaluation due to an import from derivation",
				drv.to_absolute_path(),
				outputs
					.first()
					.expect("context requests at least one output")
			);
		}
		let targets: Vec<_> = requests
			.iter()
			.map(|(drv, outputs)| DerivedPath::Built {
				drv: drv.clone(),
				outputs: outputs.clone(),
			})
			.collect();
		if self.store.store.is_none() {
			for (drv, names) in &requests {
				let outputs = store
					.query_derivation_outputs(drv)
					.map_err(io::Error::other)?;
				for name in names {
					let output = outputs.get(name).ok_or_else(|| {
						io::Error::other("cannot realize an unknown output in read-only evaluation")
					})?;
					if !store.is_valid_path(output).map_err(io::Error::other)? {
						return Err(io::Error::other(format!(
							"cannot substitute path '{}' - no write access to the Nix store",
							output.to_absolute_path()
						)));
					}
				}
			}
		} else {
			store.build_paths(&targets).map_err(io::Error::other)?;
		}
		let mut bytes = path.as_os_str().as_bytes().to_vec();
		for (drv, names) in requests {
			let outputs = store
				.query_derivation_outputs(&drv)
				.map_err(io::Error::other)?;
			for name in names {
				let output = outputs.get(&name).ok_or_else(|| {
					io::Error::other(format!(
						"derivation '{}' does not have an output named '{name}'",
						drv.to_absolute_path()
					))
				})?;
				let placeholder = store_path::downstream_placeholder(&drv, name.as_bytes());
				bytes = bytes.replace(placeholder.as_bytes(), output.to_absolute_path().as_bytes());
				store.add_temp_root(output).map_err(io::Error::other)?;
			}
		}
		Ok(PathBuf::from(OsString::from_vec(bytes)))
	}

	fn import_derivation(&self, path: &Path) -> io::Result<Option<Value>> {
		let Ok(drv_path) = StorePath::from_absolute_path(path.as_os_str().as_bytes()) else {
			return Ok(None);
		};
		if !drv_path.name().ends_with(".drv") {
			return Ok(None);
		}
		let Some(store) = self.store.access_store()? else {
			return Ok(None);
		};
		if !store.is_valid_path(&drv_path).map_err(io::Error::other)? {
			return Ok(None);
		}
		let drv = store.read_derivation(&drv_path).map_err(io::Error::other)?;
		let absolute = drv_path.to_absolute_path();
		let mut attrs: Vec<(NixString, Value)> = vec![
			(
				"drvPath".into(),
				NixString::new_context_from(
					NixContextElement::Derivation(absolute.clone()).into(),
					absolute.as_str(),
				)
				.into(),
			),
			(
				"name".into(),
				Value::from(
					drv.environment()
						.get(&bstr::BString::from("name"))
						.map(|s| s.as_slice())
						.unwrap_or_default(),
				),
			),
			(
				"outputs".into(),
				Value::List(
					drv.outputs()
						.names()
						.map(|name| Value::from(name.as_bytes()))
						.collect::<Vec<_>>()
						.into(),
				),
			),
		];
		for name in drv.outputs().names() {
			if drv.outputs().get(name).is_none() && !self.ca_derivations {
				return Err(io::Error::other(
					"experimental Nix feature 'ca-derivations' is disabled; add '--extra-experimental-features ca-derivations' to enable it",
				));
			}
			let output = drv
				.outputs()
				.get(name)
				.map(StorePath::to_absolute_path)
				.unwrap_or_else(|| store_path::downstream_placeholder(&drv_path, name.as_bytes()));
			let string = NixString::new_context_from(
				NixContextElement::Single {
					name: name.to_string(),
					derivation: absolute.clone(),
				}
				.into(),
				output,
			);
			attrs.push((NixString::from(name.as_bytes()), string.into()));
		}
		Ok(Some(Value::Attrs(NixAttrs::from_iter(attrs))))
	}

	fn read_file(&self, path: &Path) -> io::Result<NixString> {
		let mut bytes = Vec::new();
		self.open(path)?.read_to_end(&mut bytes)?;
		if bytes.contains(&0) {
			return Err(io::Error::other(format!(
				"the contents of the file '{}' cannot be represented as a Nix string",
				path.display()
			)));
		}
		let mut context = NixContext::new();
		if let Ok((stored, _)) = StorePath::from_absolute_path_full(path)
			&& let Some(store) = self.store.access_store()?
			&& let Ok(Some(info)) = store.query_path_info(&stored)
		{
			for reference in info.references {
				let absolute = reference.to_absolute_path();
				let start = STORE_DIR.len() + 1;
				let hash = &absolute.as_bytes()[start..start + 32];
				if bytes.windows(hash.len()).any(|window| window == hash) {
					context = context.append(NixContextElement::Plain(absolute));
				}
			}
		}
		Ok(NixString::new_context_from(context, bytes))
	}

	fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
		self.store.check_visible(path)?;
		StdIO.read_link(&self.store.physical_path(path))
	}

	fn open(&self, path: &Path) -> io::Result<Box<dyn io::Read>> {
		self.store.check_visible(path)?;
		match corepkg(path) {
			Some(Corepkg::File(contents)) => Ok(Box::new(contents.as_bytes())),
			_ => StdIO.open(&self.store.physical_path(path)),
		}
	}

	fn file_type(&self, path: &Path) -> io::Result<FileType> {
		self.store.check_visible(path)?;
		match corepkg(path) {
			Some(Corepkg::Root) => Ok(FileType::Directory),
			Some(Corepkg::File(_)) => Ok(FileType::Regular),
			None => StdIO.file_type(&self.store.physical_path(path)),
		}
	}

	fn read_dir(&self, path: &Path) -> io::Result<Vec<(bytes::Bytes, FileType)>> {
		self.store.check_visible(path)?;
		if self.store.real_store_dir.is_some() && path == Path::new(STORE_DIR) {
			return Err(io::Error::other("path '/' is not a valid store path"));
		}
		match corepkg(path) {
			Some(Corepkg::Root) => Ok(COREPKGS
				.iter()
				.map(|(name, _)| {
					(
						bytes::Bytes::from_static(name.as_bytes()),
						FileType::Regular,
					)
				})
				.collect()),
			_ => {
				let entries = StdIO.read_dir(&self.store.physical_path(path))?;
				match self.store.mount_of(path) {
					Some((
						Mount {
							filter: Some(filter),
							..
						},
						rel,
					)) => Ok(entries
						.into_iter()
						.filter(|(name, _)| filter.allows(&rel.join(OsStr::from_bytes(name))))
						.collect()),
					_ => Ok(entries),
				}
			}
		}
	}

	fn import_path(&self, path: &Path) -> io::Result<PathBuf> {
		let name = import_name(path)?;
		if let Some(Corepkg::File(contents)) = corepkg(path) {
			if let Some(store) = &self.store.store {
				let imported = store
					.add_to_store_from_dump(
						&mut contents.as_bytes(),
						name,
						FileIngestionMethod::Flat,
						FileIngestionMethod::NixArchive,
						HashAlgo::Sha256,
						&BTreeSet::new(),
					)
					.map_err(io::Error::other)?;
				return Ok(PathBuf::from(imported.to_absolute_path()));
			}
			let digest = nar::bytes_nar_sha256(contents.as_bytes())?;
			let imported = ca_path(name, true, &NixHash::Sha256(digest), path)?;
			return Ok(PathBuf::from(imported.to_absolute_path()));
		}
		// CppNix resolves symlinks in the ancestors only, so a symlink
		// named directly is copied as a symlink.
		let resolved = match path.parent() {
			Some(parent) => self.store.physical_path(parent).canonicalize()?.join(name),
			None => path.to_owned(),
		};
		let imported = self.store.import_nar(&resolved, name, None)?;
		Ok(PathBuf::from(imported.to_absolute_path()))
	}

	fn store_dir(&self) -> Option<String> {
		Some(STORE_DIR.to_owned())
	}

	fn get_env(&self, key: &OsStr) -> Option<OsString> {
		StdIO.get_env(key)
	}
}
