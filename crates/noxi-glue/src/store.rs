//! Evaluator store state, read-only by default. Writable mode registers
//! text, imported paths and derivations in an explicitly diverted local
//! store, while retaining CppNix 2.35.2's logical `/nix/store` paths.

use std::cell::{RefCell, RefMut};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bstr::ByteSlice;
use nix_compat::derivation::{Derivation, OutputHashMode};
use nix_compat::nixhash::{HashAlgo, NixHash};
use nix_compat::store_path::{self, StorePath};
use noxi_eval::{
	EvalIO, FileType, NixAttrs, NixContext, NixContextElement, NixString, StdIO, Value,
};
use noxi_store::build_derivation::{
	BuildDerivation, ExperimentalMethod, ExperimentalOutput, OutputFeatures,
};
use noxi_store::{
	CapturedSource, DerivedPath, FileIngestionMethod, LocalStore, LocalStoreConfig, SourceCapture,
	Store,
};
use sha2::{Digest, Sha256};

use crate::errors::ImportError;
use crate::known_paths::KnownPaths;
use crate::mount::Mount;
use crate::nar::{self, EntryType, Filter};

/// State shared by the store-aware builtins of one evaluation.
#[derive(Default)]
pub struct EvaluatorStore {
	known_paths: RefCell<KnownPaths>,
	output_features: std::cell::Cell<OutputFeatures>,
	/// The store that registers text, imports and derivations; `None`
	/// evaluates read-only, computing paths without writing them.
	store: Option<Rc<dyn Store>>,
	/// Derivations this evaluation registered. Each holds a temporary root
	/// for the life of the process, so a repeat needs no store request.
	registered_derivations: RefCell<HashSet<StorePath>>,
	real_store_dir: Option<PathBuf>,
	read_access: RefCell<Option<Rc<dyn Store>>>,
	opener: Option<Box<dyn Fn() -> io::Result<Option<Rc<dyn Store>>>>>,
	allow_ifd: std::cell::Cell<Option<bool>>,
	trace_ifd: std::cell::Cell<bool>,
	access: RefCell<Option<EvaluationAccess>>,
	/// Fetched trees served at logical store paths, keyed by store path.
	mounts: RefCell<BTreeMap<PathBuf, Mount>>,
	/// Mounts that are accessible only once their path is allowed.
	hidden_mounts: RefCell<BTreeSet<PathBuf>>,
	pub(crate) fetch_config: RefCell<crate::fetch::FetchConfig>,
	pub(crate) fetch_state: RefCell<crate::fetch::FetchState>,
}

/// Source roots visible to pure or restricted evaluation.
struct EvaluationAccess {
	pure: bool,
	/// The allowed prefixes, ordered like CppNix's `std::set<CanonPath>` so
	/// that a lookup stays logarithmic as every copied path is added.
	roots: BTreeSet<PathBuf>,
}

impl std::fmt::Debug for EvaluatorStore {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("EvaluatorStore")
			.field("known_paths", &self.known_paths)
			.field("writable", &self.store.is_some())
			.field("real_store_dir", &self.real_store_dir)
			.finish()
	}
}

/// A source captured once, before any store registration.
pub(crate) struct CapturedImport {
	name: String,
	bytes: CapturedSource,
	recursive: bool,
	hash: NixHash,
	path: StorePath,
}

/// Bound the resident capture while preserving exactly the bytes that were hashed.
const IMPORT_MEMORY_LIMIT: usize = 8 * 1024 * 1024;

/// Compatibility name for the default, read-only evaluator store.
pub type ReadOnlyStore = EvaluatorStore;

impl EvaluatorStore {
	pub fn new() -> Rc<Self> {
		Rc::new(Self::default())
	}

	pub(crate) fn set_output_features(&self, features: OutputFeatures) {
		self.output_features.set(features);
	}

	fn read_evaluation_derivation(
		&self,
		store: &dyn Store,
		path: &StorePath,
	) -> io::Result<BuildDerivation> {
		let mut drv = store
			.read_evaluation_derivation(path, self.output_features.get())
			.map_err(io::Error::other)?;
		let drv_name = path.name().strip_suffix(".drv").unwrap_or(path.name());
		let output_name = |name: &str| {
			if name == "out" {
				drv_name.to_owned()
			} else {
				format!("{drv_name}-{name}")
			}
		};
		// CppNix ignores the tuple's path for a fixed output and computes it
		// from its method, digest and derivation basename. The stored tuple
		// can contain a stale path even in a reference-accepted derivation.
		for (name, output) in &mut drv.experimental {
			if let Some((path, hash)) = &mut output.fixed {
				*path = output
					.method
					.fixed_path(&output_name(name), hash)
					.map_err(io::Error::other)?;
			}
		}
		if let Some(hash) = drv.basic.outputs().as_fixed_output_hash() {
			let name = drv.basic.outputs().names().next().unwrap().to_string();
			let method = match hash.mode {
				OutputHashMode::Flat => ExperimentalMethod::Flat,
				OutputHashMode::Recursive => ExperimentalMethod::NixArchive,
			};
			let path = method
				.fixed_path(&output_name(&name), &hash.hash)
				.map_err(io::Error::other)?;
			drv.experimental.insert(
				name,
				ExperimentalOutput {
					method,
					hash_algo: hash.hash.algo(),
					impure: false,
					fixed: Some((path, hash.hash.clone())),
				},
			);
		}
		Ok(drv)
	}

	fn diverted_config(root: &Path) -> io::Result<LocalStoreConfig> {
		LocalStoreConfig::diverted(root)
	}

	/// Map a diverted store without opening its database or creating files.
	pub fn read_only(root: &Path) -> io::Result<Rc<Self>> {
		let config = Self::diverted_config(root)?;
		Ok(Self::with_read_access(
			Some(config.real_store_dir.clone()),
			move || {
				LocalStore::open(config.clone())
					.map(|store| Some(Rc::new(store) as Rc<dyn Store>))
					.map_err(io::Error::other)
			},
		))
	}

	/// Compute values without registration while querying an already opened
	/// owner. Legacy commands open their owner before evaluation.
	pub fn with_read_store(store: Rc<dyn Store>, real_store_dir: Option<PathBuf>) -> Rc<Self> {
		Rc::new(Self {
			real_store_dir,
			read_access: RefCell::new(Some(store)),
			..Self::default()
		})
	}

	/// Compute store paths without registration, opening the selected owner
	/// only when context realization or derivation inspection needs it.
	pub fn with_read_access(
		real_store_dir: Option<PathBuf>,
		opener: impl Fn() -> io::Result<Option<Rc<dyn Store>>> + 'static,
	) -> Rc<Self> {
		Rc::new(Self {
			real_store_dir,
			opener: Some(Box::new(opener)),
			..Self::default()
		})
	}

	fn access_store(&self) -> io::Result<Option<Rc<dyn Store>>> {
		if let Some(store) = &self.store {
			return Ok(Some(store.clone()));
		}
		if let Some(store) = self.read_access.borrow().as_ref() {
			return Ok(Some(store.clone()));
		}
		let store = match &self.opener {
			Some(open) => open()?,
			None => None,
		};
		*self.read_access.borrow_mut() = store.clone();
		Ok(store)
	}

	/// Whether output-context access may build during evaluation.
	pub fn allow_import_from_derivation(&self, enabled: bool) {
		self.allow_ifd.set(Some(enabled));
	}

	/// Log the reference's warning for every output-context realization.
	pub fn trace_import_from_derivation(&self, enabled: bool) {
		self.trace_ifd.set(enabled);
	}

	/// Open a writable `local?root=ROOT` store. ROOT must already exist.
	/// Logical paths remain `/nix/store`; system stores are never opened.
	pub fn writable(root: &Path) -> io::Result<Rc<Self>> {
		Self::writable_with_settings(root, &nix_compat::nixcpp::settings::Settings::default())
	}

	pub fn writable_with_settings(
		root: &Path,
		settings: &nix_compat::nixcpp::settings::Settings,
	) -> io::Result<Rc<Self>> {
		let mut config = Self::diverted_config(root)?;
		config.apply_config(settings);
		let real_store_dir = Some(config.real_store_dir.clone());
		let local = LocalStore::open(config).map_err(io::Error::other)?;
		Ok(Self::with_store(Rc::new(local), real_store_dir))
	}

	/// Register paths in `store`, whose paths appear under
	/// `real_store_dir` when it is not `/nix/store`.
	pub fn with_store(store: Rc<dyn Store>, real_store_dir: Option<PathBuf>) -> Rc<Self> {
		Rc::new(Self {
			known_paths: RefCell::default(),
			store: Some(store),
			real_store_dir,
			..Self::default()
		})
	}

	/// The store paths are registered in, if evaluation is writable.
	pub fn store(&self) -> Option<&dyn Store> {
		self.store.as_deref()
	}

	pub fn add_text(
		&self,
		name: &str,
		contents: &[u8],
		references: &BTreeSet<StorePath>,
	) -> io::Result<StorePath> {
		if let Some(store) = &self.store {
			return store
				.add_text_to_store(name, contents, references)
				.map_err(io::Error::other);
		}
		store_path::build_text_path(name, contents, references.iter().map(StorePath::as_ref))
			.map(|p| p.to_owned())
			.map_err(io::Error::other)
	}

	/// Load dependency hashes from the selected owner before constructing a
	/// derivation from output contexts created in an earlier evaluation.
	pub(crate) fn load_context(&self, context: &NixContext) -> io::Result<()> {
		for element in context.iter() {
			let (path, deep) = match element {
				NixContextElement::Plain(_) => continue,
				NixContextElement::Single { derivation, .. } => (derivation, false),
				NixContextElement::Derivation(path) => (path, true),
			};
			let path = StorePath::from_absolute_path(path.as_bytes()).map_err(io::Error::other)?;
			self.load_derivation(&path)?;
			if deep && let Some(store) = self.access_store()? {
				let mut pending = vec![path];
				let mut seen = BTreeSet::new();
				while let Some(path) = pending.pop() {
					if !seen.insert(path.clone()) {
						continue;
					}
					let info = store
						.query_path_info(&path)
						.map_err(io::Error::other)?
						.ok_or_else(|| {
							io::Error::other(format!(
								"path '{}' is not valid",
								path.to_absolute_path()
							))
						})?;
					pending.extend(info.references.iter().cloned());
					self.known_paths
						.borrow_mut()
						.add_text(path.clone(), info.references);
					if path.name().ends_with(".drv") {
						self.load_derivation(&path)?;
					}
				}
			}
		}
		Ok(())
	}

	fn load_derivation(&self, path: &StorePath) -> io::Result<()> {
		if self
			.known_paths
			.borrow()
			.get_unverified_drv(&path.as_ref())
			.is_some()
		{
			return Ok(());
		}
		let store = self
			.access_store()?
			.ok_or_else(|| io::Error::other("cannot read a derivation without a store"))?;
		let mut pending: Vec<(StorePath, Option<BuildDerivation>)> = vec![(path.clone(), None)];
		let mut active = BTreeSet::new();
		while let Some((path, drv)) = pending.pop() {
			if self
				.known_paths
				.borrow()
				.get_unverified_drv(&path.as_ref())
				.is_some()
			{
				continue;
			}
			if let Some(drv) = drv {
				active.remove(&path);
				let mut known = self.known_paths.borrow_mut();
				if !drv.floating.is_empty()
					|| !drv.experimental.is_empty()
					|| drv.basic.outputs().is_deferred()
				{
					known.add_deferred_derivation(path, drv);
				} else {
					known
						.add_imported_derivation(path, drv.basic)
						.map_err(io::Error::other)?;
				}
				continue;
			}
			if !active.insert(path.clone()) {
				return Err(io::Error::other("cycle in derivation dependencies"));
			}
			let drv = self.read_evaluation_derivation(store.as_ref(), &path)?;
			let inputs: Vec<_> = drv.basic.input_derivations().keys().cloned().collect();
			pending.push((path, Some(drv)));
			pending.extend(inputs.into_iter().rev().map(|path| (path, None)));
		}
		Ok(())
	}

	pub(crate) fn register_derivation(&self, drv: &Derivation) -> io::Result<()> {
		self.write_derivation(
			drv.drv_path(),
			|| drv.to_aterm_bytes(),
			|| {
				drv.input_sources()
					.iter()
					.chain(drv.input_derivations().keys())
					.cloned()
					.collect()
			},
		)
	}

	pub(crate) fn register_build_derivation(
		&self,
		path: &StorePath,
		drv: &noxi_store::build_derivation::BuildDerivation,
	) -> io::Result<()> {
		self.write_derivation(
			path,
			|| drv.to_aterm_bytes(),
			|| {
				drv.basic
					.input_sources()
					.iter()
					.chain(drv.basic.input_derivations().keys())
					.cloned()
					.collect()
			},
		)
	}

	/// CppNix's `writeDerivation`: a derivation whose path is already valid
	/// is kept alive with a temporary root instead of being sent again.
	/// The root is taken before the validity check, so a collection cannot
	/// delete the path between the check and the end of the evaluation.
	fn write_derivation(
		&self,
		path: &StorePath,
		contents: impl FnOnce() -> Vec<u8>,
		references: impl FnOnce() -> BTreeSet<StorePath>,
	) -> io::Result<()> {
		let Some(store) = &self.store else {
			return Ok(());
		};
		if self.registered_derivations.borrow().contains(path) {
			return Ok(());
		}
		store.add_temp_root(path).map_err(io::Error::other)?;
		if !store.is_valid_path(path).map_err(io::Error::other)? {
			let actual = store
				.add_text_to_store(path.name(), &contents(), &references())
				.map_err(io::Error::other)?;
			if &actual != path {
				return Err(io::Error::other(
					"registered derivation path differs from evaluator path",
				));
			}
		}
		self.registered_derivations
			.borrow_mut()
			.insert(path.clone());
		Ok(())
	}

	/// The mount serving `path` and the path relative to its root.
	pub(crate) fn mount_of(&self, path: &Path) -> Option<(Mount, PathBuf)> {
		let mounts = self.mounts.borrow();
		if mounts.is_empty() {
			return None;
		}
		let (store_path, rest) = StorePath::from_absolute_path_full(path).ok()?;
		let mount = mounts.get(Path::new(&store_path.to_absolute_path()))?;
		Some((mount.clone(), rest.to_owned()))
	}

	/// Where `path` can be read, if it is mounted or valid in the store.
	pub(crate) fn valid_store_path(&self, path: &StorePath) -> Option<PathBuf> {
		if self.is_mounted(path) {
			return Some(self.physical_path(Path::new(&path.to_absolute_path())));
		}
		let store = self.access_store().ok()??;
		store
			.is_valid_path(path)
			.ok()?
			.then(|| store.real_path(path))
	}

	/// Hash a fetched tree as a NAR and make it available at its store
	/// path: copied into a writable store, otherwise served from `mount`.
	pub(crate) fn add_tree(&self, name: &str, mount: &Mount) -> io::Result<(StorePath, NixHash)> {
		let mut keep = |p: &Path, _| Ok(mount.allows_physical(p));
		let filter: Option<&mut Filter<'_>> = if mount.filter.is_some() {
			Some(&mut keep)
		} else {
			None
		};
		if let Some(store) = &self.store {
			let mut capture = SourceCapture::new(IMPORT_MEMORY_LIMIT);
			nar::dump_path(&mut capture, &mount.root, filter)?;
			let captured = capture.finish()?;
			let hash = NixHash::Sha256(captured.sha256());
			let path = ca_path(name, true, &hash, &mount.root)?;
			if !store.is_valid_path(&path).map_err(io::Error::other)? {
				store
					.add_to_store_from_capture(
						captured,
						name,
						FileIngestionMethod::NixArchive,
						&BTreeSet::new(),
					)
					.map_err(io::Error::other)?;
			}
			return Ok((path, hash));
		}
		let mut hasher = Sha256::new();
		nar::dump_path(&mut hasher, &mount.root, filter)?;
		let hash = NixHash::Sha256(hasher.finalize().into());
		let path = ca_path(name, true, &hash, &mount.root)?;
		self.mount(&path, mount.clone());
		Ok((path, hash))
	}

	/// Fetchers whose CppNix contract imports an archive register it even when
	/// ordinary expression evaluation only computes derivation paths.
	pub(crate) fn register_fetched_tree(
		&self,
		name: &str,
		mount: &Mount,
	) -> io::Result<(StorePath, NixHash)> {
		let store = self.access_store()?.ok_or_else(|| {
			io::Error::other("operation 'addToStoreFromDump' is not supported by store 'dummy://'")
		})?;
		let mut capture = SourceCapture::new(IMPORT_MEMORY_LIMIT);
		nar::dump_path(&mut capture, &mount.root, None)?;
		let captured = capture.finish()?;
		let hash = NixHash::Sha256(captured.sha256());
		let path = ca_path(name, true, &hash, &mount.root)?;
		if !store.is_valid_path(&path).map_err(io::Error::other)? {
			store
				.add_to_store_from_capture(
					captured,
					name,
					FileIngestionMethod::NixArchive,
					&BTreeSet::new(),
				)
				.map_err(io::Error::other)?;
		}
		self.mount(&path, mount.clone());
		Ok((path, hash))
	}

	/// Whether `path` is the root of, or inside, a mounted tree.
	pub fn is_mounted(&self, path: &StorePath) -> bool {
		self.mounts
			.borrow()
			.contains_key(Path::new(&path.to_absolute_path()))
	}

	/// Serve `root` at `path` for the rest of the evaluation.
	pub(crate) fn mount(&self, path: &StorePath, mount: Mount) {
		self.mounts
			.borrow_mut()
			.insert(PathBuf::from(path.to_absolute_path()), mount);
	}

	/// Run `add`, which may mount trees, without letting a restricted
	/// evaluation access the mounts it makes.
	pub(crate) fn with_hidden_mounts<T>(&self, add: impl FnOnce() -> T) -> T {
		let before: BTreeSet<_> = self.mounts.borrow().keys().cloned().collect();
		let result = add();
		self.hidden_mounts.borrow_mut().extend(
			self.mounts
				.borrow()
				.keys()
				.filter(|path| !before.contains(*path))
				.cloned(),
		);
		result
	}

	/// The mount whose physical root contains `physical`.
	fn mount_at_physical(&self, physical: &Path) -> Option<(PathBuf, Mount)> {
		self.mounts
			.borrow()
			.iter()
			.filter(|(_, mount)| physical.starts_with(&mount.root))
			.max_by_key(|(_, mount)| mount.root.as_os_str().len())
			.map(|(logical, mount)| (logical.clone(), mount.clone()))
	}

	/// Restrict expression filesystem access to these search path roots, as
	/// CppNix's `AllowListSourceAccessor` does. Each root is allowed as
	/// written, without resolving symlinks, and a root in the store also
	/// allows its closure. Other paths are allowed where CppNix calls
	/// `allowPath`: fetched and imported store paths and the closures of
	/// outputs built for import from derivation.
	pub fn restrict_access(&self, pure: bool, roots: Vec<PathBuf>) {
		let roots: BTreeSet<PathBuf> = roots.into_iter().map(noxi_eval::canon_path).collect();
		*self.access.borrow_mut() = Some(EvaluationAccess {
			pure,
			roots: roots.clone(),
		});
		for root in roots {
			if let Ok((path, _)) = StorePath::from_absolute_path_full(&root) {
				// CppNix ignores a search path entry that is not a valid path.
				let _ = self.allow_closure(&path);
			}
		}
	}

	fn allow_prefix(&self, path: &Path) {
		if let Some(access) = self.access.borrow_mut().as_mut() {
			access.roots.insert(path.to_owned());
		}
	}

	/// Check that evaluation may access `path` and where it really leads,
	/// through the allow list `restrict_access` started. Metadata operations
	/// pass `follow_final: false`, as CppNix's lstat does not resolve the
	/// final symlink. Without a list every path is refused, so an evaluator
	/// that relies on the list fails closed.
	pub fn check_restricted_access(&self, path: &Path, follow_final: bool) -> io::Result<()> {
		if self.access.borrow().is_none() {
			return Err(io::Error::new(
				io::ErrorKind::PermissionDenied,
				format!(
					"access to absolute path '{}' is not restricted by an allow list",
					path.display()
				),
			));
		}
		self.check_access(path, follow_final)
	}

	/// `EvalState::allowPath`.
	pub(crate) fn allow_store_path(&self, path: &StorePath) {
		self.allow_prefix(Path::new(&path.to_absolute_path()));
	}

	/// `EvalState::allowClosure`: allow `path` and every path it refers to.
	pub(crate) fn allow_closure(&self, path: &StorePath) -> io::Result<()> {
		if self.access.borrow().is_none() {
			return Ok(());
		}
		let Some(store) = self.access_store()? else {
			self.allow_store_path(path);
			return Ok(());
		};
		let closure = store
			.compute_closure(&BTreeSet::from([path.clone()]))
			.map_err(io::Error::other)?;
		for path in &closure {
			self.allow_store_path(path);
		}
		Ok(())
	}

	pub(crate) fn check_uri_path(&self, path: &Path) -> io::Result<()> {
		self.check_access(&noxi_eval::canon_path(path.to_owned()), false)
	}

	/// `CanonPath::isAllowed`: `path` is inside an allowed prefix, or is an
	/// ancestor of one, so that the allowed paths can be reached.
	fn is_allowed(&self, access: &EvaluationAccess, path: &Path) -> bool {
		if access.pure && hidden_by_pure_root(path) {
			return false;
		}
		let mounts = self.mounts.borrow();
		let hidden = self.hidden_mounts.borrow();
		// Paths order by component, so the descendants of `path` follow it
		// directly: the first prefix at or after it is one if any is.
		let from = (std::ops::Bound::Included(path), std::ops::Bound::Unbounded);
		path.starts_with(COREPKGS_DIR)
			|| path.ancestors().any(|prefix| {
				access.roots.contains(prefix)
					|| (mounts.contains_key(prefix) && !hidden.contains(prefix))
			})
			|| access
				.roots
				.range::<Path, _>(from)
				.next()
				.is_some_and(|root| root.starts_with(path))
			|| mounts
				.range::<Path, _>(from)
				.find(|(mounted, _)| !hidden.contains(*mounted))
				.is_some_and(|(mounted, _)| mounted.starts_with(path))
	}

	/// The entries of the directory `path` that restricted evaluation may
	/// list: CppNix shows only allowed paths and ancestors of allowed paths.
	fn visible_entries(
		&self,
		path: &Path,
		entries: Vec<(bytes::Bytes, FileType)>,
	) -> Vec<(bytes::Bytes, FileType)> {
		let access = self.access.borrow();
		let Some(access) = access.as_ref() else {
			return entries;
		};
		let dir = self.resolved_logical_path(path);
		entries
			.into_iter()
			.filter(|(name, _)| self.is_allowed(access, &dir.join(OsStr::from_bytes(name))))
			.collect()
	}

	fn check_access(&self, path: &Path, follow_final: bool) -> io::Result<()> {
		let access = self.access.borrow();
		let Some(access) = access.as_ref() else {
			return Ok(());
		};
		let allowed = |path: &Path| self.is_allowed(access, path);
		let denied = |path: &Path| {
			if access.pure && hidden_by_pure_root(path) {
				return io::Error::new(
					io::ErrorKind::NotFound,
					format!("path '{}' does not exist", path.display()),
				);
			}
			let mode = if access.pure {
				"pure evaluation mode (use '--impure' to override)"
			} else {
				"restricted mode"
			};
			io::Error::new(
				io::ErrorKind::PermissionDenied,
				format!(
					"access to absolute path '{}' is forbidden in {mode}",
					path.display()
				),
			)
		};
		let lexical = noxi_eval::canon_path(path.to_owned());
		if !allowed(&lexical) {
			return Err(denied(&lexical));
		}
		let raw = path.as_os_str().as_bytes();
		let resolved = if !follow_final && !raw.ends_with(b"/") && !raw.ends_with(b"/.") {
			path.to_owned()
		} else {
			self.resolved_logical_path(path)
		};
		if resolved.starts_with(COREPKGS_DIR) {
			return Ok(());
		}
		if allowed(&resolved) {
			return Ok(());
		}
		Err(denied(&resolved))
	}

	/// Whether a directory walk that copies or filters a tree may include the
	/// entry at `physical`. CppNix's allow-list accessor lists only allowed
	/// paths and their ancestors, so a walk never leaves the allowed trees.
	pub(crate) fn walk_visible(&self, physical: &Path) -> bool {
		match self.access.borrow().as_ref() {
			Some(access) => self.is_allowed(access, &self.logical_path(physical)),
			None => true,
		}
	}

	/// Whether evaluation access is restricted by an allow list.
	pub(crate) fn is_restricted(&self) -> bool {
		self.access.borrow().is_some()
	}

	/// Check that a mounted `path` is visible through its tree's filter.
	pub(crate) fn check_visible(&self, path: &Path) -> io::Result<()> {
		self.check_visibility(path, true)
	}

	/// Metadata operations inspect the final directory entry without granting
	/// access to the symlink's target. Ancestor symlinks still need checking.
	fn check_visible_metadata(&self, path: &Path) -> io::Result<()> {
		self.check_visibility(path, false)?;
		let mut ancestor = path.parent();
		while let Some(parent) = ancestor {
			if std::fs::symlink_metadata(self.physical_path(parent))
				.is_ok_and(|metadata| metadata.file_type().is_symlink())
			{
				return Err(io::Error::other(format!(
					"path '{}' (or its ancestor) is a symlink",
					parent.display()
				)));
			}
			ancestor = parent.parent();
		}
		Ok(())
	}

	fn check_visibility(&self, path: &Path, follow_final: bool) -> io::Result<()> {
		self.check_access(path, follow_final)?;
		if let Some((mount, rel)) = self.mount_of(path)
			&& let Some(filter) = &mount.filter
			&& !filter.allows(&rel)
		{
			return Err(filter.not_allowed(&rel));
		}
		Ok(())
	}

	/// Dump `physical` as a NAR, hiding what its mount's filter hides.
	pub(crate) fn dump_filtered<W: io::Write>(
		&self,
		writer: &mut W,
		physical: &Path,
		mut filter: Option<&mut Filter<'_>>,
	) -> io::Result<()> {
		let mount = self
			.mount_at_physical(physical)
			.map(|(_, mount)| mount)
			.filter(|mount| mount.filter.is_some());
		match mount {
			None => nar::dump_path(writer, physical, filter),
			Some(mount) => {
				let mut combined = |p: &Path, ty: EntryType| -> io::Result<bool> {
					if !mount.allows_physical(p) {
						return Ok(false);
					}
					match filter.as_deref_mut() {
						Some(filter) => filter(p, ty),
						None => Ok(true),
					}
				};
				nar::dump_path(writer, physical, Some(&mut combined))
			}
		}
	}

	/// The logical path `path` names after removing `.` and `..` lexically,
	/// as CppNix's `canonPath` does, and then resolving symlinks through
	/// the physical tree. An access check on the result cannot be escaped
	/// through `..` or a link out of an allowed tree. A path that does not
	/// exist keeps its lexical form.
	pub fn resolved_logical_path(&self, path: &Path) -> PathBuf {
		let mut lexical = PathBuf::new();
		for component in path.components() {
			match component {
				std::path::Component::ParentDir => {
					lexical.pop();
				}
				std::path::Component::CurDir => {}
				other => lexical.push(other),
			}
		}
		match std::fs::canonicalize(self.physical_path(&lexical)) {
			Ok(physical) => self.logical_path(&physical),
			Err(_) => lexical,
		}
	}

	/// Where a logical path is on disk: its mounted source, the file system,
	/// or the diverted store. Like `EvalState`'s root accessor, impure
	/// evaluation prefers a path that exists at its own location over the
	/// diverted store's.
	pub fn physical_path(&self, path: &Path) -> PathBuf {
		if let Some((mount, rest)) = self.mount_of(path) {
			return if rest.as_os_str().is_empty() {
				mount.root
			} else {
				mount.root.join(rest)
			};
		}
		if let Some(real) = &self.real_store_dir
			&& let Ok(rest) = path.strip_prefix(store_path::store_dir())
		{
			return if rest.as_os_str().is_empty() {
				real.clone()
			} else if self.reads_host_store() && std::fs::symlink_metadata(path).is_ok() {
				path.to_owned()
			} else {
				real.join(rest)
			};
		}
		path.to_owned()
	}

	/// Whether paths below the store directory may also be read where they
	/// are on the host: pure evaluation sees only the store's own objects.
	fn reads_host_store(&self) -> bool {
		self.access
			.borrow()
			.as_ref()
			.is_none_or(|access| !access.pure)
	}

	pub(crate) fn logical_path(&self, path: &Path) -> PathBuf {
		if let Some((logical, mount)) = self.mount_at_physical(path)
			&& let Ok(rest) = path.strip_prefix(&mount.root)
		{
			return if rest.as_os_str().is_empty() {
				logical
			} else {
				logical.join(rest)
			};
		}
		if let Some(real) = &self.real_store_dir
			&& let Ok(rest) = path.strip_prefix(real)
		{
			return Path::new(store_path::store_dir()).join(rest);
		}
		path.to_owned()
	}

	pub(crate) fn capture_import(
		&self,
		path: &Path,
		name: &str,
		recursive: bool,
		filter: Option<&mut Filter<'_>>,
	) -> io::Result<CapturedImport> {
		let physical = self.physical_path(path);
		let mut capture = SourceCapture::new(IMPORT_MEMORY_LIMIT);
		if recursive {
			self.dump_filtered(&mut capture, &physical, filter)?;
		} else {
			io::copy(&mut std::fs::File::open(physical)?, &mut capture)?;
		};
		let bytes = capture.finish()?;
		let hash = NixHash::Sha256(bytes.sha256());
		let store_path = ca_path(name, recursive, &hash, path)?;
		Ok(CapturedImport {
			name: name.to_owned(),
			bytes,
			recursive,
			hash,
			path: store_path,
		})
	}

	pub(crate) fn import_captured(
		&self,
		captured: CapturedImport,
		expected: Option<&NixHash>,
	) -> io::Result<StorePath> {
		// Both hashing and ingestion use these bytes. No source filesystem
		// reads, or store writes, occur between verification and ingestion.
		if expected.is_some_and(|hash| hash != &captured.hash) {
			return Err(io::Error::other(
				"store path mismatch in (possibly filtered) path",
			));
		}
		if let Some(store) = &self.store {
			let method = if captured.recursive {
				FileIngestionMethod::NixArchive
			} else {
				FileIngestionMethod::Flat
			};
			return store
				.add_to_store_from_capture(captured.bytes, &captured.name, method, &BTreeSet::new())
				.map_err(io::Error::other);
		}
		Ok(captured.path)
	}

	pub fn known_paths(&self) -> RefMut<'_, KnownPaths> {
		self.known_paths.borrow_mut()
	}

	/// Output `output` of the derivation `drv_path`, as CppNix's
	/// `readDerivation` declares it: `None` when there is no such output,
	/// otherwise its statically known path, if it has one.
	pub(crate) fn derivation_output_path(
		&self,
		drv_path: &StorePath,
		output: &str,
	) -> io::Result<Option<Option<StorePath>>> {
		let Ok(name) = output.parse::<nix_compat::derivation::OutputName>() else {
			return Ok(None);
		};
		let declared = |drv: &BuildDerivation| {
			drv.basic.outputs().contains_key(&name).then(|| {
				drv.experimental
					.get(output)
					.and_then(|output| output.fixed.as_ref())
					.map(|(path, _)| path.clone())
					.or_else(|| drv.basic.outputs().get(&name).cloned())
			})
		};
		// A writable evaluation registers its derivations as it creates
		// them; a read-only one finds only those already in the store.
		if self.store.is_some() {
			let known = self.known_paths.borrow();
			if let Some(drv) = known.get_deferred(drv_path) {
				return Ok(declared(drv));
			}
			if let Some(drv) = known.get_unverified_drv(&drv_path.as_ref()) {
				return Ok(drv
					.outputs()
					.contains_key(&name)
					.then(|| drv.outputs().get(&name).cloned()));
			}
		}
		let absolute = drv_path.to_absolute_path();
		let Some(store) = self.access_store()? else {
			return Err(io::Error::other(format!(
				"derivation '{absolute}' is not valid"
			)));
		};
		if !store.is_valid_path(drv_path).map_err(io::Error::other)? {
			return Err(io::Error::other(format!(
				"path '{absolute}' is not a valid store path"
			)));
		}
		if !drv_path.name().ends_with(".drv") {
			return Err(io::Error::other(format!(
				"error parsing derivation '{absolute}': store path '{drv_path}' is not a valid derivation path"
			)));
		}
		Ok(declared(
			&self.read_evaluation_derivation(store.as_ref(), drv_path)?,
		))
	}

	/// The store path a recursive (NAR) copy of `path` named `name` gets.
	pub fn import_nar(
		&self,
		path: &Path,
		name: &str,
		filter: Option<&mut Filter<'_>>,
	) -> io::Result<StorePath> {
		if self.store.is_some() {
			let captured = self.capture_import(path, name, true, filter)?;
			return self.import_captured(captured, None);
		}
		let mut hasher = Sha256::new();
		self.dump_filtered(&mut hasher, &self.physical_path(path), filter)?;
		let digest: [u8; 32] = hasher.finalize().into();
		ca_path(name, true, &NixHash::Sha256(digest), path)
	}

	/// The store path a flat copy of the regular file `path` gets, hashed
	/// with SHA-256.
	pub fn import_flat(&self, path: &Path, name: &str) -> io::Result<StorePath> {
		if self.store.is_some() {
			let captured = self.capture_import(path, name, false, None)?;
			return self.import_captured(captured, None);
		}
		let digest = nar::file_sha256(&self.physical_path(path))?;
		ca_path(name, false, &NixHash::Sha256(digest), path)
	}
}

/// In pure mode CppNix's root accessor serves only the store, mounted at the
/// store directory: `/` is an empty directory and the store directory's
/// other ancestors, such as `/nix`, do not exist.
fn hidden_by_pure_root(path: &Path) -> bool {
	let store_dir = Path::new(store_path::store_dir());
	path.parent().is_some()
		&& store_dir.starts_with(path)
		&& !path.components().eq(store_dir.components())
}

pub(crate) fn ca_path(
	name: &str,
	recursive: bool,
	hash: &NixHash,
	path: &Path,
) -> io::Result<StorePath> {
	store_path::build_ca_path(name, recursive, hash, std::iter::empty(), false)
		.map(|path| path.to_owned())
		.map_err(|_| io_error(ImportError::InvalidName(path.to_owned())))
}

#[cfg(test)]
mod tests {
	use super::*;

	/// `CanonPath::isAllowed`: an allowed prefix admits itself, what is
	/// inside it and its ancestors, but not a sibling that shares its name
	/// as a string prefix. A copied store path joins the list.
	#[test]
	fn allow_list_admits_prefixes_their_contents_and_ancestors() {
		let store = EvaluatorStore::new();
		assert!(store.check_restricted_access(Path::new("/"), true).is_err());
		store.restrict_access(true, vec!["/noxi-access/a/b".into()]);
		let copied = StorePath::from_bytes(b"00000000000000000000000000000000-copied").unwrap();
		store.allow_store_path(&copied);
		let copied = copied.to_absolute_path();
		for allowed in [
			"/",
			"/noxi-access",
			"/noxi-access/a",
			"/noxi-access/a/b",
			"/noxi-access/a/b/c/d",
			"/noxi-access/a/b/c/../d",
			store_path::store_dir(),
			&copied,
			&format!("{copied}/patches/fix.patch"),
		] {
			assert!(
				store
					.check_restricted_access(Path::new(allowed), true)
					.is_ok(),
				"{allowed}"
			);
		}
		for denied in [
			"/noxi-access/a/b.c",
			"/noxi-access/a/bc",
			"/noxi-access/a/c",
			"/noxi-access/a/b/../c",
			&format!("{copied}-other"),
			&format!(
				"{}/11111111111111111111111111111111-other",
				store_path::store_dir()
			),
		] {
			let error = store
				.check_restricted_access(Path::new(denied), true)
				.unwrap_err();
			assert!(
				error
					.to_string()
					.ends_with("is forbidden in pure evaluation mode (use '--impure' to override)"),
				"{denied}: {error}"
			);
		}
		// The pure root serves only the store: `/` stays an empty directory
		// and the store directory's other ancestors do not exist.
		let store_dir = Path::new(store_path::store_dir());
		for ancestor in store_dir.ancestors().skip(1) {
			let result = store.check_restricted_access(ancestor, false);
			if ancestor.parent().is_none() {
				assert!(result.is_ok(), "{}", ancestor.display());
			} else {
				assert_eq!(
					result.unwrap_err().kind(),
					io::ErrorKind::NotFound,
					"{}",
					ancestor.display()
				);
			}
		}
	}

	#[test]
	fn expected_hash_is_checked_against_captured_bytes_before_registration() {
		for size in [8, IMPORT_MEMORY_LIMIT + 1] {
			check_capture_survives_replacement(size);
		}
	}

	fn check_capture_survives_replacement(size: usize) {
		for recursive in [false, true] {
			let root = noxi_test_support::tempdir().unwrap();
			let source = noxi_test_support::tempdir().unwrap();
			let file = source.path().join("source");
			let original = vec![b'o'; size];
			std::fs::write(&file, &original).unwrap();
			let store = EvaluatorStore::writable(root.path()).unwrap();
			let captured = store
				.capture_import(&file, "source", recursive, None)
				.unwrap();
			assert_eq!(captured.bytes.is_rolled(), size > IMPORT_MEMORY_LIMIT);
			let expected_hash = captured.hash.clone();
			let expected_path = captured.path.clone();
			// Deterministically replace the source between capture and ingestion.
			std::fs::write(&file, "replacement").unwrap();
			let wrong_hash = if recursive {
				nar::nar_sha256(&file, None).unwrap()
			} else {
				nar::file_sha256(&file).unwrap()
			};
			assert_ne!(expected_hash, NixHash::Sha256(wrong_hash));
			let rejected = store
				.capture_import(&file, "rejected", recursive, None)
				.unwrap();
			let rejected_path = rejected.path.clone();
			assert!(
				store
					.import_captured(rejected, Some(&expected_hash))
					.is_err()
			);
			assert!(
				!store
					.store()
					.unwrap()
					.is_valid_path(&rejected_path)
					.unwrap()
			);
			let imported = store
				.import_captured(captured, Some(&expected_hash))
				.unwrap();
			assert_eq!(imported, expected_path);
			let local = store.store().unwrap();
			assert_eq!(std::fs::read(local.real_path(&imported)).unwrap(), original);
			assert!(local.is_valid_path(&expected_path).unwrap());
			assert!(!local.is_valid_path(&rejected_path).unwrap());
		}
	}
}

pub(crate) fn io_error(error: ImportError) -> io::Error {
	io::Error::other(error)
}

/// The name CppNix gives a copy of `path`: its last component.
pub(crate) fn import_name(path: &Path) -> io::Result<&str> {
	let name = path
		.file_name()
		.ok_or_else(|| io_error(ImportError::InvalidName(path.to_owned())))?;
	if name.as_bytes().ends_with(b".drv") {
		return Err(io_error(ImportError::DerivationFileName(path.to_owned())));
	}
	store_path::validate_name(name.as_bytes())
		.map_err(|_| io_error(ImportError::InvalidName(path.to_owned())))
}

/// Where CppNix's built-in `<nix/...>` files appear. CppNix serves them from
/// an in-memory filesystem after the search path finds nothing.
pub const COREPKGS_DIR: &str = "/__corepkgs__";

const COREPKGS: &[(&str, &str)] = &[
	("fetchurl.nix", include_str!("corepkgs/fetchurl.nix")),
	("call-flake.nix", include_str!("corepkgs/call-flake.nix")),
];

enum Corepkg {
	Root,
	File(&'static str),
}

fn corepkg(path: &Path) -> Option<Corepkg> {
	let rest = path.strip_prefix(COREPKGS_DIR).ok()?;
	if rest.as_os_str().is_empty() {
		return Some(Corepkg::Root);
	}
	COREPKGS
		.iter()
		.find(|(name, _)| rest == Path::new(name))
		.map(|(_, contents)| Corepkg::File(contents))
}

/// Local file access and imports through an [`EvaluatorStore`].
pub struct StoreIO {
	store: Rc<ReadOnlyStore>,
	ca_derivations: bool,
}

impl StoreIO {
	pub fn new(store: Rc<ReadOnlyStore>) -> Self {
		Self {
			store,
			ca_derivations: false,
		}
	}

	/// Enable the reference's placeholder gate for deferred outputs.
	pub fn with_ca_derivations(mut self, enabled: bool) -> Self {
		self.ca_derivations = enabled;
		let mut features = self.store.output_features.get();
		features.ca_derivations = enabled;
		self.store.set_output_features(features);
		self
	}
}

impl EvalIO for StoreIO {
	fn resolve_lookup_path(&self, value: &str) -> io::Result<Option<PathBuf>> {
		crate::fetch::lookup::resolve(&self.store, value).map(Some)
	}

	fn path_exists(&self, path: &Path) -> io::Result<bool> {
		if self.store.check_visibility(path, false).is_err() {
			return Ok(false);
		}
		// pathExists follows ancestor symlinks, unlike readFileType's
		// direct lstat. It still must not follow the final component.
		if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
			let resolved = self.store.resolved_logical_path(parent).join(name);
			if self.store.check_visibility(&resolved, false).is_err() {
				return Ok(false);
			}
		}
		if path.starts_with(COREPKGS_DIR) {
			return Ok(corepkg(path).is_some());
		}
		StdIO.path_exists(&self.store.physical_path(path))
	}

	fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
		if path.starts_with(COREPKGS_DIR) {
			return match corepkg(path) {
				Some(_) => Ok(path.to_owned()),
				None => Err(io::ErrorKind::NotFound.into()),
			};
		}
		let real = StdIO.canonicalize(&self.store.physical_path(path))?;
		Ok(self.store.logical_path(&real))
	}

	fn realise_path(&self, string: &NixString) -> io::Result<PathBuf> {
		let path = string.to_path().map_err(io::Error::other)?.to_owned();
		let elements: Vec<_> = string
			.iter_context()
			.flat_map(|context| context.iter())
			.filter(|element| match element {
				NixContextElement::Plain(path) => !StorePath::from_absolute_path(path.as_bytes())
					.is_ok_and(|path| self.store.is_mounted(&path)),
				_ => true,
			})
			.collect();
		if elements.is_empty() {
			return Ok(path);
		}
		let store = self.store.access_store()?.ok_or_else(|| {
			let first = match elements[0] {
				NixContextElement::Plain(path) | NixContextElement::Derivation(path) => path,
				NixContextElement::Single { derivation, .. } => derivation,
			};
			io::Error::other(format!("path '{first}' is not valid"))
		})?;
		let mut requests: BTreeMap<StorePath, BTreeSet<String>> = BTreeMap::new();
		for element in &elements {
			let raw = match element {
				NixContextElement::Plain(path) | NixContextElement::Derivation(path) => path,
				NixContextElement::Single { name, derivation } => {
					let drv = StorePath::from_absolute_path(derivation.as_bytes())
						.map_err(io::Error::other)?;
					requests.entry(drv).or_default().insert(name.clone());
					derivation
				}
			};
			let path = StorePath::from_absolute_path(raw.as_bytes()).map_err(io::Error::other)?;
			if !store.is_valid_path(&path).map_err(io::Error::other)? {
				return Err(io::Error::other(format!(
					"path '{}' is not valid",
					path.to_absolute_path()
				)));
			}
		}
		// Like CppNix's `realiseContext`, a path context grants no access:
		// paths are allowed where they are created or fetched.
		if requests.is_empty() {
			return Ok(path);
		}
		if !self.store.allow_ifd.get().unwrap_or(true) {
			let (drv, outputs) = requests.first_key_value().unwrap();
			return Err(io::Error::other(format!(
				"cannot build '{}^{}' during evaluation because the option 'allow-import-from-derivation' is disabled",
				drv.to_absolute_path(),
				outputs
					.first()
					.expect("context requests at least one output")
			)));
		}
		if self.store.trace_ifd.get()
			&& nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn)
		{
			let (drv, outputs) = requests.first_key_value().unwrap();
			eprintln!(
				"warning: built '{}^{}' during evaluation due to an import from derivation",
				drv.to_absolute_path(),
				outputs
					.first()
					.expect("context requests at least one output")
			);
		}
		let targets: Vec<_> = requests
			.iter()
			.map(|(drv, outputs)| DerivedPath::Built {
				drv: drv.clone(),
				outputs: outputs.clone(),
			})
			.collect();
		if self.store.store.is_none() {
			for (drv, names) in &requests {
				let outputs = store
					.deep_query_partial_derivation_output_map(drv)
					.map_err(io::Error::other)?;
				for name in names {
					let output = outputs.get(name).cloned().flatten().ok_or_else(|| {
						io::Error::other("cannot realize an unknown output in read-only evaluation")
					})?;
					let output = &output;
					if !store.is_valid_path(output).map_err(io::Error::other)? {
						return Err(io::Error::other(format!(
							"cannot substitute path '{}' - no write access to the Nix store",
							output.to_absolute_path()
						)));
					}
				}
			}
		} else {
			store.build_paths(&targets).map_err(io::Error::other)?;
		}
		let mut bytes = path.as_os_str().as_bytes().to_vec();
		for (drv, names) in requests {
			// `resolveDerivedPath`: content-addressed outputs resolve through
			// their realisations.
			let outputs = store
				.deep_query_partial_derivation_output_map(&drv)
				.map_err(io::Error::other)?;
			for name in names {
				let output = outputs
					.get(&name)
					.ok_or_else(|| {
						io::Error::other(format!(
							"derivation '{}' does not have an output named '{name}'",
							drv.to_absolute_path()
						))
					})?
					.as_ref()
					.ok_or_else(|| {
						io::Error::other(format!(
							"cannot operate on output '{name}' of the unbuilt derivation '{}'",
							drv.to_absolute_path()
						))
					})?;
				let placeholder = store_path::downstream_placeholder(&drv, name.as_bytes());
				bytes = bytes.replace(placeholder.as_bytes(), output.to_absolute_path().as_bytes());
				store.add_temp_root(output).map_err(io::Error::other)?;
				// CppNix allows the closure of each output built for import.
				self.store.allow_closure(output)?;
			}
		}
		Ok(PathBuf::from(OsString::from_vec(bytes)))
	}

	fn ensure_context_path(&self, path: &Path) -> io::Result<()> {
		if let Some(store) = &self.store.store {
			let path = StorePath::from_absolute_path(path.as_os_str().as_bytes())
				.map_err(io::Error::other)?;
			store.ensure_path(&path).map_err(io::Error::other)?;
		}
		Ok(())
	}

	fn import_derivation(&self, path: &Path) -> io::Result<Option<Value>> {
		// CppNix reads an imported derivation through the store, without
		// checking that the evaluator may access its file.
		let Ok(drv_path) = StorePath::from_absolute_path(path.as_os_str().as_bytes()) else {
			return Ok(None);
		};
		if !drv_path.name().ends_with(".drv") {
			return Ok(None);
		}
		let Some(store) = self.store.access_store()? else {
			return Ok(None);
		};
		if !store.is_valid_path(&drv_path).map_err(io::Error::other)? {
			return Ok(None);
		}
		let declaration = self
			.store
			.read_evaluation_derivation(store.as_ref(), &drv_path)?;
		let drv = &declaration.basic;
		let absolute = drv_path.to_absolute_path();
		let mut attrs: Vec<(NixString, Value)> = vec![
			(
				"drvPath".into(),
				NixString::new_context_from(
					NixContextElement::Derivation(absolute.clone()).into(),
					absolute.as_str(),
				)
				.into(),
			),
			(
				"name".into(),
				Value::from(
					drv.environment()
						.get(&bstr::BString::from("name"))
						.map(|s| s.as_slice())
						.unwrap_or_default(),
				),
			),
			(
				"outputs".into(),
				Value::List(
					drv.outputs()
						.names()
						.map(|name| Value::from(name.as_bytes()))
						.collect::<Vec<_>>()
						.into(),
				),
			),
		];
		for name in drv.outputs().names() {
			let fixed = declaration
				.experimental
				.get(&name.to_string())
				.and_then(|output| output.fixed.as_ref())
				.map(|(path, _)| path);
			let output_path = fixed.or_else(|| drv.outputs().get(name));
			if output_path.is_none() && !self.ca_derivations {
				return Err(io::Error::other(
					"experimental Nix feature 'ca-derivations' is disabled; add '--extra-experimental-features ca-derivations' to enable it",
				));
			}
			let output = output_path
				.map(StorePath::to_absolute_path)
				.unwrap_or_else(|| store_path::downstream_placeholder(&drv_path, name.as_bytes()));
			let string = NixString::new_context_from(
				NixContextElement::Single {
					name: name.to_string(),
					derivation: absolute.clone(),
				}
				.into(),
				output,
			);
			attrs.push((NixString::from(name.as_bytes()), string.into()));
		}
		Ok(Some(Value::Attrs(NixAttrs::from_iter(attrs))))
	}

	fn read_file(&self, path: &Path) -> io::Result<NixString> {
		let mut bytes = Vec::new();
		self.open(path)?.read_to_end(&mut bytes)?;
		if bytes.contains(&0) {
			return Err(io::Error::other(format!(
				"the contents of the file '{}' cannot be represented as a Nix string",
				path.display()
			)));
		}
		let mut context = NixContext::new();
		if let Ok((stored, _)) = StorePath::from_absolute_path_full(path)
			&& let Some(store) = self.store.access_store()?
			&& let Ok(Some(info)) = store.query_path_info(&stored)
		{
			for reference in info.references {
				let absolute = reference.to_absolute_path();
				let start = store_path::store_dir().len() + 1;
				let hash = &absolute.as_bytes()[start..start + 32];
				if bytes.windows(hash.len()).any(|window| window == hash) {
					context = context.append(NixContextElement::Plain(absolute));
				}
			}
		}
		Ok(NixString::new_context_from(context, bytes))
	}

	fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
		self.store.check_visible_metadata(path)?;
		StdIO.read_link(&self.store.physical_path(path))
	}

	fn open(&self, path: &Path) -> io::Result<Box<dyn io::Read>> {
		self.store.check_visible(path)?;
		match corepkg(path) {
			Some(Corepkg::File(contents)) => Ok(Box::new(contents.as_bytes())),
			_ => StdIO.open(&self.store.physical_path(path)),
		}
	}

	fn file_type(&self, path: &Path) -> io::Result<FileType> {
		self.store.check_visible_metadata(path)?;
		match corepkg(path) {
			Some(Corepkg::Root) => Ok(FileType::Directory),
			Some(Corepkg::File(_)) => Ok(FileType::Regular),
			None => StdIO.file_type(&self.store.physical_path(path)),
		}
	}

	fn read_dir(&self, path: &Path) -> io::Result<Vec<(bytes::Bytes, FileType)>> {
		self.store.check_visible(path)?;
		// CppNix lists the store directory through the store's accessor,
		// which serves only store objects.
		if path == Path::new(store_path::store_dir())
			&& (self.store.real_store_dir.is_some() || self.store.access_store()?.is_some())
		{
			return Err(io::Error::other("path '/' is not a valid store path"));
		}
		match corepkg(path) {
			Some(Corepkg::Root) => Ok(COREPKGS
				.iter()
				.map(|(name, _)| {
					(
						bytes::Bytes::from_static(name.as_bytes()),
						FileType::Regular,
					)
				})
				.collect()),
			_ => {
				let entries = StdIO.read_dir(&self.store.physical_path(path))?;
				let entries = self.store.visible_entries(path, entries);
				match self.store.mount_of(path) {
					Some((
						Mount {
							filter: Some(filter),
							..
						},
						rel,
					)) => Ok(entries
						.into_iter()
						.filter(|(name, _)| filter.allows(&rel.join(OsStr::from_bytes(name))))
						.collect()),
					_ => Ok(entries),
				}
			}
		}
	}

	fn import_path(&self, path: &Path) -> io::Result<PathBuf> {
		self.store.check_visible(path)?;
		let name = import_name(path)?;
		if let Some(Corepkg::File(contents)) = corepkg(path) {
			if let Some(store) = &self.store.store {
				let imported = store
					.add_to_store_from_dump(
						&mut contents.as_bytes(),
						name,
						FileIngestionMethod::Flat,
						FileIngestionMethod::NixArchive,
						HashAlgo::Sha256,
						&BTreeSet::new(),
					)
					.map_err(io::Error::other)?;
				self.store.allow_store_path(&imported);
				return Ok(PathBuf::from(imported.to_absolute_path()));
			}
			let digest = nar::bytes_nar_sha256(contents.as_bytes())?;
			let imported = ca_path(name, true, &NixHash::Sha256(digest), path)?;
			self.store.allow_store_path(&imported);
			return Ok(PathBuf::from(imported.to_absolute_path()));
		}
		// CppNix resolves symlinks in the ancestors only, so a symlink
		// named directly is copied as a symlink.
		let resolved = match path.parent() {
			// A store object is found where it is, not where its parent is.
			Some(parent) if parent == Path::new(store_path::store_dir()) => {
				match self.store.mount_of(path) {
					Some(_) => self.store.physical_path(parent).canonicalize()?.join(name),
					None => self.store.physical_path(path),
				}
			}
			Some(parent) => self.store.physical_path(parent).canonicalize()?.join(name),
			None => path.to_owned(),
		};
		// Under an allow list the copy holds only what the evaluation may see.
		let mut visible = |p: &Path, _| Ok(self.store.walk_visible(p));
		let filter: Option<&mut Filter<'_>> = if self.store.is_restricted() {
			Some(&mut visible)
		} else {
			None
		};
		let imported = self.store.import_nar(&resolved, name, filter)?;
		self.store.allow_store_path(&imported);
		Ok(PathBuf::from(imported.to_absolute_path()))
	}

	fn store_dir(&self) -> Option<String> {
		Some(store_path::store_dir().to_owned())
	}

	fn get_env(&self, key: &OsStr) -> Option<OsString> {
		if self.store.access.borrow().is_some() {
			None
		} else {
			StdIO.get_env(key)
		}
	}
}
