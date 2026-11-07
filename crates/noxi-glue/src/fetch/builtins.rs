use std::rc::Rc;

use bstr::ByteSlice;
use nix_compat::nixhash::{HashAlgo, NixHash};
use nix_compat::store_path::StorePath;
use noxi_eval::builtin_macros::builtins;
use noxi_eval::generators::{self, GenCo};
use noxi_eval::{CoercionKind, ErrorKind, NixAttrs, NixContextElement, NixString, Value};
use noxi_flakeref::input::{Attr, Attrs, Input, Scheme, git_rev};
use noxi_flakeref::lockfile::format_last_modified_date;
use noxi_flakeref::registry::UseRegistries;

use crate::store::EvaluatorStore;

pub(crate) fn error(error: impl std::fmt::Display) -> ErrorKind {
	ErrorKind::EvalError(error.to_string())
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
	if let Some(count) = input.get_rev_count().map_err(error)? {
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
							value.type_of()
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
			let url = string(co, value).await?;
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
	if final_input {
		input.attrs.insert("__final".into(), Attr::Bool(true));
	} else if input.is_final() {
		return Err(error(format!(
			"input '{}' is not allowed to use the '__final' attribute",
			input.display()
		)));
	}
	let (mount, _, mut locked, _) =
		super::get_cached(store, &input, UseRegistries::No).map_err(error)?;
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
	let mut url = None;
	let mut name = if unpack {
		"source".to_owned()
	} else {
		String::new()
	};
	let mut expected = None;
	match value {
		Value::Attrs(attrs) => {
			for (key, value) in attrs.iter_sorted() {
				let value = generators::request_force(co, value.clone()).await;
				if value.is_catchable() {
					return Ok(value);
				}
				match key.to_str()? {
					"url" => url = Some(string(co, value).await?),
					"name" => name = string(co, value).await?,
					"sha256" => {
						let hash = string(co, value).await?;
						if !hash.is_empty() {
							expected = Some(
								NixHash::from_str(&hash, Some(HashAlgo::Sha256)).map_err(error)?,
							);
						}
					}
					other => {
						return Err(error(format!("unsupported argument '{other}' to '{who}'")));
					}
				}
			}
		}
		value => url = Some(string(co, value).await?),
	}
	let url = url.ok_or_else(|| error("'url' argument required"))?;
	if name.is_empty() {
		name = noxi_flakeref::url::base_name_of(&url).to_owned();
	}
	nix_compat::store_path::validate_name(name.as_bytes()).map_err(error)?;
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
	let (mount, _, mut locked, _) =
		super::get_cached(store, &input, UseRegistries::No).map_err(error)?;
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
		value: Value,
	) -> Result<Value, ErrorKind> {
		fetch_url(&state, &co, value, false).await
	}

	#[builtin("fetchTarball")]
	async fn builtin_fetch_tarball(
		state: Rc<EvaluatorStore>,
		co: GenCo,
		value: Value,
	) -> Result<Value, ErrorKind> {
		fetch_url(&state, &co, value, true).await
	}

	#[builtin("getFlake")]
	async fn builtin_get_flake(
		state: Rc<EvaluatorStore>,
		co: GenCo,
		value: Value,
	) -> Result<Value, ErrorKind> {
		if !state.fetch_settings().flakes {
			return Err(error(
				"experimental Nix feature 'flakes' is disabled; add '--extra-experimental-features flakes' to enable it",
			));
		}
		let reference = string(&co, value).await?;
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
