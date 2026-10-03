//! A local store that builds and substitutes the paths it is asked to
//! realise, as CppNix's `LocalStore` does.

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use nix_compat::derivation::UnverifiedDerivation;
use nix_compat::nixhash::HashAlgo;
use nix_compat::store_path::StorePath;
use nix_rs_store::binary_cache::BinaryCache;
use nix_rs_store::{
	DerivedPath, Error, FileIngestionMethod, LocalStore, Missing, Result, Store, ValidPathInfo,
};

use crate::local::{BuildEvent, LocalBuilder};
use crate::missing;
use crate::settings::BuildSettings;

/// A [`LocalStore`] with the settings and substituters to realise paths.
pub struct LocalBackend {
	pub store: LocalStore,
	pub settings: BuildSettings,
	/// The `substituters` setting, opened when a path is first realised.
	substituter_uris: Vec<String>,
	substituters: OnceCell<Vec<BinaryCache>>,
}

impl LocalBackend {
	pub fn new(store: LocalStore, settings: BuildSettings, substituters: Vec<String>) -> Self {
		Self {
			store,
			settings,
			substituter_uris: substituters,
			substituters: OnceCell::new(),
		}
	}

	/// The substituters that could be opened. Like CppNix, a substituter
	/// that cannot be opened is reported and skipped.
	fn substituters(&self) -> &[BinaryCache] {
		self.substituters.get_or_init(|| {
			self.substituter_uris
				.iter()
				.filter_map(|uri| match BinaryCache::open(uri) {
					Ok(cache) => Some(cache),
					Err(e) => {
						eprintln!("warning: disabling substituter '{uri}': {e}");
						None
					}
				})
				.collect()
		})
	}
}

impl Store for LocalBackend {
	fn real_path(&self, path: &StorePath) -> PathBuf {
		self.store.real_path(path)
	}

	fn is_valid_path(&self, path: &StorePath) -> Result<bool> {
		self.store.is_valid_path(path)
	}

	fn query_path_info(&self, path: &StorePath) -> Result<Option<ValidPathInfo>> {
		self.store.query_path_info(path)
	}

	fn query_referrers(&self, path: &StorePath) -> Result<BTreeSet<StorePath>> {
		self.store.query_referrers(path)
	}

	fn query_valid_derivers(&self, path: &StorePath) -> Result<BTreeSet<StorePath>> {
		self.store.query_valid_derivers(path)
	}

	fn query_derivation_outputs(&self, drv: &StorePath) -> Result<BTreeMap<String, StorePath>> {
		self.store.query_derivation_outputs(drv)
	}

	fn read_derivation(&self, drv: &StorePath) -> Result<UnverifiedDerivation> {
		self.store.read_derivation(drv)
	}

	fn add_text_to_store(
		&self,
		name: &str,
		contents: &[u8],
		references: &BTreeSet<StorePath>,
	) -> Result<StorePath> {
		self.store
			.add_text_to_store(name, contents, references, false)
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
		self.store.add_to_store_from_dump(
			source,
			name,
			dump_method,
			hash_method,
			hash_algo,
			references,
			false,
		)
	}

	fn add_temp_root(&self, path: &StorePath) -> Result<()> {
		self.store.add_temp_root(path)
	}

	fn add_perm_root(&self, path: &StorePath, gc_root: &Path) -> Result<PathBuf> {
		self.store.add_perm_root(path, gc_root)
	}

	fn query_missing(&self, paths: &[DerivedPath]) -> Result<Missing> {
		let targets: Vec<missing::DerivedPath> = paths
			.iter()
			.map(|path| match path {
				DerivedPath::Opaque(path) => missing::DerivedPath::Opaque(path.clone()),
				DerivedPath::Built { drv, outputs } => missing::DerivedPath::Built {
					drv_path: drv.clone(),
					outputs: (!outputs.is_empty()).then(|| outputs.clone()),
				},
			})
			.collect();
		let substituters = self.substituters();
		missing::query_missing(
			&self.store,
			substituters,
			!substituters.is_empty(),
			self.settings.always_allow_substitutes,
			&targets,
		)
		.map_err(|e| Error::Other(e.to_string()))
	}

	/// Build output goes to standard error, as the legacy commands print it,
	/// after a line naming each derivation as it starts.
	fn build_paths(&self, paths: &[DerivedPath]) -> Result<()> {
		let substituters = self.substituters();
		let builder =
			LocalBuilder::new(&self.store, &self.settings).with_substituters(substituters);
		let mut events = |event: BuildEvent<'_>| {
			let mut stderr = std::io::stderr();
			let _ = match event {
				BuildEvent::Started { drv_path } => {
					writeln!(stderr, "building '{}'...", drv_path.to_absolute_path())
				}
				BuildEvent::Output(data) => stderr.write_all(data),
				BuildEvent::Finished { .. } => Ok(()),
			};
		};
		for path in paths {
			match path {
				DerivedPath::Opaque(path) => {
					if self.store.is_valid_path(path)? {
						continue;
					}
					self.store.substitute(substituters, path).map_err(|e| {
						Error::Other(format!(
							"path '{}' does not exist and cannot be created: {e}",
							path.to_absolute_path()
						))
					})?;
				}
				DerivedPath::Built { drv, outputs } => {
					let wanted = (!outputs.is_empty()).then_some(outputs);
					let result = builder.realise_with_events(drv, wanted, &mut events);
					result.map_err(|e| Error::Other(e.to_string()))?;
				}
			}
		}
		Ok(())
	}
}
