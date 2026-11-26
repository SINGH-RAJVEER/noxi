//! The store operations commands and the evaluator need, independent of
//! whether the store is opened directly or through a daemon. CppNix's
//! `Store`, `LocalFSStore` and `GcStore` interfaces provide these.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use nix_compat::derivation::UnverifiedDerivation;
use nix_compat::log::VerbosityLevel;
use nix_compat::nixhash::HashAlgo;
use nix_compat::store_path::StorePath;

use crate::gc::{GcOptions, GcResults, Roots};
use crate::{
	CapturedSource, Error, FileIngestionMethod, LocalStore, Realisation, Result, ValidPathInfo,
};

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

/// CppNix's `BuildMode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BuildMode {
	#[default]
	Normal,
	/// Check registered contents as well as validity, and rebuild damaged
	/// outputs of the derivation and its inputs.
	Repair,
	/// Build the requested derivations again and compare the outputs with
	/// the registered ones. Their inputs are built normally.
	Check,
}

/// The outputs of built derivations that the store does not record: those of
/// impure derivations, by derivation and output name.
pub type ImpureOutputs = BTreeMap<StorePath, BTreeMap<String, StorePath>>;

/// What realising some paths would take, CppNix's `MissingPaths`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Missing {
	pub will_build: BTreeSet<StorePath>,
	pub will_substitute: BTreeSet<StorePath>,
	pub unknown: BTreeSet<StorePath>,
	/// Derivations that are outputs still to be built, whose outputs are not
	/// looked up.
	pub ignored_dynamic: BTreeSet<String>,
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

	/// `queryPartialDerivationOutputMap`: the registered outputs, extended by
	/// the known realisations of content-addressed outputs. An output
	/// without a known path maps to `None`.
	fn query_partial_derivation_output_map(
		&self,
		drv: &StorePath,
	) -> Result<BTreeMap<String, Option<StorePath>>> {
		Ok(self
			.query_derivation_outputs(drv)?
			.into_iter()
			.map(|(name, path)| (name, Some(path)))
			.collect())
	}

	/// `deepQueryPartialDerivationOutputMap`: like
	/// [`Store::query_partial_derivation_output_map`], but an output with no
	/// realisation of its own is looked up through the derivation resolved
	/// with its inputs' outputs. Stores that cannot resolve derivations
	/// answer with the partial map.
	fn deep_query_partial_derivation_output_map(
		&self,
		drv: &StorePath,
	) -> Result<BTreeMap<String, Option<StorePath>>> {
		self.query_partial_derivation_output_map(drv)
	}

	/// The outputs of `drv` whose paths are known: registered, or realised
	/// for content-addressed ones.
	fn known_derivation_outputs(&self, drv: &StorePath) -> Result<BTreeMap<String, StorePath>> {
		Ok(self
			.deep_query_partial_derivation_output_map(drv)?
			.into_iter()
			.filter_map(|(name, path)| Some((name, path?)))
			.collect())
	}

	fn read_derivation(&self, drv: &StorePath) -> Result<UnverifiedDerivation>;

	/// Read a derivation of any kind the store's experimental features
	/// allow; [`Store::read_derivation`] holds only ordinary ones.
	fn read_build_derivation(
		&self,
		drv: &StorePath,
	) -> Result<crate::build_derivation::BuildDerivation> {
		self.read_evaluation_derivation(
			drv,
			crate::build_derivation::OutputFeatures {
				ca_derivations: true,
				impure_derivations: true,
				dynamic_derivations: true,
				git_hashing: true,
			},
		)
	}

	/// Read output declarations for evaluation with the evaluator's feature
	/// settings, independently of the feature set used to open the owner.
	fn read_evaluation_derivation(
		&self,
		drv: &StorePath,
		features: crate::build_derivation::OutputFeatures,
	) -> Result<crate::build_derivation::BuildDerivation> {
		let bytes = std::fs::read(self.real_path(drv)).map_err(|error| {
			Error::io(
				format!("reading derivation '{}'", drv.to_absolute_path()),
				error,
			)
		})?;
		crate::build_derivation::BuildDerivation::from_aterm_bytes_with_features(&bytes, features)
	}

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

	/// `Installable::build`: realise `paths` with their results, as the
	/// modern commands do. A failed build is an error with exit status 1.
	/// Outputs the store records are not returned.
	fn build_paths_with_results(
		&self,
		paths: &[DerivedPath],
		mode: BuildMode,
	) -> Result<ImpureOutputs> {
		match self.build_paths_with_mode(paths, mode) {
			Ok(()) => Ok(ImpureOutputs::new()),
			Err(Error::Build { message, .. }) => Err(Error::Other(message)),
			Err(error) => Err(error),
		}
	}

	/// `EvalState::repair`: write the paths this client adds again even when
	/// they are valid, as an evaluation with `--repair` does.
	fn repair_additions(&self, _repair: bool) {}

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

	/// Check the store's paths and metadata through their owner,
	/// `Store::verifyStore`. Whether errors remain.
	fn verify_store(
		&self,
		_check_contents: bool,
		_repair: bool,
		_log: &mut dyn FnMut(VerbosityLevel, &str),
	) -> Result<bool> {
		Err(Error::Other(
			"store does not support verification".to_owned(),
		))
	}

	/// The paths of `paths` that are valid, `Store::queryValidPaths`.
	fn query_valid_paths(&self, paths: &BTreeSet<StorePath>) -> Result<BTreeSet<StorePath>> {
		let mut valid = BTreeSet::new();
		for path in paths {
			if self.is_valid_path(path)? {
				valid.insert(path.clone());
			}
		}
		Ok(valid)
	}

	/// Query validity, giving a remote destination the opportunity to
	/// substitute the paths before the caller uploads them.
	fn query_valid_paths_with_substitute(
		&self,
		paths: &BTreeSet<StorePath>,
		_substitute: bool,
	) -> Result<BTreeSet<StorePath>> {
		self.query_valid_paths(paths)
	}

	/// `Store::querySubstitutablePaths`: the paths a substituter that answers
	/// mass queries has.
	fn query_substitutable_paths(
		&self,
		_paths: &BTreeSet<StorePath>,
	) -> Result<BTreeSet<StorePath>> {
		Ok(BTreeSet::new())
	}

	/// `Store::queryAllValidPaths`.
	fn query_all_valid_paths(&self) -> Result<BTreeSet<StorePath>> {
		Err(Error::Other(
			"store does not support listing its paths".to_owned(),
		))
	}

	/// Write the NAR serialisation of a valid path.
	fn nar_from_path(&self, path: &StorePath, mut sink: &mut dyn Write) -> Result<()> {
		crate::nar::dump_path(&mut sink, &self.real_path(path))?;
		Ok(())
	}

	/// `Store::queryRealisation`; `None` without `ca-derivations`.
	fn query_realisation(&self, _drv: &StorePath, _output: &str) -> Result<Option<Realisation>> {
		Ok(None)
	}

	/// `Store::registerDrvOutput`.
	fn register_drv_output(
		&self,
		_drv: &StorePath,
		_output: &str,
		_realisation: &Realisation,
	) -> Result<()> {
		Err(Error::Other(
			"store does not support registering realisations".to_owned(),
		))
	}

	/// `Store::registerDrvOutput` with signature checks: a store that needs
	/// signatures refuses a realisation without one by a trusted key.
	fn register_checked_drv_output(
		&self,
		drv: &StorePath,
		output: &str,
		realisation: &Realisation,
	) -> Result<()> {
		self.register_drv_output(drv, output, realisation)
	}

	/// Whether the store was opened with `ca-derivations`.
	fn ca_derivations(&self) -> bool {
		false
	}

	/// Import `paths` in order, `Store::addMultipleToStore`. `nar` writes
	/// each path's NAR.
	fn add_multiple_to_store(
		&self,
		_paths: &[ValidPathInfo],
		_nar: &mut dyn FnMut(&StorePath, &mut dyn Write) -> Result<()>,
		_repair: bool,
		_check_sigs: bool,
	) -> Result<()> {
		Err(Error::Other(
			"store does not support importing paths".to_owned(),
		))
	}

	/// Realise the paths, building or substituting as needed.
	fn build_paths(&self, paths: &[DerivedPath]) -> Result<()>;

	/// Realise the paths in a build mode other than the normal one.
	fn build_paths_with_mode(&self, paths: &[DerivedPath], mode: BuildMode) -> Result<()> {
		match mode {
			BuildMode::Normal => self.build_paths(paths),
			BuildMode::Repair | BuildMode::Check => Err(Error::Other(
				"store does not support repairing or checking builds".to_owned(),
			)),
		}
	}

	/// Restore the contents of a valid path, `Store::repairPath`.
	fn repair_path(&self, _path: &StorePath) -> Result<()> {
		Err(Error::Other(
			"operation 'repairPath' is not supported by this store".to_owned(),
		))
	}

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
	fn verify_store(
		&self,
		check_contents: bool,
		repair: bool,
		log: &mut dyn FnMut(VerbosityLevel, &str),
	) -> Result<bool> {
		if repair {
			return Err(Error::Other(
				"noxi does not support repairing paths yet".to_owned(),
			));
		}
		LocalStore::verify_store(self, check_contents, None, log)
	}

	fn query_valid_paths(&self, paths: &BTreeSet<StorePath>) -> Result<BTreeSet<StorePath>> {
		LocalStore::query_valid_paths(self, paths)
	}

	fn query_all_valid_paths(&self) -> Result<BTreeSet<StorePath>> {
		LocalStore::query_all_valid_paths(self)
	}

	fn query_realisation(&self, drv: &StorePath, output: &str) -> Result<Option<Realisation>> {
		LocalStore::query_realisation(self, drv, output)
	}

	fn register_drv_output(
		&self,
		drv: &StorePath,
		output: &str,
		realisation: &Realisation,
	) -> Result<()> {
		LocalStore::register_drv_output(self, drv, output, realisation)
	}

	fn register_checked_drv_output(
		&self,
		drv: &StorePath,
		output: &str,
		realisation: &Realisation,
	) -> Result<()> {
		if self.realisation_is_untrusted(drv, output, realisation) {
			return Err(Error::Other(format!(
				"cannot register realisation '{}' because it lacks a signature by a trusted key",
				realisation.out_path
			)));
		}
		LocalStore::register_drv_output(self, drv, output, realisation)
	}

	fn ca_derivations(&self) -> bool {
		self.config().ca_derivations
	}

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

	fn query_partial_derivation_output_map(
		&self,
		drv: &StorePath,
	) -> Result<BTreeMap<String, Option<StorePath>>> {
		LocalStore::query_partial_derivation_output_map(self, drv)
	}

	fn read_derivation(&self, drv: &StorePath) -> Result<UnverifiedDerivation> {
		LocalStore::read_derivation(self, drv)
	}

	fn read_build_derivation(
		&self,
		drv: &StorePath,
	) -> Result<crate::build_derivation::BuildDerivation> {
		LocalStore::read_build_derivation(self, drv)
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
