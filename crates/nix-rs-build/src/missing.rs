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
	let substitutable = |path: &StorePath| -> Option<(u64, u64, BTreeSet<StorePath>)> {
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
				let drv = store.read_derivation(&drv_path)?;
				if drv.outputs().is_deferred() {
					return Err(BuildError::Other(format!(
						"querying missing paths of '{}' needs content-addressed derivations, which nix-rs does not support yet",
						drv_path.to_absolute_path()
					)));
				}
				let mut invalid = BTreeSet::new();
				for (name, path) in drv.outputs().iter() {
					let wanted = outputs
						.as_ref()
						.is_none_or(|o| o.contains(&name.to_string()));
					if wanted && !store.is_valid_path(path)? {
						invalid.insert(path.clone());
					}
				}
				if invalid.is_empty() {
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
					false,
					&mut Vec::new(),
				)
				.map_err(|e| {
					BuildError::Other(format!(
						"{e}\n… while parsing derivation '{}'",
						drv_path.to_absolute_path()
					))
				})?;
				let mut must_build = true;
				if use_substitutes && drv_options.substitutes_allowed(always_allow_substitutes) {
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
