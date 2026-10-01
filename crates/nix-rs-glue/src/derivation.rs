//! `derivationStrict` and `placeholder`, derived from Snix's
//! `glue/src/builtins/derivation.rs` and changed where CppNix 2.35.2 differs.

use std::collections::{BTreeMap, btree_map};
use std::rc::Rc;
use std::sync::Arc;

use bstr::{BString, ByteSlice};
use nix_compat::derivation::{DerivationBuilder, OutputHash, OutputName, OutputsBuilder};
use nix_compat::nixhash::{HashAlgo, NixHash};
use nix_compat::store_path::{StorePath, hash_placeholder};
use nix_rs_eval::builtin_macros::builtins;
use nix_rs_eval::generators::{self, GenCo};
use nix_rs_eval::{
	AddContext, CatchableErrorKind, CoercionKind, ErrorKind, NixAttrs, NixContext,
	NixContextElement, NixString, Value, try_cek, try_cek_to_value,
};

use crate::errors::DerivationError;
use crate::known_paths::KnownPaths;
use crate::store::ReadOnlyStore;

const IGNORE_NULLS: &str = "__ignoreNulls";
const STRUCTURED_ATTRS: &str = "__structuredAttrs";
const JSON_KEY: &str = "__json";

/// Force `value` and coerce it to a string as CppNix's `coerceToString`
/// does with `coerceMore` and `copyToStore` set.
pub(crate) async fn coerce_importing(
	co: &GenCo,
	value: Value,
) -> Result<NixString, CatchableErrorKind> {
	let value = generators::request_force(co, value).await;
	generators::request_string_coerce(
		co,
		value,
		CoercionKind {
			strong: true,
			import_paths: true,
		},
	)
	.await
}

async fn select_string(
	co: &GenCo,
	attrs: &NixAttrs,
	key: &str,
) -> Result<Result<Option<String>, CatchableErrorKind>, ErrorKind> {
	if let Some(attr) = attrs.select(key) {
		let string = try_cek!(coerce_importing(co, attr.clone()).await);
		return Ok(Ok(Some(string.to_str()?.to_owned())));
	}
	Ok(Ok(None))
}

fn parse_store_path(path: &str) -> Result<StorePath, DerivationError> {
	StorePath::from_absolute_path(path.as_bytes())
		.map_err(|_| DerivationError::InvalidContext(path.to_owned()))
}

/// Turn the context collected from a derivation's attributes into its
/// input sources and input derivations, as CppNix's `derivationStrict` does.
fn populate_inputs(
	drv: &mut DerivationBuilder,
	context: NixContext,
	known_paths: &KnownPaths,
) -> Result<(), DerivationError> {
	for element in context.iter() {
		match element {
			NixContextElement::Plain(path) => {
				drv.input_sources.insert(parse_store_path(path)?);
			}
			NixContextElement::Single { name, derivation } => {
				let derivation = parse_store_path(derivation)?;
				let name: OutputName = name
					.parse()
					.map_err(|_| DerivationError::InvalidContext(name.clone()))?;
				drv.input_derivations
					.entry(derivation)
					.or_default()
					.insert(name);
			}
			// A whole derivation, as `drvPath` refers to it: CppNix adds its
			// entire closure, and every output of each derivation in it. It
			// reads that closure from the store, so in read-only mode each
			// path in it must already be there.
			NixContextElement::Derivation(drv_path) => {
				let drv_path = parse_store_path(drv_path)?;
				if known_paths.get_drv_by_drvpath(&drv_path.as_ref()).is_none() {
					return Err(DerivationError::UnknownDerivation(
						drv_path.to_absolute_path(),
					));
				}
				let closure = known_paths.closure(&drv_path);
				for path in &closure {
					let absolute = path.to_absolute_path();
					if std::fs::symlink_metadata(&absolute).is_err() {
						return Err(DerivationError::InvalidPath(absolute));
					}
				}
				for path in closure {
					if let Some(input) = known_paths.get_drv_by_drvpath(&path.as_ref()) {
						let names = input.outputs().names().cloned();
						match drv.input_derivations.entry(path.clone()) {
							btree_map::Entry::Vacant(entry) => {
								entry.insert(names.collect());
							}
							btree_map::Entry::Occupied(mut entry) => entry.get_mut().extend(names),
						}
					}
					drv.input_sources.insert(path);
				}
			}
		}
	}
	Ok(())
}

#[builtins(state = "Rc<ReadOnlyStore>")]
pub(crate) mod derivation_builtins {
	use super::*;
	use nix_rs_eval::generators::Gen;

	#[builtin("placeholder")]
	async fn builtin_placeholder(co: GenCo, input: Value) -> Result<Value, ErrorKind> {
		if input.is_catchable() {
			return Ok(input);
		}
		let output_name = input
			.to_str()
			.context("looking at output name in builtins.placeholder")?;
		Ok(hash_placeholder(output_name.to_str()?).into())
	}

	/// Strictly construct a derivation from its attributes. Users normally
	/// call `builtins.derivation`, which wraps this.
	#[builtin("derivationStrict")]
	async fn builtin_derivation_strict(
		state: Rc<ReadOnlyStore>,
		co: GenCo,
		input: Value,
	) -> Result<Value, ErrorKind> {
		if input.is_catchable() {
			return Ok(input);
		}

		let input = input.to_attrs()?;
		let name = generators::request_force(&co, input.select_required("name")?.clone()).await;
		if name.is_catchable() {
			return Ok(name);
		}
		let name = name.to_str().context("determining derivation name")?;
		if name.is_empty() {
			return Err(ErrorKind::Abort("derivation has empty name".to_string()));
		}
		let name = name.to_str()?.to_owned();

		let mut drv = DerivationBuilder::default();
		let mut input_context = NixContext::new();
		// The outputs as given, in order; `out` when the attribute is absent.
		let mut output_names = vec![OutputName::out()];

		fn insert_env(
			drv: &mut DerivationBuilder,
			key: &str,
			value: BString,
		) -> Result<(), DerivationError> {
			if drv.environment.insert(key.into(), value).is_some() {
				return Err(DerivationError::DuplicateEnvVar(key.into()));
			}
			Ok(())
		}

		// A catchable error from any forced attribute is returned as is, so
		// `builtins.tryEval` still sees the original `throw`.
		let ignore_nulls = match input.select(IGNORE_NULLS) {
			Some(b) => {
				let b = generators::request_force(&co, b.clone()).await;
				if b.is_catchable() {
					return Ok(b);
				}
				b.as_bool()?
			}
			None => false,
		};

		// A sorted map, so the `__json` keys come out in CppNix's order.
		let mut structured_attrs: Option<BTreeMap<&str, serde_json::Value>> =
			match input.select(STRUCTURED_ATTRS) {
				Some(b) => {
					let b = generators::request_force(&co, b.clone()).await;
					if b.is_catchable() {
						return Ok(b);
					}
					b.as_bool()?.then_some(Default::default())
				}
				None => None,
			};

		for (arg_name, arg_value) in input.iter_sorted() {
			let arg_name = arg_name.to_str()?;
			let value = generators::request_force(&co, arg_value.clone()).await;
			if value.is_catchable() {
				return Ok(value);
			}

			if ignore_nulls && matches!(value, Value::Null) {
				continue;
			}

			match arg_name {
				"args" => {
					for arg in value.to_list()? {
						let s = try_cek_to_value!(coerce_importing(&co, arg).await);
						input_context.mimic(&s);
						drv.arguments.push(s.as_bytes().into())
					}
				}

				"outputs" => {
					let outputs = value
						.to_list()
						.context("looking at the `outputs` parameter of the derivation")?;

					output_names = Vec::with_capacity(outputs.len());
					for output in outputs {
						let output_name = generators::request_force(&co, output).await;
						if output_name.is_catchable() {
							return Ok(output_name);
						}
						let output_name =
							output_name.to_str().context("determining output name")?;
						input_context.mimic(&output_name);
						let output_name: OutputName = output_name
							.to_str()?
							.parse()
							.map_err(|err| ErrorKind::NixRsError(Arc::new(err)))?;
						if output_names.contains(&output_name) {
							return Err(DerivationError::DuplicateOutput(output_name).into());
						}
						output_names.push(output_name);
					}
					drv.outputs =
						OutputsBuilder::InputAddressed(output_names.iter().cloned().collect());

					match structured_attrs.as_mut() {
						Some(structured_attrs) => {
							let names = output_names.iter().map(ToString::to_string).collect();
							structured_attrs.insert(arg_name, names);
						}
						None => {
							let joined = output_names
								.iter()
								.map(ToString::to_string)
								.collect::<Vec<_>>()
								.join(" ");
							insert_env(&mut drv, arg_name, joined.into())?;
						}
					}
				}

				"builder" | "system" => {
					let val_str = try_cek_to_value!(coerce_importing(&co, value).await);
					input_context.mimic(&val_str);

					if arg_name == "builder" {
						drv.command = val_str.as_bytes().into();
					} else {
						drv.system = val_str.as_bytes().into();
					}

					if let Some(ref mut structured_attrs) = structured_attrs {
						structured_attrs.insert(arg_name, val_str.to_str()?.to_owned().into());
					} else {
						insert_env(&mut drv, arg_name, val_str.as_bytes().into())?;
					}
				}

				STRUCTURED_ATTRS if structured_attrs.is_some() => continue,
				IGNORE_NULLS => continue,

				_ => {
					if arg_name == JSON_KEY {
						return Err(DerivationError::StructuredAttrsJsonKeyPresent.into());
					}

					match structured_attrs {
						Some(ref mut structured_attrs) => {
							let (val_json, context) = match value.into_contextful_json(&co).await {
								Ok(json) => json,
								// A `throw` nested in the value.
								Err(ErrorKind::CatchableError(cek)) => {
									return Ok(Value::Catchable(Box::new(cek)));
								}
								Err(err) => return Err(err),
							};
							input_context.extend(context);
							structured_attrs.insert(arg_name, val_json);
						}
						None => {
							let val_str = try_cek_to_value!(coerce_importing(&co, value).await);
							input_context.mimic(&val_str);
							insert_env(&mut drv, arg_name, val_str.as_bytes().into())?;
						}
					}
				}
			}
		}

		// A non-empty `outputHash` makes this a fixed-output derivation.
		let hash_str = try_cek_to_value!(
			select_string(&co, &input, "outputHash")
				.await
				.context("evaluating the `outputHash` parameter")?
		)
		.filter(|s| !s.is_empty());
		if let Some(hash_str) = hash_str {
			let hash_algo = try_cek_to_value!(
				select_string(&co, &input, "outputHashAlgo")
					.await
					.context("evaluating the `outputHashAlgo` parameter")?
			)
			.filter(|s| !s.is_empty());
			let hash_mode = try_cek_to_value!(
				select_string(&co, &input, "outputHashMode")
					.await
					.context("evaluating the `outputHashMode` parameter")?
			)
			.filter(|s| !s.is_empty());

			if output_names != [OutputName::out()] {
				return Err(DerivationError::ConflictingOutputTypes.into());
			}

			let mode = hash_mode
				.map(|s| {
					s.parse()
						.map_err(|err| ErrorKind::NixRsError(Arc::new(err)))
				})
				.transpose()?
				.unwrap_or_default();
			let want_algo: Option<HashAlgo> = hash_algo
				.map(|s| s.parse())
				.transpose()
				.map_err(|err| ErrorKind::NixRsError(Arc::new(err)))?;
			let hash = NixHash::from_str(&hash_str, want_algo)
				.map_err(|err| ErrorKind::NixRsError(Arc::new(err)))?;
			drv.outputs = OutputsBuilder::Fixed(OutputHash { mode, hash });
		}

		// CppNix overwrites an attribute named after an output with the
		// output's path, so an attribute of that name has no effect.
		for output_name in drv.outputs.names() {
			drv.environment.remove(output_name.as_bytes());
		}

		if let Some(structured_attrs) = structured_attrs {
			drv.environment.insert(
				JSON_KEY.into(),
				BString::from(serde_json::to_string(&structured_attrs)?),
			);
		}

		let mut known_paths = state.known_paths();
		populate_inputs(&mut drv, input_context, &known_paths)?;

		let drv = drv
			.build(&name, &*known_paths)
			.map_err(DerivationError::InvalidDerivation)?;
		let drv_path = drv.drv_path().to_absolute_path();

		let out = Value::Attrs(NixAttrs::from_iter(
			drv.outputs()
				.iter()
				.map(|(output_name, path)| {
					(
						output_name.to_string(),
						NixString::new_context_from(
							NixContextElement::Single {
								name: output_name.to_string(),
								derivation: drv_path.clone(),
							}
							.into(),
							path.to_absolute_path(),
						),
					)
				})
				.chain(std::iter::once((
					"drvPath".to_owned(),
					NixString::new_context_from(
						NixContextElement::Derivation(drv_path.clone()).into(),
						drv_path.clone(),
					),
				))),
		));

		known_paths
			.add_verified_derivation(drv)
			.map_err(DerivationError::InvalidDerivation)?;

		Ok(out)
	}
}
