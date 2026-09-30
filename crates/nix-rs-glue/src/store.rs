//! A store that computes paths without writing them, as CppNix does in
//! read-only mode (the default for `nix-instantiate --eval`).
//!
//! Imported paths, derivations and `builtins.toFile` results get the store
//! paths CppNix would give them, but nothing is written to `/nix/store`.
//! Derivations are kept in memory so later derivations can refer to them.

use std::cell::{RefCell, RefMut};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use nix_compat::nixhash::NixHash;
use nix_compat::store_path::{self, STORE_DIR, StorePath};
use nix_rs_eval::{EvalIO, FileType, StdIO};

use crate::errors::ImportError;
use crate::known_paths::KnownPaths;
use crate::nar::{self, Filter};

/// State shared by the store-aware builtins of one evaluation.
#[derive(Debug, Default)]
pub struct ReadOnlyStore {
	known_paths: RefCell<KnownPaths>,
}

impl ReadOnlyStore {
	pub fn new() -> Rc<Self> {
		Rc::new(Self::default())
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
		let digest = nar::nar_sha256(path, filter)?;
		ca_path(name, true, &NixHash::Sha256(digest), path)
	}

	/// The store path a flat copy of the regular file `path` gets, hashed
	/// with SHA-256.
	pub fn import_flat(&self, path: &Path, name: &str) -> io::Result<StorePath> {
		let digest = nar::file_sha256(path)?;
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

/// Local file access, with path imports computed by a [`ReadOnlyStore`].
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
		StdIO.path_exists(path)
	}

	fn open(&self, path: &Path) -> io::Result<Box<dyn io::Read>> {
		match corepkg(path) {
			Some(Corepkg::File(contents)) => Ok(Box::new(contents.as_bytes())),
			_ => StdIO.open(path),
		}
	}

	fn file_type(&self, path: &Path) -> io::Result<FileType> {
		match corepkg(path) {
			Some(Corepkg::Root) => Ok(FileType::Directory),
			Some(Corepkg::File(_)) => Ok(FileType::Regular),
			None => StdIO.file_type(path),
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
			_ => StdIO.read_dir(path),
		}
	}

	fn import_path(&self, path: &Path) -> io::Result<PathBuf> {
		let name = import_name(path)?;
		if let Some(Corepkg::File(contents)) = corepkg(path) {
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
