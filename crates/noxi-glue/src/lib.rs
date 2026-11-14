//! Store-aware builtins for the noxi evaluator.
//!
//! [`configure`] adds CppNix 2.35.2-compatible store builtins. The default
//! [`EvaluatorStore`] computes paths without writing; explicitly diverted
//! writable stores also register text, sources and derivations. A configured
//! owner can build requested output contexts during writable evaluation.
//! Local and URL fetching is implemented by [`fetch`]. Derived from
//! Snix's `glue` and `build-glue` crates; see `PROVENANCE.md` in
//! `crates/noxi/`.

use std::rc::Rc;

use noxi_eval::builtin_macros::builtins;
use noxi_eval::{EvalIO, EvaluationBuilder};

mod derivation;
pub mod errors;
pub mod fetch;
mod import;
pub mod known_paths;
mod mount;
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
	let ca_derivations = builder.experimental_feature_enabled("ca-derivations");
	let impure_derivations = builder.experimental_feature_enabled("impure-derivations");
	let blake3_hashes = builder.experimental_feature_enabled("blake3-hashes");
	let dynamic_derivations = builder.experimental_feature_enabled("dynamic-derivations");
	let git_hashing = builder.experimental_feature_enabled("git-hashing");
	store.set_output_features(noxi_store::build_derivation::OutputFeatures {
		ca_derivations,
		impure_derivations,
		dynamic_derivations,
		git_hashing,
	});
	let builder = builder.enable_impure(Some(Box::new(
		StoreIO::new(Rc::clone(&store)).with_ca_derivations(ca_derivations),
	)));
	// `fetchTree` exists only with the `fetch-tree` experimental feature,
	// which `flakes` implies.
	let fetch_tree = builder.experimental_feature_enabled("fetch-tree");
	let flakes = builder.experimental_feature_enabled("flakes");
	store.fetch_config.borrow_mut().settings.flakes = flakes;
	builder
		// CppNix looks up `<nix/...>` in its built-in files after the search path.
		.corepkgs_dir(Some(store::COREPKGS_DIR.into()))
		.add_builtins(
			derivation::derivation_builtins::builtins(Rc::new(derivation::DerivationState {
				store: Rc::clone(&store),
				ca_derivations,
				impure_derivations,
				blake3_hashes,
				dynamic_derivations,
				git_hashing,
			}))
			.into_iter()
			// `outputOf` exists only with the `dynamic-derivations` feature.
			.filter(move |(name, _)| dynamic_derivations || *name != "outputOf"),
		)
		.add_builtins(import::import_builtins::builtins(Rc::clone(&store)))
		.add_builtins(fetch::mercurial::mercurial_builtins::builtins(Rc::clone(
			&store,
		)))
		.add_builtins(
			fetch::builtins::fetch_builtins::builtins(store)
				.into_iter()
				.filter(move |(name, _)| fetch_tree || *name != "fetchTree"),
		)
		.add_builtins(context_builtins::builtins())
		.add_builtins(flake_builtins::builtins(flakes))
		.add_src_builtin("derivation", {
			static SOURCE: std::sync::LazyLock<String> =
				std::sync::LazyLock::new(|| include_str!("derivation.nix").replace('\t', "  "));
			SOURCE.as_str()
		})
}

#[builtins]
mod context_builtins {
	use bstr::ByteSlice;
	use nix_compat::store_path::StorePath;
	use noxi_eval::generators::{self, Gen, GenCo};
	use noxi_eval::{CoercionKind, ErrorKind, NixContextElement, NixString, Value};

	#[builtin("addDrvOutputDependencies")]
	async fn builtin_add_drv_output_dependencies(
		co: GenCo,
		#[lazy] value: Value,
	) -> Result<Value, ErrorKind> {
		generators::request_builtin_context(
			&co,
			vec![noxi_eval::Trace::new(
				None,
				"while evaluating the argument passed to builtins.addDrvOutputDependencies",
			)],
		)
		.await;
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
		generators::request_builtin_context(&co, Vec::new()).await;
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
	use noxi_eval::generators::{self, Gen, GenCo};
	use noxi_eval::{ErrorKind, NixAttrs, Value};
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
		#[lazy] value: Value,
	) -> Result<Value, ErrorKind> {
		enabled(state)?;
		let string = crate::fetch::builtins::string_no_context(
			&co,
			value,
			noxi_eval::Trace::new(
				None,
				"while evaluating the argument passed to builtins.parseFlakeRef",
			),
		)
		.await?;
		let settings = noxi_flakeref::input::FetchSettings {
			flakes: state,
			..Default::default()
		};
		let parsed = noxi_flakeref::flake_ref::parse_flake_ref(
			&settings,
			string.to_str()?,
			noxi_flakeref::flake_ref::ParseOptions {
				allow_missing: true,
				..Default::default()
			},
		)
		.map_err(|error| ErrorKind::EvalError(error.to_string()))?;
		let attrs = noxi_flakeref::input::attrs_to_json(&parsed.to_attrs())
			.map_err(|error| ErrorKind::EvalError(error.to_string()))?;
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
		let attrs = value.to_attrs().map_err(|error| {
			noxi_eval::AddContext::trace(
				error,
				None,
				"while evaluating the argument passed to builtins.flakeRefToString",
			)
		})?;
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
		Ok(Value::from({
			let attrs: noxi_flakeref::input::Attrs = values
				.into_iter()
				.map(|(key, value)| {
					let attr = match value {
						Json::String(value) => noxi_flakeref::input::Attr::String(value),
						Json::Bool(value) => noxi_flakeref::input::Attr::Bool(value),
						Json::Number(value) => {
							noxi_flakeref::input::Attr::Int(value.as_u64().unwrap())
						}
						_ => unreachable!("validated scalar attribute"),
					};
					(key, attr)
				})
				.collect();
			let settings = noxi_flakeref::input::FetchSettings {
				flakes: state,
				..Default::default()
			};
			noxi_flakeref::flake_ref::FlakeRef::from_attrs(&settings, &attrs)
				.and_then(|reference| reference.try_to_string())
				.map_err(|error| ErrorKind::EvalError(error.to_string()))?
		}))
	}
}
