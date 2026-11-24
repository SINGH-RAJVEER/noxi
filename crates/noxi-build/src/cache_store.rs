//! A `file://` binary cache as a store, CppNix's `LocalBinaryCacheStore`.
//!
//! Paths are read from and written to the cache as NARs. Nothing can be
//! built in it: realising outputs succeeds only if they are there.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use nix_compat::derivation::UnverifiedDerivation;
use nix_compat::nixhash::{CAHash, HashAlgo, NixHash};
use nix_compat::store_path::{self, StorePath};
use noxi_store::binary_cache::BinaryCache;
use noxi_store::build_derivation::{BuildDerivation, OutputFeatures};
use noxi_store::{
	DerivedPath, Error, FileIngestionMethod, Missing, Realisation, Result, Store, ValidPathInfo,
	ca_hash_of, nar,
};

pub struct CacheStore {
	cache: BinaryCache,
	features: OutputFeatures,
}

impl CacheStore {
	/// Open the cache, creating its directories and `nix-cache-info` as
	/// opening it does in CppNix.
	pub fn open(uri: &str, features: OutputFeatures) -> Result<Self> {
		let cache = BinaryCache::open(uri)?;
		cache.init_for_upload()?;
		Ok(Self { cache, features })
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
		}
		let zero = NixHash::from_algo_and_digest(hash_algo, &vec![0; hash_algo.digest_length()])
			.map_err(|e| Error::Other(e.to_string()))?;
		let claimed = match (hash_method, zero) {
			(FileIngestionMethod::NixArchive, zero) => CAHash::Nar(zero),
			(FileIngestionMethod::Text, NixHash::Sha256(zero)) => CAHash::Text(zero),
			(FileIngestionMethod::Text, _) => {
				return Err(Error::Other("text-hashed paths require SHA-256".into()));
			}
			(FileIngestionMethod::Flat, zero) => CAHash::Flat(zero),
		};
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
			return Err(Error::Build {
				exit_status: 100,
				message: "Unable to build with a primary store that isn't a local store; either pass a different '--store' or enable remote builds.\n\nFor more information check 'man nix.conf' and search for '/machines'.".to_owned(),
			});
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
