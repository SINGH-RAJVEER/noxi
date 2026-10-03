//! Evaluator store state, read-only by default. Writable mode registers
//! text, imported paths and derivations in an explicitly diverted local
//! store, while retaining CppNix 2.35.2's logical `/nix/store` paths.

use std::cell::{RefCell, RefMut};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use nix_compat::derivation::Derivation;
use nix_compat::nixhash::{HashAlgo, NixHash};
use nix_compat::store_path::{self, STORE_DIR, StorePath};
use nix_rs_eval::{EvalIO, FileType, StdIO};
use nix_rs_store::{FileIngestionMethod, LocalStore, LocalStoreConfig, Store};
use sha2::{Digest, Sha256};

use crate::errors::ImportError;
use crate::known_paths::KnownPaths;
use crate::nar::{self, Filter};

/// State shared by the store-aware builtins of one evaluation.
#[derive(Default)]
pub struct EvaluatorStore {
	known_paths: RefCell<KnownPaths>,
	/// The store that registers text, imports and derivations; `None`
	/// evaluates read-only, computing paths without writing them.
	store: Option<Rc<dyn Store>>,
	real_store_dir: Option<PathBuf>,
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
	bytes: Vec<u8>,
	recursive: bool,
	hash: NixHash,
	path: StorePath,
}

/// Compatibility name for the default, read-only evaluator store.
pub type ReadOnlyStore = EvaluatorStore;

impl EvaluatorStore {
	pub fn new() -> Rc<Self> {
		Rc::new(Self::default())
	}

	fn diverted_config(root: &Path) -> io::Result<LocalStoreConfig> {
		let root = root.canonicalize()?;
		if root == Path::new("/") || root.starts_with("/nix") || !root.is_dir() {
			return Err(io::Error::other(
				"writable evaluation requires a diverted store root",
			));
		}
		// Reject redirected store/state directories before LocalStore opens
		// anything. In particular, a root with nix -> /nix is not diverted.
		for relative in [
			"nix/store/.links",
			"nix/var/nix/profiles/per-user",
			"nix/var/nix/gcroots/per-user",
			"nix/var/nix/temproots",
			"nix/var/nix/gc.lock",
			"nix/var/nix/gc-socket/socket",
			"nix/var/nix/db/reserved",
			"nix/var/nix/db/big-lock",
			"nix/var/nix/db/schema",
			"nix/var/nix/db/db.sqlite",
			"nix/var/nix/db/db.sqlite-wal",
			"nix/var/nix/db/db.sqlite-shm",
			"nix/var/log/nix",
		] {
			let mut path = root.clone();
			for component in Path::new(relative).components() {
				path.push(component);
				match std::fs::symlink_metadata(&path) {
					Ok(meta) if meta.file_type().is_symlink() => {
						return Err(io::Error::other(
							"diverted store directories must not be symlinks",
						));
					}
					Ok(_) => {}
					Err(e) if e.kind() == io::ErrorKind::NotFound => {}
					Err(e) => return Err(e),
				}
			}
		}
		Ok(LocalStoreConfig::with_root(&root))
	}

	/// Map a diverted store without opening its database or creating files.
	pub fn read_only(root: &Path) -> io::Result<Rc<Self>> {
		let config = Self::diverted_config(root)?;
		Ok(Rc::new(Self {
			real_store_dir: Some(config.real_store_dir),
			..Self::default()
		}))
	}

	/// Open a writable `local?root=ROOT` store. ROOT must already exist.
	/// Logical paths remain `/nix/store`; system stores are never opened.
	pub fn writable(root: &Path) -> io::Result<Rc<Self>> {
		let config = Self::diverted_config(root)?;
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

	pub(crate) fn physical_path(&self, path: &Path) -> PathBuf {
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
		let bytes = if recursive {
			let mut bytes = Vec::new();
			nar::dump_path(&mut bytes, &physical, filter)?;
			bytes
		} else {
			std::fs::read(physical)?
		};
		let hash = NixHash::Sha256(Sha256::digest(&bytes).into());
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
				.add_to_store_from_dump(
					&mut io::Cursor::new(captured.bytes),
					&captured.name,
					method,
					method,
					HashAlgo::Sha256,
					&BTreeSet::new(),
				)
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
		let digest = nar::nar_sha256(&self.physical_path(path), filter)?;
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
		for recursive in [false, true] {
			let root = tempfile::tempdir().unwrap();
			let source = tempfile::tempdir().unwrap();
			let file = source.path().join("source");
			std::fs::write(&file, "original").unwrap();
			let store = EvaluatorStore::writable(root.path()).unwrap();
			let captured = store
				.capture_import(&file, "source", recursive, None)
				.unwrap();
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
			assert_eq!(
				std::fs::read(local.real_path(&imported)).unwrap(),
				b"original"
			);
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

const COREPKGS: &[(&str, &str)] = &[("fetchurl.nix", include_str!("corepkgs/fetchurl.nix"))];

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
}

impl StoreIO {
	pub fn new(store: Rc<ReadOnlyStore>) -> Self {
		Self { store }
	}
}

impl EvalIO for StoreIO {
	fn path_exists(&self, path: &Path) -> io::Result<bool> {
		if path.starts_with(COREPKGS_DIR) {
			return Ok(corepkg(path).is_some());
		}
		StdIO.path_exists(&self.store.physical_path(path))
	}

	fn open(&self, path: &Path) -> io::Result<Box<dyn io::Read>> {
		match corepkg(path) {
			Some(Corepkg::File(contents)) => Ok(Box::new(contents.as_bytes())),
			_ => StdIO.open(&self.store.physical_path(path)),
		}
	}

	fn file_type(&self, path: &Path) -> io::Result<FileType> {
		match corepkg(path) {
			Some(Corepkg::Root) => Ok(FileType::Directory),
			Some(Corepkg::File(_)) => Ok(FileType::Regular),
			None => StdIO.file_type(&self.store.physical_path(path)),
		}
	}

	fn read_dir(&self, path: &Path) -> io::Result<Vec<(bytes::Bytes, FileType)>> {
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
			_ => StdIO.read_dir(&self.store.physical_path(path)),
		}
	}

	fn import_path(&self, path: &Path) -> io::Result<PathBuf> {
		if self.store.real_store_dir.is_some()
			&& let Ok((store_path, _)) = StorePath::from_absolute_path_full(path)
		{
			if let Some(store) = &self.store.store {
				if !store.is_valid_path(&store_path).map_err(io::Error::other)? {
					return Err(io::Error::other("cannot import an invalid store path"));
				}
			}
			return Ok(path.to_owned());
		}
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
			Some(parent) => parent.canonicalize()?.join(name),
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
