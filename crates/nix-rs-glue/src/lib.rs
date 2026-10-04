//! Store-aware builtins for the nix-rs evaluator.
//!
//! [`configure`] adds CppNix 2.35.2-compatible store builtins. The default
//! [`EvaluatorStore`] computes paths without writing; explicitly diverted
//! writable stores also register text, sources and derivations. Neither
//! mode builds outputs or fetches sources. Derived from
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

pub use store::{EvaluatorStore, ReadOnlyStore, StoreIO};

/// Enable impure evaluation with `store` computing store paths, and add the
/// store-aware builtins. A search path already set on `builder` is kept;
/// otherwise `NIX_PATH` is used. Set the builder's experimental features
/// first: builtins gated on a feature are added only when it is enabled.
pub fn configure<'co, 'ro, 'env>(
	builder: EvaluationBuilder<'co, 'ro, 'env, Box<dyn EvalIO>>,
	store: Rc<EvaluatorStore>,
) -> EvaluationBuilder<'co, 'ro, 'env, Box<dyn EvalIO>> {
	let builder = builder.enable_impure(Some(Box::new(StoreIO::new(Rc::clone(&store)))));
	// `fetchTree` exists only with the `fetch-tree` experimental feature,
	// which `flakes` implies.
	let fetch_tree = builder.experimental_feature_enabled("fetch-tree");
	let flakes = builder.experimental_feature_enabled("flakes");
	builder
		// CppNix looks up `<nix/...>` in its built-in files after the search path.
		.corepkgs_dir(Some(store::COREPKGS_DIR.into()))
		.add_builtins(derivation::derivation_builtins::builtins(Rc::clone(&store)))
		.add_builtins(import::import_builtins::builtins(store))
		.add_builtins(context_builtins::builtins())
		.add_builtins(flake_builtins::builtins(flakes))
		.add_builtins(
			unsupported_builtins::builtins()
				.into_iter()
				.filter(move |(name, _)| fetch_tree || *name != "fetchTree"),
		)
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

#[builtins]
mod context_builtins {
	use bstr::ByteSlice;
	use nix_compat::store_path::StorePath;
	use nix_rs_eval::generators::{self, Gen, GenCo};
	use nix_rs_eval::{CoercionKind, ErrorKind, NixContextElement, NixString, Value};

	#[builtin("addDrvOutputDependencies")]
	async fn builtin_add_drv_output_dependencies(
		co: GenCo,
		value: Value,
	) -> Result<Value, ErrorKind> {
		let string = generators::request_string_coerce(
			&co,
			value,
			CoercionKind {
				strong: false,
				import_paths: true,
			},
		)
		.await
		.map_err(ErrorKind::CatchableError)?;
		let context: Vec<_> = string
			.iter_context()
			.flat_map(|ctx| ctx.iter().cloned())
			.collect();
		if context.len() != 1 {
			return Err(ErrorKind::EvalError(format!(
				"context of string '{}' must have exactly one element, but has {}",
				string.to_str()?,
				context.len()
			)));
		}
		let element = match &context[0] {
			NixContextElement::Plain(path) => {
				let path = StorePath::from_absolute_path(path.as_bytes())
					.map_err(|error| ErrorKind::EvalError(error.to_string()))?;
				if !path.to_absolute_path().ends_with(".drv") {
					return Err(ErrorKind::EvalError(format!(
						"path '{}' is not a derivation",
						path.to_absolute_path()
					)));
				}
				NixContextElement::Derivation(path.to_absolute_path())
			}
			NixContextElement::Single { name, .. } => {
				return Err(ErrorKind::EvalError(format!(
					"`addDrvOutputDependencies` can only act on derivations, not on a derivation output such as '{name}'"
				)));
			}
			element @ NixContextElement::Derivation(_) => element.clone(),
		};
		Ok(Value::String(NixString::new_context_from(
			element.into(),
			string.as_bytes(),
		)))
	}
}

#[builtins(state = "bool")]
mod flake_builtins {
	use bstr::ByteSlice;
	use nix_rs_eval::generators::{self, Gen, GenCo};
	use nix_rs_eval::{ErrorKind, NixAttrs, Value};
	use serde_json::{Map, Value as Json};

	fn enabled(enabled: bool) -> Result<(), ErrorKind> {
		if enabled {
			Ok(())
		} else {
			Err(ErrorKind::EvalError("experimental Nix feature 'flakes' is disabled; add '--extra-experimental-features flakes' to enable it".into()))
		}
	}

	#[builtin("parseFlakeRef")]
	async fn builtin_parse_flake_ref(
		state: bool,
		co: GenCo,
		value: Value,
	) -> Result<Value, ErrorKind> {
		let _ = co;
		enabled(state)?;
		let string = value.to_contextful_str()?;
		if string.has_context() {
			return Err(ErrorKind::EvalError(
				"the string is not allowed to refer to a store path".into(),
			));
		}
		let attrs =
			nix_rs_flakeref::attrs::parse(string.to_str()?).map_err(ErrorKind::EvalError)?;
		let attrs = attrs
			.into_iter()
			.map(|(key, value)| {
				let value = match value {
					Json::String(value) => Value::from(value),
					Json::Bool(value) => Value::Bool(value),
					Json::Number(value) => {
						Value::Integer(value.as_i64().unwrap_or(value.as_u64().unwrap_or(0) as i64))
					}
					_ => unreachable!("fetcher attributes are scalars"),
				};
				(key, value)
			})
			.collect::<NixAttrs>();
		Ok(Value::Attrs(attrs))
	}

	#[builtin("flakeRefToString")]
	async fn builtin_flake_ref_to_string(
		state: bool,
		co: GenCo,
		value: Value,
	) -> Result<Value, ErrorKind> {
		enabled(state)?;
		let attrs = value.to_attrs()?;
		let mut values = Map::new();
		for (key, value) in attrs.iter_sorted() {
			let value = generators::request_force(&co, value.clone()).await;
			let json = match value {
				Value::String(value) => Json::String(value.to_str()?.to_owned()),
				Value::Bool(value) => Json::Bool(value),
				Value::Integer(value) if value >= 0 => Json::from(value),
				Value::Integer(value) => {
					return Err(ErrorKind::EvalError(format!(
						"negative value given for flake ref attr {}: {value}",
						key.to_str()?
					)));
				}
				value => {
					return Err(ErrorKind::EvalError(format!(
						"flake reference attribute sets may only contain integers, Booleans, and strings, but attribute '{}' is {}",
						key.to_str()?,
						value.type_of()
					)));
				}
			};
			values.insert(key.to_str()?.to_owned(), json);
		}
		Ok(Value::from(
			nix_rs_flakeref::attrs::to_string(&values).map_err(ErrorKind::EvalError)?,
		))
	}
}
