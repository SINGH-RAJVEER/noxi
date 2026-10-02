//! `builtins.path`, `builtins.filterSource`, `builtins.toFile` and
//! `builtins.storePath`, with optional diverted local-store registration.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use bstr::ByteSlice;
use nix_compat::nixhash::{HashAlgo, NixHash};
use nix_compat::store_path::StorePath;
use nix_rs_eval::builtin_macros::builtins;
use nix_rs_eval::generators::{self, GenCo};
use nix_rs_eval::{
	CatchableErrorKind, CoercionKind, ErrorKind, NixContextElement, NixString, Value,
	try_cek_to_value,
};

use crate::errors::ImportError;
use crate::nar::EntryType;
use crate::store::{ReadOnlyStore, import_name};

fn io_error(path: &Path, error: std::io::Error) -> ErrorKind {
	ErrorKind::IO {
		path: Some(path.to_owned()),
		error: Rc::new(error),
	}
}

fn entry_type(path: &Path) -> Result<EntryType, ErrorKind> {
	let file_type = fs::symlink_metadata(path)
		.map_err(|e| io_error(path, e))?
		.file_type();
	if file_type.is_symlink() {
		Ok(EntryType::Symlink)
	} else if file_type.is_dir() {
		Ok(EntryType::Directory)
	} else if file_type.is_file() {
		Ok(EntryType::Regular)
	} else {
		Err(ImportError::UnsupportedFileType(path.to_owned()).into())
	}
}

/// Coerce a value to a filesystem path as CppNix's `coerceToPath` does:
/// paths as they are, strings only when absolute.
async fn coerce_to_path(
	co: &GenCo,
	value: Value,
) -> Result<Result<PathBuf, CatchableErrorKind>, ErrorKind> {
	let value = generators::request_force(co, value).await;
	if let Value::Path(path) = value {
		return Ok(Ok(*path));
	}
	let string = match generators::request_string_coerce(
		co,
		value,
		CoercionKind {
			strong: false,
			import_paths: false,
		},
	)
	.await
	{
		Ok(string) => string,
		Err(cek) => return Ok(Err(cek)),
	};
	let path = PathBuf::from(std::ffi::OsStr::from_bytes(string.as_bytes()));
	if !path.is_absolute() {
		return Err(ErrorKind::NotAnAbsolutePath(path));
	}
	Ok(Ok(path))
}

/// Call the Nix `filter` on every entry below `root`, depth first in name
/// order as CppNix's `dumpPath` does, and return the entries it keeps.
async fn filtered_entries(
	state: &ReadOnlyStore,
	co: &GenCo,
	root: &Path,
	filter: &Value,
) -> Result<Result<HashSet<PathBuf>, CatchableErrorKind>, ErrorKind> {
	let mut included = HashSet::new();
	if entry_type(root)? != EntryType::Directory {
		return Ok(Ok(included));
	}

	let read_sorted = |dir: &Path| -> Result<std::vec::IntoIter<OsString>, ErrorKind> {
		let mut names = fs::read_dir(dir)
			.and_then(|entries| {
				entries
					.map(|entry| entry.map(|entry| entry.file_name()))
					.collect::<std::io::Result<Vec<_>>>()
			})
			.map_err(|e| io_error(dir, e))?;
		names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
		Ok(names.into_iter())
	};

	let mut stack = vec![(root.to_path_buf(), read_sorted(root)?)];
	while let Some((dir, names)) = stack.last_mut() {
		let Some(name) = names.next() else {
			stack.pop();
			continue;
		};
		let child = dir.join(name);
		let ty = entry_type(&child)?;
		let keep = generators::request_call_with(
			co,
			filter.clone(),
			[
				Value::from(state.logical_path(&child).as_os_str().as_bytes()),
				Value::from(ty.as_nix_str()),
			],
		)
		.await;
		let keep = generators::request_force(co, keep).await;
		if let Value::Catchable(cek) = keep {
			return Ok(Err(*cek));
		}
		if keep.as_bool()? {
			included.insert(child.clone());
			if ty == EntryType::Directory {
				let names = read_sorted(&child)?;
				stack.push((child, names));
			}
		}
	}
	Ok(Ok(included))
}

/// Shared by `builtins.path` and `builtins.filterSource`: CppNix's `addPath`.
async fn add_path(
	state: &ReadOnlyStore,
	co: &GenCo,
	name: &str,
	path: &Path,
	filter: Option<Value>,
	recursive: bool,
	expected: Option<NixHash>,
) -> Result<Value, ErrorKind> {
	// `addPath` follows every symlink in the path, including the last.
	let resolved = fs::canonicalize(state.physical_path(path)).map_err(|e| io_error(path, e))?;

	let included = match &filter {
		Some(filter) => match filtered_entries(state, co, &resolved, filter).await? {
			Ok(included) => Some(included),
			Err(cek) => return Ok(Value::Catchable(Box::new(cek))),
		},
		None => None,
	};

	let mut keep = |p: &Path, _| Ok(included.as_ref().is_none_or(|set| set.contains(p)));
	let store_path = if expected.is_some() {
		let captured = state
			.capture_import(&resolved, name, recursive, Some(&mut keep))
			.map_err(|e| io_error(path, e))?;
		state.import_captured(captured, expected.as_ref())
	} else if recursive {
		state.import_nar(&resolved, name, Some(&mut keep))
	} else {
		state.import_flat(&resolved, name)
	}
	.map_err(|e| io_error(path, e))?;

	Ok(store_path_string(&store_path))
}

fn store_path_string(path: &StorePath) -> Value {
	let absolute = path.to_absolute_path();
	Value::String(NixString::new_context_from(
		NixContextElement::Plain(absolute.clone()).into(),
		absolute,
	))
}

#[builtins(state = "Rc<ReadOnlyStore>")]
pub(crate) mod import_builtins {
	use super::*;
	use nix_rs_eval::generators::Gen;

	#[builtin("filterSource")]
	async fn builtin_filter_source(
		state: Rc<ReadOnlyStore>,
		co: GenCo,
		#[lazy] filter: Value,
		path: Value,
	) -> Result<Value, ErrorKind> {
		let path = try_cek_to_value!(coerce_to_path(&co, path).await?);
		let filter = generators::request_force(&co, filter).await;
		if filter.is_catchable() {
			return Ok(filter);
		}
		let name = import_name(&path)
			.map_err(|e| io_error(&path, e))?
			.to_owned();
		add_path(&state, &co, &name, &path, Some(filter), true, None).await
	}

	#[builtin("path")]
	async fn builtin_path(
		state: Rc<ReadOnlyStore>,
		co: GenCo,
		args: Value,
	) -> Result<Value, ErrorKind> {
		let args = args.to_attrs()?;
		let mut path = None;
		let mut name = None;
		let mut filter = None;
		let mut recursive = true;
		let mut expected = None;

		for (key, value) in args.iter_sorted() {
			match key.to_str()? {
				"path" => {
					path = Some(try_cek_to_value!(coerce_to_path(&co, value.clone()).await?));
				}
				"name" => {
					let value = generators::request_force(&co, value.clone()).await;
					if value.is_catchable() {
						return Ok(value);
					}
					name = Some(value.to_str()?.to_str()?.to_owned());
				}
				"filter" => {
					let value = generators::request_force(&co, value.clone()).await;
					if value.is_catchable() {
						return Ok(value);
					}
					filter = Some(value);
				}
				"recursive" => {
					let value = generators::request_force(&co, value.clone()).await;
					if value.is_catchable() {
						return Ok(value);
					}
					recursive = value.as_bool()?;
				}
				"sha256" => {
					let value = generators::request_force(&co, value.clone()).await;
					if value.is_catchable() {
						return Ok(value);
					}
					let hash = value.to_str()?;
					let hash = hash.to_str()?;
					// An empty hash means none, as in CppNix's `newHashAllowEmpty`.
					if !hash.is_empty() {
						expected = Some(
							NixHash::from_str(hash, Some(HashAlgo::Sha256))
								.map_err(|e| ErrorKind::NixRsError(Arc::new(e)))?,
						);
					}
				}
				other => {
					return Err(ErrorKind::Abort(format!(
						"unsupported argument '{other}' to 'builtins.path'"
					)));
				}
			}
		}

		let Some(path) = path else {
			return Err(ErrorKind::Abort(
				"missing required 'path' attribute in the first argument to 'builtins.path'".into(),
			));
		};
		let name = match name.filter(|n| !n.is_empty()) {
			Some(name) => name,
			None => import_name(&path)
				.map_err(|e| io_error(&path, e))?
				.to_owned(),
		};
		add_path(&state, &co, &name, &path, filter, recursive, expected).await
	}

	#[builtin("toFile")]
	async fn builtin_to_file(
		state: Rc<ReadOnlyStore>,
		co: GenCo,
		name: Value,
		contents: Value,
	) -> Result<Value, ErrorKind> {
		if name.is_catchable() {
			return Ok(name);
		}
		if contents.is_catchable() {
			return Ok(contents);
		}
		let name = name.to_str()?;
		let name = name.to_str()?.to_owned();
		let contents = contents.to_contextful_str()?;

		let mut references = BTreeSet::new();
		for element in contents.iter_context().flat_map(|context| context.iter()) {
			match element {
				NixContextElement::Plain(path) => {
					references.insert(
						StorePath::from_absolute_path(path.as_bytes())
							.map_err(|e| ErrorKind::NixRsError(Arc::new(e)))?,
					);
				}
				_ => return Err(ImportError::ToFileOutputReference(name).into()),
			}
		}

		let path = state
			.add_text(&name, contents.as_bytes(), &references)
			.map_err(|e| ErrorKind::NixRsError(Arc::new(e)))?;
		state.known_paths().add_text(path.clone(), references);
		Ok(store_path_string(&path))
	}

	#[builtin("storePath")]
	async fn builtin_store_path(
		state: Rc<ReadOnlyStore>,
		co: GenCo,
		path: Value,
	) -> Result<Value, ErrorKind> {
		let path = try_cek_to_value!(coerce_to_path(&co, path).await?);
		let absolute = path.as_os_str().as_bytes();
		let (store_path, _rest) = StorePath::from_absolute_path_full(&path).map_err(|_| {
			ErrorKind::Abort(format!("path '{}' is not in the Nix store", path.display()))
		})?;
		// Read-only mode records the dependency without checking validity.
		if let Some(local) = state.local_store() {
			if !local
				.is_valid_path(&store_path)
				.map_err(|e| ErrorKind::NixRsError(Arc::new(e)))?
			{
				return Err(ErrorKind::Abort(format!(
					"path '{}' is not valid",
					store_path.to_absolute_path()
				)));
			}
			local
				.add_temp_root(&store_path)
				.map_err(|e| ErrorKind::NixRsError(Arc::new(e)))?;
		}
		Ok(Value::String(NixString::new_context_from(
			NixContextElement::Plain(store_path.to_absolute_path()).into(),
			absolute,
		)))
	}
}
