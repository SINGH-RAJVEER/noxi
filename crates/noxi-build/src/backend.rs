//! A local store that builds and substitutes the paths it is asked to
//! realise, as CppNix's `LocalStore` does.

use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use nix_compat::derivation::UnverifiedDerivation;
use nix_compat::nixhash::HashAlgo;
use nix_compat::store_path::StorePath;
use noxi_store::binary_cache::BinaryCache;
use noxi_store::{
	DerivedPath, Error, FileIngestionMethod, LocalStore, Missing, Result, Store, ValidPathInfo,
};

use crate::error::BuildError;
use crate::local::{BuildEvent, LocalBuilder};
use crate::missing;
use crate::settings::BuildSettings;

/// A [`LocalStore`] with the settings and substituters to realise paths.
pub struct LocalBackend {
	pub store: LocalStore,
	pub settings: BuildSettings,
	/// The `substituters` setting, opened when a path is first realised.
	substituter_uris: Vec<String>,
	substituters: OnceCell<Vec<BinaryCache>>,
}

impl LocalBackend {
	pub fn new(store: LocalStore, settings: BuildSettings, substituters: Vec<String>) -> Self {
		Self {
			store,
			settings,
			substituter_uris: substituters,
			substituters: OnceCell::new(),
		}
	}

	/// The substituters that could be opened. Like CppNix, a substituter
	/// that cannot be opened is reported and skipped.
	fn substituters(&self) -> &[BinaryCache] {
		self.substituters.get_or_init(|| {
			self.substituter_uris
				.iter()
				.filter_map(|uri| match BinaryCache::open(uri) {
					Ok(cache) => Some(cache.with_narinfo_cache_ttls(
						self.store.config().narinfo_cache_positive_ttl,
						self.store.config().narinfo_cache_negative_ttl,
					)),
					Err(e) => {
						if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn) {
							eprintln!("warning: disabling substituter '{uri}': {e}");
						}
						None
					}
				})
				.collect()
		})
	}
}

impl LocalBackend {
	/// The derivations realising `paths` builds, each after its inputs and
	/// otherwise in order of name and path.
	fn build_order(&self, paths: &[DerivedPath]) -> Result<Vec<(StorePath, BTreeSet<StorePath>)>> {
		let will_build = self.query_missing(paths)?.will_build;
		let mut inputs: BTreeMap<StorePath, BTreeSet<StorePath>> = BTreeMap::new();
		for drv in &will_build {
			let drv_inputs = self
				.store
				.read_build_derivation(drv)?
				.basic
				.input_derivations()
				.keys()
				.filter(|input| will_build.contains(*input))
				.cloned()
				.collect();
			inputs.insert(drv.clone(), drv_inputs);
		}
		Ok(order_builds(inputs))
	}

	/// CppNix's failure of a derivation goal whose input failed to build.
	fn dependency_failed(&self, drv: &StorePath) -> Result<String> {
		let mut message = format!(
			"Cannot build '{}'.\nReason: 1 dependency failed.",
			drv.to_absolute_path()
		);
		let derivation = self.store.read_derivation(drv)?;
		let mut outputs: Vec<String> = derivation
			.outputs()
			.store_paths()
			.map(StorePath::to_absolute_path)
			.collect();
		outputs.sort();
		if !outputs.is_empty() {
			message += "\nOutput paths:";
		}
		for output in outputs {
			message += &format!("\n  {output}");
		}
		Ok(message)
	}
}

/// Visit only a completed node's dependents instead of rescanning all waiting nodes.
/// The ready queue retains the existing name/path ordering and each goal's inputs.
fn order_builds(
	mut inputs: BTreeMap<StorePath, BTreeSet<StorePath>>,
) -> Vec<(StorePath, BTreeSet<StorePath>)> {
	let key = |path: &StorePath| {
		(
			path.name().to_owned(),
			path.to_absolute_path(),
			path.clone(),
		)
	};
	let mut dependents: BTreeMap<StorePath, Vec<StorePath>> = BTreeMap::new();
	let mut remaining = BTreeMap::new();
	let mut ready = BTreeSet::new();
	for (drv, drv_inputs) in &inputs {
		remaining.insert(drv.clone(), drv_inputs.len());
		if drv_inputs.is_empty() {
			ready.insert(key(drv));
		}
		for input in drv_inputs {
			dependents
				.entry(input.clone())
				.or_default()
				.push(drv.clone());
		}
	}
	let mut order = Vec::with_capacity(inputs.len());
	while let Some((_, _, drv)) = ready.pop_first() {
		if let Some(nodes) = dependents.get(&drv) {
			for other in nodes {
				let count = remaining.get_mut(other).expect("dependent is in the graph");
				*count -= 1;
				if *count == 0 {
					ready.insert(key(other));
				}
			}
		}
		order.push((
			drv.clone(),
			inputs.remove(&drv).expect("ready goal is in the graph"),
		));
	}
	order
}

impl Store for LocalBackend {
	fn verify_store(
		&self,
		check_contents: bool,
		repair: bool,
		log: &mut dyn FnMut(nix_compat::log::VerbosityLevel, &str),
	) -> Result<bool> {
		Store::verify_store(&self.store, check_contents, repair, log)
	}

	fn query_valid_paths(&self, paths: &BTreeSet<StorePath>) -> Result<BTreeSet<StorePath>> {
		Store::query_valid_paths(&self.store, paths)
	}

	fn optimise_store(&self, log: &mut dyn FnMut(&str)) -> Result<()> {
		Store::optimise_store(&self.store, log)
	}

	fn find_roots(&self, censor: bool, log: &mut dyn FnMut(&str)) -> Result<noxi_store::gc::Roots> {
		self.store.find_roots(censor, log)
	}

	fn collect_garbage(
		&self,
		options: &noxi_store::gc::GcOptions,
		log: &mut dyn FnMut(&str),
	) -> Result<noxi_store::gc::GcResults> {
		self.store.collect_garbage(options, log)
	}

	fn real_path(&self, path: &StorePath) -> PathBuf {
		self.store.real_path(path)
	}

	fn is_valid_path(&self, path: &StorePath) -> Result<bool> {
		self.store.is_valid_path(path)
	}

	fn query_path_info(&self, path: &StorePath) -> Result<Option<ValidPathInfo>> {
		self.store.query_path_info(path)
	}

	fn query_referrers(&self, path: &StorePath) -> Result<BTreeSet<StorePath>> {
		self.store.query_referrers(path)
	}

	fn query_valid_derivers(&self, path: &StorePath) -> Result<BTreeSet<StorePath>> {
		self.store.query_valid_derivers(path)
	}

	fn query_derivation_outputs(&self, drv: &StorePath) -> Result<BTreeMap<String, StorePath>> {
		self.store.query_derivation_outputs(drv)
	}

	fn read_derivation(&self, drv: &StorePath) -> Result<UnverifiedDerivation> {
		self.store.read_derivation(drv)
	}

	fn add_text_to_store(
		&self,
		name: &str,
		contents: &[u8],
		references: &BTreeSet<StorePath>,
	) -> Result<StorePath> {
		self.store
			.add_text_to_store(name, contents, references, false)
	}

	fn add_multiple_to_store(
		&self,
		paths: &[noxi_store::ValidPathInfo],
		nar: &mut dyn FnMut(&StorePath, &mut dyn std::io::Write) -> Result<()>,
		repair: bool,
		check_sigs: bool,
	) -> Result<()> {
		use std::io::Seek;
		for info in paths {
			let mut file = tempfile::tempfile()?;
			nar(&info.path, &mut file)?;
			file.rewind()?;
			self.store.add_to_store(
				info,
				&mut std::io::BufReader::new(file),
				repair,
				check_sigs,
			)?;
		}
		Ok(())
	}

	fn add_to_store_from_dump(
		&self,
		source: &mut (dyn Read + Send),
		name: &str,
		dump_method: FileIngestionMethod,
		hash_method: FileIngestionMethod,
		hash_algo: HashAlgo,
		references: &BTreeSet<StorePath>,
	) -> Result<StorePath> {
		self.store.add_to_store_from_dump(
			source,
			name,
			dump_method,
			hash_method,
			hash_algo,
			references,
			false,
		)
	}

	fn add_to_store_from_capture(
		&self,
		source: noxi_store::CapturedSource,
		name: &str,
		method: FileIngestionMethod,
		references: &BTreeSet<StorePath>,
	) -> Result<StorePath> {
		self.store
			.add_to_store_from_capture(source, name, method, references)
	}

	fn add_temp_root(&self, path: &StorePath) -> Result<()> {
		self.store.add_temp_root(path)
	}

	fn add_perm_root(&self, path: &StorePath, gc_root: &Path) -> Result<PathBuf> {
		self.store.add_perm_root(path, gc_root)
	}

	fn query_missing(&self, paths: &[DerivedPath]) -> Result<Missing> {
		let targets: Vec<missing::DerivedPath> = paths
			.iter()
			.map(|path| match path {
				DerivedPath::Opaque(path) => missing::DerivedPath::Opaque(path.clone()),
				DerivedPath::Built { drv, outputs } => missing::DerivedPath::Built {
					drv_path: drv.clone(),
					outputs: (!outputs.is_empty()).then(|| outputs.clone()),
				},
			})
			.collect();
		let substituters = self.substituters();
		missing::query_missing(
			&self.store,
			substituters,
			!substituters.is_empty(),
			self.settings.always_allow_substitutes,
			&targets,
		)
		.map_err(|e| Error::Other(e.to_string()))
	}

	/// Build output goes to standard error, as the legacy commands print it,
	/// after a line naming each derivation as it starts.
	fn build_paths(&self, paths: &[DerivedPath]) -> Result<()> {
		let substituters = self.substituters();
		let builder =
			LocalBuilder::new(&self.store, &self.settings).with_substituters(substituters);
		// The `bar` loggers print each completed line as `NAME> LINE`, the line
		// assembled as `handleChildOutput` does: `\r` rewinds, `\n` ends it.
		let mut prefixed = PrefixedLog::default();
		let mut events = |event: BuildEvent<'_>| {
			let mut stderr = std::io::stderr();
			let prefix = nix_compat::log::prefix_build_logs();
			let _ = match event {
				// The build activity is at `lvlInfo`; its output is shown
				// unless build logs are off.
				BuildEvent::Started { drv_path } => {
					prefixed.start(drv_path);
					if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
						writeln!(stderr, "building '{}'...", drv_path.to_absolute_path())
					} else {
						Ok(())
					}
				}
				BuildEvent::Output(data) if prefix => {
					prefixed.push(data, &mut stderr);
					Ok(())
				}
				BuildEvent::Output(data) if nix_compat::log::print_build_logs() => {
					stderr.write_all(data)
				}
				BuildEvent::Finished { .. } if prefix => {
					prefixed.flush(&mut stderr);
					Ok(())
				}
				BuildEvent::Output(_) => Ok(()),
				BuildEvent::Finished { .. } | BuildEvent::PathChecked { .. } => Ok(()),
			};
		};
		if self.settings.max_build_jobs > 1 || self.settings.keep_going {
			let targets: Vec<_> = paths
				.iter()
				.filter_map(|path| match path {
					DerivedPath::Built { drv, outputs } => Some((
						drv.clone(),
						(!outputs.is_empty()).then_some(outputs.clone()),
					)),
					DerivedPath::Opaque(_) => None,
				})
				.collect();
			let mut batch =
				builder.realise_many_with_events(&targets, &mut |_, event| events(event))?;
			let exit_status = batch.exit_status();
			let mut failed_targets = Vec::new();
			for (path, _) in &targets {
				if let Some(error) = batch.failures.remove(path) {
					let message = if error.status() == crate::FailureStatus::DependencyFailed {
						self.dependency_failed(path)?
					} else {
						error.to_string()
					};
					failed_targets.push((path.clone(), message));
				}
			}
			for error in batch.failures.into_values() {
				if !matches!(error, BuildError::Cancelled) {
					eprintln!("{}", format_error(&error.to_string()));
				}
			}
			if failed_targets.len() == 1 {
				return Err(Error::Build {
					exit_status,
					message: failed_targets.remove(0).1,
				});
			}
			if !failed_targets.is_empty() {
				let mut quoted: Vec<_> = failed_targets
					.iter()
					.map(|(p, _)| format!("'{}'", p.to_absolute_path()))
					.collect();
				quoted.sort();
				for (_, message) in failed_targets {
					eprintln!("{}", format_error(&message));
				}
				return Err(Error::Build {
					exit_status,
					message: format!("build of {} failed", quoted.join(", ")),
				});
			}
			for path in paths {
				if let DerivedPath::Opaque(path) = path
					&& !self.store.is_valid_path(path)?
				{
					self.store.substitute(substituters, path)?;
				}
			}
			return Ok(());
		}
		// CppNix's worker starts ready builds in the order of their goal keys,
		// `dd$NAME$PATH`, so independent derivations build in name order.
		// Without `--keep-going`, a failure starts nothing new: the
		// derivations waiting on it fail in turn until a requested one
		// fails, which stops the worker. A failure another derivation waits
		// on is logged at once.
		let targets: BTreeSet<&StorePath> = paths
			.iter()
			.filter_map(|path| match path {
				DerivedPath::Built { drv, .. } => Some(drv),
				DerivedPath::Opaque(_) => None,
			})
			.collect();
		let order = self.build_order(paths)?;
		let has_waiters = |drv: &StorePath| order.iter().any(|(_, inputs)| inputs.contains(drv));
		let mut built: BTreeSet<&StorePath> = BTreeSet::new();
		let mut failed: BTreeSet<&StorePath> = BTreeSet::new();
		let mut exit_status = 1;
		let mut failure = None;
		for (drv, inputs) in &order {
			let message = if failed.is_empty() {
				match builder.realise_with_events(drv, None, &mut events) {
					Ok(_) => {
						built.insert(drv);
						continue;
					}
					Err(BuildError::Failed { status, message }) => {
						exit_status = status.exit_status();
						message
					}
					Err(error) => return Err(error.into()),
				}
			} else if inputs.iter().any(|input| failed.contains(input)) {
				self.dependency_failed(drv)?
			} else {
				continue;
			};
			failed.insert(drv);
			if has_waiters(drv) {
				eprintln!("{}", format_error(&message));
			}
			if targets.contains(drv) {
				failure = Some(message);
				break;
			}
		}
		if let Some(message) = failure {
			// `Store::buildPaths` walks the requested goals in key order. The
			// first failure is held and later ones are logged; requested
			// goals the stopped worker never finished carry an empty failure,
			// which logs as an empty line.
			let mut unfinished: Vec<&StorePath> = targets
				.iter()
				.copied()
				.filter(|drv| !built.contains(drv) && order.iter().any(|(other, _)| other == *drv))
				.collect();
			unfinished.sort_by_key(|drv| (drv.name().to_owned(), drv.to_absolute_path()));
			if unfinished.len() == 1 {
				return Err(Error::Build {
					exit_status,
					message,
				});
			}
			let failed_target = failed.iter().find(|drv| targets.contains(*drv));
			let mut held = None;
			for drv in &unfinished {
				let text = if Some(drv) == failed_target {
					format_error(&message)
				} else {
					String::new()
				};
				match held {
					None => held = Some(text),
					Some(_) => eprintln!("{text}"),
				}
			}
			if let Some(text) = held {
				eprintln!("{text}");
			}
			let mut quoted: Vec<String> = unfinished
				.iter()
				.map(|drv| format!("'{}'", drv.to_absolute_path()))
				.collect();
			quoted.sort();
			return Err(Error::Build {
				exit_status,
				message: format!("build of {} failed", quoted.join(", ")),
			});
		}
		for path in paths {
			match path {
				DerivedPath::Opaque(path) => {
					if self.store.is_valid_path(path)? {
						continue;
					}
					// CppNix throws the substitution goal's own failure.
					self.store.substitute(substituters, path)?;
				}
				DerivedPath::Built { drv, outputs } => {
					let wanted = (!outputs.is_empty()).then_some(outputs);
					let result = builder.realise_with_events(drv, wanted, &mut events);
					result?;
				}
			}
		}
		Ok(())
	}
}

/// Format an error as CppNix's `logError` does: continuation lines line up
/// after the `error: ` prefix.
fn format_error(message: &str) -> String {
	nix_compat::log::format_error(message)
}

#[cfg(test)]
mod order_tests {
	use super::*;

	// The prior scan implementation is an independent compatibility oracle.
	fn scan_order(
		mut inputs: BTreeMap<StorePath, BTreeSet<StorePath>>,
	) -> Vec<(StorePath, BTreeSet<StorePath>)> {
		let key = |p: &StorePath| (p.name().to_owned(), p.to_absolute_path(), p.clone());
		let mut waiting = inputs.clone();
		let mut ready: BTreeSet<_> = inputs
			.iter()
			.filter(|(_, deps)| deps.is_empty())
			.map(|(p, _)| key(p))
			.collect();
		let mut order = Vec::new();
		while let Some((_, _, path)) = ready.pop_first() {
			waiting.remove(&path);
			for (other, deps) in &mut waiting {
				if deps.remove(&path) && deps.is_empty() {
					ready.insert(key(other));
				}
			}
			order.push((path.clone(), inputs.remove(&path).unwrap()));
		}
		order
	}

	fn path(i: usize) -> StorePath {
		StorePath::from_bytes(format!("{i:032b}-goal-{}.drv", i % 5).as_bytes()).unwrap()
	}

	#[test]
	fn indexed_order_preserves_dependencies_and_ready_ties() {
		for seed in 0..40u64 {
			let mut random = seed;
			let mut graph = BTreeMap::new();
			for i in 0..60 {
				let mut deps = BTreeSet::new();
				for j in 0..i {
					random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
					if random >> 60 == 0 {
						deps.insert(path(j));
					}
				}
				graph.insert(path(i), deps);
			}
			assert_eq!(order_builds(graph.clone()), scan_order(graph));
		}
	}

	#[test]
	fn empty_and_cyclic_graphs_preserve_scan_behavior() {
		assert!(order_builds(BTreeMap::new()).is_empty());
		let graph = BTreeMap::from([
			(path(0), BTreeSet::from([path(1)])),
			(path(1), BTreeSet::from([path(0)])),
			(path(2), BTreeSet::new()),
		]);
		assert_eq!(order_builds(graph.clone()), scan_order(graph));
	}

	/// Isolate O09 from sandbox setup and store I/O. This is a native algorithm
	/// comparison, not a CppNix or end-to-end realization benchmark.
	#[test]
	#[ignore = "diagnostic timing; run in release mode on an otherwise idle host"]
	fn ordering_scaling() {
		for shape in ["chain", "wide"] {
			for size in [256, 1024, 4096] {
				let graph: BTreeMap<_, _> = (0..size)
					.map(|i| {
						let inputs = if shape == "chain" && i > 0 {
							BTreeSet::from([path(i - 1)])
						} else {
							BTreeSet::new()
						};
						(path(i), inputs)
					})
					.collect();
				let expected = scan_order(graph.clone());
				for sample in 0..30 {
					let roles = if sample % 2 == 0 {
						["scan", "indexed"]
					} else {
						["indexed", "scan"]
					};
					for role in roles {
						let input = graph.clone();
						let start = std::time::Instant::now();
						let actual = if role == "scan" {
							scan_order(std::hint::black_box(input))
						} else {
							order_builds(std::hint::black_box(input))
						};
						let elapsed = start.elapsed().as_secs_f64();
						assert_eq!(actual, expected);
						println!(
							"{{\"benchmark\":\"ordering\",\"shape\":\"{shape}\",\"size\":{size},\"sample\":{sample},\"role\":\"{role}\",\"elapsed_s\":{elapsed}}}"
						);
					}
				}
			}
		}
	}
}

/// Build output as CppNix's `ProgressBar` prints it with `--print-build-logs`
/// off a terminal: each line after the derivation's name and `> `.
#[derive(Default)]
struct PrefixedLog {
	name: String,
	line: Vec<u8>,
	pos: usize,
}

impl PrefixedLog {
	fn start(&mut self, drv_path: &StorePath) {
		// `DrvName(...).name`: the name without its version.
		let full = drv_path
			.name()
			.strip_suffix(".drv")
			.unwrap_or(drv_path.name());
		let bytes = full.as_bytes();
		let end = (0..bytes.len())
			.find(|&i| {
				bytes[i] == b'-' && i + 1 < bytes.len() && !bytes[i + 1].is_ascii_alphabetic()
			})
			.unwrap_or(bytes.len());
		self.name = full[..end].to_owned();
		self.line.clear();
		self.pos = 0;
	}

	fn push(&mut self, data: &[u8], out: &mut dyn std::io::Write) {
		for &byte in data {
			match byte {
				b'\n' => self.emit(out),
				b'\r' => self.pos = 0,
				byte => {
					if self.pos < self.line.len() {
						self.line[self.pos] = byte;
					} else {
						self.line.push(byte);
					}
					self.pos += 1;
				}
			}
		}
	}

	/// The unterminated last line when the build ends.
	fn flush(&mut self, out: &mut dyn std::io::Write) {
		if !self.line.is_empty() {
			self.emit(out);
		}
	}

	/// `flushLine`.
	fn emit(&mut self, out: &mut dyn std::io::Write) {
		// The whole message is filtered, so tabs align after the prefix.
		let mut message = format!("{}> ", self.name).into_bytes();
		message.extend_from_slice(&self.line);
		let mut text = nix_compat::log::filter_ansi_escapes(&message, true);
		text.push(b'\n');
		let _ = out.write_all(&text);
		self.line.clear();
		self.pos = 0;
	}
}
