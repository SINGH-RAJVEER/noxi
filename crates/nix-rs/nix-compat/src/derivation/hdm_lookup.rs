use std::collections::BTreeMap;

use crate::{derivation::OutputName, nixhash::Sha256, store_path::StorePathRef};

/// The two concrete hash-modulo results used for input-addressed derivations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DrvHashModulo {
	/// A regular derivation has one hash for all its outputs.
	Regular(Sha256),
	/// A fixed-output derivation has a hash for each named output.
	Fixed(BTreeMap<OutputName, Sha256>),
}

/// Lookup hash derivation modulo for a derivation.
///
/// The [hash derivation modulo] is recursive. And so to calculate the HDM of a
/// derivation it requires the HDM for all its input derivations.
///
/// This trait exists to provide the HDM values for those input derivations to
/// the functions that calculate the HDM.
///
/// To help with implementing this trait [`lookup_fn`] is provided.
///
/// [hash derivation modulo]: nix_compat::derivation#hash-derivation-modulo
pub trait HashDerivationModuloLookup {
	/// Lookup the [hash derivation modulo] of the provided derivation store path.
	///
	/// [hash derivation modulo]: nix_compat::derivation#hash-derivation-modulo
	fn lookup_hdm(&self, drv_path: &StorePathRef<'_>) -> Option<DrvHashModulo>;
}

/// Implement [`HashDerivationModuloLookup`] using the provided closure.
pub fn lookup_fn<F>(func: F) -> impl HashDerivationModuloLookup
where
	F: Fn(&StorePathRef) -> Option<DrvHashModulo>,
{
	LookupFn { func }
}

struct LookupFn<F> {
	func: F,
}

impl<F> HashDerivationModuloLookup for LookupFn<F>
where
	F: Fn(&StorePathRef) -> Option<DrvHashModulo>,
{
	fn lookup_hdm(&self, drv_path: &StorePathRef) -> Option<DrvHashModulo> {
		(self.func)(drv_path)
	}
}

#[cfg(feature = "hashbrown")]
impl HashDerivationModuloLookup
	for hashbrown::HashMap<crate::store_path::StorePath, DrvHashModulo>
{
	fn lookup_hdm(&self, drv_path: &StorePathRef<'_>) -> Option<DrvHashModulo> {
		self.get(drv_path).cloned()
	}
}
