//! What realising some paths would take, following CppNix 2.35.2's
//! `Store::queryMissing` in `misc.cc`.

use std::collections::{BTreeMap, BTreeSet};

use bstr::BString;
use nix_compat::store_path::StorePath;
use nix_rs_store::LocalStore;
use nix_rs_store::binary_cache::BinaryCache;

use crate::error::{BuildError, Result};
use crate::options::{self, DerivationOptions};

/// A path to realise: a store path, or outputs of a derivation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DerivedPath {
	Opaque(StorePath),
	/// `None` means all outputs (`drv^*`).
	Built {
		drv_path: StorePath,
		outputs: Option<BTreeSet<String>>,
	},
}

impl DerivedPath {
	/// `DerivedPath::parseLegacy`: `PATH`, or `DRV!OUTPUTS` with `*` or a
	/// comma-separated list.
	pub fn parse_legacy(s: &str, parse: impl Fn(&str) -> Result<StorePath>) -> Result<Self> {
		match s.rsplit_once('!') {
			None => Ok(DerivedPath::Opaque(parse(s)?)),
			Some((drv, outputs)) => Ok(DerivedPath::Built {
				drv_path: parse(drv)?,
				outputs: if outputs == "*" {
					None
				} else {
					Some(outputs.split(',').map(str::to_owned).collect())
				},
			}),
		}
	}
}

pub use nix_rs_store::Missing;

/// `queryMissing`.
pub fn query_missing(
	store: &LocalStore,
	substituters: &[BinaryCache],
	use_substitutes: bool,
	always_allow_substitutes: bool,
	targets: &[DerivedPath],
) -> Result<Missing> {
	let mut res = Missing::default();
	let mut visited = BTreeSet::new();
	let mut todo: Vec<DerivedPath> = targets.to_vec();
	let prefetch_enabled = use_substitutes
		&& store.config().substitution_jobs() > 1
		&& store.config().narinfo_cache_positive_ttl != 0
		&& store.config().narinfo_cache_negative_ttl != 0
		&& substituters.first().is_some_and(|cache| {
			cache.uri().starts_with("http://") || cache.uri().starts_with("https://")
		});
	let prefetch = |paths: Vec<StorePath>| {
		if use_substitutes && let Some(cache) = substituters.first() {
			let paths: Vec<_> = paths
				.into_iter()
				.filter(|path| !store.is_valid_path(path).unwrap_or(true))
				.collect();
			cache.prefetch_path_info(&paths, store.config().substitution_jobs());
		}
	};
	if prefetch_enabled {
		prefetch(
			targets
				.iter()
				.filter_map(|target| match target {
					DerivedPath::Opaque(path) => Some(path.clone()),
					_ => None,
				})
				.collect(),
		);
	}
	let substitutable = |path: &StorePath| -> Option<(u64, u64, BTreeSet<StorePath>)> {
		if !use_substitutes {
			return None;
		}
		substituters.iter().find_map(|cache| {
			let cached = cache.query_path_info(path).ok().flatten()?;
			Some((
				cached.file_size.unwrap_or(0),
				cached.info.nar_size,
				cached.info.references,
			))
		})
	};
	while let Some(req) = todo.pop() {
		if !visited.insert(req.clone()) {
			continue;
		}
		match req {
			DerivedPath::Opaque(path) => {
				if store.is_valid_path(&path)? {
					continue;
				}
				match substitutable(&path) {
					None => {
						res.unknown.insert(path);
					}
					Some((download, nar, references)) => {
						if prefetch_enabled {
							prefetch(
								references
									.iter()
									.filter(|path| {
										!visited.contains(&DerivedPath::Opaque((*path).clone()))
									})
									.cloned()
									.collect(),
							);
						}
						res.will_substitute.insert(path);
						res.download_size += download;
						res.nar_size += nar;
						todo.extend(references.into_iter().map(DerivedPath::Opaque));
					}
				}
			}
			DerivedPath::Built { drv_path, outputs } => {
				if !store.is_valid_path(&drv_path)? {
					res.unknown.insert(drv_path);
					continue;
				}
				let build_drv = store.read_build_derivation(&drv_path)?;
				let drv = &build_drv.basic;
				let output_map = store.query_partial_derivation_output_map(&drv_path)?;
				let mut invalid = BTreeSet::new();
				let mut unknown_output = false;
				for (name, path) in output_map {
					let wanted = outputs.as_ref().is_none_or(|o| o.contains(&name));
					if wanted {
						match path {
							Some(path) if !store.is_valid_path(&path)? => {
								invalid.insert(path);
							}
							None => unknown_output = true,
							_ => {}
						}
					}
				}
				if invalid.is_empty() && !unknown_output {
					continue;
				}
				let env: BTreeMap<BString, BString> = drv
					.environment()
					.iter()
					.filter(|(k, _)| k.as_slice() != b"__json")
					.map(|(k, v)| (k.clone(), v.clone()))
					.collect();
				let parsed = options::structured_attrs(drv.environment())?;
				let drv_options = DerivationOptions::new(
					&env,
					parsed.as_ref(),
					drv.input_derivations(),
					store.config().ca_derivations,
					&mut Vec::new(),
				)
				.map_err(|e| {
					BuildError::Other(format!(
						"{e}\n… while parsing derivation '{}'",
						drv_path.to_absolute_path()
					))
				})?;
				let mut must_build = true;
				if !unknown_output
					&& use_substitutes
					&& drv_options.substitutes_allowed(always_allow_substitutes)
				{
					let found: Vec<StorePath> = invalid
						.iter()
						.filter(|p| substitutable(p).is_some())
						.cloned()
						.collect();
					if found.len() == invalid.len() {
						must_build = false;
						todo.extend(found.into_iter().map(DerivedPath::Opaque));
					}
				}
				if must_build {
					res.will_build.insert(drv_path.clone());
					for (input, names) in drv.input_derivations() {
						todo.push(DerivedPath::Built {
							drv_path: input.clone(),
							outputs: Some(names.iter().map(|n| n.to_string()).collect()),
						});
					}
				}
			}
		}
	}
	Ok(res)
}
