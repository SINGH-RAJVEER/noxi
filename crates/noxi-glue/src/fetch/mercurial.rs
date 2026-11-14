//! Mercurial working trees and immutable archives, following CppNix 2.35.2.
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;

use noxi_flakeref::error::{Error, Result, warn};
use noxi_flakeref::input::{Attr, Attrs, Input, get_str_attr, git_rev};
use noxi_flakeref::url::{parse_url, url_path_to_path};

use crate::mount::Mount;
use crate::store::EvaluatorStore;

fn run(args: &[&std::ffi::OsStr]) -> Result<Vec<u8>> {
	let output = Command::new("hg").args(args).env("HGPLAIN", "").output()?;
	if !output.status.success() {
		use std::io::Write;
		std::io::stderr().write_all(&output.stderr)?;
		return Err(Error::new(format!(
			"hg failed with exit code {}",
			output.status.code().unwrap_or(1)
		)));
	}
	Ok(output.stdout)
}

fn hg(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
	let mut argv = vec![
		std::ffi::OsStr::new(args[0]),
		std::ffi::OsStr::new("-R"),
		repo.as_os_str(),
	];
	argv.extend(args[1..].iter().map(std::ffi::OsStr::new));
	run(&argv)
}

/// CppNix's Mercurial `putFile` adds a written file to the working copy.
pub(super) fn track_file(path: &Path) -> Result<()> {
	run(&["add".as_ref(), path.as_os_str()])?;
	Ok(())
}

fn text(repo: &Path, args: &[&str]) -> Result<String> {
	Ok(String::from_utf8_lossy(&hg(repo, args)?)
		.trim_end()
		.to_owned())
}

fn dirty_snapshot(repo: &Path) -> Result<Mount> {
	let directory = Rc::new(tempfile::tempdir()?);
	let files = hg(
		repo,
		&[
			"status",
			"--clean",
			"--modified",
			"--added",
			"--no-status",
			"--print0",
		],
	)?;
	for file in files.split(|b| *b == 0).filter(|s| !s.is_empty()) {
		let relative = Path::new(std::ffi::OsStr::from_bytes(file));
		let source = repo.join(relative);
		let target = directory.path().join(relative);
		fs::create_dir_all(target.parent().unwrap())?;
		if fs::symlink_metadata(&source)?.file_type().is_symlink() {
			symlink(fs::read_link(&source)?, target)?;
		} else {
			fs::copy(source, target)?;
		}
	}
	Ok(Mount {
		root: directory.path().to_owned(),
		filter: None,
		keep: Some(directory),
	})
}

fn registered(store: &EvaluatorStore, input: &Input, mount: Mount) -> Result<(Mount, Input)> {
	let name = input.get_name();
	let (path, hash) = store.register_fetched_tree(&name, &mount)?;
	store
		.fetch_state
		.borrow_mut()
		.hashed
		.push((mount.root.clone(), name, path, hash));
	Ok((mount, input.clone()))
}

pub(crate) fn get_accessor(store: &EvaluatorStore, input: &Input) -> Result<(Mount, Input)> {
	use sha2::{Digest, Sha256};
	let url = get_str_attr(&input.attrs, "url")?;
	let parsed = parse_url(&url, false)?;
	let local = (parsed.scheme == "file")
		.then(|| url_path_to_path(&parsed.path))
		.transpose()?;
	let actual_url = local
		.as_ref()
		.map(|p| p.to_string_lossy().into_owned())
		.unwrap_or(url);
	let mut result = input.clone();
	let requested_rev = input.get_rev()?.map(|rev| git_rev(&rev));
	if let Some(repo) = &local {
		if input.get_ref()?.is_none()
			&& requested_rev.is_none()
			&& repo.join(".hg").exists()
			&& !hg(repo, &["status", "--modified", "--added", "--removed"])?.is_empty()
		{
			if !store.fetch_settings().allow_dirty {
				return Err(Error::new(format!(
					"Mercurial tree '\"{}\"' is unclean",
					repo.display()
				)));
			}
			warn(&format!(
				"Mercurial tree '\"{}\"' is unclean",
				repo.display()
			));
			result
				.attrs
				.insert("ref".into(), text(repo, &["branch"])?.into());
			return registered(store, &result, dirty_snapshot(repo)?);
		}
	}
	let reference = input.get_ref()?.unwrap_or_else(|| "default".into());
	let key: Attrs = [
		("url".into(), actual_url.clone().into()),
		("ref".into(), reference.clone().into()),
	]
	.into();
	let config = store.fetch_config.borrow().clone();
	let cache = store.fetch_cache()?;
	let cached_rev = if requested_rev.is_none() {
		cache
			.lookup_expired("hgRefToRev", &key, config.settings.tarball_ttl)?
			.filter(|entry| config.offline || !entry.expired)
			.map(|entry| get_str_attr(&entry.value, "rev"))
			.transpose()?
	} else {
		None
	};
	let revision = requested_rev.clone().or(cached_rev);
	let hash = nix_compat::nixhash::NixHash::Sha256(Sha256::digest(actual_url.as_bytes()).into())
		.to_nix_nixbase32();
	let repo = store
		.fetch_cache_dir()
		.join("hg")
		.join(hash.strip_prefix("sha256:").unwrap());
	fs::create_dir_all(repo.parent().unwrap())?;
	let lock = fs::OpenOptions::new()
		.create(true)
		.truncate(false)
		.write(true)
		.open(repo.with_extension("lock"))?;
	lock.lock()?;
	let have_revision = repo.join(".hg").exists()
		&& revision.as_ref().is_some_and(|rev| {
			Command::new("hg")
				.args(["log", "-R"])
				.arg(&repo)
				.args(["-r", rev, "--template", "1"])
				.env("HGPLAIN", "")
				.output()
				.is_ok_and(|p| p.status.success() && p.stdout == b"1")
		});
	if !have_revision {
		if config.offline {
			return Err(Error::new(format!(
				"cannot fetch Mercurial input '{}' in offline mode",
				input.display()
			)));
		}
		if repo.join(".hg").exists() {
			if let Err(error) = hg(&repo, &["pull", "--", &actual_url]) {
				if !repo.join(".hg/store/journal").exists() {
					return Err(error);
				}
				hg(&repo, &["recover"])?;
				hg(&repo, &["pull", "--", &actual_url])?;
			}
		} else {
			run(&[
				"clone".as_ref(),
				"--noupdate".as_ref(),
				"--".as_ref(),
				actual_url.as_ref(),
				repo.as_os_str(),
			])?;
		}
	}
	let info = text(
		&repo,
		&[
			"log",
			"-r",
			revision.as_deref().unwrap_or(&reference),
			"--template",
			"{node} {rev} {branch}",
		],
	)?;
	let mut fields = info.splitn(3, ' ');
	let rev = fields
		.next()
		.ok_or_else(|| Error::new("missing Mercurial revision"))?;
	let count = fields
		.next()
		.ok_or_else(|| Error::new("missing Mercurial revision count"))?
		.parse::<u64>()
		.map_err(|e| Error::new(e.to_string()))?;
	let branch = fields
		.next()
		.ok_or_else(|| Error::new("missing Mercurial branch"))?;
	result.attrs.insert("rev".into(), rev.into());
	result.attrs.insert("ref".into(), branch.into());
	result.attrs.insert("revCount".into(), Attr::Int(count));
	let directory = Rc::new(tempfile::tempdir()?);
	hg(
		&repo,
		&[
			"archive",
			"-r",
			rev,
			directory
				.path()
				.to_str()
				.ok_or_else(|| Error::new("non-UTF-8 archive path"))?,
		],
	)?;
	fs::remove_file(directory.path().join(".hg_archival.txt"))?;
	let fetched = registered(
		store,
		&result,
		Mount {
			root: directory.path().to_owned(),
			filter: None,
			keep: Some(directory),
		},
	)?;
	if requested_rev.is_none() {
		cache.upsert("hgRefToRev", &key, &[("rev".into(), rev.into())].into())?;
	}
	Ok(fetched)
}

#[noxi_eval::builtin_macros::builtins(state = "Rc<EvaluatorStore>")]
pub(crate) mod mercurial_builtins {
	use super::*;
	use bstr::ByteSlice;
	use noxi_eval::AddContext;
	use noxi_eval::generators::{self, Gen, GenCo};
	use noxi_eval::{CoercionKind, ErrorKind, NixAttrs, Value};

	fn fetch_error(error: Error) -> ErrorKind {
		let mut kind = ErrorKind::EvalError(error.message);
		for trace in error.traces {
			kind = kind.trace(None, trace);
		}
		kind
	}

	#[builtin("fetchMercurial")]
	async fn builtin_fetch_mercurial(
		state: Rc<EvaluatorStore>,
		co: GenCo,
		args: Value,
	) -> Result<Value, ErrorKind> {
		let mut url = String::new();
		let mut name = "source".to_owned();
		let mut revision = None;
		let mut reference = None;
		let is_attrs = matches!(&args, Value::Attrs(_));
		let arguments: Vec<_> = match args {
			Value::Attrs(attrs) => attrs
				.iter_sorted()
				.map(|(key, value)| {
					Ok((
						key.to_str()?.to_owned(),
						value.clone(),
						attrs.get_attr_pos(key),
					))
				})
				.collect::<Result<Vec<_>, ErrorKind>>()?,
			value => vec![("url".to_owned(), value, None)],
		};
		for (key, value, position) in arguments {
			let hint = match key.as_str() {
				"url" if !is_attrs => {
					"while evaluating the first argument passed to builtins.fetchMercurial"
				}
				"url" => "while evaluating the `url` attribute passed to builtins.fetchMercurial",
				"name" => "while evaluating the `name` attribute passed to builtins.fetchMercurial",
				"rev" => "while evaluating the `rev` attribute passed to builtins.fetchMercurial",
				_ => {
					let error = ErrorKind::EvalError(format!(
						"unsupported argument '{key}' to 'fetchMercurial'"
					));
					return Err(match position {
						Some(span) => ErrorKind::PositionedAt {
							span,
							underlying: Box::new(error),
						},
						None => error,
					});
				}
			};
			generators::request_builtin_context(&co, vec![noxi_eval::Trace::new(position, hint)])
				.await;
			let value = generators::request_force(&co, value).await;
			if value.is_catchable() {
				return Ok(value);
			}
			if key != "url" {
				generators::request_builtin_context(&co, Vec::new()).await;
			}
			let string = if key == "url" {
				generators::request_string_coerce(
					&co,
					value,
					CoercionKind {
						strong: false,
						import_paths: false,
					},
				)
				.await
				.map_err(ErrorKind::CatchableError)?
			} else {
				value.to_contextful_str().map_err(|error| {
					let error = match position {
						Some(span) => ErrorKind::PositionedAt {
							span,
							underlying: Box::new(error),
						},
						None => error,
					};
					error.trace(position, hint)
				})?
			};
			generators::request_builtin_context(&co, Vec::new()).await;
			if key != "url" && string.has_context() {
				let element = string.iter_context().flat_map(|c| c.iter()).next().unwrap();
				let path = match element {
					noxi_eval::NixContextElement::Plain(path) => path.clone(),
					noxi_eval::NixContextElement::Derivation(path) => format!("{path} (deep)"),
					noxi_eval::NixContextElement::Single { name, derivation } => {
						format!("{derivation}^{name}")
					}
				};
				return Err(ErrorKind::EvalError(format!(
					"the string '{}' is not allowed to refer to a store path (such as '{path}')",
					string.to_str()?
				))
				.trace(position, hint));
			}
			let string = string.to_str()?.to_owned();
			match key.as_str() {
				"url" => url = string,
				"name" => name = string,
				"rev" => {
					if string.len() == 40 && string.bytes().all(|b| b.is_ascii_hexdigit()) {
						revision = Some(string);
					} else {
						reference = Some(string);
					}
				}
				_ => unreachable!(),
			}
		}
		if is_attrs && url.is_empty() {
			return Err(ErrorKind::EvalError("'url' argument required".into()));
		}
		state.check_fetch_uri(&url).map_err(ErrorKind::from)?;
		if state.pure_eval() && revision.is_none() {
			return Err(ErrorKind::EvalError(
				"in pure evaluation mode, 'fetchMercurial' requires a Mercurial revision".into(),
			));
		}
		let mut attrs: Attrs = [
			("type".into(), "hg".into()),
			(
				"url".into(),
				(if url.contains("://") {
					url
				} else {
					format!("file://{url}")
				})
				.into(),
			),
			("name".into(), name.into()),
		]
		.into();
		if let Some(revision) = revision {
			attrs.insert("rev".into(), revision.into());
		}
		if let Some(reference) = reference {
			attrs.insert("ref".into(), reference.into());
		}
		let input = Input::from_attrs(&state.fetch_settings(), attrs).map_err(fetch_error)?;
		let (mount, _, mut locked, _) =
			super::super::get_cached(&state, &input, noxi_flakeref::registry::UseRegistries::No)
				.map_err(fetch_error)?;
		let path =
			super::super::mount_input(&state, &mut locked, &input, &mount).map_err(fetch_error)?;
		let rev = locked
			.get_rev()
			.map_err(fetch_error)?
			.map(|r| git_rev(&r))
			.unwrap_or_else(|| "0000000000000000000000000000000000000000".into());
		let mut result = vec![
			(
				"outPath".to_owned(),
				super::super::builtins::path_value(&path),
			),
			("rev".to_owned(), Value::from(rev.clone())),
			("shortRev".to_owned(), Value::from(rev[..12].to_owned())),
		];
		if let Some(branch) = locked.get_ref().map_err(fetch_error)? {
			result.push(("branch".into(), branch.into()));
		}
		if let Some(count) = locked.get_rev_count().map_err(fetch_error)? {
			result.push(("revCount".into(), Value::Integer(count as i64)));
		}
		Ok(Value::attrs(NixAttrs::from_iter(result)))
	}
}
