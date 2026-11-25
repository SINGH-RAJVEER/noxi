use std::rc::Rc;

use bstr::ByteSlice;
use nix_compat::nixhash::{HashAlgo, NixHash};
use nix_compat::store_path::StorePath;
use noxi_eval::builtin_macros::builtins;
use noxi_eval::generators::{self, GenCo};
use noxi_eval::{
	AddContext, CoercionKind, ErrorKind, NixAttrs, NixContextElement, NixString, Value,
};
use noxi_flakeref::input::{Attr, Attrs, Input, Scheme, git_rev};
use noxi_flakeref::lockfile::format_last_modified_date;
use noxi_flakeref::registry::UseRegistries;

use crate::store::EvaluatorStore;

pub(crate) fn error(error: impl std::fmt::Display) -> ErrorKind {
	let message = error.to_string();
	ErrorKind::EvalError(
		if message.starts_with("'") && message.ends_with("doesn't have a scheme") {
			format!("URL {message}")
		} else {
			message
		},
	)
}

pub(crate) fn fetch_error(error: noxi_flakeref::error::Error) -> ErrorKind {
	let mut kind = ErrorKind::EvalError(error.message);
	for trace in error.traces {
		kind = kind.trace(None, trace);
	}
	kind
}

pub(crate) fn final_tree_builtin(store: &Rc<EvaluatorStore>) -> Value {
	final_fetch_builtins::builtins(store.clone()).remove(0).1
}

#[builtins(state = "Rc<EvaluatorStore>")]
mod final_fetch_builtins {
	use super::*;
	use noxi_eval::generators::Gen;
	#[builtin("fetchFinalTree")]
	async fn builtin_final_tree(
		state: Rc<EvaluatorStore>,
		co: GenCo,
		value: Value,
	) -> Result<Value, ErrorKind> {
		fetch_tree(&state, &co, value, false, true).await
	}
}

pub(crate) async fn string(co: &GenCo, value: Value) -> Result<String, ErrorKind> {
	let string = generators::request_string_coerce(
		co,
		value,
		CoercionKind {
			strong: false,
			import_paths: false,
		},
	)
	.await
	.map_err(ErrorKind::CatchableError)?;
	Ok(string.to_str()?.to_owned())
}

/// CppNix's forceStringNoCtx, including the force/type distinction in traces.
pub(crate) async fn string_no_context(
	co: &GenCo,
	value: Value,
	trace: noxi_eval::Trace,
) -> Result<NixString, ErrorKind> {
	generators::request_builtin_context(co, vec![trace.clone()]).await;
	let value = generators::request_force(co, value).await;
	if let Value::Catchable(kind) = value {
		return Err(ErrorKind::CatchableError(*kind));
	}
	generators::request_builtin_context(co, Vec::new()).await;
	let string = value.to_contextful_str().map_err(|error| {
		let error = match trace.span {
			Some(span) => ErrorKind::PositionedAt {
				span,
				underlying: Box::new(error),
			},
			None => error,
		};
		error.trace(trace.span, trace.hint.clone())
	})?;
	if let Some(message) = string.context_not_allowed() {
		return Err(ErrorKind::EvalError(message).trace(trace.span, trace.hint));
	}
	Ok(string)
}

pub(crate) fn path_value(path: &StorePath) -> Value {
	let absolute = path.to_absolute_path();
	Value::String(NixString::new_context_from(
		NixContextElement::Plain(absolute.clone()).into(),
		absolute,
	))
}

pub(crate) fn tree_attrs(
	path: &StorePath,
	input: &Input,
	empty_rev: bool,
) -> Result<Value, ErrorKind> {
	let mut attrs: Vec<(String, Value)> = vec![("outPath".into(), path_value(path))];
	if let Some(hash) = input.get_nar_hash().map_err(error)? {
		attrs.push(("narHash".into(), hash.to_sri_string().into()));
	}
	if input.scheme == Some(Scheme::Git) {
		attrs.push((
			"submodules".into(),
			Value::Bool(
				noxi_flakeref::input::maybe_get_bool_attr(&input.attrs, "submodules")
					.map_err(error)?
					.unwrap_or(false),
			),
		));
	}
	if let Some(rev) = input.get_rev().map_err(error)? {
		let rev = git_rev(&rev);
		attrs.push(("shortRev".into(), rev[..7].into()));
		attrs.push(("rev".into(), rev.into()));
	} else if empty_rev {
		attrs.push((
			"rev".into(),
			"0000000000000000000000000000000000000000".into(),
		));
		attrs.push(("shortRev".into(), "0000000".into()));
	}
	if let Some(Attr::Lazy(count)) = input.attrs.get("revCount") {
		let count = count.clone();
		attrs.push((
			"revCount".into(),
			Value::suspended_native_thunk(Box::new(move || {
				match count.force().map_err(error)? {
					Attr::Int(count) => Ok(Value::Integer(count as i64)),
					_ => Err(error("revCount is not an integer")),
				}
			})),
		));
	} else if let Some(count) = input.get_rev_count().map_err(error)? {
		attrs.push(("revCount".into(), Value::Integer(count as i64)));
	} else if empty_rev {
		attrs.push(("revCount".into(), Value::Integer(0)));
	}
	if let Some(modified) = input.get_last_modified().map_err(error)? {
		attrs.push(("lastModified".into(), Value::Integer(modified as i64)));
		attrs.push((
			"lastModifiedDate".into(),
			format_last_modified_date(modified).into(),
		));
	}
	for key in ["dirtyRev", "dirtyShortRev"] {
		if let Some(value) =
			noxi_flakeref::input::maybe_get_str_attr(&input.attrs, key).map_err(error)?
		{
			attrs.push((key.into(), value.into()));
		}
	}
	Ok(Value::attrs(NixAttrs::from_iter(attrs)))
}

async fn fetch_tree(
	store: &EvaluatorStore,
	co: &GenCo,
	value: Value,
	git: bool,
	final_input: bool,
) -> Result<Value, ErrorKind> {
	let settings = store.fetch_settings();
	let who = if git { "fetchGit" } else { "fetchTree" };
	let mut input = match value {
		Value::Attrs(attrs) => {
			let mut values = Attrs::new();
			if git {
				values.insert("type".into(), "git".into());
			}
			for (key, value) in attrs.iter_sorted() {
				let key = key.to_str()?;
				if git && key == "type" {
					return Err(error("unexpected argument 'type'"));
				}
				let value = generators::request_force(co, value.clone()).await;
				if value.is_catchable() {
					return Ok(value);
				}
				let value = match value {
					Value::String(_) | Value::Path(_) => {
						let mut s = string(co, value).await?;
						if git && key == "url" {
							s = noxi_flakeref::url::fix_git_url(&s)
								.map_err(error)?
								.to_string();
						}
						Attr::String(s)
					}
					Value::Bool(value) => Attr::Bool(value),
					Value::Integer(value) if value >= 0 => Attr::Int(value as u64),
					Value::Integer(value) => {
						return Err(error(format!(
							"negative value given for '{who}' argument '{key}': {value}"
						)));
					}
					value => {
						return Err(error(format!(
							"argument '{key}' to '{who}' is {} while a string, Boolean or integer is expected",
							if matches!(value, Value::Builtin(_) | Value::Closure(_)) {
								"a function"
							} else {
								value.type_of()
							}
						)));
					}
				};
				values.insert(key.into(), value);
			}
			if !values.contains_key("type") {
				return Err(error(format!(
					"argument 'type' is missing in call to '{who}'"
				)));
			}
			if !git && values.contains_key("name") {
				return Err(error(format!(
					"argument 'name' isn’t supported in call to '{who}'"
				)));
			}
			if git
				&& !values.contains_key("exportIgnore")
				&& !noxi_flakeref::input::maybe_get_bool_attr(&values, "submodules")
					.map_err(error)?
					.unwrap_or(false)
			{
				values.insert("exportIgnore".into(), Attr::Bool(true));
			}
			if !git && values.get("type") == Some(&Attr::from("git")) {
				values.entry("shallow".into()).or_insert(Attr::Bool(true));
			}
			Input::from_attrs(&settings, values).map_err(error)?
		}
		value => {
			generators::request_builtin_context(
				co,
				vec![noxi_eval::Trace::new(
					None,
					format!("while evaluating the first argument passed to '{who}'"),
				)],
			)
			.await;
			let url = string(co, value).await?;
			generators::request_builtin_context(co, Vec::new()).await;
			if git {
				Input::from_attrs(
					&settings,
					[
						("type".into(), "git".into()),
						(
							"url".into(),
							noxi_flakeref::url::fix_git_url(&url)
								.map_err(error)?
								.to_string()
								.into(),
						),
						("exportIgnore".into(), Attr::Bool(true)),
					]
					.into(),
				)
				.map_err(error)?
			} else {
				if !settings.flakes {
					return Err(error(format!(
						"passing a string argument to '{who}' requires the 'flakes' experimental feature"
					)));
				}
				Input::from_url_string(&settings, &url, true).map_err(error)?
			}
		}
	};
	if !store.pure_eval() && !input.is_direct() && settings.flakes {
		input = noxi_flakeref::registry::lookup_in_registries(
			&store.registries().map_err(error)?,
			&input,
			UseRegistries::Limited,
		)
		.map_err(error)?
		.0;
	}
	if store.pure_eval()
		&& !input.is_locked(&settings)
		&& input.get_nar_hash().map_err(error)?.is_none()
	{
		return Err(error(format!(
			"in pure evaluation mode, '{who}' doesn't fetch unlocked input '{}'",
			input.display()
		)));
	}
	if store.fetch_config.borrow().restrict_eval {
		store
			.check_fetch_uri(
				&input
					.to_url_string(&std::collections::BTreeMap::new())
					.map_err(error)?,
			)
			.map_err(ErrorKind::from)?;
	}
	if final_input {
		input.attrs.insert("__final".into(), Attr::Bool(true));
	} else if input.is_final() {
		return Err(error(format!(
			"input '{}' is not allowed to use the '__final' attribute",
			input.display()
		)));
	}
	if let Some(url) =
		noxi_flakeref::input::maybe_get_str_attr(&input.attrs, "url").map_err(error)?
	{
		if let Some(path) = url.strip_prefix("file:")
			&& !path.starts_with('/')
		{
			generators::request_builtin_context(
				co,
				vec![noxi_eval::Trace::new(
					None,
					format!("while fetching the input '{url}'"),
				)],
			)
			.await;
			return Err(error(format!(
				"tarball '{url}' must use an absolute path. The 'file' scheme does not support relative paths."
			)));
		}
	}
	let (mount, _, mut locked, _) =
		super::get_cached(store, &input, UseRegistries::No).map_err(fetch_error)?;
	let path = super::mount_input(store, &mut locked, &input, &mount).map_err(error)?;
	tree_attrs(&path, &locked, git)
}

async fn fetch_url(
	store: &EvaluatorStore,
	co: &GenCo,
	value: Value,
	unpack: bool,
) -> Result<Value, ErrorKind> {
	let who = if unpack { "fetchTarball" } else { "fetchurl" };
	// CppNix forces the argument with no context of its own, so an error
	// raised while forcing it carries no "while evaluating the url" frame;
	// a type error in the string itself does, below.
	let value = generators::request_force(co, value).await;
	if value.is_catchable() {
		return Ok(value);
	}
	let mut url = None;
	let mut name = if unpack {
		"source".to_owned()
	} else {
		String::new()
	};
	let mut expected = None;
	let is_attrs = matches!(&value, Value::Attrs(_));
	let mut explicit_name = false;
	match value {
		Value::Attrs(attrs) => {
			for (key, value) in attrs.iter_sorted() {
				let key_name = key.to_str()?;
				let hint = match key_name {
					"url" => "while evaluating the url we should fetch",
					"name" => "while evaluating the name of the content we should fetch",
					"sha256" => "while evaluating the sha256 of the content we should fetch",
					other => {
						return Err(error(format!("unsupported argument '{other}' to '{who}'")));
					}
				};
				let value = string_no_context(
					co,
					value.clone(),
					noxi_eval::Trace::new(attrs.get_attr_pos(key), hint),
				)
				.await?;
				match key_name {
					"url" => url = Some(value.to_str()?.to_owned()),
					"name" => {
						explicit_name = true;
						name = value.to_str()?.to_owned();
					}
					"sha256" => {
						if !value.is_empty() {
							expected = Some(
								NixHash::from_str(value.to_str()?, Some(HashAlgo::Sha256))
									.map_err(error)?,
							);
						}
					}
					_ => unreachable!(),
				}
			}
		}
		value => {
			url = Some(
				string_no_context(
					co,
					value,
					noxi_eval::Trace::new(None, "while evaluating the url we should fetch"),
				)
				.await?
				.to_str()?
				.to_owned(),
			)
		}
	}
	let url = url.ok_or_else(|| error("'url' argument required"))?;
	if !unpack {
		noxi_flakeref::url::parse_url(&url, false).map_err(|err| error(format!("URL {err}")))?;
	}
	store.check_fetch_uri(&url).map_err(ErrorKind::from)?;
	if name.is_empty() {
		name = noxi_flakeref::url::base_name_of(&url).to_owned();
	}
	if let Some(reason) = crate::errors::invalid_store_name(&name) {
		let advice = if explicit_name {
			format!(
				"Please change the value for the 'name' attribute passed to '{who}', so that it can create a valid store path."
			)
		} else if is_attrs {
			format!(
				"Please add a valid 'name' attribute to the argument for '{who}', so that it can create a valid store path."
			)
		} else {
			format!(
				"Please pass an attribute set with 'url' and 'name' attributes to '{who}',  so that it can create a valid store path."
			)
		};
		return Err(error(format!(
			"invalid store path name when fetching URL '{url}': {reason}. {advice}"
		)));
	}
	if store.pure_eval() && expected.is_none() {
		return Err(error(format!(
			"in pure evaluation mode, '{who}' requires a 'sha256' argument"
		)));
	}
	let settings = store.fetch_settings();
	let mut attrs: Attrs = [
		(
			"type".into(),
			if unpack { "tarball" } else { "file" }.into(),
		),
		("url".into(), url.into()),
		("name".into(), name.clone().into()),
	]
	.into();
	if unpack && let Some(hash) = &expected {
		attrs.insert("narHash".into(), hash.to_sri_string().into());
	}
	let input = Input::from_attrs(&settings, attrs).map_err(error)?;
	if let Some(url) =
		noxi_flakeref::input::maybe_get_str_attr(&input.attrs, "url").map_err(error)?
	{
		if let Some(path) = url.strip_prefix("file:")
			&& !path.starts_with('/')
		{
			generators::request_builtin_context(
				co,
				vec![noxi_eval::Trace::new(
					None,
					format!("while fetching the input '{url}'"),
				)],
			)
			.await;
			return Err(error(format!(
				"tarball '{url}' must use an absolute path. The 'file' scheme does not support relative paths."
			)));
		}
	}
	let (mount, _, mut locked, _) =
		super::get_cached(store, &input, UseRegistries::No).map_err(fetch_error)?;
	let path = if unpack {
		super::mount_input(store, &mut locked, &input, &mount).map_err(error)?
	} else {
		let captured = store
			.capture_import(&mount.root, &name, false, None)
			.map_err(error)?;
		let path = store
			.import_captured(captured, expected.as_ref())
			.map_err(error)?;
		if store.store().is_none() {
			store.mount(&path, mount);
		}
		store.allow_store_path(&path);
		path
	};
	Ok(path_value(&path))
}

#[builtins(state = "Rc<EvaluatorStore>")]
pub(crate) mod fetch_builtins {
	use super::*;
	use noxi_eval::generators::Gen;

	#[builtin("fetchTree")]
	async fn builtin_fetch_tree(
		state: Rc<EvaluatorStore>,
		co: GenCo,
		value: Value,
	) -> Result<Value, ErrorKind> {
		fetch_tree(&state, &co, value, false, false).await
	}

	#[builtin("fetchGit")]
	async fn builtin_fetch_git(
		state: Rc<EvaluatorStore>,
		co: GenCo,
		value: Value,
	) -> Result<Value, ErrorKind> {
		fetch_tree(&state, &co, value, true, false).await
	}

	#[builtin("fetchurl")]
	async fn builtin_fetch_url(
		state: Rc<EvaluatorStore>,
		co: GenCo,
		#[lazy] value: Value,
	) -> Result<Value, ErrorKind> {
		fetch_url(&state, &co, value, false).await
	}

	#[builtin("fetchTarball")]
	async fn builtin_fetch_tarball(
		state: Rc<EvaluatorStore>,
		co: GenCo,
		#[lazy] value: Value,
	) -> Result<Value, ErrorKind> {
		fetch_url(&state, &co, value, true).await
	}

	#[builtin("getFlake")]
	async fn builtin_get_flake(
		state: Rc<EvaluatorStore>,
		co: GenCo,
		#[lazy] value: Value,
	) -> Result<Value, ErrorKind> {
		if !state.fetch_settings().flakes {
			return Err(error(
				"experimental Nix feature 'flakes' is disabled; add '--extra-experimental-features flakes' to enable it",
			));
		}
		// CppNix's `forceStringNoCtx` forces the argument before it adds this
		// context, so an error raised while forcing carries no such frame.
		let value = generators::request_force(&co, value).await;
		if let Value::Catchable(kind) = value {
			return Err(ErrorKind::CatchableError(*kind));
		}
		if let Value::Path(path) = value {
			return super::super::flake::lock_flake_path(&state, &co, &path)
				.await?
				.evaluate(&state, &co)
				.await;
		}
		let reference = string_no_context(
			&co,
			value,
			noxi_eval::Trace::new(
				None,
				"while evaluating the argument passed to builtins.getFlake",
			),
		)
		.await?
		.to_str()?
		.to_owned();
		let locked = super::super::flake::lock_flake(
			&state,
			&co,
			&reference,
			&super::super::flake::LockOptions::default(),
		)
		.await?;
		locked.evaluate(&state, &co).await
	}
}
