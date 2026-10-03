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

use crate::error::BuildError;
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

impl LocalBackend {
	/// The derivations realising `paths` builds, each after its inputs and
	/// otherwise in order of name and path.
	fn build_order(&self, paths: &[DerivedPath]) -> Result<Vec<(StorePath, BTreeSet<StorePath>)>> {
		let will_build = self.query_missing(paths)?.will_build;
		let mut inputs: BTreeMap<StorePath, BTreeSet<StorePath>> = BTreeMap::new();
		for drv in &will_build {
			let drv_inputs = self
				.store
				.read_derivation(drv)?
				.input_derivations()
				.keys()
				.filter(|input| will_build.contains(*input))
				.cloned()
				.collect();
			inputs.insert(drv.clone(), drv_inputs);
		}
		let key = |path: &StorePath| (path.name().to_owned(), path.to_absolute_path());
		let mut ready: BTreeSet<(String, String, StorePath)> = BTreeSet::new();
		for (drv, drv_inputs) in &inputs {
			if drv_inputs.is_empty() {
				let (name, path) = key(drv);
				ready.insert((name, path, drv.clone()));
			}
		}
		let mut waiting = inputs.clone();
		let mut order = Vec::new();
		while let Some((_, _, drv)) = ready.pop_first() {
			waiting.remove(&drv);
			for (other, other_inputs) in waiting.iter_mut() {
				if other_inputs.remove(&drv) && other_inputs.is_empty() {
					let (name, path) = key(other);
					ready.insert((name, path, other.clone()));
				}
			}
			let drv_inputs = inputs.remove(&drv).unwrap_or_default();
			order.push((drv, drv_inputs));
		}
		Ok(order)
	}

	/// CppNix's failure of a derivation goal whose input failed to build.
	fn dependency_failed(&self, drv: &StorePath) -> Result<String> {
		let mut message = format!(
			"Cannot build '{}'.\nReason: 1 dependency failed.",
			drv.to_absolute_path()
		);
		let derivation = self.store.read_derivation(drv)?;
		let mut outputs: Vec<String> = derivation
			.outputs()
			.store_paths()
			.map(StorePath::to_absolute_path)
			.collect();
		outputs.sort();
		if !outputs.is_empty() {
			message += "\nOutput paths:";
		}
		for output in outputs {
			message += &format!("\n  {output}");
		}
		Ok(message)
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
		// CppNix's worker starts ready builds in the order of their goal keys,
		// `dd$NAME$PATH`, so independent derivations build in name order.
		// Without `--keep-going`, a failure starts nothing new: the
		// derivations waiting on it fail in turn, those that are not
		// requested logging why, until a requested one fails.
		let targets: BTreeSet<&StorePath> = paths
			.iter()
			.filter_map(|path| match path {
				DerivedPath::Built { drv, .. } => Some(drv),
				DerivedPath::Opaque(_) => None,
			})
			.collect();
		let order = self.build_order(paths)?;
		let mut built: BTreeSet<&StorePath> = BTreeSet::new();
		let mut failed: BTreeSet<&StorePath> = BTreeSet::new();
		let mut exit_status = 1;
		let mut failure = None;
		for (drv, inputs) in &order {
			let message = if failed.is_empty() {
				match builder.realise_with_events(drv, None, &mut events) {
					Ok(_) => {
						built.insert(drv);
						continue;
					}
					Err(BuildError::Failed { status, message }) => {
						exit_status = status.exit_status();
						message
					}
					Err(error) => return Err(error.into()),
				}
			} else if inputs.iter().any(|input| failed.contains(input)) {
				self.dependency_failed(drv)?
			} else {
				continue;
			};
			failed.insert(drv);
			if targets.contains(drv) {
				failure = Some(message);
				break;
			}
			log_error(&message);
		}
		if let Some(message) = failure {
			let unfinished: BTreeSet<String> = targets
				.iter()
				.filter(|drv| {
					!built.contains(**drv) && order.iter().any(|(other, _)| other == **drv)
				})
				.map(|drv| drv.to_absolute_path())
				.collect();
			if unfinished.len() == 1 {
				return Err(Error::Build {
					exit_status,
					message,
				});
			}
			log_error(&message);
			let quoted: Vec<String> = unfinished.iter().map(|p| format!("'{p}'")).collect();
			return Err(Error::Build {
				exit_status,
				message: format!("build of {} failed", quoted.join(", ")),
			});
		}
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
					result?;
				}
			}
		}
		Ok(())
	}
}

/// Print an error as CppNix's `logError` does: continuation lines line up
/// after the `error: ` prefix.
fn log_error(message: &str) {
	eprintln!("error: {}", message.trim_end().replace('\n', "\n       "));
}
