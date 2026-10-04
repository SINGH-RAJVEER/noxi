//! Realising derivations in a local store, following CppNix 2.35.2's
//! `DerivationBuildingGoal`, `DerivationBuilderImpl` and
//! `ChrootLinuxDerivationBuilder`.
//!
//! Input derivations are realised first, depth first. Each build runs in
//! the Linux sandbox with only its input closure visible, then its outputs
//! are canonicalised, scanned for references, checked and registered with
//! the derivation as their deriver.
//!
//! Not supported yet: builds without a sandbox, `builtin:` builders,
//! content-addressed floating, impure and deferred outputs,
//! remote builders, and `--check` and `--repair` modes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use bstr::{BString, ByteSlice};
use nix_compat::derivation::{OutputHashMode, UnverifiedDerivation};
use nix_compat::nixbase32;
use nix_compat::nixhash::{CAHash, NixHash};
use nix_compat::store_path::{self, STORE_DIR, StorePath, hash_placeholder};
use nix_rs_store::binary_cache::BinaryCache;
use nix_rs_store::canonicalise::canonicalise_path_metadata;
use nix_rs_store::lock::PathLocks;
use nix_rs_store::{LocalStore, ValidPathInfo, ca_hash_of, delete_path, nar};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::{BuildError, FailureStatus, Result};
use crate::options::{self, DerivationOptions, DrvRef, OutputChecks, OutputChecksSpec};
use crate::refscan::scan_for_references;
use crate::sandbox::{self, Bind, SandboxSpec};
use crate::settings::{BuildSettings, SandboxMode};
use crate::structured;

/// `DerivationBuilderImpl::homeDir`.
const HOME_DIR: &str = "/homeless-shelter";

fn print(path: &StorePath) -> String {
	path.to_absolute_path()
}

/// What happens while realising, for callers that report progress the
/// way CppNix's build activities do.
#[derive(Debug)]
pub enum BuildEvent<'a> {
	/// A derivation is about to be built.
	Started { drv_path: &'a StorePath },
	/// Output from the builder.
	Output(&'a [u8]),
	/// The build ended, successfully or with `error`.
	Finished {
		drv_path: &'a StorePath,
		error: Option<&'a BuildError>,
	},
}

/// Builds derivations into one local store.
pub struct LocalBuilder<'a> {
	store: &'a LocalStore,
	settings: &'a BuildSettings,
	substituters: &'a [BinaryCache],
}

/// An output a derivation declares, with the path it must produce.
struct KnownOutput {
	path: StorePath,
	/// The expected content address of a fixed output.
	fixed: Option<(OutputHashMode, NixHash)>,
}

impl<'a> LocalBuilder<'a> {
	pub fn new(store: &'a LocalStore, settings: &'a BuildSettings) -> Self {
		Self {
			store,
			settings,
			substituters: &[],
		}
	}

	/// Substitute from `caches` before building, as CppNix does with
	/// `substituters`.
	pub fn with_substituters(mut self, caches: &'a [BinaryCache]) -> Self {
		self.substituters = caches;
		self
	}

	/// Try to substitute `path`; a failure is logged, not returned.
	fn try_substitute(&self, path: &StorePath) -> Result<bool> {
		if !self.settings.use_substitutes || self.substituters.is_empty() {
			return Ok(false);
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
			return Ok(false);
		}
		match self.store.substitute(self.substituters, path) {
			Ok(()) => Ok(true),
			Err(error) if !self.settings.try_fallback => Err(error.into()),
			Err(e) => {
				tracing::debug!("cannot substitute '{}': {e}", print(path));
				Ok(false)
			}
		}
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
		if stack.contains(drv_path) {
			return Err(BuildError::Other(format!(
				"cycle detected in the input derivations of '{}'",
				print(drv_path)
			)));
		}
		let drv = self.store.read_derivation(drv_path)?;
		let outputs = known_outputs(drv_path, &drv)?;
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
			.map(|(name, output)| {
				(
					name.clone(),
					KnownOutput {
						path: output.path.clone(),
						fixed: output.fixed.clone(),
					},
				)
			})
			.collect();

		// Like `DerivationGoal`, substitute missing outputs before
		// realising any inputs, which then may not be needed at all.
		if self.settings.use_substitutes
			&& !self.substituters.is_empty()
			&& !self.all_valid(&wanted_outputs)?
			&& self.substitutes_allowed(&drv)?
		{
			for output in wanted_outputs.values() {
				if !self.store.is_valid_path(&output.path)? {
					self.try_substitute(&output.path)?;
				}
			}
		}
		if !self.all_valid(&wanted_outputs)? {
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
				self.realise_inner(input, Some(&names), done, stack, events)?;
			}
			stack.pop();

			if !self.all_valid(&wanted_outputs)? {
				if self.settings.max_build_jobs == 0 {
					return Err(BuildError::failed(
						FailureStatus::InputRejected,
						format!(
							"Cannot build '{}'.\nReason: local builds are disabled (max-jobs = 0)\nHint: set 'max-jobs' to a non-zero value to enable local builds, or configure remote builders via 'builders'",
							print(drv_path)
						),
					));
				}
				events(BuildEvent::Started { drv_path });
				let result = self.build(drv_path, &drv, &outputs, events);
				events(BuildEvent::Finished {
					drv_path,
					error: result.as_ref().err(),
				});
				result?;
			}
		}
		done.entry(drv_path.clone()).or_default().extend(wanted);
		Ok(all_paths(&outputs))
	}

	fn all_valid(&self, outputs: &BTreeMap<String, KnownOutput>) -> Result<bool> {
		for output in outputs.values() {
			if !self.store.is_valid_path(&output.path)? {
				return Ok(false);
			}
		}
		Ok(true)
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

	/// Check that this machine can build `drv` (`checkCanBuildLocally`).
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
		let missing: Vec<&str> = features
			.iter()
			.filter(|f| !self.settings.system_features.contains(*f))
			.map(String::as_str)
			.collect();
		if !bad_platform && missing.is_empty() {
			return Ok(());
		}
		let mut msg = format!("Cannot build '{}'.", print(drv_path));
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
				self.settings
					.system_features
					.iter()
					.map(String::as_str)
					.collect::<Vec<_>>()
					.join(", ")
			);
		}
		Err(BuildError::failed(FailureStatus::InputRejected, msg))
	}

	fn build(
		&self,
		drv_path: &StorePath,
		drv: &UnverifiedDerivation,
		outputs: &BTreeMap<String, KnownOutput>,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<()> {
		let drv_name = drv_path
			.name()
			.strip_suffix(".drv")
			.unwrap_or(drv_path.name());
		let is_fixed = drv.outputs().is_fixed();

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

		let features = drv_options.get_required_system_features(true);
		self.check_platform(drv_path, drv, &features)?;
		let builtin = drv.command().strip_prefix(b"builtin:");
		if let Some(name) = builtin.filter(|name| *name != b"buildenv") {
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

		// The closure of the input sources and of the input derivations'
		// requested outputs.
		let mut input_paths = BTreeSet::new();
		for (input_drv, names) in drv.input_derivations() {
			let input = self.store.read_derivation(input_drv)?;
			for name in names {
				let path = input.outputs().get(name).ok_or_else(|| {
					BuildError::Other(format!(
						"derivation '{}' has no output '{name}'",
						print(input_drv)
					))
				})?;
				self.closure(path, &mut input_paths)?;
			}
		}
		for src in drv.input_sources() {
			self.closure(src, &mut input_paths)?;
		}

		// Lock the outputs, then check again whether another process built
		// them while we waited.
		let real_outputs: Vec<PathBuf> = outputs
			.values()
			.map(|o| self.store.real_path(&o.path))
			.collect();
		let mut locks = PathLocks::lock(real_outputs.clone())?;
		for output in outputs.values() {
			self.store.add_temp_root(&output.path)?;
		}
		if self.all_valid(outputs)? {
			locks.set_deletion(true);
			return Ok(());
		}
		for (output, real) in outputs.values().zip(&real_outputs) {
			if !self.store.is_valid_path(&output.path)? && fs::symlink_metadata(real).is_ok() {
				tracing::debug!("removing invalid path '{}'", print(&output.path));
				delete_path(real)?;
			}
		}

		let diverted = self.store.config().real_store_dir != Path::new(STORE_DIR);
		let use_sandbox = diverted
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
				SandboxMode::Relaxed => !is_fixed && !drv_options.no_chroot,
			};
		if builtin.is_some() {
			// CppNix runs builtin builders in the builder's process, in the
			// sandbox when there is one and in the store otherwise; their
			// outputs are registered as any build's are.
			let mut build = Build::start(self, drv_path, outputs, &drv_options, parsed, is_fixed)?;
			build.direct = !use_sandbox;
			let result = build.run_buildenv(drv, outputs, events).and_then(|()| {
				self.register_outputs(
					drv_path,
					drv_name,
					outputs,
					&drv_options,
					&input_paths,
					&build,
				)
			});
			build.cleanup();
			result?;
			locks.set_deletion(true);
			return Ok(());
		}
		if !use_sandbox {
			return Err(BuildError::Other(format!(
				"cannot build '{}': nix-rs only builds in the sandbox so far",
				print(drv_path)
			)));
		}

		let build = Build::start(self, drv_path, outputs, &drv_options, parsed, is_fixed)?;
		let result = build.run(drv, &env, &input_paths, events).and_then(|()| {
			self.register_outputs(
				drv_path,
				drv_name,
				outputs,
				&drv_options,
				&input_paths,
				&build,
			)
		});
		build.cleanup();
		result?;
		locks.set_deletion(true);
		Ok(())
	}

	/// `DerivationBuilderImpl::registerOutputs`.
	fn register_outputs(
		&self,
		drv_path: &StorePath,
		drv_name: &str,
		outputs: &BTreeMap<String, KnownOutput>,
		drv_options: &DerivationOptions<StorePath>,
		input_paths: &BTreeSet<StorePath>,
		build: &Build,
	) -> Result<()> {
		let mut referenceable = input_paths.clone();
		referenceable.extend(outputs.values().map(|o| o.path.clone()));

		// Check each output exists and find its references.
		let mut scanned: BTreeMap<&str, (PathBuf, BTreeSet<StorePath>, fs::Metadata)> =
			BTreeMap::new();
		for (name, output) in outputs {
			if self.store.is_valid_path(&output.path)? {
				continue;
			}
			let actual = build.path_in_chroot(&output.path);
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
			if !metadata.file_type().is_symlink() && metadata.mode() & 0o022 != 0 {
				return Err(BuildError::failed(
					FailureStatus::OutputRejected,
					format!(
						"suspicious ownership or permission on \"{}\" for output '{name}'; rejecting this build output",
						actual.display()
					),
				));
			}
			canonicalise_path_metadata(&actual, None)?;
			let references = if drv_options.unsafe_discard_references.get(name) == Some(&true) {
				BTreeSet::new()
			} else {
				scan_for_references(&actual, &referenceable)?
			};
			scanned.insert(name, (actual, references, metadata));
		}

		// Register outputs that refer to other outputs after those.
		let by_path: BTreeMap<&StorePath, &str> = outputs
			.iter()
			.map(|(name, o)| (&o.path, name.as_str()))
			.collect();
		let order = topo_sort(drv_path, &scanned, &by_path)?;

		let mut infos: BTreeMap<String, ValidPathInfo> = BTreeMap::new();
		let mut hash_mismatch = None;
		for name in order {
			let output = &outputs[name];
			let (actual, references, metadata) = &scanned[name];
			let mut self_ref = false;
			let mut others = BTreeSet::new();
			for r in references {
				if *r == output.path {
					self_ref = true;
				} else {
					others.insert(r.clone());
				}
			}

			let info = match &output.fixed {
				None => {
					let (nar_hash, nar_size) = nar::hash_path(actual)?;
					let mut info = ValidPathInfo::new(output.path.clone(), nar_hash);
					info.nar_size = nar_size;
					info.references = others;
					if self_ref {
						info.references.insert(output.path.clone());
					}
					let real = self.store.real_path(&output.path);
					move_path(actual, &real)?;
					info
				}
				Some((mode, wanted)) => {
					let recursive = matches!(mode, OutputHashMode::Recursive);
					if !recursive && (!metadata.is_file() || metadata.mode() & 0o100 != 0) {
						return Err(BuildError::failed(
							FailureStatus::OutputRejected,
							format!(
								"output path \"{}\" should be a non-executable regular file since recursive hashing is not enabled (one of outputHashMode={{flat,text}} is true)",
								actual.display()
							),
						));
					}
					let claimed = if recursive {
						CAHash::Nar(wanted.clone())
					} else {
						CAHash::Flat(wanted.clone())
					};
					let got = ca_hash_of(actual, &claimed)?;
					let path = store_path::build_ca_path(
						&output_path_name(drv_name, name),
						recursive,
						got.hash().as_ref(),
						others.iter().map(StorePath::as_ref),
						self_ref,
					)
					.map_err(|e| BuildError::Other(e.to_string()))?
					.to_owned();
					let (nar_hash, nar_size) = nar::hash_path(actual)?;
					let mut info = ValidPathInfo::new(path.clone(), nar_hash);
					info.nar_size = nar_size;
					info.references = others;
					info.ca = Some(got.clone());
					let real = self.store.real_path(&path);
					if self.store.is_valid_path(&path)? {
						delete_path(actual)?;
					} else {
						delete_path(&real)?;
						move_path(actual, &real)?;
					}
					if got.hash() != claimed.hash() {
						// CppNix registers the wrongly hashed path so the
						// download need not be repeated, then fails.
						info.deriver = Some(drv_path.clone());
						info.ultimate = true;
						self.store.register_valid_path(&info)?;
						hash_mismatch.get_or_insert(format!(
							"hash mismatch in fixed-output derivation '{}':\n  specified: {}\n     got:    {}",
							print(drv_path),
							claimed.hash().to_sri_string(),
							got.hash().to_sri_string()
						));
					} else if !info.references.is_empty() {
						return Err(BuildError::failed(
							FailureStatus::HashMismatch,
							format!(
								"fixed-output derivations must not reference store paths: '{}' references {} distinct paths, e.g. '{}'",
								print(drv_path),
								info.references.len(),
								print(info.references.iter().next().unwrap())
							),
						));
					}
					info
				}
			};
			let real = self.store.real_path(&info.path);
			canonicalise_path_metadata(&real, None)?;
			let mut info = info;
			info.deriver = Some(drv_path.clone());
			info.ultimate = true;
			infos.insert(name.to_owned(), info);
		}
		if let Some(message) = hash_mismatch {
			return Err(BuildError::failed(FailureStatus::HashMismatch, message));
		}

		self.check_outputs(drv_path, drv_options, &infos)?;

		let infos: Vec<ValidPathInfo> = infos.into_values().collect();
		self.store.register_valid_paths(&infos)?;
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
fn known_outputs(
	drv_path: &StorePath,
	drv: &UnverifiedDerivation,
) -> Result<BTreeMap<String, KnownOutput>> {
	if drv.outputs().is_deferred() {
		return Err(BuildError::Other(format!(
			"cannot build '{}': its outputs are content-addressed or deferred, which nix-rs cannot build yet",
			print(drv_path)
		)));
	}
	if let Some((hash, path)) = drv.outputs().as_fixed_output() {
		return Ok(BTreeMap::from([(
			"out".to_owned(),
			KnownOutput {
				path: path.clone(),
				fixed: Some((hash.mode.clone(), hash.hash.clone())),
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
				},
			)
		})
		.collect())
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
	impure_env_vars: BTreeSet<String>,
	pass_as_file: BTreeSet<String>,
	export_references_graph: BTreeMap<String, BTreeSet<StorePath>>,
	structured: Option<Map<String, Value>>,
}

impl<'b> Build<'b> {
	fn start(
		builder: &'b LocalBuilder<'b>,
		drv_path: &StorePath,
		outputs: &BTreeMap<String, KnownOutput>,
		drv_options: &DerivationOptions<StorePath>,
		structured: Option<Map<String, Value>>,
		is_fixed: bool,
	) -> Result<Self> {
		let store = builder.store;
		let build_dir = builder
			.settings
			.build_dir
			.clone()
			.unwrap_or_else(|| store.config().state_dir.join("builds"));
		fs::create_dir_all(&build_dir)?;
		let top_tmp_dir = create_temp_dir(&build_dir, "nix")?;
		let tmp_dir = top_tmp_dir.join("build");
		fs::DirBuilder::new().mode(0o700).create(&tmp_dir)?;

		let mut chroot_parent = store.real_path(drv_path).into_os_string();
		chroot_parent.push(".chroot");
		let chroot_parent = PathBuf::from(chroot_parent);
		let chroot_root = sandbox::setup_chroot(&chroot_parent, Path::new(STORE_DIR), !is_fixed)?;
		Ok(Self {
			builder,
			drv_path: drv_path.clone(),
			outputs: outputs.values().map(|o| o.path.clone()).collect(),
			top_tmp_dir,
			tmp_dir,
			chroot_parent,
			chroot_root,
			direct: false,
			sandboxed: !is_fixed,
			impure_env_vars: drv_options.impure_env_vars.clone(),
			pass_as_file: drv_options.pass_as_file.clone(),
			export_references_graph: drv_options.export_references_graph.clone(),
			structured,
		})
	}

	fn path_in_chroot(&self, path: &StorePath) -> PathBuf {
		if self.direct {
			return self.builder.store.real_path(path);
		}
		self.chroot_root
			.join(STORE_DIR.trim_start_matches('/'))
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
		let build_top = settings.sandbox_build_dir.as_str();
		let mut out: BTreeMap<BString, BString> = BTreeMap::new();
		let mut set = |k: &str, v: &str| {
			out.insert(k.into(), v.into());
		};
		set("PATH", "/path-not-set");
		set("HOME", HOME_DIR);
		set("NIX_STORE", STORE_DIR);
		set("NIX_BUILD_CORES", &settings.effective_cores().to_string());

		if let Some(attrs) = &self.structured {
			// `StructuredAttrs::prepareStructuredAttrs`.
			let mut json = attrs.clone();
			let outputs: Map<String, Value> = drv
				.outputs()
				.iter()
				.map(|(name, _)| {
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
		if !self.sandboxed {
			set("NIX_OUTPUT_CHECKED", "1");
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
				let drv = store.read_derivation(&path)?;
				for output in drv.outputs().store_paths() {
					self.builder.closure(output, &mut closure)?;
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

	fn run(
		&self,
		drv: &UnverifiedDerivation,
		env: &BTreeMap<BString, BString>,
		input_paths: &BTreeSet<StorePath>,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<()> {
		let settings = self.builder.settings;
		let store = self.builder.store;

		// Placeholders for this derivation's outputs become their paths.
		let rewrites: BTreeMap<String, String> = drv
			.outputs()
			.iter()
			.map(|(name, path)| (hash_placeholder(&name.to_string()), print(path)))
			.collect();
		let env = self.environment(drv, env, input_paths, &rewrites)?;

		let mut binds: BTreeMap<PathBuf, Bind> = BTreeMap::new();
		for path in &settings.sandbox_paths {
			binds.insert(
				PathBuf::from(&path.target),
				Bind {
					target: PathBuf::from(&path.target),
					source: PathBuf::from(&path.source),
					optional: path.optional,
				},
			);
		}
		let build_top = PathBuf::from(&settings.sandbox_build_dir);
		binds.insert(
			build_top.clone(),
			Bind {
				target: build_top.clone(),
				source: self.tmp_dir.clone(),
				optional: false,
			},
		);
		for path in input_paths {
			let target = PathBuf::from(print(path));
			binds.insert(
				target.clone(),
				Bind {
					target,
					source: store.real_path(path),
					optional: false,
				},
			);
		}
		for output in &self.outputs {
			binds.remove(Path::new(&print(output)));
		}

		let spec = SandboxSpec {
			chroot_root: self.chroot_root.clone(),
			store_dir: PathBuf::from(STORE_DIR),
			binds: binds.into_values().collect(),
			sandboxed: self.sandboxed,
			kvm: settings.system_features.contains("kvm"),
			shm_size: settings.sandbox_shm_size.clone(),
			allow_new_privileges: settings.allow_new_privileges,
			filter_syscalls: settings.filter_syscalls,
			build_dir_in_sandbox: build_top,
			builder: drv.command().to_vec(),
			args: drv
				.arguments()
				.iter()
				.map(|a| rewrite_strings(a.to_vec(), &rewrites))
				.collect(),
			env: env
				.iter()
				.map(|(k, v)| {
					let mut kv = k.to_vec();
					kv.push(b'=');
					kv.extend_from_slice(v);
					rewrite_strings(kv, &rewrites)
				})
				.collect(),
			max_silent_time: settings.max_silent_time,
			timeout: settings.timeout,
		};
		let mut log_file = self.open_log_file()?;
		let mut tail = LogTail::new(settings.log_lines);
		let mut write_error = None;
		let status = sandbox::run(&spec, &mut |data: &[u8]| {
			tail.feed(data);
			if let Some(file) = &mut log_file
				&& let Err(e) = file.write_all(data)
			{
				write_error.get_or_insert(e);
			}
			events(BuildEvent::Output(data));
		});
		tail.flush();
		if let Some(file) = log_file {
			file.finish()?;
		}
		if let Some(e) = write_error {
			return Err(e.into());
		}
		let status = status?;
		if !status.success() {
			return Err(self.failure(&status, &tail));
		}
		Ok(())
	}

	/// `builtin:buildenv`, logged as a builder process is: its warnings and
	/// error go to the build log, and an error fails the build with exit
	/// code 1.
	fn run_buildenv(
		&self,
		drv: &UnverifiedDerivation,
		outputs: &BTreeMap<String, KnownOutput>,
		events: &mut dyn FnMut(BuildEvent<'_>),
	) -> Result<()> {
		let settings = self.builder.settings;
		let store = self.builder.store;
		let env: BTreeMap<String, String> = drv
			.environment()
			.iter()
			.map(|(k, v)| (k.to_str_lossy().into_owned(), v.to_str_lossy().into_owned()))
			.collect();
		let out = outputs.get("out").ok_or_else(|| {
			BuildError::Other("builtin:buildenv needs an output 'out'".to_owned())
		})?;
		let output_names: BTreeSet<String> = outputs.values().map(|o| o.path.to_string()).collect();
		let chroot_store = self.chroot_root.join(STORE_DIR.trim_start_matches('/'));
		let real_store = store.config().real_store_dir.clone();
		let real = |path: &Path| -> PathBuf {
			let Ok(rest) = path.strip_prefix(STORE_DIR) else {
				return path.to_owned();
			};
			let name = rest.iter().next().map(|n| n.to_string_lossy().into_owned());
			if !self.direct && name.is_some_and(|n| output_names.contains(&n)) {
				chroot_store.join(rest)
			} else {
				real_store.join(rest)
			}
		};
		let mut log_file = self.open_log_file()?;
		let mut tail = LogTail::new(settings.log_lines);
		let mut write_error = None;
		let mut log = |line: &str| {
			let data = format!("{line}\n");
			tail.feed(data.as_bytes());
			if let Some(file) = &mut log_file
				&& let Err(e) = file.write_all(data.as_bytes())
			{
				write_error.get_or_insert(e);
			}
			events(BuildEvent::Output(data.as_bytes()));
		};
		let result = crate::buildenv::builtin_buildenv(
			&env,
			Path::new(&print(&out.path)),
			&real,
			&mut |warning| log(&format!("warning: {warning}")),
		);
		if let Err(error) = &result {
			log(&nix_compat::log::format_error(&error.0));
		}
		tail.flush();
		if let Some(file) = log_file {
			file.finish()?;
		}
		if let Some(e) = write_error {
			return Err(e.into());
		}
		match result {
			Ok(()) => Ok(()),
			// The wait status of a process that exited with code 1.
			Err(_) => Err(self.failure(&sandbox::ExitStatus(1 << 8), &tail)),
		}
	}

	/// The failure of a builder that exited with `status`, with the log
	/// tail CppNix shows.
	fn failure(&self, status: &sandbox::ExitStatus, tail: &LogTail) -> BuildError {
		let settings = self.builder.settings;
		let mut msg = format!(
			"Cannot build '{}'.\nReason: builder {}.",
			print(&self.drv_path),
			status.describe()
		);
		let mut outputs = self.outputs.clone();
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
				print(&self.drv_path)
			);
		}
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
		let settings = self.builder.settings;
		if !settings.keep_build_log {
			return Ok(None);
		}
		let base = self.drv_path.to_string();
		let dir = self
			.builder
			.store
			.config()
			.log_dir
			.join("drvs")
			.join(&base[..2]);
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

	fn cleanup(&self) {
		use std::os::unix::fs::PermissionsExt;
		let _ = fs::set_permissions(&self.top_tmp_dir, fs::Permissions::from_mode(0));
		let _ = delete_path(&self.top_tmp_dir);
		let _ = delete_path(&self.chroot_parent);
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
