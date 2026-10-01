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
//! content-addressed floating, impure and deferred outputs, substitution,
//! remote builders, build logs in the log directory, and `--check` and
//! `--repair` modes.

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

/// Builds derivations into one local store.
pub struct LocalBuilder<'a> {
	store: &'a LocalStore,
	settings: &'a BuildSettings,
}

/// An output a derivation declares, with the path it must produce.
struct KnownOutput {
	path: StorePath,
	/// The expected content address of a fixed output.
	fixed: Option<(OutputHashMode, NixHash)>,
}

impl<'a> LocalBuilder<'a> {
	pub fn new(store: &'a LocalStore, settings: &'a BuildSettings) -> Self {
		Self { store, settings }
	}

	/// Realise every output of the derivation at `drv_path`, building its
	/// input derivations first, and return its output paths. Builder output
	/// is passed to `log`.
	pub fn realise(
		&self,
		drv_path: &StorePath,
		log: &mut dyn FnMut(&[u8]),
	) -> Result<BTreeMap<String, StorePath>> {
		let mut done = BTreeMap::new();
		self.realise_inner(drv_path, &mut done, &mut Vec::new(), log)
	}

	fn realise_inner(
		&self,
		drv_path: &StorePath,
		done: &mut BTreeMap<StorePath, BTreeMap<String, StorePath>>,
		stack: &mut Vec<StorePath>,
		log: &mut dyn FnMut(&[u8]),
	) -> Result<BTreeMap<String, StorePath>> {
		if let Some(outputs) = done.get(drv_path) {
			return Ok(outputs.clone());
		}
		if stack.contains(drv_path) {
			return Err(BuildError::Other(format!(
				"cycle detected in the input derivations of '{}'",
				print(drv_path)
			)));
		}
		let drv = self.store.read_derivation(drv_path)?;
		let outputs = known_outputs(drv_path, &drv)?;

		stack.push(drv_path.clone());
		for input in drv.input_derivations().keys() {
			self.realise_inner(input, done, stack, log)?;
		}
		stack.pop();

		if !self.all_valid(&outputs)? {
			self.build(drv_path, &drv, &outputs, log)?;
		}
		let paths: BTreeMap<String, StorePath> = outputs
			.into_iter()
			.map(|(name, output)| (name, output.path))
			.collect();
		done.insert(drv_path.clone(), paths.clone());
		Ok(paths)
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
		log: &mut dyn FnMut(&[u8]),
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
		if drv.command().starts_with(b"builtin:") {
			return Err(BuildError::Other(format!(
				"unsupported builtin builder '{}'",
				drv.command().to_str_lossy()
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
		if !use_sandbox {
			return Err(BuildError::Other(format!(
				"cannot build '{}': nix-rs only builds in the sandbox so far",
				print(drv_path)
			)));
		}

		let build = Build::start(self, drv_path, outputs, &drv_options, parsed, is_fixed)?;
		let result = build.run(drv, &env, &input_paths, log).and_then(|()| {
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

/// The directories of one build attempt, removed by [`Build::cleanup`].
struct Build<'b> {
	builder: &'b LocalBuilder<'b>,
	drv_path: StorePath,
	outputs: Vec<StorePath>,
	top_tmp_dir: PathBuf,
	tmp_dir: PathBuf,
	chroot_parent: PathBuf,
	chroot_root: PathBuf,
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
			sandboxed: !is_fixed,
			impure_env_vars: drv_options.impure_env_vars.clone(),
			pass_as_file: drv_options.pass_as_file.clone(),
			export_references_graph: drv_options.export_references_graph.clone(),
			structured,
		})
	}

	fn path_in_chroot(&self, path: &StorePath) -> PathBuf {
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
		log: &mut dyn FnMut(&[u8]),
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
		let status = sandbox::run(&spec, log)?;
		if !status.success() {
			let mut msg = format!(
				"Cannot build '{}'.\nReason: builder {}.\nOutput paths:",
				print(&self.drv_path),
				status.describe()
			);
			for output in &self.outputs {
				msg += &format!("\n  {}", print(output));
			}
			return Err(BuildError::failed(
				if self.sandboxed {
					FailureStatus::PermanentFailure
				} else {
					FailureStatus::TransientFailure
				},
				msg,
			));
		}
		Ok(())
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
