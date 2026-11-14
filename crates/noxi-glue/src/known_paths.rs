//! Keeps track of the derivations and text files an evaluation has
//! instantiated, so that later derivations can refer to them.
//!
//! Derived from Snix's `build-glue/src/known_paths.rs`, without its fetch
//! tracking.

use std::collections::{BTreeSet, HashMap};

use nix_compat::derivation::{
	Derivation, DerivationError, DrvHashModulo, HashDerivationModuloLookup, UnverifiedDerivation,
};
use nix_compat::store_path::{StorePath, StorePathRef};

/// Every derivation and text file created during one evaluation.
#[derive(Debug, Default)]
pub struct KnownPaths {
	/// Derivations by their `.drv` path, with their hash derivation modulo.
	derivations: HashMap<StorePath, (DrvHashModulo, Derivation)>,
	imported_derivations: HashMap<StorePath, (DrvHashModulo, UnverifiedDerivation)>,
	/// Floating CA derivations and derivations depending on their unknown
	/// outputs have no hash derivation modulo until they are realised.
	deferred_derivations: HashMap<StorePath, noxi_store::build_derivation::BuildDerivation>,

	/// The references of text files written by `builtins.toFile`, which
	/// the closure of a derivation that depends on them has to include.
	text_references: HashMap<StorePath, BTreeSet<StorePath>>,
}

impl KnownPaths {
	/// Return the derivation stored at `drv_path`.
	pub fn get_drv_by_drvpath(&self, drv_path: &StorePathRef) -> Option<&Derivation> {
		self.derivations
			.get(&drv_path.to_owned())
			.map(|(_, derivation)| derivation)
	}

	/// Return data for a local or previously registered derivation.
	pub fn get_unverified_drv(&self, path: &StorePathRef) -> Option<&UnverifiedDerivation> {
		self.get_drv_by_drvpath(path)
			.map(Derivation::as_unverified)
			.or_else(|| {
				self.imported_derivations
					.get(&path.to_owned())
					.map(|(_, drv)| drv)
			})
			.or_else(|| {
				self.deferred_derivations
					.get(&path.to_owned())
					.map(|drv| &drv.basic)
			})
	}

	/// Retain the actual floating declarations for later store registration;
	/// their ordinary body also supplies closure and output-name information.
	pub fn add_deferred_derivation(
		&mut self,
		path: StorePath,
		drv: noxi_store::build_derivation::BuildDerivation,
	) {
		self.deferred_derivations.insert(path, drv);
	}

	/// The full declaration of a deferred derivation of this evaluation.
	pub fn get_deferred(
		&self,
		path: &StorePath,
	) -> Option<&noxi_store::build_derivation::BuildDerivation> {
		self.deferred_derivations.get(path)
	}

	pub fn has_deferred_input(&self, path: &StorePath) -> bool {
		self.deferred_derivations
			.get(path)
			.is_some_and(|drv| drv.basic.outputs().is_deferred())
			|| self
				.get_unverified_drv(&path.as_ref())
				.is_some_and(|drv| drv.outputs().is_deferred())
	}

	/// Cache an existing store derivation without revalidating its output
	/// paths, matching the reference store's derivation reader.
	pub fn add_imported_derivation(
		&mut self,
		path: StorePath,
		drv: UnverifiedDerivation,
	) -> Result<(), DerivationError> {
		let hash = drv.hash_derivation_modulo(&*self)?;
		self.imported_derivations.insert(path, (hash, drv));
		Ok(())
	}

	/// Insert a derivation whose inputs are all already known.
	pub fn add_verified_derivation(&mut self, drv: Derivation) -> Result<(), DerivationError> {
		let hash_derivation_modulo = drv.hash_derivation_modulo(&*self)?;
		self.derivations
			.insert(drv.drv_path().to_owned(), (hash_derivation_modulo, drv));
		Ok(())
	}

	/// Record the references of a text file created by `builtins.toFile`.
	pub fn add_text(&mut self, path: StorePath, references: BTreeSet<StorePath>) {
		self.text_references.insert(path, references);
	}

	/// The references of `path` as the store would record them: a
	/// derivation refers to its input sources and input derivations, and a
	/// text file to the paths in its context. Anything else, such as an
	/// imported source, has none.
	fn references(&self, path: &StorePath) -> BTreeSet<StorePath> {
		if let Some(references) = self.text_references.get(path) {
			return references.clone();
		}
		if let Some(drv) = self.get_unverified_drv(&path.as_ref()) {
			drv.input_sources()
				.iter()
				.chain(drv.input_derivations().keys())
				.cloned()
				.collect()
		} else {
			self.text_references.get(path).cloned().unwrap_or_default()
		}
	}

	/// The closure of `path` under [`Self::references`], including itself.
	pub fn closure(&self, path: &StorePath) -> BTreeSet<StorePath> {
		let mut closure = BTreeSet::new();
		let mut pending = vec![path.clone()];
		while let Some(next) = pending.pop() {
			if closure.insert(next.clone()) {
				pending.extend(self.references(&next));
			}
		}
		closure
	}
}

impl HashDerivationModuloLookup for &KnownPaths {
	fn lookup_hdm(&self, drv_path: &StorePathRef<'_>) -> Option<DrvHashModulo> {
		self.derivations
			.get(&drv_path.to_owned())
			.map(|(hash_derivation_modulo, _)| hash_derivation_modulo.clone())
			.or_else(|| {
				self.imported_derivations
					.get(&drv_path.to_owned())
					.map(|(hash, _)| hash.clone())
			})
			.or_else(|| {
				self.deferred_derivations
					.get(&drv_path.to_owned())
					.and_then(|drv| drv.fixed_hash_derivation_modulo())
			})
	}
}
