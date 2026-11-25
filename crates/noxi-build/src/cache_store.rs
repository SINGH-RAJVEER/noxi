//! A `file://` binary cache as a store, CppNix's `LocalBinaryCacheStore`.
//!
//! Paths are read from and written to the cache as NARs. Nothing can be
//! built in it: realising outputs succeeds only if they are there, or if the
//! build hook builds them elsewhere.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::backend::BuildLogs;
use crate::error::{BuildError, FailureStatus};
use crate::hook::{Hooks, Reply};
use crate::local::{BuildEvent, build_with_hook, run_post_build_hook};
use crate::options;
use crate::settings::BuildSettings;
use nix_compat::derivation::UnverifiedDerivation;
use nix_compat::nixhash::{CAHash, HashAlgo, NixHash};
use nix_compat::store_path::{self, StorePath};
use noxi_store::binary_cache::BinaryCache;
use noxi_store::build_derivation::{BuildDerivation, OutputFeatures};
use noxi_store::{
	DerivedPath, Error, FileIngestionMethod, Missing, Realisation, Result, Store, ValidPathInfo,
	ca_hash_of, nar,
};

/// `LocalBuildRejection::NoLocalStore`.
const NO_LOCAL_STORE: &str = "Unable to build with a primary store that isn't a local store; either pass a different '--store' or enable remote builds.\n\nFor more information check 'man nix.conf' and search for '/machines'.";

pub struct CacheStore {
	cache: BinaryCache,
	features: OutputFeatures,
	/// How the build hook, which alone can build for a cache, is asked.
	settings: BuildSettings,
}

impl CacheStore {
	/// Open the cache, creating its directories and `nix-cache-info` as
	/// opening it does in CppNix.
	pub fn open(uri: &str, features: OutputFeatures) -> Result<Self> {
		let cache = BinaryCache::open(uri)?;
		cache.init_for_upload()?;
		Ok(Self {
			cache,
			features,
			settings: BuildSettings::default(),
		})
	}

	/// Offer builds to the build hook of `settings`.
	pub fn with_build_settings(mut self, settings: BuildSettings) -> Self {
		self.settings = settings;
		self
	}

	/// Build a derivation whose inputs the cache has with the build hook,
	/// which puts the outputs into the cache.
	fn build_with_hook(
		&self,
		hooks: &Hooks,
		drv_path: &StorePath,
		logs: &mut BuildLogs,
	) -> std::result::Result<(), BuildError> {
		let drv = self.read_build_derivation(drv_path)?;
		let structured = options::structured_attrs(drv.basic.environment())?;
		let env = drv
			.basic
			.environment()
			.iter()
			.filter(|(name, _)| name.as_slice() != b"__json")
			.map(|(name, value)| (name.clone(), value.clone()))
			.collect();
		let features = options::DerivationOptions::new(
			&env,
			structured.as_ref(),
			drv.basic.input_derivations(),
			false,
			&mut Vec::new(),
		)?
		.get_required_system_features(true);
		let mut inputs = drv.basic.input_sources().clone();
		for (input, names) in drv.basic.input_derivations() {
			let known = self.deep_query_partial_derivation_output_map(input)?;
			for name in names {
				let name = name.to_string();
				inputs.insert(known.get(&name).cloned().flatten().ok_or_else(|| {
					BuildError::Other(format!(
						"derivation '{}' requires non-existent output '{name}' from input derivation '{}'",
						drv_path.to_absolute_path(),
						input.to_absolute_path()
					))
				})?);
			}
		}
		let inputs = self.compute_closure(&inputs)?;
		let hook = loop {
			match hooks.offer(
				&self.settings,
				drv.basic.system(),
				drv_path,
				&features,
				&|| false,
			)? {
				Reply::Accept(hook) => break hook,
				Reply::Decline => {
					return Err(BuildError::failed(
						FailureStatus::InputRejected,
						NO_LOCAL_STORE,
					));
				}
				Reply::Postpone => {
					let seconds = self
						.settings
						.config
						.get_int("build-poll-interval", 5)
						.max(1);
					std::thread::sleep(std::time::Duration::from_secs(seconds as u64));
				}
			}
		};
		let outputs = self.query_partial_derivation_output_map(drv_path)?;
		let mut missing = BTreeSet::new();
		for (name, path) in &outputs {
			if !matches!(path, Some(path) if self.is_valid_path(path)?) {
				missing.insert(name.clone());
			}
		}
		let known: Vec<StorePath> = outputs.into_values().flatten().collect();
		build_with_hook(
			&self.settings,
			&noxi_store::LocalStoreConfig::system().log_dir,
			drv_path,
			&known,
			&inputs,
			&missing,
			*hook,
			&|| false,
			&mut |event| logs.event(None, event),
		)?;
		let mut built = BTreeMap::new();
		for (name, path) in self.query_partial_derivation_output_map(drv_path)? {
			match path {
				Some(path) if self.is_valid_path(&path)? => built.insert(name, path),
				_ => {
					return Err(BuildError::Other(
						"some outputs are unexpectedly invalid".to_owned(),
					));
				}
			};
		}
		run_post_build_hook(&self.settings, drv_path, &built)
	}

	/// The derivations to build, each after its inputs.
	fn build_order(&self, drvs: &BTreeSet<StorePath>) -> Result<Vec<StorePath>> {
		fn visit(
			store: &CacheStore,
			drvs: &BTreeSet<StorePath>,
			drv: &StorePath,
			order: &mut Vec<StorePath>,
			seen: &mut BTreeSet<StorePath>,
		) -> Result<()> {
			if !seen.insert(drv.clone()) {
				return Ok(());
			}
			for input in store
				.read_build_derivation(drv)?
				.basic
				.input_derivations()
				.keys()
				.filter(|input| drvs.contains(*input))
			{
				visit(store, drvs, input, order, seen)?;
			}
			order.push(drv.clone());
			Ok(())
		}
		let mut order = Vec::new();
		let mut seen = BTreeSet::new();
		for drv in drvs {
			visit(self, drvs, drv, &mut order, &mut seen)?;
		}
		Ok(order)
	}

	pub fn cache(&self) -> &BinaryCache {
		&self.cache
	}

	/// Add the files at `path` under a content address.
	fn add_files(
		&self,
		name: &str,
		files: &Path,
		ca: CAHash,
		references: &BTreeSet<StorePath>,
	) -> Result<StorePath> {
		let path = match &ca {
			CAHash::Text(digest) => store_path::build_text_path_from_content_digest(
				name,
				*digest,
				references.iter().map(StorePath::as_ref),
			),
			CAHash::Git(hash) => store_path::build_git_path(name, hash),
			_ => store_path::build_ca_path(
				name,
				matches!(ca, CAHash::Nar(_)),
				&ca.hash(),
				references.iter().map(StorePath::as_ref),
				false,
			),
		}
		.map_err(|e| Error::Other(e.to_string()))?
		.to_owned();
		if self.is_valid_path(&path)? {
			return Ok(path);
		}
		let (nar_hash, nar_size) = nar::hash_path(files)?;
		let mut info = ValidPathInfo::new(path.clone(), nar_hash);
		info.nar_size = nar_size;
		info.references = references.clone();
		info.ca = Some(ca);
		self.cache
			.add_nar(&info, &mut |mut sink| Ok(nar::dump_path(&mut sink, files)?))?;
		Ok(path)
	}
}

impl Store for CacheStore {
	/// Paths of the cache are not visible to this process.
	fn real_path(&self, path: &StorePath) -> PathBuf {
		PathBuf::from(path.to_absolute_path())
	}

	fn is_valid_path(&self, path: &StorePath) -> Result<bool> {
		Ok(self.cache.narinfo_text(path)?.is_some())
	}

	fn query_path_info(&self, path: &StorePath) -> Result<Option<ValidPathInfo>> {
		Ok(self
			.cache
			.query_path_info(path)?
			.map(|cached| cached.info)
			.filter(|info| info.path == *path))
	}

	fn query_all_valid_paths(&self) -> Result<BTreeSet<StorePath>> {
		self.cache.query_all_valid_paths()
	}

	fn nar_from_path(&self, path: &StorePath, sink: &mut dyn Write) -> Result<()> {
		let cached = self
			.cache
			.query_path_info(path)?
			.ok_or_else(|| Error::InvalidPath(path.to_absolute_path()))?;
		self.cache.copy_nar(&cached, sink)
	}

	/// A binary cache does not index its references.
	fn query_referrers(&self, _path: &StorePath) -> Result<BTreeSet<StorePath>> {
		Err(Error::Other(format!(
			"operation 'queryReferrers' is not supported by store '{}'",
			self.cache.uri()
		)))
	}

	fn query_valid_derivers(&self, _path: &StorePath) -> Result<BTreeSet<StorePath>> {
		Ok(BTreeSet::new())
	}

	fn query_derivation_outputs(&self, drv: &StorePath) -> Result<BTreeMap<String, StorePath>> {
		Ok(self
			.query_partial_derivation_output_map(drv)?
			.into_iter()
			.filter_map(|(name, path)| Some((name, path?)))
			.collect())
	}

	fn query_partial_derivation_output_map(
		&self,
		drv_path: &StorePath,
	) -> Result<BTreeMap<String, Option<StorePath>>> {
		let drv = self.read_build_derivation(drv_path)?;
		let mut outputs: BTreeMap<String, Option<StorePath>> = drv
			.basic
			.outputs()
			.names()
			.map(|name| (name.to_string(), None))
			.collect();
		let floating = !drv.floating.is_empty() || drv.basic.outputs().is_deferred();
		if let Some((_, path)) = drv.basic.outputs().as_fixed_output() {
			outputs.insert("out".to_owned(), Some(path.clone()));
		} else if !floating {
			for (name, path) in drv.basic.outputs().iter() {
				outputs.insert(name.to_string(), Some(path.clone()));
			}
		}
		for (name, output) in &drv.experimental {
			outputs.insert(
				name.clone(),
				output.fixed.as_ref().map(|(path, _)| path.clone()),
			);
		}
		if self.features.ca_derivations {
			for (name, path) in &mut outputs {
				if let Some(realisation) = self.query_realisation(drv_path, name)? {
					*path = Some(realisation.out_path);
				}
			}
		}
		Ok(outputs)
	}

	fn deep_query_partial_derivation_output_map(
		&self,
		drv: &StorePath,
	) -> Result<BTreeMap<String, Option<StorePath>>> {
		let mut outputs = self.query_partial_derivation_output_map(drv)?;
		for (name, path) in &mut outputs {
			if path.is_none() {
				*path = crate::local::deep_query_output(self, drv, name, &mut |_, _, _| {})
					.map_err(|e| Error::Other(e.to_string()))?;
			}
		}
		Ok(outputs)
	}

	fn query_realisation(&self, drv: &StorePath, output: &str) -> Result<Option<Realisation>> {
		if !self.features.ca_derivations {
			return Ok(None);
		}
		self.cache.query_realisation(drv, output)
	}

	fn register_drv_output(
		&self,
		drv: &StorePath,
		output: &str,
		realisation: &Realisation,
	) -> Result<()> {
		self.cache.upload_realisation(drv, output, realisation)
	}

	fn ca_derivations(&self) -> bool {
		self.features.ca_derivations
	}

	fn read_derivation(&self, drv: &StorePath) -> Result<UnverifiedDerivation> {
		Ok(self.read_build_derivation(drv)?.basic)
	}

	fn read_build_derivation(&self, drv: &StorePath) -> Result<BuildDerivation> {
		self.read_evaluation_derivation(drv, self.features)
	}

	fn read_evaluation_derivation(
		&self,
		drv: &StorePath,
		features: OutputFeatures,
	) -> Result<BuildDerivation> {
		let mut bytes = Vec::new();
		self.nar_from_path(drv, &mut bytes)?;
		let dir = tempfile::tempdir()?;
		let file = dir.path().join("drv");
		nar::restore_path(&file, &mut bytes.as_slice())?;
		if !std::fs::symlink_metadata(&file)?.is_file() {
			return Err(Error::Other(format!(
				"derivation '{}' is not a regular file",
				drv.to_absolute_path()
			)));
		}
		BuildDerivation::from_aterm_bytes_with_features(&std::fs::read(&file)?, features)
	}

	fn add_text_to_store(
		&self,
		name: &str,
		contents: &[u8],
		references: &BTreeSet<StorePath>,
	) -> Result<StorePath> {
		let dir = tempfile::tempdir()?;
		let file = dir.path().join("text");
		std::fs::write(&file, contents)?;
		let digest = nix_compat::nixhash::Sha256::digest_bytes(contents);
		self.add_files(name, &file, CAHash::Text(digest.into()), references)
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
		let dir = tempfile::tempdir()?;
		let files = dir.path().join("x");
		match dump_method {
			FileIngestionMethod::NixArchive => {
				nar::restore_path(&files, &mut std::io::BufReader::new(source))?
			}
			FileIngestionMethod::Flat | FileIngestionMethod::Text => {
				nar::restore_flat(&files, source)?;
			}
			FileIngestionMethod::Git => {
				return Err(Error::Other(
					"a Git object is not a whole file system object".into(),
				));
			}
		}
		let zero = NixHash::from_algo_and_digest(hash_algo, &vec![0; hash_algo.digest_length()])
			.map_err(|e| Error::Other(e.to_string()))?;
		if hash_method == FileIngestionMethod::Git && !self.features.git_hashing {
			return Err(Error::Other("experimental Nix feature 'git-hashing' is disabled; add '--extra-experimental-features git-hashing' to enable it".into()));
		}
		let claimed = hash_method.content_address(zero)?;
		let ca = ca_hash_of(&files, &claimed)?;
		let result = self.add_files(name, &files, ca, references);
		noxi_store::delete_path(&files)?;
		result
	}

	/// Only local stores have roots.
	fn add_temp_root(&self, _path: &StorePath) -> Result<()> {
		Ok(())
	}

	/// CppNix makes links only to the paths of local stores.
	fn add_perm_root(&self, _path: &StorePath, gc_root: &Path) -> Result<PathBuf> {
		Ok(gc_root.to_owned())
	}

	fn add_multiple_to_store(
		&self,
		paths: &[ValidPathInfo],
		nar: &mut dyn FnMut(&StorePath, &mut dyn Write) -> Result<()>,
		repair: bool,
		_check_sigs: bool,
	) -> Result<()> {
		for info in paths {
			if !repair && self.is_valid_path(&info.path)? {
				continue;
			}
			self.cache
				.add_nar(info, &mut |sink| nar(&info.path, sink))?;
		}
		Ok(())
	}

	/// The cache has every requested output, or the build fails.
	fn build_paths(&self, paths: &[DerivedPath]) -> Result<()> {
		let missing = self.query_missing(paths)?;
		if !missing.will_build.is_empty() {
			let Some(hooks) = Hooks::new(&self.settings) else {
				return Err(Error::Build {
					exit_status: 100,
					message: NO_LOCAL_STORE.to_owned(),
				});
			};
			let targets: Vec<&StorePath> = paths
				.iter()
				.filter_map(|path| match path {
					DerivedPath::Built { drv, .. } => Some(drv),
					DerivedPath::Opaque(_) => None,
				})
				.collect();
			let _worker = nix_compat::log::logger::WorkerGuard::new();
			let mut logs = BuildLogs::default();
			for drv in self.build_order(&missing.will_build)? {
				let result = self.build_with_hook(&hooks, &drv, &mut logs);
				logs.event(
					None,
					BuildEvent::Finished {
						drv_path: &drv,
						error: result.as_ref().err(),
					},
				);
				if let Err(error) = result {
					let target = targets.first().filter(|_| !targets.contains(&&drv));
					let Some(target) = target else {
						return Err(error.into());
					};
					crate::backend::log_error(&error.to_string());
					return Err(Error::Build {
						exit_status: error.status().exit_status(),
						message: format!(
							"Cannot build '{}'.\nReason: 1 dependency failed.",
							target.to_absolute_path()
						),
					});
				}
			}
		}
		if let Some(path) = missing.unknown.first() {
			return Err(Error::Other(format!(
				"path '{}' does not exist and cannot be created",
				path.to_absolute_path()
			)));
		}
		Ok(())
	}

	fn query_missing(&self, paths: &[DerivedPath]) -> Result<Missing> {
		let mut missing = Missing::default();
		let mut pending: Vec<DerivedPath> = paths.to_vec();
		let mut seen = BTreeSet::new();
		while let Some(target) = pending.pop() {
			if !seen.insert(target.clone()) {
				continue;
			}
			match target {
				DerivedPath::Opaque(path) => {
					if !self.is_valid_path(&path)? {
						missing.unknown.insert(path);
					}
				}
				DerivedPath::Built { drv, outputs } => {
					if !self.is_valid_path(&drv)? {
						missing.unknown.insert(drv);
						continue;
					}
					let known = self.deep_query_partial_derivation_output_map(&drv)?;
					let mut valid = true;
					for (name, path) in &known {
						if outputs.is_empty() || outputs.contains(name) {
							valid &= match path {
								Some(path) => self.is_valid_path(path)?,
								None => false,
							};
						}
					}
					if !valid {
						for (input, names) in
							self.read_build_derivation(&drv)?.basic.input_derivations()
						{
							pending.push(DerivedPath::Built {
								drv: input.clone(),
								outputs: names.iter().map(ToString::to_string).collect(),
							});
						}
						missing.will_build.insert(drv);
					}
				}
			}
		}
		Ok(missing)
	}
}
