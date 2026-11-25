//! `derivationStrict` and `placeholder`, derived from Snix's
//! `glue/src/builtins/derivation.rs` and changed where CppNix 2.35.2 differs.

use std::collections::{BTreeMap, btree_map};
use std::rc::Rc;
use std::sync::Arc;

use bstr::{BString, ByteSlice};
use nix_compat::cppnix_json::derivation::InputNode;
use nix_compat::derivation::{
	DerivationBuilder, OutputHash, OutputHashMode, OutputName, Outputs, OutputsBuilder,
};
use nix_compat::nixhash::{HashAlgo, NixHash};
use nix_compat::store_path::{StorePath, hash_placeholder};
use noxi_eval::builtin_macros::builtins;
use noxi_eval::generators::{self, GenCo};
use noxi_eval::{
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
	state: &ReadOnlyStore,
) -> Result<BTreeMap<StorePath, BTreeMap<OutputName, InputNode>>, DerivationError> {
	let mut dynamic = BTreeMap::<StorePath, BTreeMap<OutputName, InputNode>>::new();
	for element in context.iter() {
		match element {
			NixContextElement::Plain(path) => {
				drv.input_sources.insert(parse_store_path(path)?);
			}
			// An output of a derivation that is itself an output of another.
			NixContextElement::Single { name, derivation } if derivation.contains('!') => {
				let parse_name = |name: &str| {
					name.parse::<OutputName>()
						.map_err(|_| DerivationError::InvalidContext(name.to_owned()))
				};
				let mut parts = derivation.rsplit('!');
				let root = parse_store_path(parts.next().unwrap_or_default())?;
				drv.input_derivations.entry(root.clone()).or_default();
				let empty = || InputNode {
					outputs: Default::default(),
					dynamic_outputs: BTreeMap::new(),
				};
				let mut children = dynamic.entry(root).or_default();
				let mut parts = parts.peekable();
				while let Some(output) = parts.next() {
					let node = children.entry(parse_name(output)?).or_insert_with(empty);
					if parts.peek().is_none() {
						node.outputs.insert(parse_name(name)?);
					}
					children = &mut node.dynamic_outputs;
				}
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
				if known_paths.get_unverified_drv(&drv_path.as_ref()).is_none() {
					return Err(DerivationError::UnknownDerivation(
						drv_path.to_absolute_path(),
					));
				}
				let closure = known_paths.closure(&drv_path);
				for path in &closure {
					let absolute = path.to_absolute_path();
					let valid = match state.store() {
						Some(local) => local.is_valid_path(path).unwrap_or(false),
						None => std::fs::symlink_metadata(
							state.physical_path(std::path::Path::new(&absolute)),
						)
						.is_ok(),
					};
					if !valid {
						return Err(DerivationError::InvalidPath(absolute));
					}
				}
				for path in closure {
					if let Some(input) = known_paths.get_unverified_drv(&path.as_ref()) {
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
	Ok(dynamic)
}

/// CppNix's `SingleDerivedPath`: a store path, or an output of a derivation
/// that may itself be the output of another. A nested output keeps its
/// derivation in a string context element as CppNix prints it,
/// `OUTPUT!DERIVATION`.
enum DerivedPath {
	Opaque(StorePath),
	Built(Box<DerivedPath>, String),
}

impl DerivedPath {
	fn from_context(element: &str) -> Result<Self, ErrorKind> {
		match element.split_once('!') {
			Some((output, rest)) => Ok(Self::Built(
				Box::new(Self::from_context(rest)?),
				output.to_owned(),
			)),
			None => StorePath::from_absolute_path(element.as_bytes())
				.map(Self::Opaque)
				.map_err(|error| ErrorKind::EvalError(error.to_string())),
		}
	}

	fn to_context(&self) -> String {
		match self {
			Self::Opaque(path) => path.to_absolute_path(),
			Self::Built(drv, output) => format!("{output}!{}", drv.to_context()),
		}
	}

	/// `SingleDerivedPath::to_string`.
	fn display(&self) -> String {
		match self {
			Self::Opaque(path) => path.to_absolute_path(),
			Self::Built(drv, output) => format!("{}^{output}", drv.display()),
		}
	}
}

/// `DownstreamPlaceholder::fromSingleDerivedPathBuilt`, as a digest.
fn output_placeholder(drv: &DerivedPath, output: &str, ca: bool) -> Result<[u8; 32], ErrorKind> {
	use sha2::{Digest, Sha256};
	let clear_text = match drv {
		DerivedPath::Opaque(path) => {
			if !ca {
				return Err(ErrorKind::EvalError(
					"experimental Nix feature 'ca-derivations' is disabled; add '--extra-experimental-features ca-derivations' to enable it".into(),
				));
			}
			let name = path.name().strip_suffix(".drv").unwrap_or(path.name());
			let name = if output == "out" {
				name.to_owned()
			} else {
				format!("{name}-{output}")
			};
			format!(
				"nix-upstream-output:{}:{name}",
				nix_compat::nixbase32::encode(path.digest())
			)
		}
		DerivedPath::Built(inner, inner_output) => {
			let digest = output_placeholder(inner, inner_output, ca)?;
			let compressed: [u8; 20] = nix_compat::store_path::compress_hash(&digest);
			format!(
				"nix-computed-output:{}:{output}",
				nix_compat::nixbase32::encode(&compressed)
			)
		}
	};
	Ok(Sha256::digest(clear_text.as_bytes()).into())
}

/// `EvalState::mkSingleDerivedPathStringRaw`.
fn derived_path_string(state: &DerivationState, path: &DerivedPath) -> Result<String, ErrorKind> {
	let (drv, output) = match path {
		DerivedPath::Opaque(path) => return Ok(path.to_absolute_path()),
		DerivedPath::Built(drv, output) => (drv, output),
	};
	if let DerivedPath::Opaque(drv_path) = drv.as_ref() {
		match state
			.store
			.derivation_output_path(drv_path, output)
			.map_err(|error| ErrorKind::EvalError(error.to_string()))?
		{
			None => {
				return Err(ErrorKind::EvalError(format!(
					"derivation '{}' does not have output '{output}'",
					drv.display()
				)));
			}
			Some(Some(path)) => return Ok(path.to_absolute_path()),
			Some(None) => {}
		}
	}
	let digest = output_placeholder(drv, output, state.ca_derivations)?;
	Ok(format!("/{}", nix_compat::nixbase32::encode(&digest)))
}

pub(crate) struct DerivationState {
	pub store: Rc<ReadOnlyStore>,
	pub ca_derivations: bool,
	pub impure_derivations: bool,
	pub blake3_hashes: bool,
	pub dynamic_derivations: bool,
	pub git_hashing: bool,
}

#[builtins(state = "Rc<DerivationState>")]
pub(crate) mod derivation_builtins {
	use super::*;
	use noxi_eval::generators::Gen;

	#[builtin("placeholder")]
	async fn builtin_placeholder(co: GenCo, #[lazy] input: Value) -> Result<Value, ErrorKind> {
		generators::request_builtin_context(
			&co,
			vec![noxi_eval::Trace::new(
				None,
				"while evaluating the first argument passed to builtins.placeholder",
			)],
		)
		.await;
		let input = generators::request_force(&co, input).await;
		if input.is_catchable() {
			return Ok(input);
		}
		generators::request_builtin_context(&co, Vec::new()).await;
		let output_name = input.to_contextful_str().trace(
			None,
			"while evaluating the first argument passed to builtins.placeholder",
		)?;
		if let Some(message) = output_name.context_not_allowed() {
			return Err(ErrorKind::EvalError(message).trace(
				None,
				"while evaluating the first argument passed to builtins.placeholder",
			));
		}
		Ok(hash_placeholder(output_name.to_str()?).into())
	}

	/// The path of an output of a derivation, or of an output of another
	/// derivation's output, or a placeholder for it when it is not static.
	/// Only added with the `dynamic-derivations` experimental feature.
	#[builtin("outputOf")]
	async fn builtin_output_of(
		state: Rc<DerivationState>,
		co: GenCo,
		#[lazy] drv: Value,
		#[lazy] output: Value,
	) -> Result<Value, ErrorKind> {
		const FIRST: &str = "while evaluating the first argument to builtins.outputOf";
		const SECOND: &str = "while evaluating the second argument to builtins.outputOf";
		let in_argument = |hint: &'static str| {
			generators::request_builtin_context(&co, vec![noxi_eval::Trace::new(None, hint)])
		};
		// `EvalState::coerceToSingleDerivedPath`.
		in_argument(FIRST).await;
		let drv = generators::request_force(&co, drv).await;
		if drv.is_catchable() {
			return Ok(drv);
		}
		let string = drv.to_contextful_str()?;
		let text = string.to_str()?;
		let elements: Vec<_> = string
			.iter_context()
			.flat_map(|context| context.iter())
			.collect();
		if elements.len() != 1 {
			return Err(ErrorKind::EvalError(format!(
				"string '{text}' has {} entries in its context. It should only have exactly one entry",
				elements.len()
			)));
		}
		let derived = match elements[0] {
			NixContextElement::Plain(path) => DerivedPath::from_context(path)?,
			NixContextElement::Derivation(_) => {
				return Err(ErrorKind::EvalError(format!(
					"string '{text}' has a context which refers to a complete source and binary closure. This is not supported at this time"
				)));
			}
			NixContextElement::Single { name, derivation } => DerivedPath::Built(
				Box::new(DerivedPath::from_context(derivation)?),
				name.clone(),
			),
		};
		// Reading the derivation adds no argument trace.
		generators::request_builtin_context(&co, vec![]).await;
		let expected = derived_path_string(&state, &derived)?;
		if text != expected {
			let message = match &derived {
				DerivedPath::Opaque(_) => {
					format!("path string '{text}' has context with the different path '{expected}'")
				}
				DerivedPath::Built(drv, output) => format!(
					"string '{text}' has context with the output '{output}' from derivation '{}', but the string is not the right placeholder for this derivation output. It should be '{expected}'",
					drv.display()
				),
			};
			in_argument(FIRST).await;
			return Err(ErrorKind::EvalError(message));
		}

		in_argument(SECOND).await;
		let output = generators::request_force(&co, output).await;
		if output.is_catchable() {
			return Ok(output);
		}
		let output = output.to_contextful_str()?;
		if let Some(element) = output.iter_context().flat_map(|c| c.iter()).next() {
			let shown = match element {
				NixContextElement::Plain(path) => path.clone(),
				NixContextElement::Derivation(path) => format!("{path} (deep)"),
				NixContextElement::Single { name, derivation } => format!(
					"{}^{name}",
					DerivedPath::from_context(derivation)?.display()
				),
			};
			return Err(ErrorKind::EvalError(format!(
				"the string '{}' is not allowed to refer to a store path (such as '{shown}')",
				output.to_str()?
			)));
		}

		// `EvalState::mkSingleDerivedPathString`.
		generators::request_builtin_context(&co, vec![]).await;
		let output = output.to_str()?.to_owned();
		let result = DerivedPath::Built(Box::new(derived), output.clone());
		let path = derived_path_string(&state, &result)?;
		let DerivedPath::Built(drv, _) = result else {
			unreachable!("outputOf builds an output path")
		};
		Ok(NixString::new_context_from(
			NixContextElement::Single {
				name: output,
				derivation: drv.to_context(),
			}
			.into(),
			path,
		)
		.into())
	}

	/// Strictly construct a derivation from its attributes. Users normally
	/// call `builtins.derivation`, which wraps this.
	#[builtin("derivationStrict")]
	async fn builtin_derivation_strict(
		state: Rc<DerivationState>,
		co: GenCo,
		input: Value,
	) -> Result<Value, ErrorKind> {
		if input.is_catchable() {
			return Ok(input);
		}

		let input = input.to_attrs()?;
		let name_value = input
			.select_required("name")
			.trace(
				None,
				"in the attrset passed as argument to builtins.derivationStrict",
			)?
			.clone();
		generators::request_builtin_context(
			&co,
			vec![
				noxi_eval::Trace::new(
					input.get_attr_pos("name"),
					"while evaluating the derivation attribute 'name'",
				),
				noxi_eval::Trace::new(
					None,
					"while evaluating the `name` attribute passed to builtins.derivationStrict",
				),
			],
		)
		.await;
		let name = generators::request_force(&co, name_value).await;
		if name.is_catchable() {
			return Ok(name);
		}
		let name = name.to_str()?;
		let name = name.to_str()?.to_owned();
		let mut hint = format!("while evaluating derivation '{name}'");
		if let Some(span) = input.get_attr_pos("name") {
			let position = generators::request_position(&co, span).await;
			hint.push_str(&format!(
				"\n  whose name attribute is located at {}",
				position.to_str_lossy()
			));
		}
		let outer = noxi_eval::Trace::new(None, hint);
		generators::request_builtin_context(&co, vec![outer.clone()]).await;

		if let Some(reason) = crate::errors::invalid_store_name(&name) {
			return Err(ErrorKind::EvalError(format!(
				"invalid derivation name: {reason}. Please pass a different 'name'."
			)));
		}
		let mut drv = DerivationBuilder::default();
		let mut input_context = NixContext::new();
		// The outputs as given, in order; `out` when the attribute is absent.
		let mut output_names = vec![OutputName::out()];
		let mut content_addressed = false;
		let mut impure = false;

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
		// A sorted map, so the `__json` keys come out in CppNix's order.
		let mut structured_attrs: Option<BTreeMap<&str, serde_json::Value>> =
			match input.select(STRUCTURED_ATTRS) {
				Some(b) => {
					generators::request_builtin_context(
						&co,
						vec![
							outer.clone(),
							noxi_eval::Trace::new(
								None,
								"while evaluating the `__structuredAttrs` attribute passed to builtins.derivationStrict",
							),
						],
					)
					.await;
					let b = generators::request_force(&co, b.clone()).await;
					if b.is_catchable() {
						return Ok(b);
					}
					b.as_bool()?.then_some(Default::default())
				}
				None => None,
			};

		let ignore_nulls = match input.select(IGNORE_NULLS) {
			Some(b) => {
				generators::request_builtin_context(
					&co,
					vec![
						outer.clone(),
						noxi_eval::Trace::new(
							None,
							"while evaluating the `__ignoreNulls` attribute passed to builtins.derivationStrict",
						),
					],
				)
				.await;
				let b = generators::request_force(&co, b.clone()).await;
				if b.is_catchable() {
					return Ok(b);
				}
				b.as_bool()?
			}
			None => false,
		};

		for (arg_name, arg_value) in input.iter_sorted() {
			let arg_name = arg_name.to_str()?;
			generators::request_builtin_context(
				&co,
				vec![
					outer.clone(),
					noxi_eval::Trace::new(
						input.get_attr_pos(arg_name),
						format!("while evaluating attribute '{arg_name}' of derivation '{name}'"),
					),
				],
			)
			.await;
			let value = generators::request_force(&co, arg_value.clone()).await;
			if value.is_catchable() {
				return Ok(value);
			}

			if ignore_nulls && matches!(value, Value::Null) {
				continue;
			}

			match arg_name {
				"__contentAddressed" | "__impure" => {
					if value.as_bool()? {
						let (feature, enabled) = if arg_name == "__contentAddressed" {
							("ca-derivations", state.ca_derivations)
						} else {
							("impure-derivations", state.impure_derivations)
						};
						if !enabled {
							return Err(ErrorKind::EvalError(format!(
								"experimental Nix feature '{feature}' is disabled; add '--extra-experimental-features {feature}' to enable it"
							)));
						}
						if arg_name == "__contentAddressed" {
							content_addressed = true;
						} else {
							impure = true;
						}
					}
				}
				"args" => {
					for arg in value.to_list()? {
						let s = try_cek_to_value!(
							generators::request_string_coerce_in(
								&co,
								arg,
								CoercionKind {
									strong: true,
									import_paths: true
								},
								Some("while evaluating an element of the argument list")
							)
							.await
						);
						input_context.mimic(&s);
						drv.arguments.push(s.as_bytes().into())
					}
				}

				"outputs" => {
					let outputs = if structured_attrs.is_some() {
						value.to_list()?
					} else {
						let text = try_cek_to_value!(coerce_importing(&co, value).await);
						noxi_eval::NixList::from(
							text.to_str()?
								.split_whitespace()
								.map(|name| Value::from(name.to_owned()))
								.collect::<Vec<_>>(),
						)
					};

					output_names = Vec::with_capacity(outputs.len());
					for output in outputs {
						let output_name = generators::request_force(&co, output).await;
						if output_name.is_catchable() {
							return Ok(output_name);
						}
						let output_name = output_name.to_str()?;
						input_context.mimic(&output_name);
						let output_name = output_name.to_str()?;
						let output_name: OutputName = output_name.parse().map_err(|err| {
							// CppNix reports the name of the output's path.
							crate::errors::invalid_store_name(&format!("{name}-{output_name}"))
								.map_or_else(
									|| ErrorKind::NoxiError(Arc::new(err)),
									ErrorKind::EvalError,
								)
						})?;
						if output_names.contains(&output_name) {
							return Err(ErrorKind::EvalError(format!(
								"duplicate derivation output '{output_name}'"
							)));
						}
						// The result already has a `drvPath` attribute.
						if output_name.as_str() == Some("drvPath") {
							return Err(ErrorKind::EvalError(
								"invalid derivation output name 'drvPath'".into(),
							));
						}
						output_names.push(output_name);
					}
					if output_names.is_empty() {
						return Err(ErrorKind::EvalError(
							"derivation cannot have an empty set of outputs".into(),
						));
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
							if arg_name == "outputHashMode" {
								if let Some(mode) = val_json.as_str() {
									if !matches!(
										mode,
										"flat" | "recursive" | "nar" | "text" | "git"
									) {
										return Err(ErrorKind::EvalError(format!(
											"invalid value '{mode}' for 'outputHashMode' attribute"
										)));
									}
									match mode {
										"text" if !state.dynamic_derivations => return Err(ErrorKind::EvalError(format!("experimental Nix feature 'dynamic-derivations' is disabled (text-hashed derivation '{name}', outputHashMode = \"text\"); add '--extra-experimental-features dynamic-derivations' to enable it"))),
										"git" if !state.git_hashing => return Err(ErrorKind::EvalError("experimental Nix feature 'git-hashing' is disabled; add '--extra-experimental-features git-hashing' to enable it".into())),
										_ => {},
									}
								}
							}
							input_context.extend(context);
							structured_attrs.insert(arg_name, val_json);
							if matches!(
								arg_name,
								"allowedReferences"
									| "allowedRequisites"
									| "disallowedReferences"
									| "disallowedRequisites"
									| "maxClosureSize"
									| "maxSize"
							) && nix_compat::log::log_enabled(
								nix_compat::log::VerbosityLevel::Warn,
							) {
								eprintln!(
									"warning: In a derivation named '{name}', 'structuredAttrs' disables the effect of the derivation attribute '{arg_name}'; use 'outputChecks.<output>.{arg_name}' instead"
								);
							}
						}
						None => {
							let val_str = try_cek_to_value!(coerce_importing(&co, value).await);
							if arg_name == "outputHashMode"
								&& !matches!(
									val_str.to_str()?,
									"flat" | "recursive" | "nar" | "text" | "git"
								) {
								return Err(ErrorKind::EvalError(format!(
									"invalid value '{}' for 'outputHashMode' attribute",
									val_str.to_str()?
								)));
							}
							if arg_name == "outputHashMode" {
								match val_str.to_str()? {
									"text" if !state.dynamic_derivations => return Err(ErrorKind::EvalError(format!("experimental Nix feature 'dynamic-derivations' is disabled (text-hashed derivation '{name}', outputHashMode = \"text\"); add '--extra-experimental-features dynamic-derivations' to enable it"))),
									"git" if !state.git_hashing => return Err(ErrorKind::EvalError("experimental Nix feature 'git-hashing' is disabled; add '--extra-experimental-features git-hashing' to enable it".into())),
									_ => {},
								}
							}
							input_context.mimic(&val_str);
							insert_env(&mut drv, arg_name, val_str.as_bytes().into())?;
						}
					}
				}
			}
		}

		generators::request_builtin_context(&co, vec![outer]).await;

		let mut fixed_experimental = None;
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

			let want_algo: Option<HashAlgo> = hash_algo
				.map(|s| s.parse())
				.transpose()
				.map_err(|err| ErrorKind::NoxiError(Arc::new(err)))?;
			let hash = NixHash::from_str(&hash_str, want_algo)
				.map_err(|err| ErrorKind::NoxiError(Arc::new(err)))?;
			if matches!(hash_mode.as_deref(), Some("text" | "git")) {
				use noxi_store::build_derivation::{ExperimentalMethod, ExperimentalOutput};
				let method = ExperimentalMethod::parse(hash_mode.as_deref().unwrap())
					.map_err(|e| ErrorKind::EvalError(e.to_string()))?;
				let path = method
					.fixed_path(&name, &hash)
					.map_err(|e| ErrorKind::EvalError(e.to_string()))?;
				fixed_experimental = Some(ExperimentalOutput {
					method,
					hash_algo: hash.algo(),
					impure: false,
					fixed: Some((path, hash)),
				});
			} else {
				let mode = hash_mode
					.map(|s| if s == "nar" { "recursive".into() } else { s })
					.map(|s| s.parse().map_err(|err| ErrorKind::NoxiError(Arc::new(err))))
					.transpose()?
					.unwrap_or_default();
				drv.outputs = OutputsBuilder::Fixed(OutputHash { mode, hash });
			}
		}

		// CppNix overwrites an attribute named after an output with the
		// output's path, so an attribute of that name has no effect.
		for output_name in drv.outputs.names() {
			drv.environment.remove(output_name.as_bytes());
		}

		if let Some(structured_attrs) = structured_attrs {
			drv.environment.insert(
				JSON_KEY.into(),
				// nlohmann's `dump()` form (`1.5e-07`, not `1.5e-7`): these bytes
				// enter the ATerm text and so the derivation's store path.
				BString::from(nix_compat::cppnix_json::dump(&serde_json::Value::Object(
					structured_attrs
						.into_iter()
						.map(|(key, value)| (key.to_owned(), value))
						.collect(),
				))),
			);
		}

		state
			.store
			.load_context(&input_context)
			.map_err(|error| ErrorKind::IO {
				path: None,
				error: Rc::new(error),
			})?;
		let mut known_paths = state.store.known_paths();
		let dynamic = populate_inputs(&mut drv, input_context, &known_paths, &state.store)?;

		if drv.command.is_empty() {
			return Err(ErrorKind::EvalError(
				"required attribute 'builder' missing".into(),
			));
		}
		if drv.system.is_empty() {
			return Err(ErrorKind::EvalError(
				"required attribute 'system' missing".into(),
			));
		}

		if name.ends_with(".drv") {
			let mode = try_cek_to_value!(select_string(&co, &input, "outputHashMode").await?);
			if mode.as_deref() != Some("text") || output_names != [OutputName::out()] {
				return Err(ErrorKind::EvalError("derivation names are allowed to end in '.drv' only if they produce a single derivation file".into()));
			}
		}

		// The full model holds the dynamic inputs of a fixed output too.
		if !dynamic.is_empty()
			&& let OutputsBuilder::Fixed(OutputHash { mode, hash }) = &drv.outputs
		{
			use noxi_store::build_derivation::{ExperimentalMethod, ExperimentalOutput};
			let method = match mode {
				OutputHashMode::Flat => ExperimentalMethod::Flat,
				OutputHashMode::Recursive => ExperimentalMethod::NixArchive,
			};
			let path = method
				.fixed_path(&name, hash)
				.map_err(|e| ErrorKind::EvalError(e.to_string()))?;
			fixed_experimental = Some(ExperimentalOutput {
				method,
				hash_algo: hash.algo(),
				impure: false,
				fixed: Some((path, hash.clone())),
			});
		}

		// Unknown output paths need the CA placeholder feature only after
		// all attributes and the required builder fields have been checked.
		// Fixed outputs already know their path and need no placeholder.
		if impure && !drv.outputs.is_fixed() && fixed_experimental.is_none() {
			if content_addressed {
				return Err(ErrorKind::EvalError(
					"derivation cannot be both content-addressed and impure".into(),
				));
			}
			if !state.ca_derivations {
				return Err(ErrorKind::EvalError("experimental Nix feature 'ca-derivations' is disabled; add '--extra-experimental-features ca-derivations' to enable it".into()));
			}
		}

		if fixed_experimental.is_some()
			|| !drv.outputs.is_fixed()
				&& (content_addressed
					|| impure
					|| !dynamic.is_empty()
					|| drv
						.input_derivations
						.keys()
						.any(|path| known_paths.has_deferred_input(path)))
		{
			use noxi_store::build_derivation::{
				BuildDerivation, ExperimentalMethod, ExperimentalOutput, FloatingOutput,
			};
			let mut floating = BTreeMap::new();
			let mut experimental = BTreeMap::new();
			if let Some(output) = fixed_experimental {
				drv.environment.insert(
					"out".into(),
					output.fixed.as_ref().unwrap().0.to_absolute_path().into(),
				);
				experimental.insert("out".to_owned(), output);
			} else if content_addressed || impure {
				let hash_algo =
					try_cek_to_value!(select_string(&co, &input, "outputHashAlgo").await?)
						.filter(|s| !s.is_empty())
						.map(|s| {
							if s == "blake3" && !state.blake3_hashes {
								return Err(ErrorKind::EvalError("experimental Nix feature 'blake3-hashes' is disabled; add '--extra-experimental-features blake3-hashes' to enable it".into()));
							}
							s.parse::<HashAlgo>()
								.map_err(|e| ErrorKind::EvalError(e.to_string()))
						})
						.transpose()?
						.unwrap_or(HashAlgo::Sha256);
				let mode = try_cek_to_value!(select_string(&co, &input, "outputHashMode").await?)
					.filter(|s| !s.is_empty())
					.unwrap_or_else(|| "nar".to_owned());
				for output in &output_names {
					drv.environment.insert(
						output.as_bytes().into(),
						hash_placeholder(output.as_str().unwrap()).into(),
					);
					if impure || matches!(mode.as_str(), "text" | "git") {
						experimental.insert(
							output.to_string(),
							ExperimentalOutput {
								method: ExperimentalMethod::parse(&mode)
									.map_err(|e| ErrorKind::EvalError(e.to_string()))?,
								hash_algo,
								impure,
								fixed: None,
							},
						);
					} else {
						let method = if matches!(mode.as_str(), "nar" | "recursive") {
							noxi_store::FileIngestionMethod::NixArchive
						} else {
							noxi_store::FileIngestionMethod::Flat
						};
						floating.insert(output.to_string(), FloatingOutput { method, hash_algo });
					}
				}
			} else {
				for output in &output_names {
					drv.environment
						.insert(output.as_bytes().into(), BString::default());
				}
			}
			let outputs = if let Some((path, _)) = experimental
				.get("out")
				.and_then(|output| output.fixed.as_ref())
			{
				Outputs::input_addressed_from_iter([(OutputName::out(), path.clone())])
			} else {
				Outputs::deferred_from_names(output_names.iter().cloned())
			}
			.map_err(|e| ErrorKind::EvalError(e.to_string()))?;
			let basic = drv
				.build_unverified(outputs)
				.map_err(DerivationError::InvalidDerivation)?;
			let drv = BuildDerivation {
				basic,
				floating,
				experimental,
				dynamic,
			};
			let references: std::collections::BTreeSet<_> = drv
				.basic
				.input_sources()
				.iter()
				.chain(drv.basic.input_derivations().keys())
				.map(StorePath::as_ref)
				.collect();
			let path_name = format!("{name}.drv");
			let path = nix_compat::store_path::build_text_path(
				&path_name,
				drv.to_aterm_bytes(),
				references,
			)
			.map_err(|e| ErrorKind::EvalError(e.to_string()))?
			.to_owned();
			let drv_path = path.to_absolute_path();
			let values = output_names
				.iter()
				.map(|output| {
					(
						output.to_string(),
						Value::from(NixString::new_context_from(
							NixContextElement::Single {
								name: output.to_string(),
								derivation: drv_path.clone(),
							}
							.into(),
							drv.experimental
								.get(&output.to_string())
								.and_then(|spec| spec.fixed.as_ref())
								.map(|(path, _)| path.to_absolute_path())
								.unwrap_or_else(|| {
									nix_compat::store_path::downstream_placeholder(
										&path,
										output.as_bytes(),
									)
								}),
						)),
					)
				})
				.chain(std::iter::once((
					"drvPath".to_owned(),
					Value::from(NixString::new_context_from(
						NixContextElement::Derivation(drv_path.clone()).into(),
						drv_path.clone(),
					)),
				)));
			let out = Value::Attrs(NixAttrs::from_iter(values));
			state
				.store
				.register_build_derivation(&path, &drv)
				.map_err(|error| ErrorKind::IO {
					path: Some(drv_path.into()),
					error: Rc::new(error),
				})?;
			known_paths.add_deferred_derivation(path, drv);
			return Ok(out);
		}

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

		state
			.store
			.register_derivation(&drv)
			.map_err(|error| ErrorKind::IO {
				path: Some(drv_path.into()),
				error: Rc::new(error),
			})?;
		known_paths
			.add_verified_derivation(drv)
			.map_err(DerivationError::InvalidDerivation)?;

		Ok(out)
	}
}
