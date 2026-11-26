//! Realising derivations in a local store, following CppNix 2.35.2's
//! `DerivationBuildingGoal`, `DerivationBuilderImpl` and
//! `ChrootLinuxDerivationBuilder`.
//!
//! Input derivations finish before their dependents. Parallel requests use
//! a bounded ready queue, while single-job requests retain depth-first
//! realization. Each build runs in
//! the Linux sandbox with only its input closure visible, then its outputs
//! are canonicalised, scanned for references, checked and registered with
//! the derivation as their deriver.
//!
//! A derivation is offered to the build hook before it is built here, unless
//! it prefers a local build; see [`crate::hook`].

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bstr::{BString, ByteSlice};
use nix_compat::cppnix_json::derivation::InputNode;
use nix_compat::derivation::{OutputHashMode, OutputName, UnverifiedDerivation, lookup_fn};
use nix_compat::nixbase32;
use nix_compat::nixcpp::settings::ExternalBuilder;
use nix_compat::nixhash::{CAHash, HashAlgo, NixHash};
use nix_compat::store_path::{
	self, StorePath, downstream_placeholder, hash_placeholder, store_dir,
};
pub use noxi_store::BuildMode;
use noxi_store::binary_cache::BinaryCache;
use noxi_store::build_derivation::{BuildDerivation, FloatingOutput};
use noxi_store::canonicalise::canonicalise_path_metadata;
use noxi_store::lock::PathLocks;
use noxi_store::{
	FileIngestionMethod, LocalStore, Realisation, Store, ValidPathInfo, delete_path, nar,
};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::{BuildError, FailureStatus, Result};
use crate::hook::{HookEvent, HookInstance, Hooks, Reply};
use crate::options::{self, DerivationOptions, DrvRef, OutputChecks, OutputChecksSpec};
use crate::refscan::scan_for_references;
use crate::sandbox::{self, Bind, ChrootBinds, SandboxSpec};
use crate::settings::{BuildSettings, SandboxMode};
use crate::structured;
use crate::user_lock::{self, BuildUser};

#[path = "scheduler.rs"]
mod scheduler;
pub use scheduler::BuildBatch;

/// `DerivationBuilderImpl::homeDir`.
const HOME_DIR: &str = "/homeless-shelter";

fn print(path: &StorePath) -> String {
	path.to_absolute_path()
}

/// What happens while realising, for callers that report progress the
/// way CppNix's build activities do.
#[derive(Debug)]
pub enum BuildEvent<'a> {
	/// A repair checked a registered path's contents.
	PathChecked { path: &'a StorePath, valid: bool },
	/// A derivation is about to be built.
	Started {
		drv_path: &'a StorePath,
		mode: BuildMode,
	},
	/// The build hook builds a derivation on `machine`.
	StartedRemotely {
		drv_path: &'a StorePath,
		mode: BuildMode,
		machine: &'a str,
	},
	/// Output from the builder.
	Output(&'a [u8]),
	/// The build ended, successfully or with `error`.
	Finished {
		drv_path: &'a StorePath,
		error: Option<&'a BuildError>,
	},
}

/// Builds derivations into one local store.
#[derive(Clone)]
pub struct LocalBuilder<'a> {
	store: &'a LocalStore,
	settings: &'a BuildSettings,
	substituters: &'a [BinaryCache],
	cancelled: Option<&'a AtomicBool>,
	mode: BuildMode,
	/// `pathContentsGoodCache`: the paths a repair has checked.
	checked: Arc<Mutex<BTreeMap<StorePath, bool>>>,
	/// The outputs of the impure derivations built by this request, which the
	/// store does not record. Each is built once per request.
	impure: Arc<Mutex<BTreeMap<StorePath, BTreeMap<String, StorePath>>>>,
	/// The build hook, if there is one to offer builds to.
	hooks: Option<Arc<Hooks>>,
	/// The turn of the goal this builder realises; see [`Hooks`].
	turn: Arc<AtomicU64>,
	/// The first error of an automatic collection, which stops every build
	/// of the request as it ends CppNix's `Worker::run`.
	auto_gc_error: Arc<Mutex<Option<String>>>,
}

/// An output a derivation declares, with the path it must produce.
#[derive(Clone)]
struct KnownOutput {
	path: StorePath,
	/// The expected content address of a fixed output.
	fixed: Option<(FileIngestionMethod, NixHash)>,
	floating: Option<FloatingOutput>,
	/// Built at a random path, and never valid before it is built.
	impure: bool,
}

/// What a build depends on besides the derivation's fields.
#[derive(Clone, Copy)]
struct BuildKind {
	fixed: bool,
	impure: bool,
}

impl BuildKind {
	fn of(drv: &BuildDerivation) -> Self {
		Self {
			fixed: drv.is_fixed(),
			impure: drv.is_impure(),
		}
	}

	/// `DerivationType::isSandboxed`.
	fn sandboxed(self) -> bool {
		!self.fixed && !self.impure
	}
}

impl KnownOutput {
	/// How the output's content address is computed, if it has one.
	fn content_address(&self) -> Option<(FileIngestionMethod, HashAlgo)> {
		if let Some((method, hash)) = &self.fixed {
			return Some((*method, hash.algo()));
		}
		self.floating
			.as_ref()
			.map(|spec| (spec.method, spec.hash_algo))
	}
}

impl<'a> LocalBuilder<'a> {
	pub fn new(store: &'a LocalStore, settings: &'a BuildSettings) -> Self {
		Self {
			store,
			settings,
			substituters: &[],
			cancelled: None,
			mode: BuildMode::Normal,
			checked: Arc::default(),
			impure: Arc::default(),
			hooks: Hooks::new(settings),
			turn: Arc::new(AtomicU64::new(NO_TURN)),
			auto_gc_error: Arc::default(),
		}
	}

	/// Substitute from `caches` before building, as CppNix does with
	/// `substituters`.
	pub fn with_substituters(mut self, caches: &'a [BinaryCache]) -> Self {
		self.substituters = caches;
		self
	}

	/// Realise a goal that has `turn`.
	pub(crate) fn with_turn(mut self, turn: u64) -> Self {
		self.turn = Arc::new(AtomicU64::new(turn));
		self
	}

	/// The build hook, if builds can be offered to one.
	pub(crate) fn hooks(&self) -> Option<&Arc<Hooks>> {
		self.hooks.as_ref()
	}

	/// Cancel this request when the flag becomes true. Running sandbox
	/// builders are killed and reaped before the request returns.
	pub fn with_cancellation(mut self, cancelled: &'a AtomicBool) -> Self {
		self.cancelled = Some(cancelled);
		self
	}

	/// What CppNix's worker loop does each time it wakes while builds
	/// run: collect garbage if `min-free` asks for it (`autoGC(false)`), then
	/// report whether to stop. An error of the collection stops the builds;
	/// [`LocalBuilder::auto_gc_error`] then reports it.
	pub(crate) fn tick(&self) -> bool {
		if let Err(error) = self.store.auto_gc(false) {
			self.auto_gc_error
				.lock()
				.unwrap_or_else(|e| e.into_inner())
				.get_or_insert(error.to_string());
		}
		self.is_cancelled()
	}

	/// The error that stopped the builds through [`LocalBuilder::tick`], in
	/// place of `error`, which reports that they were cancelled.
	pub(crate) fn auto_gc_error(&self, error: BuildError) -> BuildError {
		match (
			&error,
			&*self.auto_gc_error.lock().unwrap_or_else(|e| e.into_inner()),
		) {
			(BuildError::Cancelled, Some(message)) => {
				BuildError::Store(noxi_store::Error::Other(message.clone()))
			}
			_ => error,
		}
	}

	fn is_cancelled(&self) -> bool {
		crate::interrupt::interrupted()
			|| self
				.auto_gc_error
				.lock()
				.unwrap_or_else(|e| e.into_inner())
				.is_some()
			|| self
				.cancelled
				.is_some_and(|flag| flag.load(Ordering::Relaxed))
	}

	/// `startBuild`'s build user: lock a free member of
	/// `build-users-group` when running as root, waiting while all are busy.
	fn acquire_build_user(&self, drv_path: &StorePath) -> Result<Option<BuildUser>> {
		if !user_lock::use_build_users(&self.settings.build_users_group) {
			return Ok(None);
		}
		let mut waiting = false;
		loop {
			if let Some(user) = user_lock::acquire(
				&self.store.config().state_dir,
				&self.settings.build_users_group,
			)? {
				return Ok(Some(user));
			}
			if !waiting {
				tracing::warn!("waiting for a free build user ID for '{}'", print(drv_path));
				waiting = true;
			}
			self.check_cancelled()?;
			std::thread::sleep(std::time::Duration::from_millis(250));
		}
	}

	fn check_cancelled(&self) -> Result<()> {
		if self.is_cancelled() {
			Err(BuildError::Cancelled)
		} else {
			Ok(())
		}
	}

	fn lock_outputs(&self, paths: impl IntoIterator<Item = PathBuf>) -> Result<PathLocks> {
		match PathLocks::lock_cancellable(paths, &|| self.is_cancelled()) {
			Err(_) if self.is_cancelled() => Err(BuildError::Cancelled),
			result => Ok(result?),
		}
	}

	/// How the requested derivations are built.
	pub fn with_mode(mut self, mode: BuildMode) -> Self {
		self.mode = mode;
		self
	}

	/// The builder of the requested derivations' inputs.
	fn for_inputs(&self) -> Self {
		Self {
			mode: match self.mode {
				BuildMode::Check => BuildMode::Normal,
				mode => mode,
			},
			..self.clone()
		}
	}

	fn repair(&self) -> bool {
		self.mode == BuildMode::Repair
	}

	/// A check needs the outputs it compares with.
	fn cannot_check(&self, drv_path: &StorePath) -> BuildError {
		BuildError::Other(format!(
			"some outputs of '{}' are not valid, so checking is not possible\nHint: --rebuild and --check error if the derivation was not previously built and cannot be substituted.\n      Remove it to perform a fresh build, or use --repair to rewrite missing or corrupted builds in the store.",
			print(drv_path)
		))
	}

	/// Build a supplied basic derivation, without registering its derivation file.
	pub fn build_derivation(
		&self,
		drv_path: &StorePath,
		drv: &UnverifiedDerivation,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<BTreeMap<String, StorePath>> {
		let build_drv = BuildDerivation {
			basic: drv.clone(),
			floating: BTreeMap::new(),
			experimental: BTreeMap::new(),
			dynamic: BTreeMap::new(),
		};
		self.build_supplied_derivation(drv_path, &build_drv, events)
	}

	/// [`LocalBuilder::build_derivation`] for any kind of outputs, whose
	/// realisations are registered.
	pub fn build_supplied_derivation(
		&self,
		drv_path: &StorePath,
		build_drv: &BuildDerivation,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<BTreeMap<String, StorePath>> {
		let outputs = self.known_outputs(drv_path, build_drv)?;
		self.report_checks(&outputs, events)?;
		let valid = self.all_valid(&outputs)?;
		if self.mode == BuildMode::Check && !valid {
			return Err(self.cannot_check(drv_path));
		}
		if self.mode == BuildMode::Check || !valid {
			let result = self.build(drv_path, build_drv, &outputs, events);
			events(BuildEvent::Finished {
				drv_path,
				error: result.as_ref().err(),
			});
			let built = result?;
			self.register_realisations(drv_path, &built)?;
			return Ok(built);
		}
		Ok(outputs
			.into_iter()
			.map(|(name, output)| (name, output.path))
			.collect())
	}

	/// Try to substitute `path`; a failure is logged, not returned.
	fn try_substitute(&self, path: &StorePath) -> Result<bool> {
		match self.substitute_attempt(path)? {
			Ok(substituted) => Ok(substituted),
			Err(error) if !self.settings.try_fallback => Err(error.into()),
			Err(e) => {
				tracing::debug!("cannot substitute '{}': {e}", print(path));
				Ok(false)
			}
		}
	}

	/// Substitute `path` if a substituter has it. The inner error is a
	/// failed substitution, which `PathSubstitutionGoal` reports as
	/// `ecFailed`; the outer one any other failure.
	fn substitute_attempt(
		&self,
		path: &StorePath,
	) -> Result<std::result::Result<bool, noxi_store::Error>> {
		if !self.settings.use_substitutes || self.substituters.is_empty() {
			return Ok(Ok(false));
		}
		let mut available = false;
		for cache in self.substituters {
			match cache.query_path_info(path) {
				Ok(Some(_)) => {
					available = true;
					break;
				}
				Ok(None) => {}
				Err(error) if !self.settings.try_fallback => return Err(error.into()),
				Err(error) => tracing::debug!("cannot query substitute '{}': {error}", print(path)),
			}
		}
		if !available {
			return Ok(Ok(false));
		}
		let substituted = if self.repair() {
			self.store.substitute_repair(self.substituters, path)
		} else {
			self.store.substitute(self.substituters, path)
		};
		Ok(match substituted {
			Ok(()) => {
				if self.repair() {
					self.checked().insert(path.clone(), true);
				}
				Ok(true)
			}
			// No substituter could provide it after all, without failing
			// (a collected NAR, say): the derivation is built instead.
			Err(noxi_store::Error::NoSubstituters(_)) => Ok(false),
			Err(error) => Err(error),
		})
	}

	/// Whether the derivation's options allow substituting its outputs.
	fn substitutes_allowed(&self, drv: &UnverifiedDerivation) -> Result<bool> {
		let parsed = options::structured_attrs(drv.environment())?;
		let env: BTreeMap<BString, BString> = drv
			.environment()
			.iter()
			.filter(|(k, _)| k.as_slice() != b"__json")
			.map(|(k, v)| (k.clone(), v.clone()))
			.collect();
		let options = DerivationOptions::new(
			&env,
			parsed.as_ref(),
			drv.input_derivations(),
			false,
			&mut Vec::new(),
		)?;
		Ok(options.substitutes_allowed(self.settings.always_allow_substitutes))
	}

	/// Realise every output of the derivation at `drv_path`, building its
	/// input derivations first, and return its output paths. Builder output
	/// is passed to `log`.
	pub fn realise(
		&self,
		drv_path: &StorePath,
		log: &mut dyn FnMut(&[u8]),
	) -> Result<BTreeMap<String, StorePath>> {
		self.realise_with_events(drv_path, None, &mut |event| {
			if let BuildEvent::Output(data) = event {
				log(data)
			}
		})
	}

	/// Realise the outputs named in `wanted` (`drv^out,dev`) and return the
	/// paths of all the derivation's outputs. Input derivations only have
	/// the outputs this derivation uses realised, as in CppNix.
	pub fn realise_outputs(
		&self,
		drv_path: &StorePath,
		wanted: &BTreeSet<String>,
		log: &mut dyn FnMut(&[u8]),
	) -> Result<BTreeMap<String, StorePath>> {
		self.realise_with_events(drv_path, Some(wanted), &mut |event| {
			if let BuildEvent::Output(data) = event {
				log(data)
			}
		})
	}

	/// Realise `wanted` outputs (all if `None`), reporting each build's
	/// start, output and end to `events`.
	pub fn realise_with_events(
		&self,
		drv_path: &StorePath,
		wanted: Option<&BTreeSet<String>>,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<BTreeMap<String, StorePath>> {
		self.check_cancelled()?;
		if self.builds_in_parallel() {
			let mut batch = self.realise_many_with_events(
				&[(drv_path.clone(), wanted.cloned())],
				&mut |_, event| events(event),
			)?;
			if let Some(outputs) = batch.outputs.remove(drv_path) {
				return Ok(outputs);
			}
			let status = batch
				.failures
				.values()
				.find(|error| {
					matches!(
						error.status(),
						FailureStatus::PermanentFailure
							| FailureStatus::InputRejected
							| FailureStatus::TimedOut
							| FailureStatus::HashMismatch
					)
				})
				.map(BuildError::status);
			let error = batch
				.failures
				.remove(drv_path)
				.unwrap_or(BuildError::Cancelled);
			return Err(match status {
				Some(status) if error.status() == FailureStatus::DependencyFailed => {
					BuildError::failed(status, error.to_string())
				}
				_ => error,
			});
		}
		let mut done = BTreeMap::new();
		self.realise_inner(drv_path, wanted, &mut done, &mut Vec::new(), events)
	}

	fn realise_inner(
		&self,
		drv_path: &StorePath,
		wanted: Option<&BTreeSet<String>>,
		done: &mut BTreeMap<StorePath, BTreeSet<String>>,
		stack: &mut Vec<StorePath>,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<BTreeMap<String, StorePath>> {
		self.realise_drv(drv_path, None, wanted, done, stack, events)
	}

	/// Realise `wanted` outputs of the derivation at `drv_path`. A
	/// `resolved` derivation is not in the store yet: like CppNix's
	/// `DerivationGoal` with `storeDerivation`, it is written only when it
	/// has to be built.
	fn realise_drv(
		&self,
		drv_path: &StorePath,
		resolved: Option<BuildDerivation>,
		wanted: Option<&BTreeSet<String>>,
		done: &mut BTreeMap<StorePath, BTreeSet<String>>,
		stack: &mut Vec<StorePath>,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<BTreeMap<String, StorePath>> {
		self.check_cancelled()?;
		if stack.contains(drv_path) {
			return Err(BuildError::Other(format!(
				"cycle detected in the input derivations of '{}'",
				print(drv_path)
			)));
		}
		// `DerivationBuildingGoal`: the derivation stays alive while it is
		// built, even when nothing else roots it.
		self.store.add_temp_root(drv_path)?;
		let supplied = resolved.is_some();
		let build_drv = match resolved {
			Some(drv) => drv,
			None => self.store.read_build_derivation(drv_path)?,
		};
		let drv = &build_drv.basic;
		let impure = build_drv.is_impure();
		if impure {
			if let Some(outputs) = self.impure().get(drv_path) {
				return Ok(outputs.clone());
			}
			// `DerivationGoal::haveDerivation`.
			if !self.store.config().ca_derivations {
				return Err(BuildError::Other("experimental Nix feature 'ca-derivations' is disabled; add '--extra-experimental-features ca-derivations' to enable it".to_owned()));
			}
		} else if let Some(error) = self.impure_input(drv_path, &build_drv)? {
			let names: BTreeSet<String> = match wanted {
				Some(names) => names.clone(),
				None => drv.outputs().names().map(ToString::to_string).collect(),
			};
			if self.mode != BuildMode::Normal || !self.have_outputs(drv_path, &build_drv, &names)? {
				return Err(error);
			}
		}
		if let Some(wanted) = wanted
			&& let Some(missing) = wanted.iter().find(|name| {
				!drv.outputs()
					.names()
					.any(|n| n.as_bytes() == name.as_bytes())
			}) {
			return Err(BuildError::Other(format!(
				"derivation '{}' does not have wanted outputs '{missing}'",
				print(drv_path)
			)));
		}
		// Outputs whose paths are unknown until they are realised are
		// first looked up in the substituters' build traces. Repair checks
		// their contents on the paths below instead.
		if !self.repair() && has_unknown_outputs(&build_drv) {
			let names: BTreeSet<String> = match wanted {
				Some(names) => names.clone(),
				None => drv.outputs().names().map(ToString::to_string).collect(),
			};
			if let Some(paths) = self.substitute_unknown_outputs(drv_path, &build_drv, &names)? {
				if self.mode != BuildMode::Check {
					done.entry(drv_path.clone()).or_default().extend(names);
					return Ok(paths);
				}
			} else if self.mode == BuildMode::Check {
				return Err(self.cannot_check(drv_path));
			}
		}
		// CppNix resolves deferred, floating and feature-enabled fixed
		// derivations by replacing all input derivations with source paths.
		if !drv.input_derivations().is_empty()
			&& if build_drv.is_fixed() {
				self.store.config().ca_derivations || !build_drv.dynamic.is_empty()
			} else {
				drv.outputs().is_deferred() || !build_drv.dynamic.is_empty()
			} {
			// A fixed output that is valid needs none of its inputs.
			if build_drv.is_fixed() && self.mode == BuildMode::Normal {
				let outputs = self.known_outputs(drv_path, &build_drv)?;
				if self.all_valid(&outputs)?
					|| self.substitute_known(drv_path, &build_drv, &outputs)?
				{
					let paths = outputs
						.into_iter()
						.map(|(name, o)| (name, o.path))
						.collect();
					self.register_realisations(drv_path, &paths)?;
					return Ok(paths);
				}
			}
			stack.push(drv_path.clone());
			let result = self.resolve_inputs(drv_path, &build_drv, wanted, done, stack, events);
			stack.pop();
			return result;
		}
		let outputs = self.known_outputs(drv_path, &build_drv)?;
		let all_paths = |outputs: &BTreeMap<String, KnownOutput>| -> BTreeMap<String, StorePath> {
			outputs
				.iter()
				.map(|(name, output)| (name.clone(), output.path.clone()))
				.collect()
		};
		let wanted: BTreeSet<String> = match wanted {
			Some(names) => {
				if let Some(missing) = names.iter().find(|n| !outputs.contains_key(*n)) {
					return Err(BuildError::Other(format!(
						"derivation '{}' does not have wanted outputs '{missing}'",
						print(drv_path)
					)));
				}
				names.clone()
			}
			None => outputs.keys().cloned().collect(),
		};
		if done
			.get(drv_path)
			.is_some_and(|ensured| wanted.is_subset(ensured))
		{
			return Ok(all_paths(&outputs));
		}
		let wanted_outputs: BTreeMap<String, KnownOutput> = outputs
			.iter()
			.filter(|(name, _)| wanted.contains(*name))
			.map(|(name, output)| (name.clone(), output.clone()))
			.collect();

		// Like `DerivationGoal`, substitute missing outputs before
		// realising any inputs, which then may not be needed at all.
		self.report_checks(&wanted_outputs, events)?;
		self.substitute_known(drv_path, &build_drv, &wanted_outputs)?;
		// An impure derivation is always built, and never checked.
		let check = self.mode == BuildMode::Check && !impure;
		let valid = self.all_valid(&wanted_outputs)?;
		if check && !valid {
			return Err(self.cannot_check(drv_path));
		}
		if self.repair() && valid {
			self.repair_closure(drv_path, &outputs, &wanted_outputs, stack, events)?;
		} else if check || !valid {
			for src in drv.input_sources() {
				if !self.store.is_valid_path(src)? && !self.try_substitute(src)? {
					return Err(BuildError::failed(
						FailureStatus::DependencyFailed,
						format!(
							"path '{}' is required, but there is no substituter that can build it",
							print(src)
						),
					));
				}
			}

			stack.push(drv_path.clone());
			for (input, names) in drv.input_derivations() {
				let names: BTreeSet<String> = names.iter().map(|n| n.to_string()).collect();
				self.for_inputs()
					.realise_inner(input, Some(&names), done, stack, events)?;
			}
			stack.pop();

			if check || !self.all_valid(&wanted_outputs)? {
				if supplied && !self.store.is_valid_path(drv_path)? {
					self.write_resolved(drv_path, &build_drv)?;
				}
				let result = self.build(drv_path, &build_drv, &outputs, events);
				events(BuildEvent::Finished {
					drv_path,
					error: result.as_ref().err(),
				});
				let built = result?;
				if impure {
					if self.mode == BuildMode::Check {
						// `assertPathValidity`: no impure output is valid.
						return Err(BuildError::Other(
							"some outputs are unexpectedly invalid".to_owned(),
						));
					}
					self.impure().insert(drv_path.clone(), built.clone());
					return Ok(built);
				}
			}
		}
		done.entry(drv_path.clone()).or_default().extend(wanted);
		let paths = all_paths(&self.known_outputs(drv_path, &build_drv)?);
		self.register_realisations(drv_path, &paths)?;
		Ok(paths)
	}

	/// The outputs of the impure derivations this request built.
	pub(crate) fn impure_outputs(&self) -> BTreeMap<StorePath, BTreeMap<String, StorePath>> {
		self.impure().clone()
	}

	fn impure(
		&self,
	) -> std::sync::MutexGuard<'_, BTreeMap<StorePath, BTreeMap<String, StorePath>>> {
		self.impure.lock().unwrap_or_else(|e| e.into_inner())
	}

	/// The error for building a pure derivation that uses an impure one.
	pub(crate) fn impure_input(
		&self,
		drv_path: &StorePath,
		drv: &BuildDerivation,
	) -> Result<Option<BuildError>> {
		if !self.store.config().impure_derivations || drv.is_impure() || drv.is_fixed() {
			return Ok(None);
		}
		for input in drv.basic.input_derivations().keys() {
			if self.store.is_valid_path(input)?
				&& self.store.read_build_derivation(input)?.is_impure()
			{
				return Ok(Some(BuildError::Other(format!(
					"pure derivation '{}' depends on impure derivation '{}'",
					print(drv_path),
					print(input)
				))));
			}
		}
		Ok(None)
	}

	/// Whether the outputs `names` of a derivation are valid, after
	/// substituting what may be substituted of them.
	fn have_outputs(
		&self,
		drv_path: &StorePath,
		drv: &BuildDerivation,
		names: &BTreeSet<String>,
	) -> Result<bool> {
		if drv.has_floating_outputs() || drv.is_deferred() {
			return Ok(self
				.substitute_unknown_outputs(drv_path, drv, names)?
				.is_some());
		}
		let outputs: BTreeMap<_, _> = self
			.known_outputs(drv_path, drv)?
			.into_iter()
			.filter(|(name, _)| names.contains(name))
			.collect();
		self.substitute_known(drv_path, drv, &outputs)
	}

	/// Substitute the outputs with known paths, if they may be and are not
	/// valid yet. Returns whether all outputs are valid afterwards.
	///
	/// As in `DerivationGoal::haveDerivation`, a failed substitution is
	/// logged as its `PathSubstitutionGoal` logs it, and unless `fallback`
	/// is set the derivation then fails instead of being built.
	fn substitute_known(
		&self,
		drv_path: &StorePath,
		drv: &BuildDerivation,
		outputs: &BTreeMap<String, KnownOutput>,
	) -> Result<bool> {
		if self.settings.use_substitutes
			&& !self.substituters.is_empty()
			&& (self.repair() || !self.all_valid(outputs)?)
			&& self.substitutes_allowed(&drv.basic)?
		{
			let mut failed = false;
			// Floating outputs were looked up by their realisations.
			for output in outputs.values().filter(|o| o.floating.is_none()) {
				if (self.repair() || !self.store.is_valid_path(&output.path)?)
					&& let Err(error) = self.substitute_attempt(&output.path)?
				{
					crate::backend::log_error(&error.to_string());
					failed = true;
				}
			}
			if failed && !self.settings.try_fallback {
				return Err(BuildError::failed(
					FailureStatus::TransientFailure,
					format!(
						"some substitutes for the outputs of derivation '{}' failed (usually happens due to networking issues); try '--fallback' to build derivation from source ",
						print(drv_path)
					),
				));
			}
		}
		self.all_valid(outputs)
	}

	fn register_realisations(
		&self,
		drv_path: &StorePath,
		paths: &BTreeMap<String, StorePath>,
	) -> Result<()> {
		if self.store.config().ca_derivations {
			for (name, path) in paths {
				if self.store.is_valid_path(path)? {
					self.store.register_drv_output(
						drv_path,
						name,
						&Realisation {
							out_path: path.clone(),
							signatures: BTreeSet::new(),
						},
					)?;
				}
			}
		}
		Ok(())
	}

	/// `DerivationGoal` after `DerivationResolutionGoal`: realise the
	/// inputs, resolve the derivation and realise the `wanted` outputs of
	/// the resolved one, which may then be substituted output by output.
	fn resolve_inputs(
		&self,
		drv_path: &StorePath,
		drv: &BuildDerivation,
		wanted: Option<&BTreeSet<String>>,
		done: &mut BTreeMap<StorePath, BTreeSet<String>>,
		stack: &mut Vec<StorePath>,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<BTreeMap<String, StorePath>> {
		if !self.store.config().ca_derivations {
			return Err(BuildError::Other("experimental Nix feature 'ca-derivations' is disabled; add '--extra-experimental-features ca-derivations' to enable it".to_owned()));
		}
		let (path, resolved) = resolve_derivation(drv_path, drv, |input, wanted| {
			self.for_inputs()
				.realise_inner(input, Some(wanted), done, stack, events)
				.map(Some)
		})?
		.expect("realised inputs resolve");
		// `DerivationResolutionGoal`'s activity lasts while the resolved
		// derivation is realised.
		let _act = nix_compat::log::logger::Activity::new(
			nix_compat::log::VerbosityLevel::Info,
			nix_compat::log::ActivityType::BuildWaiting,
			format!(
				"resolved derivation: '{}' -> '{}'",
				print(drv_path),
				print(&path)
			),
			vec![print(drv_path).into(), print(&path).into()],
			0,
		);
		let impure = resolved.is_impure();
		let resolved = (!self.store.is_valid_path(&path)?).then_some(resolved);
		let outputs = match self.realise_drv(&path, resolved, wanted, done, stack, events) {
			Err(BuildError::Failed { status, message }) => {
				crate::backend::log_error(&message);
				return Err(BuildError::failed(
					status,
					format!("build of resolved derivation '{}' failed", print(&path)),
				));
			}
			outputs => outputs?,
		};
		if impure {
			self.impure().insert(drv_path.clone(), outputs.clone());
		}
		Ok(outputs)
	}

	/// `writeDerivation` for a resolved derivation about to be built.
	fn write_resolved(&self, drv_path: &StorePath, drv: &BuildDerivation) -> Result<()> {
		let name = drv_path
			.name()
			.strip_suffix(".drv")
			.unwrap_or(drv_path.name());
		let written = self.store.add_text_to_store(
			&format!("{name}.drv"),
			&drv.to_aterm_bytes(),
			drv.basic.input_sources(),
			false,
		)?;
		if &written != drv_path {
			return Err(BuildError::Other(format!(
				"resolved derivation was written to '{}', not '{}'",
				print(&written),
				print(drv_path)
			)));
		}
		Ok(())
	}

	/// `DerivationGoal::haveDerivation` for outputs with unknown paths: a
	/// `DrvOutputSubstitutionGoal` for each wanted output without a valid
	/// local realisation, then the output path's substitution and
	/// `registerDrvOutput`. Returns the realised paths of the outputs if
	/// every wanted output is now valid.
	pub(crate) fn substitute_unknown_outputs(
		&self,
		drv_path: &StorePath,
		drv: &BuildDerivation,
		wanted: &BTreeSet<String>,
	) -> Result<Option<BTreeMap<String, StorePath>>> {
		if !self.store.config().ca_derivations {
			return Ok(None);
		}
		let usable = self.settings.use_substitutes
			&& !self.substituters.is_empty()
			&& self.substitutes_allowed(&drv.basic)?;
		for name in wanted {
			match self.store.query_realisation(drv_path, name)? {
				Some(real) if self.store.is_valid_path(&real.out_path)? => {}
				Some(real) => {
					if !usable || !self.try_substitute(&real.out_path)? {
						return Ok(None);
					}
				}
				None => {
					if !usable || !self.try_substitute_realisation(drv_path, name)? {
						return Ok(None);
					}
				}
			}
		}
		let mut paths = BTreeMap::new();
		for name in drv.basic.outputs().names() {
			let name = name.to_string();
			if let Some(real) = self.store.query_realisation(drv_path, &name)? {
				paths.insert(name, real.out_path);
			}
		}
		Ok(Some(paths))
	}

	/// `DrvOutputSubstitutionGoal`: find output `output` of `drv_path` in a
	/// substituter's build trace, substitute its path and register the
	/// realisation with the substituter's signatures, as the worker's
	/// `registerDrvOutput` does without checking them.
	fn try_substitute_realisation(&self, drv_path: &StorePath, output: &str) -> Result<bool> {
		for cache in self.substituters {
			let realisation = match cache.query_realisation(drv_path, output) {
				Ok(Some(realisation)) => realisation,
				Ok(None) => continue,
				Err(error) if !self.settings.try_fallback => return Err(error.into()),
				Err(error) => {
					tracing::debug!(
						"cannot query the realisation of '{}^{output}': {error}",
						print(drv_path)
					);
					continue;
				}
			};
			if !self.store.is_valid_path(&realisation.out_path)?
				&& !self.try_substitute(&realisation.out_path)?
			{
				return Ok(false);
			}
			self.store
				.register_drv_output(drv_path, output, &realisation)?;
			return Ok(true);
		}
		Ok(false)
	}
}

/// Whether some outputs of `drv` have no path until they are realised:
/// floating content-addressed outputs, or input-addressed outputs that wait
/// for content-addressed inputs.
pub(crate) fn has_unknown_outputs(drv: &BuildDerivation) -> bool {
	!drv.is_impure() && (drv.has_floating_outputs() || drv.is_deferred())
}

/// `Derivation::tryResolve`: replace each input derivation of `drv` with
/// the paths of the outputs it uses, given by `input_outputs`, and rewrite
/// their placeholders. Deferred input-addressed outputs get their paths.
/// Returns the resolved derivation and the path `writeDerivation` would
/// give it, without writing it, or `None` if an input output is unknown.
pub fn resolve_derivation(
	drv_path: &StorePath,
	drv: &BuildDerivation,
	mut input_outputs: impl FnMut(
		&StorePath,
		&BTreeSet<String>,
	) -> Result<Option<BTreeMap<String, StorePath>>>,
) -> Result<Option<(StorePath, BuildDerivation)>> {
	let (mut builder, mut outputs) = drv.basic.clone().into_parts();
	builder.environment = drv.basic.environment().clone();
	let mut rewrites = BTreeMap::new();
	for (input, names) in drv.basic.input_derivations() {
		if names.is_empty() {
			continue;
		}
		let wanted = names.iter().map(ToString::to_string).collect();
		let Some(paths) = input_outputs(input, &wanted)? else {
			return Ok(None);
		};
		for name in names {
			let path = paths.get(&name.to_string()).ok_or_else(|| {
				BuildError::Other(format!(
					"derivation '{}' has no output '{name}'",
					print(input)
				))
			})?;
			builder.input_sources.insert(path.clone());
			rewrites.insert(downstream_placeholder(input, name.as_ref()), print(path));
		}
	}
	for (input, children) in &drv.dynamic {
		let resolved = resolve_dynamic_inputs(
			input,
			children,
			&|name| downstream_placeholder(input, name),
			&mut input_outputs,
			&mut builder.input_sources,
			&mut rewrites,
		);
		match resolved {
			Ok(true) => {}
			Ok(false) => return Ok(None),
			Err(BuildError::Failed { status, message }) => {
				crate::backend::log_error(&message);
				let mut message = format!(
					"Cannot build '{}'.\nReason: 1 dependency failed.",
					print(drv_path)
				);
				let known: BTreeSet<String> = drv
					.basic
					.outputs()
					.store_paths()
					.map(print)
					.chain(
						drv.experimental
							.values()
							.filter_map(|output| output.fixed.as_ref())
							.map(|(path, _)| print(path)),
					)
					.collect();
				if !known.is_empty() {
					message += "\nOutput paths:";
				}
				for path in known {
					message += &format!("\n  {path}");
				}
				return Err(BuildError::failed(status, message));
			}
			Err(error) => return Err(error),
		}
	}
	builder.input_derivations.clear();
	builder.command = rewrite_strings(builder.command.to_vec(), &rewrites).into();
	builder.arguments = builder
		.arguments
		.into_iter()
		.map(|s| rewrite_strings(s.to_vec(), &rewrites).into())
		.collect();
	builder.environment = builder
		.environment
		.into_iter()
		.map(|(k, v)| {
			(
				rewrite_strings(k.to_vec(), &rewrites).into(),
				rewrite_strings(v.to_vec(), &rewrites).into(),
			)
		})
		.collect();
	let name = drv_path
		.name()
		.strip_suffix(".drv")
		.unwrap_or(drv_path.name());
	if outputs.is_deferred() && drv.floating.is_empty() && drv.experimental.is_empty() {
		outputs = builder
			.calculate_outputs(name, lookup_fn(|_| None))
			.map_err(|e| BuildError::Other(e.to_string()))?;
	}
	let references = builder.input_sources.clone();
	let basic = builder
		.build_resolved(outputs)
		.map_err(|e| BuildError::Other(e.to_string()))?;
	let resolved = BuildDerivation {
		basic,
		floating: drv.floating.clone(),
		experimental: drv.experimental.clone(),
		dynamic: BTreeMap::new(),
	};
	let path = store_path::build_text_path(
		&format!("{name}.drv"),
		resolved.to_aterm_bytes(),
		references.iter().map(StorePath::as_ref),
	)
	.map_err(|e| BuildError::Other(e.to_string()))?
	.to_owned();
	Ok(Some((path, resolved)))
}

/// `tryResolveInput` for the derivations that the outputs in `children` of the
/// derivation at `producer` are.
fn resolve_dynamic_inputs(
	producer: &StorePath,
	children: &BTreeMap<OutputName, InputNode>,
	placeholder: &dyn Fn(&[u8]) -> String,
	input_outputs: &mut dyn FnMut(
		&StorePath,
		&BTreeSet<String>,
	) -> Result<Option<BTreeMap<String, StorePath>>>,
	input_sources: &mut BTreeSet<StorePath>,
	rewrites: &mut BTreeMap<String, String>,
) -> Result<bool> {
	for (output, child) in children {
		let mut outputs_of =
			|drv: &StorePath, names: &BTreeSet<String>| match input_outputs(drv, names) {
				Ok(None) => Ok(None),
				Ok(Some(paths)) => names
					.iter()
					.map(|name| {
						paths
							.get(name)
							.map(|path| (name.clone(), path.clone()))
							.ok_or_else(|| {
								BuildError::Other(format!(
									"derivation '{}' has no output '{name}'",
									print(drv)
								))
							})
					})
					.collect::<Result<BTreeMap<_, _>>>()
					.map(Some),
				Err(error) => Err(error),
			};
		let name = output.to_string();
		let mut built = match outputs_of(producer, &BTreeSet::from([name.clone()])) {
			Ok(Some(built)) => built,
			Ok(None) => return Ok(false),
			Err(BuildError::Failed { status, message }) => {
				crate::backend::log_error(&message);
				return Err(BuildError::failed(
					status,
					format!(
						"failed to obtain derivation of '{}^{name}'",
						print(producer)
					),
				));
			}
			Err(error) => return Err(error),
		};
		let drv = built.remove(&name).expect("the output was asked for");
		let parent = placeholder(output.as_bytes());
		let wanted: BTreeSet<String> = child.outputs.iter().map(ToString::to_string).collect();
		if !wanted.is_empty() {
			let Some(paths) = outputs_of(&drv, &wanted)? else {
				return Ok(false);
			};
			for (name, path) in paths {
				rewrites.insert(
					store_path::computed_placeholder(&parent, name.as_bytes()),
					print(&path),
				);
				input_sources.insert(path);
			}
		}
		if !resolve_dynamic_inputs(
			&drv,
			&child.dynamic_outputs,
			&|name| store_path::computed_placeholder(&parent, name),
			input_outputs,
			input_sources,
			rewrites,
		)? {
			return Ok(false);
		}
	}
	Ok(true)
}

/// `deepQueryPartialDerivationOutput`: the path of output `output` of
/// `drv_path` from the store's output map or realisations, resolving the
/// derivation through its inputs' outputs when it has no realisation of
/// its own. Every realisation the query reads is passed to `realisation`,
/// as `toRealisedPaths` collects them for `nix copy`.
pub fn deep_query_output(
	store: &dyn Store,
	drv_path: &StorePath,
	output: &str,
	realisation: &mut dyn FnMut(&StorePath, &str, &Realisation),
) -> Result<Option<StorePath>> {
	// A statically known path needs no realisation.
	if store.ca_derivations()
		&& store
			.read_build_derivation(drv_path)?
			.basic
			.outputs()
			.is_deferred()
		&& let Some(found) = store.query_realisation(drv_path, output)?
	{
		realisation(drv_path, output, &found);
		return Ok(Some(found.out_path));
	}
	if let Some(Some(path)) = store
		.query_partial_derivation_output_map(drv_path)?
		.remove(output)
	{
		return Ok(Some(path));
	}
	if !store.ca_derivations() {
		return Ok(None);
	}
	let drv = store.read_build_derivation(drv_path)?;
	if drv.basic.input_derivations().is_empty() {
		return Ok(None);
	}
	let resolved = resolve_derivation(drv_path, &drv, |input, wanted| {
		if !store.is_valid_path(input)? {
			return Ok(None);
		}
		let mut paths = BTreeMap::new();
		for name in wanted {
			match deep_query_output(store, input, name, realisation)? {
				Some(path) => {
					paths.insert(name.clone(), path);
				}
				None => return Ok(None),
			}
		}
		Ok(Some(paths))
	})?;
	let Some((resolved_path, _)) = resolved else {
		return Ok(None);
	};
	match store.query_realisation(&resolved_path, output)? {
		Some(found) => {
			realisation(&resolved_path, output, &found);
			Ok(Some(found.out_path))
		}
		None => Ok(None),
	}
}

impl LocalBuilder<'_> {
	fn all_valid(&self, outputs: &BTreeMap<String, KnownOutput>) -> Result<bool> {
		for output in outputs.values() {
			if !self.path_valid(&output.path)? {
				return Ok(false);
			}
		}
		Ok(true)
	}

	/// `pathContentsGood` for the outputs a repair is about to look at.
	fn report_checks(
		&self,
		outputs: &BTreeMap<String, KnownOutput>,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<()> {
		if self.repair() {
			for output in outputs.values() {
				if self.store.is_valid_path(&output.path)? {
					self.contents_good(&output.path, events)?;
				}
			}
		}
		Ok(())
	}

	/// Whether a valid path's contents are, reporting the first check.
	fn contents_good(
		&self,
		path: &StorePath,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<bool> {
		let known = self.checked().get(path).copied();
		if let Some(good) = known {
			return Ok(good);
		}
		let valid = self.path_valid(path)?;
		events(BuildEvent::PathChecked { path, valid });
		Ok(valid)
	}

	fn checked(&self) -> std::sync::MutexGuard<'_, BTreeMap<StorePath, bool>> {
		self.checked.lock().unwrap_or_else(|e| e.into_inner())
	}

	fn path_valid(&self, path: &StorePath) -> Result<bool> {
		let Some(info) = self.store.query_path_info(path)? else {
			return Ok(false);
		};
		if !self.repair() {
			return Ok(true);
		}
		let known = self.checked().get(path).copied();
		if let Some(good) = known {
			return Ok(good);
		}
		let good = nar::hash_path(&self.store.real_path(path))
			.is_ok_and(|(hash, size)| info.nar_hash == hash && info.nar_size == size);
		self.checked().insert(path.clone(), good);
		Ok(good)
	}

	/// `Store::repairPath`: restore a path from a substituter, or else by
	/// rebuilding the derivation that produced it.
	pub fn repair_path(
		&self,
		path: &StorePath,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<()> {
		let repairer = Self {
			mode: BuildMode::Repair,
			..self.clone()
		};
		if repairer.settings.use_substitutes {
			match repairer
				.store
				.substitute_repair(repairer.substituters, path)
			{
				Ok(()) => return Ok(()),
				Err(error) => tracing::debug!("cannot substitute '{}': {error}", print(path)),
			}
		}
		let info = repairer
			.store
			.query_path_info(path)?
			.ok_or_else(|| BuildError::Other(format!("path '{}' is not valid", print(path))))?;
		match &info.deriver {
			Some(deriver) if repairer.store.is_valid_path(deriver)? => repairer
				.realise_with_events(deriver, None, events)
				.map(drop),
			_ => Err(BuildError::Other(format!(
				"cannot repair path '{}'",
				print(path)
			))),
		}
	}

	/// `DerivationGoal::repairClosure`: the wanted outputs are good, so check
	/// the rest of their closure, and repair damaged paths by rebuilding the
	/// input derivations that produced them.
	fn repair_closure(
		&self,
		drv_path: &StorePath,
		outputs: &BTreeMap<String, KnownOutput>,
		wanted: &BTreeMap<String, KnownOutput>,
		stack: &mut Vec<StorePath>,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<()> {
		let mut closure = BTreeSet::new();
		for output in wanted.values() {
			self.closure(&output.path, &mut closure)?;
		}
		for output in outputs.values() {
			closure.remove(&output.path);
		}
		let mut producers: Option<BTreeMap<StorePath, StorePath>> = None;
		let mut failed = false;
		for path in &closure {
			if self.contents_good(path, events)? {
				continue;
			}
			crate::backend::log_error_line(&format!(
				"found corrupted or missing path '{}' in the output closure of '{}'",
				print(path),
				print(drv_path)
			));
			if producers.is_none() {
				let mut map = BTreeMap::new();
				let mut inputs = BTreeSet::new();
				if self.store.is_valid_path(drv_path)? {
					self.closure(drv_path, &mut inputs)?;
				}
				for input in inputs.iter().filter(|p| p.name().ends_with(".drv")) {
					for output in self
						.store
						.query_partial_derivation_output_map(input)?
						.into_values()
						.flatten()
					{
						map.insert(output, input.clone());
					}
				}
				producers = Some(map);
			}
			let repaired = match producers.as_ref().and_then(|map| map.get(path)) {
				Some(drv) if !stack.contains(drv) => {
					let mut done_inputs = BTreeMap::new();
					stack.push(drv_path.clone());
					let result = self.realise_inner(drv, None, &mut done_inputs, stack, events);
					stack.pop();
					result.map(drop)
				}
				_ => self
					.store
					.substitute_repair(self.substituters, path)
					.map(|()| {
						self.checked().insert(path.clone(), true);
					})
					.map_err(BuildError::from),
			};
			if let Err(error) = repaired {
				crate::backend::log_error(&error.to_string());
				failed = true;
			}
		}
		if failed {
			return Err(BuildError::Other(format!(
				"some paths in the output closure of derivation '{}' could not be repaired",
				print(drv_path)
			)));
		}
		Ok(())
	}

	/// `Store::computeFSClosure`.
	fn closure(&self, path: &StorePath, out: &mut BTreeSet<StorePath>) -> Result<()> {
		let mut pending = vec![path.clone()];
		while let Some(path) = pending.pop() {
			if out.contains(&path) {
				continue;
			}
			let info = self.store.query_path_info(&path)?.ok_or_else(|| {
				BuildError::Other(format!("path '{}' is not valid", print(&path)))
			})?;
			pending.extend(info.references.iter().cloned());
			out.insert(path);
		}
		Ok(())
	}

	/// Whether derivations that do not depend on each other are built at
	/// once. Builds on other machines need no local build slot.
	pub(crate) fn builds_in_parallel(&self) -> bool {
		self.settings.max_build_jobs > 1
			|| self.settings.keep_going
			|| self.hooks.as_ref().is_some_and(|hooks| hooks.may_accept)
	}

	/// The store's `system-features`: `getDefaultSystemFeatures`, unless the
	/// store was opened with some of its own.
	fn available_features(&self) -> BTreeSet<&str> {
		let mut features: BTreeSet<&str> = self
			.settings
			.system_features
			.iter()
			.map(String::as_str)
			.collect();
		if !self.settings.exact_system_features {
			if self.store.config().ca_derivations {
				features.insert("ca-derivations");
			}
			if self
				.settings
				.config
				.experimental_features()
				.contains("recursive-nix")
			{
				features.insert("recursive-nix");
			}
		}
		features
	}

	/// Check that this machine can build `drv`: `LocalBuildRejection`.
	fn check_platform(
		&self,
		drv_path: &StorePath,
		drv: &UnverifiedDerivation,
		features: &BTreeSet<String>,
	) -> Result<()> {
		let system = drv.system().to_str_lossy();
		let builtin = drv.command().starts_with(b"builtin:");
		let bad_platform = !builtin
			&& system != self.settings.system
			&& !self.settings.extra_platforms.contains(system.as_ref());
		let available = self.available_features();
		let missing: Vec<&str> = features
			.iter()
			.filter(|f| !available.contains(f.as_str()))
			.map(String::as_str)
			.collect();
		let max_jobs_zero = self.settings.max_build_jobs == 0;
		if !max_jobs_zero && !bad_platform && missing.is_empty() {
			return Ok(());
		}
		let mut msg = format!("Cannot build '{}'.", print(drv_path));
		if max_jobs_zero {
			msg += "\nReason: local builds are disabled (max-jobs = 0)\nHint: set 'max-jobs' to a non-zero value to enable local builds, or configure remote builders via 'builders'";
		}
		if bad_platform {
			msg += &format!(
				"\nReason: platform mismatch\nRequired system: '{system}'\nCurrent system: '{}'",
				self.settings.system
			);
		}
		if !missing.is_empty() {
			msg += &format!(
				"\nReason: missing system features\nRequired features: {{{}}}\nAvailable features: {{{}}}",
				features
					.iter()
					.map(String::as_str)
					.collect::<Vec<_>>()
					.join(", "),
				available.into_iter().collect::<Vec<_>>().join(", ")
			);
		}
		Err(BuildError::failed(FailureStatus::InputRejected, msg))
	}

	fn build(
		&self,
		drv_path: &StorePath,
		build_drv: &BuildDerivation,
		outputs: &BTreeMap<String, KnownOutput>,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<BTreeMap<String, StorePath>> {
		let drv = &build_drv.basic;
		let kind = BuildKind::of(build_drv);
		let parsed = options::structured_attrs(drv.environment())?;
		let env: BTreeMap<BString, BString> = drv
			.environment()
			.iter()
			.filter(|(k, _)| k.as_slice() != b"__json")
			.map(|(k, v)| (k.clone(), v.clone()))
			.collect();
		let mut warnings = Vec::new();
		let drv_options = DerivationOptions::new(
			&env,
			parsed.as_ref(),
			drv.input_derivations(),
			false,
			&mut warnings,
		)?
		.try_resolve(|_, _| None)
		.ok_or_else(|| BuildError::Other("derivation options name unbuilt inputs".to_owned()))?;
		for warning in warnings {
			tracing::warn!("{warning}");
		}

		let features = drv_options.get_required_system_features(has_known_output_paths(build_drv));
		// `findExternalDerivationBuilderIfSupported`: an external builder for
		// the derivation's system is preferred to a local build, and the
		// platform, features and `max-jobs` are not checked for it.
		let system = drv.system().to_str_lossy();
		let external = self
			.settings
			.external_builders
			.iter()
			.find(|builder| builder.systems.contains(system.as_ref()));
		let mut rejection = match external {
			Some(_) => None,
			None => self.check_platform(drv_path, drv, &features).err(),
		};
		// Only an untouched store's derivations can be built elsewhere:
		// checks and repairs are about this store.
		let hooks = match &self.hooks {
			Some(hooks) if self.mode == BuildMode::Normal => {
				self.store.is_valid_path(drv_path)?.then_some(hooks)
			}
			_ => None,
		};
		let prefer_local = drv_options.prefer_local_build;

		// The closure of the input sources and of the input derivations'
		// requested outputs.
		let mut input_paths = BTreeSet::new();
		for (input_drv, names) in drv.input_derivations() {
			let input = self.store.query_partial_derivation_output_map(input_drv)?;
			for name in names {
				let path = input
					.get(&name.to_string())
					.and_then(Option::as_ref)
					.ok_or_else(|| {
						BuildError::Other(format!(
							"derivation '{}' requires non-existent output '{name}' from input derivation '{}'",
							print(drv_path),
							print(input_drv)
						))
					})?;
				self.closure(path, &mut input_paths)?;
			}
		}
		for src in drv.input_sources() {
			self.closure(src, &mut input_paths)?;
		}

		// Offer the build to the hook, lock the outputs and take a local
		// build slot, trying again after each wait.
		let mut waiting = None;
		let key = (drv_path.name().to_owned(), drv_path.clone());
		let turn = self.turn.swap(NO_TURN, Ordering::Relaxed);
		let mut turn = self
			.hooks
			.as_deref()
			.map(|hooks| hooks.turn((turn != NO_TURN).then_some(turn)));
		loop {
			let cancelled = || self.is_cancelled();
			if let Some(turn) = &turn {
				turn.wait(&cancelled)?;
			}
			// A goal waiting for a lock another process holds lets the
			// others go first.
			let held = turn.as_ref().is_some_and(|turn| turn.held());
			let mut locks = match self.prepare(drv_path, build_drv, outputs, !held)? {
				Prepared::Valid(paths) => return Ok(paths),
				Prepared::Locked(locks) => locks,
				Prepared::Busy => {
					if let Some(turn) = &mut turn {
						turn.release();
					}
					continue;
				}
			};
			if let Some(hooks) = hooks
				&& (!prefer_local || rejection.is_some())
			{
				match hooks.offer(self.settings, drv.system(), drv_path, &features, &|| {
					self.is_cancelled()
				})? {
					Reply::Accept(hook) => {
						drop(waiting);
						drop(turn);
						return self.build_with_hook(
							drv_path,
							build_drv,
							outputs,
							&input_paths,
							*hook,
							locks,
							events,
						);
					}
					Reply::Postpone => {
						use nix_compat::log::{ActivityType, VerbosityLevel, logger};
						locks.unlock();
						if let Some(turn) = &mut turn {
							turn.release();
						}
						waiting.get_or_insert_with(|| {
							logger::Activity::new(
								VerbosityLevel::Warn,
								ActivityType::BuildWaiting,
								format!(
									"waiting for a machine to build '{}{}{}'",
									logger::ANSI_MAGENTA,
									print(drv_path),
									logger::ANSI_NORMAL
								),
								vec![],
								0,
							)
						});
						self.wait_for_a_while()?;
						continue;
					}
					Reply::Decline => {}
				}
			}
			waiting = None;
			if let Some(error) = rejection.take() {
				return Err(error);
			}
			let slot = match &self.hooks {
				Some(hooks) => match hooks.acquire_slot(&key) {
					Some(slot) => Some(slot),
					None => {
						locks.unlock();
						drop(turn.take());
						turn = Some(hooks.wait_for_slot(&key, &cancelled)?);
						continue;
					}
				},
				None => None,
			};
			drop(turn);
			let result = self.build_locally(
				drv_path,
				build_drv,
				outputs,
				&drv_options,
				parsed,
				&env,
				kind,
				&input_paths,
				external,
				locks,
				events,
			);
			drop(slot);
			return result;
		}
	}

	/// Lock the outputs, then check again whether another process built them
	/// while we waited. Paths that are present but not valid are deleted.
	fn prepare(
		&self,
		drv_path: &StorePath,
		build_drv: &BuildDerivation,
		outputs: &BTreeMap<String, KnownOutput>,
		wait: bool,
	) -> Result<Prepared> {
		let paths = |outputs: &BTreeMap<String, KnownOutput>| {
			outputs
				.iter()
				.map(|(name, output)| (name.clone(), output.path.clone()))
				.collect::<BTreeMap<_, _>>()
		};
		let real_outputs: Vec<PathBuf> = outputs
			.values()
			.map(|o| self.store.real_path(&o.path))
			.collect();
		let mut locks = if wait {
			self.lock_outputs(real_outputs.clone())?
		} else {
			match PathLocks::try_lock(real_outputs.clone())? {
				Some(locks) => locks,
				None => return Ok(Prepared::Busy),
			}
		};
		for output in outputs.values() {
			self.store.add_temp_root(&output.path)?;
		}
		let check = self.mode == BuildMode::Check;
		if !check && self.all_valid(outputs)? {
			locks.set_deletion(true);
			return Ok(Prepared::Valid(paths(outputs)));
		}
		let current;
		let mut outputs = outputs;
		if outputs.values().any(|o| o.floating.is_some() && !o.impure) {
			current = self.known_outputs(drv_path, build_drv)?;
			if !check && self.all_valid(&current)? {
				locks.set_deletion(true);
				return Ok(Prepared::Valid(paths(&current)));
			}
			outputs = &current;
		}
		for (output, real) in outputs.values().zip(&real_outputs) {
			if !self.path_valid(&output.path)? && fs::symlink_metadata(real).is_ok() {
				tracing::debug!("removing invalid path '{}'", print(&output.path));
				delete_path(real)?;
			}
		}
		Ok(Prepared::Locked(locks))
	}

	/// `waitForAWhile`: until the next `build-poll-interval`.
	fn wait_for_a_while(&self) -> Result<()> {
		let seconds = self
			.settings
			.config
			.get_int("build-poll-interval", 5)
			.max(1) as u64;
		let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
		while std::time::Instant::now() < deadline {
			self.check_cancelled()?;
			std::thread::sleep(std::time::Duration::from_millis(50));
		}
		Ok(())
	}

	/// `DerivationBuildingGoal::buildWithHook`: the hook accepted the build.
	/// It copies the outputs into the store, so nothing is registered here
	/// and nothing is believed but the store.
	#[allow(clippy::too_many_arguments)]
	fn build_with_hook(
		&self,
		drv_path: &StorePath,
		build_drv: &BuildDerivation,
		outputs: &BTreeMap<String, KnownOutput>,
		input_paths: &BTreeSet<StorePath>,
		hook: HookInstance,
		mut locks: PathLocks,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<BTreeMap<String, StorePath>> {
		let mut missing = BTreeSet::new();
		for (name, output) in outputs {
			if !self.path_valid(&output.path)? {
				missing.insert(name.clone());
			}
		}
		let known = outputs
			.values()
			.filter(|output| output.floating.is_none())
			.map(|output| output.path.clone())
			.collect::<Vec<_>>();
		build_with_hook(
			self.settings,
			&self.store.config().log_dir,
			drv_path,
			&known,
			input_paths,
			&missing,
			hook,
			&|| self.tick(),
			events,
		)
		.map_err(|e| self.auto_gc_error(e))?;
		let outputs = self.known_outputs(drv_path, build_drv)?;
		if !self.all_valid(&outputs)? {
			return Err(BuildError::Other(
				"some outputs are unexpectedly invalid".to_owned(),
			));
		}
		let built = outputs
			.into_iter()
			.map(|(name, output)| (name, output.path))
			.collect();
		self.run_post_build_hook(drv_path, &built)?;
		locks.set_deletion(true);
		Ok(built)
	}

	/// `DerivationBuildingGoal::buildLocally`.
	#[allow(clippy::too_many_arguments)]
	fn build_locally(
		&self,
		drv_path: &StorePath,
		build_drv: &BuildDerivation,
		outputs: &BTreeMap<String, KnownOutput>,
		drv_options: &DerivationOptions<StorePath>,
		parsed: Option<Map<String, Value>>,
		env: &BTreeMap<BString, BString>,
		kind: BuildKind,
		input_paths: &BTreeSet<StorePath>,
		external: Option<&ExternalBuilder>,
		mut locks: PathLocks,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<BTreeMap<String, StorePath>> {
		let drv = &build_drv.basic;
		let drv_name = drv_path
			.name()
			.strip_suffix(".drv")
			.unwrap_or(drv_path.name());
		// `ExternalDerivationBuilder`'s constructor requires the feature
		// before the build starts.
		if external.is_some()
			&& !self
				.settings
				.config
				.experimental_features()
				.contains("external-builders")
		{
			return Err(BuildError::Other(
				"experimental Nix feature 'external-builders' is disabled; add '--extra-experimental-features external-builders' to enable it".to_owned(),
			));
		}
		// An external builder is given even a builtin builder's name.
		let builtin = drv
			.command()
			.strip_prefix(b"builtin:")
			.filter(|_| external.is_none());
		if let Some(name) = builtin
			.filter(|name| !matches!(&name[..], b"buildenv" | b"unpack-channel" | b"fetchurl"))
		{
			return Err(BuildError::Other(format!(
				"unsupported builtin builder '{}'",
				name.to_str_lossy()
			)));
		}
		if drv_options.use_uid_range(true) {
			return Err(BuildError::Other(
				"feature 'uid-range' requires the setting 'auto-allocate-uids' to be enabled"
					.to_owned(),
			));
		}
		// Another process may have realised a floating output since.
		let current;
		let mut outputs = outputs;
		if outputs.values().any(|o| o.floating.is_some() && !o.impure) {
			current = self.known_outputs(drv_path, build_drv)?;
			outputs = &current;
		}
		let diverted = self.store.config().real_store_dir != Path::new(store_dir());
		// An external builder is a `DerivationBuilderImpl`, not a chroot
		// builder: it runs outside the sandbox whatever `sandbox` says.
		let use_sandbox = external.is_none()
			&& (diverted
				|| match self.settings.sandbox {
					SandboxMode::Enabled => {
						if drv_options.no_chroot {
							return Err(BuildError::Other(format!(
								"derivation '{}' has '__noChroot' set, but that's not allowed when 'sandbox' is 'true'",
								print(drv_path)
							)));
						}
						true
					}
					SandboxMode::Disabled => false,
					SandboxMode::Relaxed => kind.sandboxed() && !drv_options.no_chroot,
				});
		let build_user = self.acquire_build_user(drv_path)?;
		// Without the sandbox the builder writes its outputs in place.
		let mut build = Build::start(
			self,
			drv_path,
			outputs,
			drv_options,
			parsed,
			kind,
			build_user,
			!use_sandbox,
			external.cloned(),
		)?;
		// A builtin builder's directory may hold credentials.
		build.keep_failed = self.settings.keep_failed && builtin.is_none();
		let result = build
			.run(drv, env, kind, outputs, input_paths, events)
			.and_then(|()| {
				self.check_cancelled()?;
				self.register_outputs(
					drv_path,
					drv_name,
					outputs,
					drv_options,
					input_paths,
					&build,
				)
			});
		let built = result?;
		build.succeeded.set(true);
		drop(build);
		self.run_post_build_hook(drv_path, &built)?;
		locks.set_deletion(true);
		Ok(built)
	}

	/// `DerivationBuilderImpl::registerOutputs`: register the outputs the
	/// builder produced, or compare them with the registered ones when
	/// checking.
	fn register_outputs(
		&self,
		drv_path: &StorePath,
		drv_name: &str,
		outputs: &BTreeMap<String, KnownOutput>,
		drv_options: &DerivationOptions<StorePath>,
		input_paths: &BTreeSet<StorePath>,
		build: &Build,
	) -> Result<BTreeMap<String, StorePath>> {
		let check = self.mode == BuildMode::Check;
		let mut referenceable = input_paths.clone();
		referenceable.extend(build.scratch.values().cloned());
		// And any paths that were added or built through recursive Nix.
		if let Some(restriction) = &*build.restriction.borrow() {
			referenceable.extend(restriction.added_paths());
		}

		// Rewrite rules from the hash parts outputs were built with to those
		// of their final paths, for the outputs that refer to them.
		let mut rewrites: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
		let mut finals: BTreeMap<StorePath, StorePath> = BTreeMap::new();
		let hash_part = |path: &StorePath| nixbase32::encode(path.digest()).into_bytes();

		// Check each output exists and find its references. Outputs that
		// were valid before the build are discarded.
		let mut scanned: BTreeMap<&str, (PathBuf, BTreeSet<StorePath>, fs::Metadata)> =
			BTreeMap::new();
		for (name, output) in outputs {
			let scratch = &build.scratch[name];
			if !check && build.valid.contains(name) {
				if *scratch != output.path {
					rewrites.insert(hash_part(scratch), hash_part(&output.path));
					finals.insert(scratch.clone(), output.path.clone());
				}
				continue;
			}
			let actual = build.path_in_chroot(scratch);
			let metadata = fs::symlink_metadata(&actual).map_err(|_| {
				BuildError::failed(
					FailureStatus::OutputRejected,
					format!(
						"builder for '{}' failed to produce output path for output '{name}' at \"{}\"",
						print(drv_path),
						actual.display()
					),
				)
			})?;
			if (!metadata.file_type().is_symlink() && metadata.mode() & 0o022 != 0)
				|| build
					.build_user
					.as_ref()
					.is_some_and(|u| metadata.uid() != u.uid)
			{
				return Err(BuildError::failed(
					FailureStatus::OutputRejected,
					format!(
						"suspicious ownership or permission on \"{}\" for output '{name}'; rejecting this build output",
						actual.display()
					),
				));
			}
			canonicalise_path_metadata(&actual, build.uid_range())?;
			let references = if drv_options.unsafe_discard_references.get(name) == Some(&true) {
				BTreeSet::new()
			} else {
				scan_for_references(&actual, &referenceable)?
			};
			scanned.insert(name, (actual, references, metadata));
		}

		// Register outputs that refer to other outputs after those.
		let by_path: BTreeMap<&StorePath, &str> = build
			.scratch
			.iter()
			.map(|(name, path)| (path, name.as_str()))
			.collect();
		let order = topo_sort(drv_path, &scanned, &by_path)?;

		// Final paths the build holds the locks of already.
		let held: BTreeSet<&StorePath> = outputs.values().map(|o| &o.path).collect();
		let mut infos: BTreeMap<String, ValidPathInfo> = BTreeMap::new();
		let mut rejected_fixed = None;
		for name in order {
			let output = &outputs[name];
			let scratch = &build.scratch[name];
			let (scanned_path, references, metadata) = &scanned[name];
			let mut actual = scanned_path.clone();
			let mut temp_copy = None;
			let self_ref = references.contains(scratch);
			let others: BTreeSet<StorePath> = references
				.iter()
				.filter(|r| *r != scratch)
				.map(|r| finals.get(r).unwrap_or(r).clone())
				.collect();

			let info = match output.content_address() {
				None => {
					// The NAR hash covers the final hash parts, so rewrite
					// this output's own too.
					if *scratch != output.path {
						rewrites.insert(hash_part(scratch), hash_part(&output.path));
						finals.insert(scratch.clone(), output.path.clone());
					}
					noxi_store::ca::rewrite_path(&actual, &rewrites)?;
					let (nar_hash, nar_size) = nar::hash_path(&actual)?;
					let mut info = ValidPathInfo::new(output.path.clone(), nar_hash);
					info.nar_size = nar_size;
					info.references = others;
					if self_ref {
						info.references.insert(output.path.clone());
					}
					info
				}
				Some((method, algo)) => {
					if output.fixed.is_some() || output.impure {
						let copy = TempCopy::new(self.store, &actual)?;
						actual = copy.path();
						temp_copy = Some(copy);
					}
					let flat = matches!(
						method,
						FileIngestionMethod::Flat | FileIngestionMethod::Text
					);
					if flat && (!metadata.is_file() || metadata.mode() & 0o100 != 0) {
						return Err(BuildError::failed(
							FailureStatus::OutputRejected,
							format!(
								"output path \"{}\" should be a non-executable regular file since recursive hashing is not enabled (one of outputHashMode={{flat,text}} is true)",
								actual.display()
							),
						));
					}
					noxi_store::ca::rewrite_path(&actual, &rewrites)?;
					let old_hash = hash_part(scratch);
					let hash = noxi_store::ca::hash_modulo(&actual, method, algo, &old_hash)?;
					let output_name = output_path_name(drv_name, name);
					let ca = method.content_address(hash)?;
					let path = output_ca_path(&output_name, &ca, &others, self_ref)?;
					if path != *scratch {
						noxi_store::ca::rewrite_path(
							&actual,
							&BTreeMap::from([(old_hash, hash_part(&path))]),
						)?;
					}
					let (nar_hash, nar_size) = nar::hash_path(&actual)?;
					let mut info = ValidPathInfo::new(path.clone(), nar_hash);
					info.nar_size = nar_size;
					info.references = others;
					if self_ref {
						info.references.insert(path);
					}
					info.ca = Some(ca);
					info
				}
			};
			canonicalise_path_metadata(&actual, None)?;

			// A path that was not known before the build is not locked yet.
			// A wrongly hashed fixed output's is not waited for, so that it
			// cannot deadlock with a build that has the right hash.
			let mut locks = PathLocks::default();
			locks.set_deletion(true);
			let real = self.store.real_path(&info.path);
			if !held.contains(&info.path) {
				if output.fixed.is_some() {
					match PathLocks::try_lock([real.clone()])? {
						Some(locked) => {
							locks = locked;
							locks.set_deletion(true);
						}
						None => {
							return Err(check_fixed_output(drv_path, output, &info)
								.unwrap_or_else(|| {
									BuildError::Other(format!(
										"cannot lock output path '{}'",
										print(&info.path)
									))
								}));
						}
					}
				} else {
					locks = self.lock_outputs([real.clone()])?;
					locks.set_deletion(true);
				}
			}
			self.store.add_temp_root(&info.path)?;

			if check {
				self.compare_output(drv_path, &info, &actual, build)?;
			} else {
				if actual != real {
					if self.mode == BuildMode::Repair {
						replace_valid_path(&real, &actual)?;
					} else if self.store.is_valid_path(&info.path)? {
						// The same content address was produced by
						// something else.
						delete_path(&actual)?;
					} else {
						delete_path(&real)?;
						move_path(&actual, &real)?;
					}
				}
			}
			drop(temp_copy);

			let mut info = info;
			if !check {
				if !self.store.is_valid_path(&info.path)? {
					self.store.optimise_path(&real, false)?;
				}
				info.deriver = Some(drv_path.clone());
				info.ultimate = true;
				self.store.sign_path_info(&mut info)?;
				if info.path != *scratch {
					rewrites.insert(hash_part(scratch), hash_part(&info.path));
				}
				finals.insert(scratch.clone(), info.path.clone());
				// A content-addressed output is registered at once: its path
				// may not be locked after this, and a wrongly hashed download
				// need not be repeated.
				if info.ca.is_some() {
					self.store.register_valid_path(&info)?;
				}
			}
			if rejected_fixed.is_none() {
				rejected_fixed = check_fixed_output(drv_path, output, &info);
			}
			infos.insert(name.to_owned(), info);
			drop(locks);
		}
		if let Some(error) = rejected_fixed {
			return Err(error);
		}

		self.check_outputs(drv_path, drv_options, &infos)?;
		if check {
			return Ok(outputs
				.iter()
				.map(|(name, output)| (name.clone(), output.path.clone()))
				.collect());
		}

		let paths: BTreeMap<String, StorePath> = infos
			.iter()
			.map(|(name, info)| (name.clone(), info.path.clone()))
			.collect();
		if self.repair() {
			// `markContentsGood`.
			self.checked()
				.extend(paths.values().map(|path| (path.clone(), true)));
		}
		self.store
			.register_valid_paths(&infos.values().cloned().collect::<Vec<_>>())?;
		for (name, info) in infos {
			if self.store.config().ca_derivations && !outputs[&name].impure {
				let mut realisation = Realisation {
					out_path: info.path,
					signatures: BTreeSet::new(),
				};
				self.store
					.sign_realisation(drv_path, &name, &mut realisation)?;
				self.store
					.register_drv_output(drv_path, &name, &realisation)?;
			}
		}
		Ok(paths)
	}

	/// `runPostBuildHook`: run the hook on the outputs a build produced or
	/// checked. A failing hook fails the build.
	fn run_post_build_hook(
		&self,
		drv_path: &StorePath,
		outputs: &BTreeMap<String, StorePath>,
	) -> Result<()> {
		run_post_build_hook(self.settings, drv_path, outputs)
	}

	/// Compare a rebuilt output with the registered one.
	fn compare_output(
		&self,
		drv_path: &StorePath,
		info: &ValidPathInfo,
		actual: &Path,
		build: &Build,
	) -> Result<()> {
		let Some(mut old) = self.store.query_path_info(&info.path)? else {
			return Ok(());
		};
		if info.nar_hash != old.nar_hash {
			let real = self.store.real_path(&info.path);
			let mut message = format!(
				"derivation '{}' may not be deterministic: output \"{}\" differs",
				print(drv_path),
				real.display()
			);
			if self.settings.diff_hook().is_some() || self.settings.keep_failed {
				let mut kept = real.into_os_string();
				kept.push(".check");
				let kept = PathBuf::from(kept);
				delete_path(&kept)?;
				move_path(actual, &kept)?;
				if let Some(hook) = self.settings.diff_hook() {
					build.run_diff_hook(hook, &print(&info.path), &kept);
				}
				message += &format!(" from \"{}\"", kept.display());
			}
			return Err(BuildError::failed(FailureStatus::NotDeterministic, message));
		}
		// The build is verified, so the path is now ultimately trusted.
		if !old.ultimate {
			old.ultimate = true;
			self.store.sign_path_info(&mut old)?;
			self.store.register_valid_path(&old)?;
		}
		Ok(())
	}

	/// `checkOutputs` from `derivation-check.cc`.
	fn check_outputs(
		&self,
		drv_path: &StorePath,
		drv_options: &DerivationOptions<StorePath>,
		infos: &BTreeMap<String, ValidPathInfo>,
	) -> Result<()> {
		let by_path: BTreeMap<&StorePath, &ValidPathInfo> =
			infos.values().map(|info| (&info.path, info)).collect();
		let closure = |path: &StorePath| -> Result<(BTreeSet<StorePath>, u64)> {
			let mut done = BTreeSet::new();
			let mut size = 0;
			let mut pending = std::collections::VecDeque::from([path.clone()]);
			while let Some(path) = pending.pop_front() {
				if !done.insert(path.clone()) {
					continue;
				}
				if let Some(info) = by_path.get(&path) {
					size += info.nar_size;
					pending.extend(info.references.iter().cloned());
				} else {
					let info = self.store.query_path_info(&path)?.ok_or_else(|| {
						BuildError::Other(format!("path '{}' is not valid", print(&path)))
					})?;
					size += info.nar_size;
					pending.extend(info.references.iter().cloned());
				}
			}
			Ok((done, size))
		};
		let rejected = |message: String| BuildError::failed(FailureStatus::OutputRejected, message);

		for (output_name, info) in infos {
			let checks: &OutputChecks<StorePath> = match &drv_options.output_checks {
				OutputChecksSpec::ForAllOutputs(checks) => checks,
				OutputChecksSpec::PerOutput(map) => match map.get(output_name) {
					Some(checks) => checks,
					None => continue,
				},
			};
			if let Some(max) = checks.max_size
				&& info.nar_size > max
			{
				return Err(rejected(format!(
					"path '{}' is too large at {} bytes; limit is {max} bytes",
					print(&info.path),
					info.nar_size
				)));
			}
			if let Some(max) = checks.max_closure_size {
				let size = closure(&info.path)?.1;
				if size > max {
					return Err(rejected(format!(
						"closure of path '{}' is too large at {size} bytes; limit is {max} bytes",
						print(&info.path)
					)));
				}
			}
			let check_refs = |spec: &BTreeSet<DrvRef<StorePath>>,
			                  allowed: bool,
			                  recursive: bool|
			 -> Result<()> {
				let mut paths = BTreeSet::new();
				for entry in spec {
					match entry {
						DrvRef::Input(path) => {
							paths.insert(path.clone());
						}
						DrvRef::Output(name) => match infos.get(name) {
							Some(output) => {
								paths.insert(output.path.clone());
							}
							None => {
								return Err(rejected(format!(
									"derivation '{}' output check for '{output_name}' contains output name '{name}', but this is not a valid output of this derivation. (Valid outputs are [{}].)",
									print(drv_path),
									infos
										.keys()
										.map(String::as_str)
										.collect::<Vec<_>>()
										.join(", ")
								)));
							}
						},
					}
				}
				let mut used = if recursive {
					closure(&info.path)?.0
				} else {
					info.references.clone()
				};
				if recursive && checks.ignore_self_refs {
					used.remove(&info.path);
				}
				let bad: Vec<&StorePath> = used
					.iter()
					.filter(|p| paths.contains(*p) != allowed)
					.collect();
				if !bad.is_empty() {
					let list: String = bad.iter().map(|p| format!("\n  {}", print(p))).collect();
					return Err(rejected(format!(
						"output '{}' is not allowed to refer to the following paths:{list}",
						print(&info.path)
					)));
				}
				Ok(())
			};
			if let Some(refs) = &checks.allowed_references {
				check_refs(refs, true, false)?;
			}
			if let Some(refs) = &checks.allowed_requisites {
				check_refs(refs, true, true)?;
			}
			if !checks.disallowed_references.is_empty() {
				check_refs(&checks.disallowed_references, false, false)?;
			}
			if !checks.disallowed_requisites.is_empty() {
				check_refs(&checks.disallowed_requisites, false, true)?;
			}
		}
		Ok(())
	}
}

/// The outputs of `drv` and their paths. Floating, impure and deferred
/// outputs have no path yet.
impl LocalBuilder<'_> {
	fn known_outputs(
		&self,
		drv_path: &StorePath,
		build_drv: &BuildDerivation,
	) -> Result<BTreeMap<String, KnownOutput>> {
		let drv = &build_drv.basic;
		let drv_name = drv_path
			.name()
			.strip_suffix(".drv")
			.unwrap_or(drv_path.name());
		let floating_output =
			|name: &str, spec: FloatingOutput, impure: bool| -> Result<KnownOutput> {
				let name_part = output_path_name(drv_name, name);
				let path = if impure {
					random_path(&name_part)?
				} else {
					match self.store.query_realisation(drv_path, name)? {
						Some(real) if self.store.is_valid_path(&real.out_path)? => real.out_path,
						_ => rewrite_path(&format!("rewrite:{drv_path}:name:{name}"), &name_part)?,
					}
				};
				Ok(KnownOutput {
					path,
					fixed: None,
					floating: Some(spec),
					impure,
				})
			};
		if !build_drv.experimental.is_empty() {
			return build_drv
				.experimental
				.iter()
				.map(|(name, spec)| {
					let method = spec.method.ingestion_method();
					let output = match &spec.fixed {
						Some((path, hash)) => KnownOutput {
							path: path.clone(),
							fixed: Some((method, hash.clone())),
							floating: None,
							impure: false,
						},
						None => floating_output(
							name,
							FloatingOutput {
								method,
								hash_algo: spec.hash_algo,
							},
							spec.impure,
						)?,
					};
					Ok((name.clone(), output))
				})
				.collect();
		}
		if !build_drv.floating.is_empty() {
			return build_drv
				.floating
				.iter()
				.map(|(name, spec)| Ok((name.clone(), floating_output(name, spec.clone(), false)?)))
				.collect();
		}
		if drv.outputs().is_deferred() {
			return Err(BuildError::Other(format!(
				"cannot build '{}': its outputs are content-addressed or deferred, which noxi cannot build yet",
				print(drv_path)
			)));
		}
		if let Some((hash, path)) = drv.outputs().as_fixed_output() {
			return Ok(BTreeMap::from([(
				"out".to_owned(),
				KnownOutput {
					path: path.clone(),
					fixed: Some((
						match hash.mode {
							OutputHashMode::Recursive => FileIngestionMethod::NixArchive,
							OutputHashMode::Flat => FileIngestionMethod::Flat,
						},
						hash.hash.clone(),
					)),
					floating: None,
					impure: false,
				},
			)]));
		}
		Ok(drv
			.outputs()
			.iter()
			.map(|(name, path)| {
				(
					name.to_string(),
					KnownOutput {
						path: path.clone(),
						fixed: None,
						floating: None,
						impure: false,
					},
				)
			})
			.collect())
	}
}

/// `StorePath::random`.
fn random_path(name: &str) -> Result<StorePath> {
	let mut digest = [0; store_path::DIGEST_SIZE];
	use std::io::Read;
	fs::File::open("/dev/urandom")?.read_exact(&mut digest)?;
	StorePath::from_name_and_digest(name, &digest).map_err(|e| BuildError::Other(e.to_string()))
}

/// `makeFixedOutputPathFromCA` for the content address of an output.
fn output_ca_path(
	name: &str,
	ca: &CAHash,
	references: &BTreeSet<StorePath>,
	self_ref: bool,
) -> Result<StorePath> {
	let refs = references.iter().map(StorePath::as_ref);
	let not_allowed = || {
		BuildError::Other(format!(
			"fixed output derivation '{name}' is not allowed to refer to other store paths.\nYou may need to use the 'unsafeDiscardReferences' derivation attribute, see the manual for more details."
		))
	};
	let has_references = self_ref || !references.is_empty();
	match ca {
		// The reference asserts that a text output has no self-reference.
		CAHash::Text(_) if self_ref => return Err(not_allowed()),
		CAHash::Text(digest) => {
			store_path::build_text_path_from_content_digest(name, *digest, refs)
		}
		CAHash::Nar(hash) if hash.algo() == HashAlgo::Sha256 || !has_references => {
			store_path::build_ca_path(name, true, hash, refs, self_ref)
		}
		_ if has_references => return Err(not_allowed()),
		CAHash::Git(hash) => store_path::build_git_path(name, hash),
		CAHash::Flat(hash) | CAHash::Nar(hash) => {
			store_path::build_ca_path(name, false, hash, refs, false)
		}
	}
	.map(|path| path.to_owned())
	.map_err(|e| BuildError::Other(e.to_string()))
}

/// `makeFallbackPath`: a path no other store path collides with, as its
/// fingerprint has a type of its own.
fn rewrite_path(path_type: &str, name: &str) -> Result<StorePath> {
	let fingerprint = format!(
		"{path_type}:sha256:{}:{}:{name}",
		"0".repeat(64),
		store_dir()
	);
	StorePath::from_name_and_digest(
		name,
		&store_path::compress_hash::<20>(&Sha256::digest(fingerprint.as_bytes())),
	)
	.map_err(|e| BuildError::Other(e.to_string()))
}

/// The path an output is built at while `path` is present.
fn fallback_path(drv_path: &StorePath, path: &StorePath) -> Result<StorePath> {
	rewrite_path(&format!("rewrite:{drv_path}:{path}"), path.name())
}

/// `checkCAFixedOutput`: why a fixed output's content is rejected.
fn check_fixed_output(
	drv_path: &StorePath,
	output: &KnownOutput,
	info: &ValidPathInfo,
) -> Option<BuildError> {
	let (_, wanted) = output.fixed.as_ref()?;
	let ca = info.ca.as_ref()?;
	let got = ca.hash();
	if *got != *wanted {
		return Some(BuildError::failed(
			FailureStatus::HashMismatch,
			format!(
				"hash mismatch in fixed-output derivation '{}':\n  specified: {}\n     got:    {}",
				print(drv_path),
				wanted.to_sri_string(),
				got.to_sri_string()
			),
		));
	}
	let first = info.references.iter().next()?;
	Some(BuildError::failed(
		FailureStatus::HashMismatch,
		format!(
			"fixed-output derivations must not reference store paths: '{}' references {} distinct paths, e.g. '{}'",
			print(drv_path),
			info.references.len(),
			print(first)
		),
	))
}

/// `moveOutputToTempDir`: a fresh copy of an output in a directory of the
/// store only this user can enter, which a process the builder left behind
/// holds no writable descriptors of.
struct TempCopy {
	dir: PathBuf,
	_lock: fs::File,
}

impl TempCopy {
	fn new(store: &LocalStore, source: &Path) -> Result<Self> {
		let (dir, lock) = store.create_temp_dir_in_store()?;
		let copy = Self { dir, _lock: lock };
		let (reader, mut writer) = io::pipe()?;
		std::thread::scope(|scope| {
			let dump = scope.spawn(move || nar::dump_path(&mut writer, source));
			let restored = nar::restore_path(&copy.path(), &mut io::BufReader::new(reader));
			dump.join()
				.unwrap_or_else(|_| Err(io::Error::other("copying an output panicked")))?;
			restored
		})?;
		Ok(copy)
	}

	fn path(&self) -> PathBuf {
		self.dir.join("x")
	}
}

impl Drop for TempCopy {
	fn drop(&mut self) {
		let _ = delete_path(&self.dir);
	}
}

/// `replaceValidPath`: the path cannot be replaced atomically, so it is
/// moved out of the way first and moved back if that fails.
fn replace_valid_path(path: &Path, replacement: &Path) -> Result<()> {
	let mut old = None;
	if fs::symlink_metadata(path).is_ok() {
		static COUNTER: AtomicU64 = AtomicU64::new(0);
		let mut moved = path.as_os_str().to_owned();
		moved.push(format!(
			".old-{}-{}",
			std::process::id(),
			COUNTER.fetch_add(1, Ordering::Relaxed)
		));
		let moved = PathBuf::from(moved);
		move_path(path, &moved)?;
		old = Some(moved);
	}
	if let Err(error) = move_path(replacement, path) {
		if let Some(old) = &old {
			move_path(old, path)?;
		}
		return Err(error.into());
	}
	if let Some(old) = &old {
		delete_path(old)?;
	}
	Ok(())
}

/// `outputPathName`.
fn output_path_name(drv_name: &str, output: &str) -> String {
	if output == "out" {
		drv_name.to_owned()
	} else {
		format!("{drv_name}-{output}")
	}
}

/// Order outputs so each comes after the outputs it refers to, failing
/// on a cycle as CppNix's `topoSort` does.
fn topo_sort<'n>(
	drv_path: &StorePath,
	scanned: &BTreeMap<&'n str, (PathBuf, BTreeSet<StorePath>, fs::Metadata)>,
	by_path: &BTreeMap<&StorePath, &'n str>,
) -> Result<Vec<&'n str>> {
	fn visit<'n>(
		name: &'n str,
		parent: &'n str,
		drv_path: &StorePath,
		scanned: &BTreeMap<&'n str, (PathBuf, BTreeSet<StorePath>, fs::Metadata)>,
		by_path: &BTreeMap<&StorePath, &'n str>,
		visiting: &mut BTreeSet<&'n str>,
		done: &mut Vec<&'n str>,
	) -> Result<()> {
		if done.contains(&name) {
			return Ok(());
		}
		if !visiting.insert(name) {
			return Err(BuildError::failed(
				FailureStatus::OutputRejected,
				format!(
					"cycle detected in build of '{}' in the references of output '{name}' from output '{parent}'",
					print(drv_path)
				),
			));
		}
		if let Some((_, refs, _)) = scanned.get(name) {
			for r in refs {
				if let Some(other) = by_path.get(r)
					&& *other != name
					&& scanned.contains_key(other)
				{
					visit(other, name, drv_path, scanned, by_path, visiting, done)?;
				}
			}
		}
		visiting.remove(name);
		done.push(name);
		Ok(())
	}
	let mut done = Vec::new();
	let mut visiting = BTreeSet::new();
	for name in scanned.keys() {
		visit(
			name,
			name,
			drv_path,
			scanned,
			by_path,
			&mut visiting,
			&mut done,
		)?;
	}
	Ok(done)
}

/// CppNix's `movePath`: a rename that makes a read-only directory
/// writable for the duration so its `..` entry can change.
fn move_path(src: &Path, dst: &Path) -> io::Result<()> {
	use std::os::unix::fs::PermissionsExt;
	let metadata = fs::symlink_metadata(src)?;
	// SAFETY: geteuid cannot fail.
	let change =
		unsafe { libc::geteuid() } != 0 && metadata.is_dir() && metadata.mode() & 0o200 == 0;
	if change {
		fs::set_permissions(src, fs::Permissions::from_mode(metadata.mode() | 0o200))?;
	}
	fs::rename(src, dst)?;
	if change {
		fs::set_permissions(dst, fs::Permissions::from_mode(metadata.mode() & 0o7777))?;
	}
	Ok(())
}

/// CppNix's `rewriteStrings`: apply each rewrite in turn.
fn rewrite_strings(mut s: Vec<u8>, rewrites: &BTreeMap<String, String>) -> Vec<u8> {
	for (from, to) in rewrites {
		if from == to || from.is_empty() {
			continue;
		}
		s = s.replace(from.as_bytes(), to.as_bytes());
	}
	s
}

/// `createTempDir(root, prefix, 0700)` with `makeTempPath`'s names.
fn create_temp_dir(root: &Path, prefix: &str) -> io::Result<PathBuf> {
	static COUNTER: AtomicU64 = AtomicU64::new(0);
	loop {
		let dir = root.join(format!(
			"{prefix}-{}-{}",
			std::process::id(),
			COUNTER.fetch_add(1, Ordering::Relaxed)
		));
		match fs::DirBuilder::new().mode(0o700).create(&dir) {
			Ok(()) => return Ok(dir),
			Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
			Err(e) => return Err(e),
		}
	}
}

/// Whether a build can go ahead once its outputs are locked.
enum Prepared {
	Valid(BTreeMap<String, StorePath>),
	Locked(PathLocks),
	/// Another process holds a lock, and the caller did not want to wait.
	Busy,
}

/// No turn; see [`Hooks`].
const NO_TURN: u64 = u64::MAX;

/// `doneFailureLogTooLong`.
fn log_too_long(drv_path: &StorePath, max_log_size: u64) -> BuildError {
	BuildError::failed(
		FailureStatus::LogLimitExceeded,
		format!(
			"building derivation '{}' killed after writing more than {max_log_size} bytes of log output",
			print(drv_path)
		),
	)
}

/// `DerivationType::hasKnownOutputPaths`.
pub fn has_known_output_paths(drv: &BuildDerivation) -> bool {
	drv.is_fixed() || !(drv.has_floating_outputs() || drv.is_impure() || drv.is_deferred())
}

/// `runPostBuildHook`.
pub(crate) fn run_post_build_hook(
	settings: &BuildSettings,
	drv_path: &StorePath,
	outputs: &BTreeMap<String, StorePath>,
) -> Result<()> {
	use nix_compat::log::{ActivityType, VerbosityLevel, logger};
	let hook = &settings.post_build_hook;
	if hook.is_empty() {
		return Ok(());
	}
	let mut log = logger::BuildLog::new(logger::Activity::new(
		VerbosityLevel::Talkative,
		ActivityType::PostBuildHook,
		format!("running post-build-hook '{hook}'"),
		vec![print(drv_path).into()],
		0,
	))
	.post_build();
	let (reader, writer) = io::pipe()?;
	let config: String = settings
		.hook_config
		.iter()
		.map(|(name, value)| format!("{name} = {value}\n"))
		.collect();
	let outputs: BTreeSet<&StorePath> = outputs.values().collect();
	let paths: Vec<String> = outputs.into_iter().map(print).collect();
	let child = std::process::Command::new(hook)
		.env("DRV_PATH", print(drv_path))
		.env("OUT_PATHS", paths.join(" "))
		.env("NIX_CONFIG", config)
		.stdin(std::process::Stdio::null())
		.stdout(writer.try_clone()?)
		.stderr(writer)
		.spawn();
	let mut child = child.map_err(|e| BuildError::Other(format!("executing \"{hook}\": {}", e)))?;
	let mut reader = io::BufReader::new(reader);
	loop {
		use io::BufRead;
		let data = reader.fill_buf()?;
		if data.is_empty() {
			break;
		}
		log.push(data);
		let len = data.len();
		reader.consume(len);
	}
	log.flush();
	let status = child.wait()?;
	if !status.success() {
		return Err(BuildError::Other(format!(
			"program \"{hook}\" {}",
			sandbox::ExitStatus(std::os::unix::process::ExitStatusExt::into_raw(status)).describe()
		)));
	}
	Ok(())
}

/// Have an accepted build done by the hook, logging its output. The outputs
/// are the hook's to copy into the store.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_with_hook(
	settings: &BuildSettings,
	log_dir: &Path,
	drv_path: &StorePath,
	outputs: &[StorePath],
	input_paths: &BTreeSet<StorePath>,
	missing_outputs: &BTreeSet<String>,
	mut hook: HookInstance,
	cancelled: &dyn Fn() -> bool,
	events: &mut dyn FnMut(BuildEvent<'_>),
) -> Result<()> {
	let machine = hook.read_machine_name(cancelled)?;
	events(BuildEvent::StartedRemotely {
		drv_path,
		mode: BuildMode::Normal,
		machine: &machine,
	});
	hook.send_paths(input_paths, missing_outputs)?;
	let mut log_file = open_log_file(settings, log_dir, drv_path)?;
	let max_log_size = settings.max_log_size();
	let mut tail = LogTail::new(settings.log_lines);
	let mut log_size = 0u64;
	let status = hook.run(cancelled, &mut |event| {
		let data = match event {
			HookEvent::Output(data) => {
				log_size += data.len() as u64;
				if max_log_size != 0 && log_size > max_log_size {
					return Err(log_too_long(drv_path, max_log_size));
				}
				tail.feed(data);
				events(BuildEvent::Output(data));
				data
			}
			HookEvent::LogLine(data) => data,
		};
		if let Some(file) = &mut log_file {
			file.write_all(data)?;
		}
		Ok(())
	})?;
	tail.flush();
	if let Some(file) = log_file {
		file.finish()?;
	}
	if !status.success() {
		return Err(BuildError::failed(
			FailureStatus::MiscFailure,
			builder_failure(settings, drv_path, outputs, &status, &tail),
		));
	}
	Ok(())
}

/// A build's log file.
enum LogFile {
	Plain(fs::File),
	Bzip2(bzip2::write::BzEncoder<fs::File>),
}

impl LogFile {
	fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
		use std::io::Write;
		match self {
			LogFile::Plain(f) => f.write_all(data),
			LogFile::Bzip2(f) => f.write_all(data),
		}
	}

	fn finish(self) -> io::Result<()> {
		use std::io::Write;
		match self {
			LogFile::Plain(mut f) => f.flush(),
			LogFile::Bzip2(f) => f.finish()?.flush(),
		}
	}
}

/// The last lines of a build's output, CppNix's `BuildLog`: a carriage
/// return restarts the line, and `@nix` JSON messages are not log lines.
struct LogTail {
	max: usize,
	lines: std::collections::VecDeque<Vec<u8>>,
	line: Vec<u8>,
	pos: usize,
}

impl LogTail {
	fn new(max: usize) -> Self {
		Self {
			max,
			lines: Default::default(),
			line: Vec::new(),
			pos: 0,
		}
	}

	fn feed(&mut self, data: &[u8]) {
		for &c in data {
			match c {
				b'\r' => self.pos = 0,
				b'\n' => self.flush_line(),
				_ => {
					if self.pos >= self.line.len() {
						self.line.resize(self.pos + 1, 0);
					}
					self.line[self.pos] = c;
					self.pos += 1;
				}
			}
		}
	}

	fn flush(&mut self) {
		if !self.line.is_empty() {
			self.flush_line();
		}
	}

	fn flush_line(&mut self) {
		self.line.truncate(self.pos);
		let line = std::mem::take(&mut self.line);
		self.pos = 0;
		let json = line
			.strip_prefix(b"@nix ")
			.is_some_and(|rest| serde_json::from_slice::<Value>(rest).is_ok());
		if !json {
			self.lines.push_back(line);
			if self.lines.len() > self.max {
				self.lines.pop_front();
			}
		}
	}
}

/// `fixupBuilderFailureErrorMessage`.
fn builder_failure(
	settings: &BuildSettings,
	drv_path: &StorePath,
	outputs: &[StorePath],
	status: &sandbox::ExitStatus,
	tail: &LogTail,
) -> String {
	let mut msg = format!(
		"Cannot build '{}'.\nReason: builder {}.",
		print(drv_path),
		status.describe()
	);
	let mut outputs = outputs.to_vec();
	outputs.sort();
	if !outputs.is_empty() {
		msg += "\nOutput paths:";
	}
	for output in &outputs {
		msg += &format!("\n  {}", print(output));
	}
	if settings.show_log_tail && !tail.lines.is_empty() {
		msg += &format!("\nLast {} log lines:\n", tail.lines.len());
		for line in &tail.lines {
			msg += "> ";
			msg += &String::from_utf8_lossy(line);
			msg.push('\n');
		}
		msg += &format!(
			"For full logs, run:\n  {} {}",
			if settings.nix_command {
				"nix log"
			} else {
				"nix-store -l"
			},
			print(drv_path)
		);
	}
	msg
}

fn open_log_file(
	settings: &BuildSettings,
	log_dir: &Path,
	drv_path: &StorePath,
) -> Result<Option<LogFile>> {
	if !settings.keep_build_log {
		return Ok(None);
	}
	let base = drv_path.to_string();
	let dir = log_dir.join("drvs").join(&base[..2]);
	fs::create_dir_all(&dir)?;
	let mut name = base[2..].to_owned();
	if settings.compress_build_log {
		name += ".bz2";
	}
	let file = fs::File::create(dir.join(name))?;
	Ok(Some(if settings.compress_build_log {
		LogFile::Bzip2(bzip2::write::BzEncoder::new(
			file,
			bzip2::Compression::best(),
		))
	} else {
		LogFile::Plain(file)
	}))
}

/// The directories of one build attempt, removed by [`Build::cleanup`].
struct Build<'b> {
	builder: &'b LocalBuilder<'b>,
	drv_path: StorePath,
	outputs: Vec<StorePath>,
	top_tmp_dir: PathBuf,
	tmp_dir: PathBuf,
	chroot_parent: PathBuf,
	chroot_root: PathBuf,
	/// Whether outputs are built at their real store paths, as an
	/// unsandboxed build's are, rather than in the chroot.
	direct: bool,
	sandboxed: bool,
	/// The builder is told its output's hash is checked.
	fixed: bool,
	/// The outputs that were valid before the build.
	valid: BTreeSet<String>,
	/// The paths the builder is told to write its outputs to. They differ
	/// from the final ones for outputs whose path is not known yet, and for
	/// unsandboxed builds that must not touch a path that is present.
	scratch: BTreeMap<String, StorePath>,
	/// `redirectedOutputs`: scratch paths in the store directory that stand
	/// for paths that are present.
	redirected: Vec<StorePath>,
	/// `inputRewrites`: output placeholders become scratch paths, and the
	/// hash parts of redirected outputs those of their scratch paths.
	input_rewrites: BTreeMap<String, String>,
	/// Keep the build directory when the build fails.
	keep_failed: bool,
	succeeded: Cell<bool>,
	impure_env_vars: BTreeSet<String>,
	pass_as_file: BTreeSet<String>,
	export_references_graph: BTreeMap<String, BTreeSet<StorePath>>,
	structured: Option<Map<String, Value>>,
	/// The build user the builder runs as, when the daemon runs as root.
	build_user: Option<BuildUser>,
	/// Whether the derivation requires `recursive-nix`.
	recursive: bool,
	/// The paths a recursive build may see; set while it runs.
	restriction: std::cell::RefCell<Option<std::sync::Arc<crate::recursive::Restriction>>>,
	/// The program that builds the derivation instead of its builder.
	external: Option<ExternalBuilder>,
}

impl<'b> Build<'b> {
	#[allow(clippy::too_many_arguments)]
	fn start(
		builder: &'b LocalBuilder<'b>,
		drv_path: &StorePath,
		outputs: &BTreeMap<String, KnownOutput>,
		drv_options: &DerivationOptions<StorePath>,
		structured: Option<Map<String, Value>>,
		kind: BuildKind,
		build_user: Option<BuildUser>,
		direct: bool,
		external: Option<ExternalBuilder>,
	) -> Result<Self> {
		// `prepareUser`: no process may still run as the build user.
		if let Some(user) = &build_user {
			user.kill()?;
		}
		let store = builder.store;
		let mut valid = BTreeSet::new();
		let mut scratch = BTreeMap::new();
		let mut redirected = Vec::new();
		let mut input_rewrites = BTreeMap::new();
		for (name, output) in outputs {
			let is_valid = builder.path_valid(&output.path)?;
			if is_valid {
				valid.insert(name.clone());
			}
			// A path that is present cannot be overwritten by an unsandboxed
			// builder, except that an invalid one was deleted. A damaged path
			// may still be in use while it is repaired.
			let path = if direct && store.is_valid_path(&output.path)? {
				let path = fallback_path(drv_path, &output.path)?;
				delete_path(&store.real_path(&path))?;
				input_rewrites.insert(
					nixbase32::encode(output.path.digest()),
					nixbase32::encode(path.digest()),
				);
				redirected.push(path.clone());
				path
			} else {
				output.path.clone()
			};
			input_rewrites.insert(hash_placeholder(name), print(&path));
			scratch.insert(name.clone(), path);
		}
		let build_dir = builder
			.settings
			.build_dir
			.clone()
			.unwrap_or_else(|| store.config().state_dir.join("builds"));
		fs::create_dir_all(&build_dir)?;
		let top_tmp_dir = create_temp_dir(&build_dir, "nix")?;
		let tmp_dir = top_tmp_dir.join("build");
		fs::DirBuilder::new().mode(0o700).create(&tmp_dir)?;
		if let Some(user) = &build_user {
			user.chown(&tmp_dir)?;
		}

		let mut chroot_parent = store.real_path(drv_path).into_os_string();
		chroot_parent.push(".chroot");
		let chroot_parent = PathBuf::from(chroot_parent);
		let chroot_root = sandbox::setup_chroot(
			&chroot_parent,
			Path::new(store_dir()),
			kind.sandboxed(),
			build_user.as_ref().map(|u| u.gid),
		)?;
		Ok(Self {
			builder,
			drv_path: drv_path.clone(),
			outputs: outputs
				.values()
				.filter(|o| o.floating.is_none())
				.map(|o| o.path.clone())
				.collect(),
			top_tmp_dir,
			tmp_dir,
			chroot_parent,
			chroot_root,
			direct,
			sandboxed: kind.sandboxed(),
			fixed: kind.fixed,
			valid,
			scratch,
			redirected,
			input_rewrites,
			keep_failed: false,
			succeeded: Cell::new(false),
			impure_env_vars: drv_options.impure_env_vars.clone(),
			pass_as_file: drv_options.pass_as_file.clone(),
			export_references_graph: drv_options.export_references_graph.clone(),
			structured,
			build_user,
			recursive: drv_options
				.required_system_features
				.contains("recursive-nix"),
			restriction: std::cell::RefCell::new(None),
			external,
		})
	}

	/// The uids an output's files may be owned by before canonicalisation.
	fn uid_range(&self) -> Option<(u32, u32)> {
		self.build_user.as_ref().map(|u| (u.uid, u.uid))
	}

	/// The build directory as the builder sees it: `sandbox-build-dir` in
	/// the sandbox, the temporary directory itself without one.
	fn build_top(&self) -> PathBuf {
		// `ExternalDerivationBuilder::tmpDirInSandbox`, for determinism.
		if self.external.is_some() {
			return PathBuf::from("/build");
		}
		if self.direct {
			self.tmp_dir.clone()
		} else {
			PathBuf::from(&self.builder.settings.sandbox_build_dir)
		}
	}

	fn path_in_chroot(&self, path: &StorePath) -> PathBuf {
		if self.direct {
			return self.builder.store.real_path(path);
		}
		self.chroot_root
			.join(store_dir().trim_start_matches('/'))
			.join(path.to_string())
	}

	fn write_builder_file(&self, name: &str, contents: &[u8]) -> Result<()> {
		if name.is_empty() || name.contains('/') || name == "." || name == ".." {
			return Err(BuildError::Other(format!(
				"invalid builder file name '{name}'"
			)));
		}
		use std::io::Write;
		let mut file = fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(self.tmp_dir.join(name))?;
		file.write_all(contents)?;
		if let Some(user) = &self.build_user {
			user.chown(&self.tmp_dir.join(name))?;
		}
		Ok(())
	}

	/// `initEnv` with `DesugaredEnv::create`.
	fn environment(
		&self,
		drv: &UnverifiedDerivation,
		env: &BTreeMap<BString, BString>,
		input_paths: &BTreeSet<StorePath>,
		rewrites: &BTreeMap<String, String>,
	) -> Result<BTreeMap<BString, BString>> {
		let settings = self.builder.settings;
		let build_top = self.build_top();
		let build_top = build_top.to_str().unwrap_or_default();
		let mut out: BTreeMap<BString, BString> = BTreeMap::new();
		let mut set = |k: &str, v: &str| {
			out.insert(k.into(), v.into());
		};
		set("PATH", "/path-not-set");
		set("HOME", HOME_DIR);
		set("NIX_STORE", store_dir());
		set("NIX_BUILD_CORES", &settings.effective_cores().to_string());

		if let Some(attrs) = &self.structured {
			// `StructuredAttrs::prepareStructuredAttrs`.
			let mut json = attrs.clone();
			let outputs: Map<String, Value> = drv
				.outputs()
				.names()
				.map(|name| {
					let name = name.to_string();
					let placeholder = hash_placeholder(&name);
					(name, Value::String(placeholder))
				})
				.collect();
			json.insert("outputs".to_owned(), Value::Object(outputs));
			for (key, paths) in &self.export_references_graph {
				let paths = self.export_references(paths, input_paths)?;
				json.insert(key.clone(), self.path_info_json(&paths)?);
			}
			for (var, file, contents) in [
				(
					"NIX_ATTRS_SH_FILE",
					".attrs.sh",
					structured::write_shell(&json),
				),
				(
					"NIX_ATTRS_JSON_FILE",
					".attrs.json",
					structured::dump(&Value::Object(json.clone())),
				),
			] {
				self.write_builder_file(file, &rewrite_strings(contents.into_bytes(), rewrites))?;
				out.insert(var.into(), format!("{build_top}/{file}").into());
			}
		}
		for (name, value) in env.iter().filter(|_| self.structured.is_none()) {
			let name_str = name.to_str_lossy();
			if self.pass_as_file.contains(name_str.as_ref()) {
				let file = format!(
					".attr-{}",
					nixbase32::encode(&Sha256::digest(name.as_slice()))
				);
				self.write_builder_file(&file, &rewrite_strings(value.to_vec(), rewrites))?;
				out.insert(
					format!("{name_str}Path").into(),
					format!("{build_top}/{file}").into(),
				);
			} else {
				out.insert(name.clone(), value.clone());
			}
		}
		for (file, paths) in self
			.export_references_graph
			.iter()
			.filter(|_| self.structured.is_none())
		{
			let registration = self.validity_registration(paths, input_paths)?;
			self.write_builder_file(file, &rewrite_strings(registration.into_bytes(), rewrites))?;
		}

		let mut set = |k: &str, v: &str| {
			out.insert(k.into(), v.into());
		};
		set("NIX_BUILD_TOP", build_top);
		for name in ["TMPDIR", "TEMPDIR", "TMP", "TEMP", "PWD"] {
			set(name, build_top);
		}
		if self.fixed {
			set("NIX_OUTPUT_CHECKED", "1");
		}
		if !self.sandboxed {
			for name in &self.impure_env_vars {
				let value = settings
					.impure_env
					.iter()
					.find(|(k, _)| k == name)
					.map(|(_, v)| v.clone())
					.or_else(|| std::env::var(name).ok())
					.unwrap_or_default();
				set(name, &value);
			}
		}
		set("NIX_LOG_FD", "2");
		set("TERM", "xterm-256color");
		Ok(out)
	}

	/// `Store::exportReferences`: the closure of `paths`, which must be
	/// inputs, with the closures of the outputs of derivations in it.
	fn export_references(
		&self,
		paths: &BTreeSet<StorePath>,
		input_paths: &BTreeSet<StorePath>,
	) -> Result<BTreeSet<StorePath>> {
		let store = self.builder.store;
		let mut closure = BTreeSet::new();
		for path in paths {
			if !input_paths.contains(path) {
				return Err(BuildError::failed(
					FailureStatus::InputRejected,
					format!(
						"cannot export references of path '{}' because it is not in the input closure of the derivation",
						print(path)
					),
				));
			}
			self.builder.closure(path, &mut closure)?;
		}
		for path in closure.clone() {
			if path.name().ends_with(".drv") {
				for output in store
					.query_partial_derivation_output_map(&path)?
					.into_values()
				{
					let Some(output) = output else {
						return Err(BuildError::Other(
							"exportReferences on CA derivations is not yet implemented".to_owned(),
						));
					};
					self.builder.closure(&output, &mut closure)?;
				}
			}
		}
		Ok(closure)
	}

	fn query(&self, path: &StorePath) -> Result<ValidPathInfo> {
		self.builder
			.store
			.query_path_info(path)?
			.ok_or_else(|| BuildError::Other(format!("path '{}' is not valid", print(path))))
	}

	/// `pathInfoToJSON` from `parsed-derivations.cc`.
	fn path_info_json(&self, paths: &BTreeSet<StorePath>) -> Result<Value> {
		let mut list = Vec::new();
		for path in paths {
			let info = self.query(path)?;
			let mut closure = BTreeSet::new();
			self.builder.closure(path, &mut closure)?;
			let mut closure_size = 0;
			for p in &closure {
				closure_size += self.query(p)?.nar_size;
			}
			let mut object = Map::new();
			object.insert("narHash".to_owned(), Value::String(info.nar_hash_nix32()));
			object.insert("narSize".to_owned(), info.nar_size.into());
			object.insert(
				"references".to_owned(),
				Value::Array(
					info.references
						.iter()
						.map(|r| Value::String(print(r)))
						.collect(),
				),
			);
			if let Some(ca) = &info.ca {
				object.insert("ca".to_owned(), Value::String(ca.to_string()));
			}
			object.insert("path".to_owned(), Value::String(print(path)));
			object.insert("valid".to_owned(), Value::Bool(true));
			object.insert("closureSize".to_owned(), closure_size.into());
			list.push(Value::Object(object));
		}
		Ok(Value::Array(list))
	}

	/// `makeValidityRegistration(exportReferences(paths, inputPaths), false, false)`.
	fn validity_registration(
		&self,
		paths: &BTreeSet<StorePath>,
		input_paths: &BTreeSet<StorePath>,
	) -> Result<String> {
		let store = self.builder.store;
		let closure = self.export_references(paths, input_paths)?;
		let mut s = String::new();
		for path in &closure {
			let info = store
				.query_path_info(path)?
				.ok_or_else(|| BuildError::Other(format!("path '{}' is not valid", print(path))))?;
			s += &format!("{}\n\n{}\n", print(path), info.references.len());
			for r in &info.references {
				s += &print(r);
				s.push('\n');
			}
		}
		Ok(s)
	}

	/// `getPathsInSandbox` and `ChrootDerivationBuilder::prepareSandbox`:
	/// the host paths the builder sees.
	fn chroot_binds(&self, input_paths: &BTreeSet<StorePath>) -> Result<Vec<Bind>> {
		let settings = self.builder.settings;
		let store = self.builder.store;
		let mut binds = ChrootBinds::default();
		if self.direct {
			return Ok(Vec::new());
		}
		for path in &settings.sandbox_paths {
			binds.add(Bind {
				target: PathBuf::from(&path.target),
				source: PathBuf::from(&path.source),
				optional: path.optional,
			});
		}
		// Store paths made visible bring their closures.
		let mut closure = BTreeSet::new();
		for path in &settings.sandbox_paths {
			let in_store = Path::new(&path.source)
				.strip_prefix(store_dir())
				.ok()
				.and_then(|rest| rest.iter().next())
				.and_then(|name| StorePath::from_bytes(name.as_encoded_bytes()).ok());
			if let Some(source) = in_store
				&& store.is_valid_path(&source)?
			{
				self.builder.closure(&source, &mut closure)?;
			}
		}
		for path in closure {
			let path = PathBuf::from(print(&path));
			binds.set(Bind {
				target: path.clone(),
				source: path,
				optional: false,
			});
		}
		let build_top = self.build_top();
		if store_dir()
			.as_bytes()
			.starts_with(build_top.as_os_str().as_encoded_bytes())
		{
			return Err(BuildError::Other(
				"`sandbox-build-dir` must not contain the storeDir".to_owned(),
			));
		}
		binds.set(Bind {
			target: build_top,
			source: self.tmp_dir.clone(),
			optional: false,
		});
		for path in input_paths {
			binds.set(Bind {
				target: PathBuf::from(print(path)),
				source: store.real_path(path),
				optional: false,
			});
		}
		for output in self.scratch.values() {
			binds.remove(Path::new(&print(output)));
		}
		// `ssl-cert-file`, for builders with access to the network.
		if !self.sandboxed
			&& let Some(file) = settings.ca_file().filter(|file| file.exists())
		{
			binds.add(Bind {
				target: PathBuf::from("/etc/ssl/certs/ca-certificates.crt"),
				source: fs::canonicalize(file)?,
				optional: true,
			});
		}
		Ok(binds.iter().cloned().collect())
	}

	fn run(
		&self,
		drv: &UnverifiedDerivation,
		env: &BTreeMap<BString, BString>,
		kind: BuildKind,
		outputs: &BTreeMap<String, KnownOutput>,
		input_paths: &BTreeSet<StorePath>,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<()> {
		let settings = self.builder.settings;

		let rewrites = &self.input_rewrites;
		let mut env = self.environment(drv, env, input_paths, rewrites)?;

		// `ExternalDerivationBuilder::startChild`: the program is told about
		// the build in `TOP/build.json` and runs outside the sandbox.
		let external = match &self.external {
			Some(_) if self.recursive => {
				return Err(BuildError::Other(
					"'recursive-nix' is not supported yet by external derivation builders"
						.to_owned(),
				));
			}
			Some(external) => {
				let json = self.external_description(drv, &env, input_paths)?;
				let file = self.top_tmp_dir.join("build.json");
				fs::write(&file, json)?;
				if let Some(user) = &self.build_user {
					user.chown(&self.top_tmp_dir)?;
				}
				let mut args: Vec<std::ffi::OsString> =
					external.args.iter().map(Into::into).collect();
				args.push(file.into_os_string());
				Some(sandbox::External {
					program: external.program.clone(),
					args,
				})
			}
			None => None,
		};

		let binds = if external.is_some() {
			Vec::new()
		} else {
			self.chroot_binds(input_paths)?
		};

		// `startDaemon`: a socket for recursive Nix calls.
		let recursive = if self.recursive {
			Some(self.start_daemon(input_paths, &mut env)?)
		} else {
			None
		};

		let builtin = match external {
			Some(_) => None,
			None => self.builtin(drv, kind, outputs)?,
		};
		let spec = SandboxSpec {
			external,
			drv_path: print(&self.drv_path),
			builtin: builtin.as_deref(),
			chroot_root: self.chroot_root.clone(),
			chroot: !self.direct,
			store_dir: PathBuf::from(store_dir()),
			binds,
			sandboxed: self.sandboxed,
			build_user: self.build_user.as_ref().map(|u| (u.uid, u.gid)),
			build_user_groups: self
				.build_user
				.as_ref()
				.map(|u| u.supplementary_gids.clone())
				.unwrap_or_default(),
			kvm: settings.system_features.contains("kvm"),
			shm_size: settings.sandbox_shm_size.clone(),
			allow_new_privileges: settings.allow_new_privileges,
			require_drop_supplementary_groups: settings.require_drop_supplementary_groups,
			filter_syscalls: settings.filter_syscalls,
			// An external builder starts in the build directory on the host.
			build_dir_in_sandbox: match &self.external {
				Some(_) => self.tmp_dir.clone(),
				None => self.build_top(),
			},
			builder: drv.command().to_vec(),
			args: drv
				.arguments()
				.iter()
				.map(|a| rewrite_strings(a.to_vec(), rewrites))
				.collect(),
			env: env
				.iter()
				.map(|(k, v)| {
					let mut kv = k.to_vec();
					kv.push(b'=');
					kv.extend_from_slice(v);
					rewrite_strings(kv, rewrites)
				})
				.collect(),
			max_silent_time: settings.max_silent_time,
			timeout: settings.timeout,
		};
		let mut log_file = self.open_log_file()?;
		let mut tail = LogTail::new(settings.log_lines);
		let mut write_error = None;
		let max_log_size = settings.max_log_size();
		let log_size = Cell::new(0u64);
		let too_long = || max_log_size != 0 && log_size.get() > max_log_size;
		// The build is reported started once its environment is set up, as
		// `DerivationBuildingGoal` starts its activity after `startBuild`.
		let events = std::cell::RefCell::new(events);
		let mut run = || {
			sandbox::run_cancellable(
				&spec,
				&mut || {
					(events.borrow_mut())(BuildEvent::Started {
						drv_path: &self.drv_path,
						mode: self.builder.mode,
					})
				},
				&mut |data: &[u8]| {
					log_size.set(log_size.get() + data.len() as u64);
					if too_long() {
						return;
					}
					tail.feed(data);
					if let Some(file) = &mut log_file
						&& let Err(e) = file.write_all(data)
					{
						write_error.get_or_insert(e);
					}
					(events.borrow_mut())(BuildEvent::Output(data));
				},
				&|| too_long() || self.builder.tick(),
			)
		};
		let status = match recursive {
			Some((listener, context)) => serve_recursive(listener, &context, run),
			None => run(),
		};
		let status = status.map_err(|e| self.builder.auto_gc_error(e));
		// `killSandbox`: nothing the builder started may outlive it.
		if let Some(user) = &self.build_user {
			user.kill()?;
		}
		tail.flush();
		if let Some(file) = log_file {
			file.finish()?;
		}
		if let Some(e) = write_error {
			return Err(e.into());
		}
		if too_long() {
			return Err(log_too_long(&self.drv_path, max_log_size));
		}
		let status = status?;
		if !status.success() {
			return Err(self.failure(&status, &tail));
		}
		Ok(())
	}

	/// The JSON document `ExternalDerivationBuilder::startChild` writes to
	/// `build.json`, as nlohmann dumps it: compact, with sorted keys. The
	/// arguments and environment have the input rewrites applied, the
	/// builder does not, and the outputs are the scratch paths.
	fn external_description(
		&self,
		drv: &UnverifiedDerivation,
		env: &BTreeMap<BString, BString>,
		input_paths: &BTreeSet<StorePath>,
	) -> Result<String> {
		// nlohmann refuses to dump a string that is not UTF-8.
		let text = |bytes: &[u8]| -> Result<Value> {
			match std::str::from_utf8(bytes) {
				Ok(s) => Ok(Value::String(s.to_owned())),
				Err(error) => {
					let index = error.valid_up_to();
					Err(BuildError::Other(format!(
						"[json.exception.type_error.316] invalid UTF-8 byte at index {index}: 0x{:02X}",
						bytes[index]
					)))
				}
			}
		};
		let path = |path: &Path| text(path.as_os_str().as_encoded_bytes());
		let rewrites = &self.input_rewrites;
		let args = drv
			.arguments()
			.iter()
			.map(|arg| text(&rewrite_strings(arg.to_vec(), rewrites)))
			.collect::<Result<Vec<_>>>()?;
		let mut environment = Map::new();
		for (name, value) in env {
			let Value::String(name) = text(name)? else {
				unreachable!()
			};
			environment.insert(name, text(&rewrite_strings(value.to_vec(), rewrites))?);
		}
		// `StorePathSet` is ordered by the paths' base names.
		let mut inputs: Vec<String> = input_paths.iter().map(print).collect();
		inputs.sort();
		let outputs: Map<String, Value> = self
			.scratch
			.iter()
			.map(|(name, path)| (name.clone(), Value::String(print(path))))
			.collect();
		let mut json = Map::new();
		json.insert("version".to_owned(), Value::from(1));
		json.insert("builder".to_owned(), text(drv.command())?);
		json.insert("args".to_owned(), Value::Array(args));
		json.insert("env".to_owned(), Value::Object(environment));
		json.insert("topTmpDir".to_owned(), path(&self.top_tmp_dir)?);
		json.insert("tmpDir".to_owned(), path(&self.tmp_dir)?);
		json.insert("tmpDirInSandbox".to_owned(), path(&self.build_top())?);
		json.insert("storeDir".to_owned(), Value::String(store_dir().to_owned()));
		json.insert(
			"realStoreDir".to_owned(),
			path(&self.builder.store.config().real_store_dir)?,
		);
		json.insert("system".to_owned(), text(drv.system())?);
		json.insert(
			"inputPaths".to_owned(),
			Value::Array(inputs.into_iter().map(Value::String).collect()),
		);
		json.insert("outputs".to_owned(), Value::Object(outputs));
		Ok(Value::Object(json).to_string())
	}

	/// `startDaemon`: create the recursive Nix socket in the build directory,
	/// readable and writable only by the builder, and name it in
	/// `NIX_REMOTE`.
	fn start_daemon(
		&self,
		input_paths: &BTreeSet<StorePath>,
		env: &mut BTreeMap<BString, BString>,
	) -> Result<(
		std::os::unix::net::UnixListener,
		crate::recursive::Context<'b>,
	)> {
		use crate::recursive::{Context, Restriction, SOCKET_NAME};
		use std::os::unix::fs::PermissionsExt;
		let settings = self.builder.settings;
		if !settings
			.config
			.experimental_features()
			.contains("recursive-nix")
		{
			return Err(BuildError::Other("experimental Nix feature 'recursive-nix' is disabled; add '--extra-experimental-features recursive-nix' to enable it".to_owned()));
		}
		if crate::recursive::server().is_none() {
			return Err(BuildError::Other(
				"this program cannot serve recursive Nix calls".to_owned(),
			));
		}
		// Sandboxed builders see only the paths bound into their chroot;
		// paths added while they run are not bound in yet.
		let add_to_sandbox: Option<crate::recursive::AddToSandbox> = if self.direct {
			None
		} else {
			Some(Box::new(|path: &StorePath| {
				Err(format!(
					"adding store path '{}' to the sandbox is not implemented (recursive-nix)",
					print(path)
				))
			}))
		};
		let restriction = Arc::new(Restriction::new(input_paths.clone(), add_to_sandbox));
		*self.restriction.borrow_mut() = Some(restriction.clone());
		let socket = self.tmp_dir.join(SOCKET_NAME);
		let listener = noxi_store::unix_socket::bind(&socket).map_err(|e| {
			BuildError::Other(format!("cannot bind to socket '{}': {e}", socket.display()))
		})?;
		fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
		if let Some(user) = &self.build_user {
			user.chown(&socket)?;
		}
		env.insert(
			"NIX_REMOTE".into(),
			format!("unix://{}/{SOCKET_NAME}", self.build_top().display()).into(),
		);
		let context = Context {
			store: self.builder.store,
			settings,
			substituters: self
				.builder
				.substituters
				.iter()
				.map(|cache| cache.uri().to_owned())
				.collect(),
			restriction,
		};
		Ok((listener, context))
	}

	/// A builtin builder, with what it needs of this process. It runs in the
	/// builder's process, so it sees paths as a builder does.
	#[allow(clippy::type_complexity)]
	fn builtin(
		&self,
		drv: &UnverifiedDerivation,
		kind: BuildKind,
		outputs: &BTreeMap<String, KnownOutput>,
	) -> Result<Option<Box<dyn Fn(&mut dyn FnMut(&str)) -> std::result::Result<(), String>>>> {
		let Some(name) = drv.command().strip_prefix(b"builtin:") else {
			return Ok(None);
		};
		let settings = self.builder.settings;
		let env: BTreeMap<String, String> = drv
			.environment()
			.iter()
			.map(|(k, v)| (k.to_str_lossy().into_owned(), v.to_str_lossy().into_owned()))
			.collect();
		let Some(out) = self.scratch.get("out") else {
			let message = format!("'{}' requires an 'out' output", drv.command());
			return Ok(Some(Box::new(move |_| Err(message.clone()))));
		};
		let out = PathBuf::from(print(out));
		Ok(Some(match name {
			b"unpack-channel" => Box::new(move |_| {
				crate::unpack_channel::build(&env, &out, &Path::to_path_buf).map_err(|e| e.0)
			}),
			b"fetchurl" => {
				if kind.sandboxed() {
					return Ok(Some(Box::new(|_| {
						Err(
							"'builtin:fetchurl' must be a fixed-output or impure derivation"
								.to_owned(),
						)
					})));
				}
				// What the builder may not be able to read.
				let netrc = fs::read(&settings.netrc_file).unwrap_or_default();
				let ca_certificates = settings
					.ca_file()
					.and_then(|file| fs::read(file).ok())
					.unwrap_or_default();
				let fetch = crate::fetchurl::Fetch {
					env,
					out,
					flat_hash: outputs.get("out").and_then(|output| match &output.fixed {
						Some((FileIngestionMethod::Flat, hash)) => Some(hash.clone()),
						_ => None,
					}),
					hashed_mirrors: settings.hashed_mirrors.clone(),
					options: noxi_store::download::Options {
						transfer: settings.transfer,
						netrc: noxi_store::netrc::Netrc::parse(&String::from_utf8_lossy(&netrc)),
						ca_certificates,
						stalled_timeout: settings.stalled_download_timeout,
					},
					ca_copy: self.build_top().join("ca-certificates.crt"),
				};
				Box::new(move |log| fetch.run(log).map_err(|e| e.0))
			}
			_ => Box::new(move |log| {
				crate::buildenv::builtin_buildenv(&env, &out, &Path::to_path_buf, &mut |warning| {
					log(&format!("warning: {warning}\n"))
				})
				.map_err(|e| e.0)
			}),
		}))
	}

	/// `handleDiffHook`: show what a nondeterministic output's rebuild
	/// changed. The hook runs as the build user, as it reads what the
	/// builder wrote; its failure is only logged.
	fn run_diff_hook(&self, hook: &str, registered: &str, rebuilt: &Path) {
		use nix_compat::log::{VerbosityLevel, logger};
		use std::os::unix::process::CommandExt;
		let mut command = std::process::Command::new(hook);
		command
			.arg(registered)
			.arg(rebuilt)
			.arg(print(&self.drv_path))
			.arg(&self.tmp_dir)
			.current_dir("/")
			.stdin(std::process::Stdio::null())
			.stderr(std::process::Stdio::inherit());
		if let Some(user) = &self.build_user {
			command.uid(user.uid).gid(user.gid);
		}
		let failed = match command.output() {
			Err(error) => Some(format!("executing \"{hook}\": {error}")),
			Ok(output) if output.status.success() => {
				let text = String::from_utf8_lossy(&output.stdout);
				if !text.is_empty() {
					logger::log(VerbosityLevel::Error, text.trim_end());
				}
				None
			}
			Ok(output) => Some(format!(
				"diff-hook program \"{hook}\" {}",
				sandbox::ExitStatus(std::os::unix::process::ExitStatusExt::into_raw(
					output.status
				))
				.describe()
			)),
		};
		if let Some(failed) = failed {
			let message = format!("diff hook execution failed: {failed}");
			logger::log_error(
				&format!(
					"{}error:{} {message}",
					logger::ANSI_RED,
					logger::ANSI_NORMAL
				),
				&message,
			);
		}
	}

	/// The failure of a builder that exited with `status`, with the log
	/// tail CppNix shows.
	fn failure(&self, status: &sandbox::ExitStatus, tail: &LogTail) -> BuildError {
		let msg = builder_failure(
			self.builder.settings,
			&self.drv_path,
			&self.outputs,
			status,
			tail,
		);
		BuildError::failed(
			if self.sandboxed {
				FailureStatus::PermanentFailure
			} else {
				FailureStatus::TransientFailure
			},
			msg,
		)
	}

	/// `LogFile`: `LOGDIR/drvs/XX/REST`, compressed with bzip2 unless
	/// `compress-build-log` is off.
	fn open_log_file(&self) -> Result<Option<LogFile>> {
		open_log_file(
			self.builder.settings,
			&self.builder.store.config().log_dir,
			&self.drv_path,
		)
	}

	fn cleanup(&self) {
		use std::os::unix::fs::PermissionsExt;
		let _ = fs::set_permissions(&self.top_tmp_dir, fs::Permissions::from_mode(0o000));
		if self.keep_failed && !self.succeeded.get() {
			nix_compat::log::logger::log(
				nix_compat::log::VerbosityLevel::Error,
				&format!(
					"note: keeping build directory \"{}\"",
					self.tmp_dir.display()
				),
			);
			for dir in [&self.top_tmp_dir, &self.tmp_dir] {
				let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
			}
		} else {
			let _ = delete_path(&self.top_tmp_dir);
		}
		for path in &self.redirected {
			let _ = delete_path(&self.builder.store.real_path(path));
		}
		let _ = delete_path(&self.chroot_parent);
	}
}

impl Drop for Build<'_> {
	fn drop(&mut self) {
		self.cleanup();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn rewrites_apply_in_order() {
		let rewrites = BTreeMap::from([
			("a".to_owned(), "b".to_owned()),
			("b".to_owned(), "c".to_owned()),
		]);
		assert_eq!(rewrite_strings(b"ab".to_vec(), &rewrites), b"cc");
	}

	#[test]
	fn output_path_names() {
		assert_eq!(output_path_name("hello", "out"), "hello");
		assert_eq!(output_path_name("hello", "dev"), "hello-dev");
	}
}

/// Run the builder with `run` while serving its recursive Nix socket, as
/// `startDaemon`'s thread does: one thread accepts connections and each
/// connection is served by its own thread. When the builder is done the
/// socket is closed and every connection shut down, as `stopDaemon` does,
/// so a process the builder left behind cannot keep a worker alive.
fn serve_recursive<T>(
	listener: std::os::unix::net::UnixListener,
	context: &crate::recursive::Context<'_>,
	run: impl FnOnce() -> T,
) -> T {
	use std::sync::atomic::{AtomicBool, Ordering};
	let server = crate::recursive::server().expect("checked by start_daemon");
	let stop = AtomicBool::new(false);
	let connections: Mutex<Vec<std::os::unix::net::UnixStream>> = Mutex::new(Vec::new());
	std::thread::scope(|scope| {
		let (stop, connections) = (&stop, &connections);
		scope.spawn(move || {
			if listener.set_nonblocking(true).is_err() {
				return;
			}
			while !stop.load(Ordering::Relaxed) {
				match listener.accept() {
					Ok((stream, _)) => {
						if stream.set_nonblocking(false).is_err() {
							continue;
						}
						// Under the lock, so that a connection accepted as the
						// builder finishes is shut down too.
						let mut open = connections.lock().unwrap_or_else(|e| e.into_inner());
						if stop.load(Ordering::Relaxed) {
							let _ = stream.shutdown(std::net::Shutdown::Both);
							break;
						}
						match stream.try_clone() {
							Ok(copy) => open.push(copy),
							Err(_) => continue,
						}
						drop(open);
						tracing::debug!("received daemon connection");
						scope.spawn(move || server(stream, context));
					}
					Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
						std::thread::sleep(std::time::Duration::from_millis(20));
					}
					Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
					Err(e) => {
						tracing::debug!("accepting connection: {e}");
						break;
					}
				}
			}
		});
		let result = run();
		let mut open = connections.lock().unwrap_or_else(|e| e.into_inner());
		stop.store(true, Ordering::Relaxed);
		for stream in open.drain(..) {
			let _ = stream.shutdown(std::net::Shutdown::Both);
		}
		result
	})
}
