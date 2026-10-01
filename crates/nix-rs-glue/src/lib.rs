//! Store-aware builtins for the nix-rs evaluator.
//!
//! [`configure`] turns an evaluation into CppNix 2.35.2's read-only mode,
//! the default for `nix-instantiate --eval`. Derivations, imported paths and
//! `builtins.toFile` results get the store paths CppNix computes for them,
//! but nothing is written to the store and nothing is fetched. Derived from
//! Snix's `glue` and `build-glue` crates; see `PROVENANCE.md` in
//! `crates/nix-rs/`.

use std::rc::Rc;

use nix_rs_eval::builtin_macros::builtins;
use nix_rs_eval::{EvalIO, EvaluationBuilder};

mod derivation;
pub mod errors;
mod import;
pub mod known_paths;
pub mod nar;
pub mod store;

pub use store::{ReadOnlyStore, StoreIO};

/// Enable impure evaluation with `store` computing store paths, and add the
/// store-aware builtins. A search path already set on `builder` is kept;
/// otherwise `NIX_PATH` is used.
pub fn configure<'co, 'ro, 'env>(
	builder: EvaluationBuilder<'co, 'ro, 'env, Box<dyn EvalIO>>,
	store: Rc<ReadOnlyStore>,
) -> EvaluationBuilder<'co, 'ro, 'env, Box<dyn EvalIO>> {
	let builder = builder.enable_impure(Some(Box::new(StoreIO::new(Rc::clone(&store)))));
	// CppNix looks up `<nix/...>` in its built-in files after the search path.
	let nix_path = match builder.get_nix_path() {
		Some(path) if !path.is_empty() => format!("{path}:nix={}", store::COREPKGS_DIR),
		_ => format!("nix={}", store::COREPKGS_DIR),
	};
	builder
		.nix_path(Some(nix_path))
		.add_builtins(derivation::derivation_builtins::builtins(Rc::clone(&store)))
		.add_builtins(import::import_builtins::builtins(store))
		.add_builtins(unsupported_builtins::builtins())
		.add_src_builtin("derivation", include_str!("derivation.nix"))
}

/// Builtins that need the network or a writable store. They exist so that
/// files mentioning them still compile, and fail only when called.
#[builtins]
mod unsupported_builtins {
	use nix_rs_eval::generators::Gen;
	use nix_rs_eval::generators::GenCo;
	use nix_rs_eval::{ErrorKind, Value};

	#[builtin("fetchurl")]
	async fn builtin_fetchurl(_co: GenCo, #[lazy] _args: Value) -> Result<Value, ErrorKind> {
		Err(ErrorKind::NotImplemented("builtins.fetchurl"))
	}

	#[builtin("fetchTarball")]
	async fn builtin_fetch_tarball(_co: GenCo, #[lazy] _args: Value) -> Result<Value, ErrorKind> {
		Err(ErrorKind::NotImplemented("builtins.fetchTarball"))
	}

	#[builtin("fetchGit")]
	async fn builtin_fetch_git(_co: GenCo, #[lazy] _args: Value) -> Result<Value, ErrorKind> {
		Err(ErrorKind::NotImplemented("builtins.fetchGit"))
	}

	#[builtin("fetchMercurial")]
	async fn builtin_fetch_mercurial(_co: GenCo, #[lazy] _args: Value) -> Result<Value, ErrorKind> {
		Err(ErrorKind::NotImplemented("builtins.fetchMercurial"))
	}

	#[builtin("fetchTree")]
	async fn builtin_fetch_tree(_co: GenCo, #[lazy] _args: Value) -> Result<Value, ErrorKind> {
		Err(ErrorKind::NotImplemented("builtins.fetchTree"))
	}

	#[builtin("getFlake")]
	async fn builtin_get_flake(_co: GenCo, #[lazy] _args: Value) -> Result<Value, ErrorKind> {
		Err(ErrorKind::NotImplemented("builtins.getFlake"))
	}
}

#[cfg(test)]
mod tests {
	use nix_rs_eval::Evaluation;

	use super::*;

	#[test]
	fn keeps_a_search_path_set_on_the_builder() {
		let builder = Evaluation::builder_pure().nix_path(Some("a=/b".into()));
		let builder = configure(builder, ReadOnlyStore::new());
		assert_eq!(
			builder.get_nix_path(),
			Some(format!("a=/b:nix={}", store::COREPKGS_DIR).as_str())
		);
	}
}
