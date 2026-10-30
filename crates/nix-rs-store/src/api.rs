//! The store operations commands and the evaluator need, independent of
//! whether the store is opened directly or through a daemon. CppNix's
//! `Store`, `LocalFSStore` and `GcStore` interfaces provide these.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use nix_compat::derivation::UnverifiedDerivation;
use nix_compat::nixhash::HashAlgo;
use nix_compat::store_path::StorePath;

use crate::gc::{GcOptions, GcResults, Roots};
use crate::{CapturedSource, Error, FileIngestionMethod, LocalStore, Result, ValidPathInfo};

/// A path to realise: a store path, or outputs of a derivation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DerivedPath {
	/// A store path that must be valid, substituting it if necessary.
	Opaque(StorePath),
	/// Outputs of the derivation at the path; an empty set means all.
	Built {
		drv: StorePath,
		outputs: BTreeSet<String>,
	},
}

/// What realising some paths would take, CppNix's `MissingPaths`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Missing {
	pub will_build: BTreeSet<StorePath>,
	pub will_substitute: BTreeSet<StorePath>,
	pub unknown: BTreeSet<StorePath>,
	pub download_size: u64,
	pub nar_size: u64,
}

/// A Nix store as the legacy commands use it.
pub trait Store {
	/// Where store paths are visible to this process. This differs from
	/// `/nix/store` only for a diverted `local?root=` store.
	fn real_path(&self, path: &StorePath) -> PathBuf;

	fn is_valid_path(&self, path: &StorePath) -> Result<bool>;

	fn query_path_info(&self, path: &StorePath) -> Result<Option<ValidPathInfo>>;

	/// The valid paths that refer to `path`.
	fn query_referrers(&self, path: &StorePath) -> Result<BTreeSet<StorePath>>;

	/// The valid derivations that list `path` as an output.
	fn query_valid_derivers(&self, path: &StorePath) -> Result<BTreeSet<StorePath>>;

	/// The registered outputs of a valid derivation.
	fn query_derivation_outputs(&self, drv: &StorePath) -> Result<BTreeMap<String, StorePath>>;

	fn read_derivation(&self, drv: &StorePath) -> Result<UnverifiedDerivation>;

	fn add_text_to_store(
		&self,
		name: &str,
		contents: &[u8],
		references: &BTreeSet<StorePath>,
	) -> Result<StorePath>;

	/// Add a serialised file (`Flat`) or archive (`NixArchive`), hashed with
	/// `hash_method`.
	fn add_to_store_from_dump(
		&self,
		source: &mut (dyn Read + Send),
		name: &str,
		dump_method: FileIngestionMethod,
		hash_method: FileIngestionMethod,
		hash_algo: HashAlgo,
		references: &BTreeSet<StorePath>,
	) -> Result<StorePath>;

	/// Import an owned snapshot hashed during capture. Remote stores retain their
	/// ordinary independently verified import protocol.
	fn add_to_store_from_capture(
		&self,
		mut source: CapturedSource,
		name: &str,
		method: FileIngestionMethod,
		references: &BTreeSet<StorePath>,
	) -> Result<StorePath> {
		self.add_to_store_from_dump(
			source.reader()?,
			name,
			method,
			method,
			HashAlgo::Sha256,
			references,
		)
	}

	/// Keep `path` alive until this process exits.
	fn add_temp_root(&self, path: &StorePath) -> Result<()>;

	/// Make `gc_root` a symlink to `path` and register it as an indirect
	/// root; returns the link.
	fn add_perm_root(&self, path: &StorePath, gc_root: &Path) -> Result<PathBuf>;

	/// The collector's roots. Remote stores let the daemon decide which
	/// root names the client may see.
	fn find_roots(&self, _censor: bool, _log: &mut dyn FnMut(&str)) -> Result<Roots> {
		Err(Error::Other(
			"store does not support root listing".to_owned(),
		))
	}

	/// Collect through the store owner, retaining its liveness checks.
	fn collect_garbage(
		&self,
		_options: &GcOptions,
		_log: &mut dyn FnMut(&str),
	) -> Result<GcResults> {
		Err(Error::Other(
			"store does not support garbage collection".to_owned(),
		))
	}

	/// Deduplicate store files through their owner.
	fn optimise_store(&self, _log: &mut dyn FnMut(&str)) -> Result<()> {
		Err(Error::Other(
			"store does not support optimisation".to_owned(),
		))
	}

	/// Realise the paths, building or substituting as needed.
	fn build_paths(&self, paths: &[DerivedPath]) -> Result<()>;

	/// What realising `paths` would build and substitute. A store that
	/// cannot realise anything reports nothing.
	fn query_missing(&self, _paths: &[DerivedPath]) -> Result<Missing> {
		Ok(Missing::default())
	}

	/// Make `path` valid, substituting it if necessary.
	fn ensure_path(&self, path: &StorePath) -> Result<()> {
		self.build_paths(&[DerivedPath::Opaque(path.clone())])
	}

	/// The paths `paths` refer to, directly or indirectly, including
	/// themselves.
	fn compute_closure(&self, paths: &BTreeSet<StorePath>) -> Result<BTreeSet<StorePath>> {
		let mut closure = BTreeSet::new();
		let mut pending: Vec<StorePath> = paths.iter().cloned().collect();
		while let Some(path) = pending.pop() {
			if !closure.insert(path.clone()) {
				continue;
			}
			let info = self
				.query_path_info(&path)?
				.ok_or_else(|| Error::InvalidPath(path.to_absolute_path()))?;
			pending.extend(info.references.into_iter().filter(|r| !closure.contains(r)));
		}
		Ok(closure)
	}
}

/// A local store without a builder only realises paths that are already
/// valid.
impl Store for LocalStore {
	fn optimise_store(&self, log: &mut dyn FnMut(&str)) -> Result<()> {
		let stats = self.optimise()?;
		log(&format!(
			"{} freed by hard-linking {} files",
			crate::gc::render_size(stats.bytes_freed as i64, false),
			stats.files_linked
		));
		Ok(())
	}

	fn find_roots(&self, censor: bool, log: &mut dyn FnMut(&str)) -> Result<Roots> {
		LocalStore::find_roots(self, censor, log)
	}

	fn collect_garbage(&self, options: &GcOptions, log: &mut dyn FnMut(&str)) -> Result<GcResults> {
		LocalStore::collect_garbage(self, options, log)
	}

	fn real_path(&self, path: &StorePath) -> PathBuf {
		LocalStore::real_path(self, path)
	}

	fn is_valid_path(&self, path: &StorePath) -> Result<bool> {
		LocalStore::is_valid_path(self, path)
	}

	fn query_path_info(&self, path: &StorePath) -> Result<Option<ValidPathInfo>> {
		LocalStore::query_path_info(self, path)
	}

	fn query_referrers(&self, path: &StorePath) -> Result<BTreeSet<StorePath>> {
		LocalStore::query_referrers(self, path)
	}

	fn query_valid_derivers(&self, path: &StorePath) -> Result<BTreeSet<StorePath>> {
		LocalStore::query_valid_derivers(self, path)
	}

	fn query_derivation_outputs(&self, drv: &StorePath) -> Result<BTreeMap<String, StorePath>> {
		LocalStore::query_derivation_outputs(self, drv)
	}

	fn read_derivation(&self, drv: &StorePath) -> Result<UnverifiedDerivation> {
		LocalStore::read_derivation(self, drv)
	}

	fn add_text_to_store(
		&self,
		name: &str,
		contents: &[u8],
		references: &BTreeSet<StorePath>,
	) -> Result<StorePath> {
		LocalStore::add_text_to_store(self, name, contents, references, false)
	}

	fn add_to_store_from_dump(
		&self,
		source: &mut (dyn Read + Send),
		name: &str,
		dump_method: FileIngestionMethod,
		hash_method: FileIngestionMethod,
		hash_algo: HashAlgo,
		references: &BTreeSet<StorePath>,
	) -> Result<StorePath> {
		LocalStore::add_to_store_from_dump(
			self,
			source,
			name,
			dump_method,
			hash_method,
			hash_algo,
			references,
			false,
		)
	}

	fn add_to_store_from_capture(
		&self,
		source: CapturedSource,
		name: &str,
		method: FileIngestionMethod,
		references: &BTreeSet<StorePath>,
	) -> Result<StorePath> {
		LocalStore::add_to_store_from_capture(self, source, name, method, references)
	}

	fn add_temp_root(&self, path: &StorePath) -> Result<()> {
		LocalStore::add_temp_root(self, path)
	}

	fn add_perm_root(&self, path: &StorePath, gc_root: &Path) -> Result<PathBuf> {
		LocalStore::add_perm_root(self, path, gc_root)
	}

	fn build_paths(&self, paths: &[DerivedPath]) -> Result<()> {
		for path in paths {
			let path = match path {
				DerivedPath::Opaque(path) => path,
				DerivedPath::Built { drv, .. } => drv,
			};
			if !LocalStore::is_valid_path(self, path)? {
				return Err(Error::Other(format!(
					"path '{}' does not exist and cannot be created",
					path.to_absolute_path()
				)));
			}
		}
		if paths
			.iter()
			.any(|path| matches!(path, DerivedPath::Built { .. }))
		{
			return Err(Error::Other(
				"building derivations requires a builder".to_owned(),
			));
		}
		Ok(())
	}
}
