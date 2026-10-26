//! Fetchers for `builtins.fetchTree`, `fetchGit`, `fetchTarball`,
//! `fetchurl` and flake inputs, following CppNix 2.35.2's
//! `src/libfetchers`. A fetch yields a [`Mount`] (a local tree, optionally
//! filtered) and the locked input; [`mount_input`] hashes the tree and
//! serves it at its store path, copying it only into a writable store.

pub(crate) mod builtins;
mod cache;
pub mod flake;
pub(crate) mod git;
pub(crate) mod github;
pub mod http;
pub(crate) mod path;
pub(crate) mod tarball;

use std::path::{Path, PathBuf};
use std::rc::Rc;

use nix_compat::nixhash::NixHash;
use nix_compat::store_path::StorePath;
use nix_rs_flakeref::error::{Error, Result};
use nix_rs_flakeref::input::{Attr, FetchSettings, Input, Scheme, attrs_to_json, git_rev};
use nix_rs_flakeref::registry::{
	Registry, RegistryType, UseRegistries, flag_registry, lookup_in_registries,
	system_registry_path, user_registry_path,
};

use crate::mount::Mount;
use crate::store::EvaluatorStore;

/// How an evaluation fetches: settings from `nix.conf` and the command
/// line, `--override-flake` pairs and the directory relative flake
/// references are resolved against.
#[derive(Clone, Debug, Default)]
pub struct FetchConfig {
	pub settings: FetchSettings,
	pub pure_eval: bool,
	pub override_flakes: Vec<(String, String)>,
	pub cwd: PathBuf,
	pub cache_dir: Option<PathBuf>,
	pub offline: bool,
	pub http: Option<Rc<dyn http::Client>>,
}

/// A fetched input, as CppNix's input cache keeps it.
#[derive(Clone, Debug)]
pub(crate) struct CachedInput {
	pub locked: Input,
	pub mount: Mount,
	pub extra_attrs: nix_rs_flakeref::input::Attrs,
}

/// Per-evaluation fetcher state.
#[derive(Default)]
pub struct FetchState {
	registries: Option<Rc<Vec<Registry>>>,
	cache: Vec<(Input, CachedInput)>,
	/// Store paths of trees already hashed, by physical root and name.
	hashed: Vec<(PathBuf, String, StorePath, NixHash)>,
}

impl EvaluatorStore {
	/// Set the settings fetchers use for the rest of the evaluation.
	pub fn configure_fetching(&self, config: FetchConfig) {
		*self.fetch_config.borrow_mut() = config;
	}

	pub(crate) fn fetch_settings(&self) -> FetchSettings {
		self.fetch_config.borrow().settings.clone()
	}

	pub(crate) fn pure_eval(&self) -> bool {
		self.fetch_config.borrow().pure_eval
	}

	pub(crate) fn fetch_cache_dir(&self) -> PathBuf {
		self.fetch_config
			.borrow()
			.cache_dir
			.clone()
			.unwrap_or_else(nix_rs_flakeref::cache::cache_dir)
	}

	pub(crate) fn fetch_cache(&self) -> Result<nix_rs_flakeref::cache::Cache> {
		nix_rs_flakeref::cache::Cache::open(&self.fetch_cache_dir())
	}

	pub(crate) fn http_request(&self, request: &http::Request) -> Result<http::Response> {
		let config = self.fetch_config.borrow();
		if config.offline {
			return Err(Error::new(format!(
				"cannot download '{}' in offline mode",
				request.url
			)));
		}
		let client = config.http.clone().unwrap_or_else(|| Rc::new(http::Native));
		drop(config);
		client.request(request)
	}

	/// The flag, user, system and global registries, read once.
	pub(crate) fn registries(&self) -> Result<Rc<Vec<Registry>>> {
		if let Some(registries) = &self.fetch_state.borrow().registries {
			return Ok(registries.clone());
		}
		let config = self.fetch_config.borrow().clone();
		let settings = &config.settings;
		let mut registries = vec![flag_registry(
			settings,
			&config.override_flakes,
			&config.cwd,
		)?];
		registries.push(Registry::read(
			settings,
			&user_registry_path(),
			RegistryType::User,
		));
		registries.push(Registry::read(
			settings,
			&system_registry_path(),
			RegistryType::System,
		));
		let global = &settings.flake_registry;
		registries.push(if global.is_empty() {
			Registry::new(RegistryType::Global)
		} else if global.starts_with('/') {
			Registry::read(settings, Path::new(global), RegistryType::Global)
		} else {
			let bytes = tarball::download_bytes(self, global)?;
			Registry::parse(
				settings,
				&String::from_utf8_lossy(&bytes),
				global,
				RegistryType::Global,
			)
		});
		let registries = Rc::new(registries);
		self.fetch_state.borrow_mut().registries = Some(registries.clone());
		Ok(registries)
	}
}

/// `InputCache::getAccessor`: fetch `original`, resolving it through the
/// registries if it is indirect, and remember the result.
pub(crate) fn get_cached(
	store: &EvaluatorStore,
	original: &Input,
	use_registries: UseRegistries,
) -> Result<(Mount, Input, Input, nix_rs_flakeref::input::Attrs)> {
	let lookup = |input: &Input| {
		store
			.fetch_state
			.borrow()
			.cache
			.iter()
			.find(|(key, _)| key == input)
			.map(|(_, cached)| cached.clone())
	};
	let mut resolved = original.clone();
	let fetched = match lookup(original) {
		Some(fetched) => fetched,
		None => {
			let fetched = if original.is_direct() {
				let (mount, locked) = get_accessor(store, original)?;
				CachedInput {
					locked,
					mount,
					extra_attrs: Default::default(),
				}
			} else if use_registries != UseRegistries::No {
				let registries = store.registries()?;
				let (res, extra_attrs) =
					lookup_in_registries(&registries, original, use_registries)?;
				resolved = res;
				let fetched = match lookup(&resolved) {
					Some(fetched) => fetched,
					None => {
						let (mount, locked) = get_accessor(store, &resolved)?;
						CachedInput {
							locked,
							mount,
							extra_attrs,
						}
					}
				};
				store
					.fetch_state
					.borrow_mut()
					.cache
					.push((resolved.clone(), fetched.clone()));
				fetched
			} else {
				return Err(Error::new(format!(
					"'{}' is an indirect flake reference, but registry lookups are not allowed",
					original.display()
				)));
			};
			store
				.fetch_state
				.borrow_mut()
				.cache
				.push((original.clone(), fetched.clone()));
			fetched
		}
	};
	Ok((fetched.mount, resolved, fetched.locked, fetched.extra_attrs))
}

/// `Input::getAccessor`: fetch, mark the result final and check it
/// against what was asked for.
pub(crate) fn get_accessor(store: &EvaluatorStore, input: &Input) -> Result<(Mount, Input)> {
	let result = (|| {
		let (mount, mut result) = get_accessor_unchecked(store, input)?;
		result.attrs.insert("__final".into(), Attr::Bool(true));
		check_locks(input.clone(), &mut result)?;
		Ok((mount, result))
	})();
	result.map_err(|error: Error| {
		error.trace(format!("while fetching the input '{}'", input.display()))
	})
}

fn get_accessor_unchecked(store: &EvaluatorStore, input: &Input) -> Result<(Mount, Input)> {
	let Some(scheme) = input.scheme else {
		return Err(Error::new(format!(
			"cannot fetch unsupported input '{}'",
			json_string(input)
		)));
	};
	// A final input with a NAR hash may already be in the store.
	if input.is_final()
		&& let Some(nar_hash) = input.get_nar_hash()?
		&& let Ok(path) = nix_compat::store_path::build_ca_path(
			&input.get_name(),
			true,
			&nar_hash,
			std::iter::empty(),
			false,
		) && let Some(physical) = store.valid_store_path(&path.to_owned())
	{
		return Ok((Mount::new(physical), input.clone()));
	}
	let settings = store.fetch_settings();
	match scheme {
		Scheme::Path => path::get_accessor(store, input),
		Scheme::Git => git::get_accessor(store, &settings, input),
		Scheme::File => tarball::get_file_accessor(store, input),
		Scheme::Tarball => tarball::get_tarball_accessor(store, input),
		Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut => {
			github::get_accessor(store, &settings, scheme, input)
		}
		Scheme::Indirect => Err(Error::new(format!(
			"indirect input '{}' cannot be fetched directly",
			input.display()
		))),
		Scheme::Mercurial => Err(Error::new(format!(
			"nix-rs does not support fetching Mercurial input '{}'",
			input.display()
		))),
	}
}

fn json_string(input: &Input) -> String {
	attrs_to_json(&input.attrs)
		.map(|json| nix_rs_flakeref::json::dump(&serde_json::Value::Object(json), None))
		.unwrap_or_default()
}

/// `Input::checkLocks`.
pub(crate) fn check_locks(mut specified: Input, result: &mut Input) -> Result<()> {
	if specified.is_final() {
		if let Some(hash) = specified.get_nar_hash()? {
			specified
				.attrs
				.insert("narHash".into(), Attr::String(hash.to_sri_string()));
		}
		if let Some(hash) = result.get_nar_hash()? {
			result
				.attrs
				.insert("narHash".into(), Attr::String(hash.to_sri_string()));
		}
		for (name, value) in &specified.attrs {
			if let Some(value2) = result.attrs.get(name)
				&& value.force()? != value2.force()?
			{
				return Err(Error::new(format!(
					"mismatch in field '{name}' of input '{}', got '{}'",
					json_string(&specified),
					json_string(result)
				)));
			}
		}
		result.attrs = specified.attrs;
		return Ok(());
	}
	if let Some(prev) = specified.get_nar_hash()? {
		match result.get_nar_hash()? {
			Some(hash) if hash == prev => {}
			Some(hash) => {
				return Err(Error::new(format!(
					"NAR hash mismatch in input '{}', expected '{}' but got '{}'",
					specified.display(),
					prev.to_sri_string(),
					hash.to_sri_string()
				))
				.with_status(102));
			}
			None => {
				return Err(Error::new(format!(
					"NAR hash mismatch in input '{}', expected '{}' but got none",
					specified.display(),
					prev.to_sri_string()
				))
				.with_status(102));
			}
		}
	}
	if let Some(prev) = specified.get_rev()?
		&& result.get_rev()? != Some(prev.clone())
	{
		return Err(Error::new(format!(
			"'rev' attribute mismatch in input '{}', expected {}",
			result.display(),
			git_rev(&prev)
		)));
	}
	Ok(())
}

/// `EvalState::mountInput`: hash the tree, serve it at its store path and
/// record its NAR hash in `input`.
pub(crate) fn mount_input(
	store: &EvaluatorStore,
	input: &mut Input,
	original: &Input,
	mount: &Mount,
) -> Result<StorePath> {
	let (path, nar_hash) = add_tree(store, &input.get_name(), mount)?;
	input
		.attrs
		.insert("narHash".into(), Attr::String(nar_hash.to_sri_string()));
	if let Some(expected) = original.get_nar_hash()?
		&& expected != nar_hash
	{
		return Err(Error::new(format!(
			"NAR hash mismatch in input '{}', expected '{}' but got '{}'",
			original.display(),
			expected.to_sri_string(),
			nar_hash.to_sri_string()
		))
		.with_status(102));
	}
	Ok(path)
}

/// Hash `mount` as a NAR and make it available at the resulting store
/// path: copied into a writable store, or served from its root.
pub(crate) fn add_tree(
	store: &EvaluatorStore,
	name: &str,
	mount: &Mount,
) -> Result<(StorePath, NixHash)> {
	if let Some((_, _, path, hash)) = store
		.fetch_state
		.borrow()
		.hashed
		.iter()
		.find(|(root, n, ..)| *root == mount.root && n == name)
	{
		return Ok((path.clone(), hash.clone()));
	}
	let (path, hash) = store
		.add_tree(name, mount)
		.map_err(|e| Error::new(e.to_string()))?;
	store.fetch_state.borrow_mut().hashed.push((
		mount.root.clone(),
		name.to_owned(),
		path.clone(),
		hash.clone(),
	));
	Ok((path, hash))
}
