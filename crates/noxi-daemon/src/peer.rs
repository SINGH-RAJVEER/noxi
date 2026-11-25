//! A store that another process's derivations can be built in: what `nix
//! __build-remote` needs of a machine, and `nix-store --serve` of the store
//! it serves.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};

use nix_compat::derivation::OutputHashMode;
use nix_compat::nixhash::{HashAlgo, NixHash};
use nix_compat::store_path::StorePath;
use noxi_build::backend::{BuildLogs, LocalBackend};
use noxi_build::local::{BuildEvent, LocalBuilder};
use noxi_build::settings::BuildSettings;
use noxi_store::binary_cache::BinaryCache;
use noxi_store::build_derivation::{BuildDerivation, ExperimentalMethod};
use noxi_store::{
	BuildMode, DerivedPath, FileIngestionMethod, LocalStore, Result, Store, ValidPathInfo,
};

use crate::connection::{failure_code, now};
use crate::wire::{Reader, Writer};

/// Writes the NAR of a path.
pub type NarSource<'a> = dyn FnMut(&StorePath, &mut dyn Write) -> Result<()> + 'a;

/// `BuildResult`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BuildResult {
	/// `BuildResult::Success::Status` or `BuildResult::Failure::Status`.
	pub status: u64,
	pub message: String,
	pub times_built: u64,
	pub non_deterministic: bool,
	pub start: u64,
	pub stop: u64,
	pub outputs: BTreeMap<String, StorePath>,
}

impl BuildResult {
	/// `Built`, `Substituted`, `AlreadyValid` and `ResolvesToAlreadyValid`.
	pub fn success(&self) -> bool {
		matches!(self.status, 0 | 1 | 2 | 13)
	}

	pub fn failure(status: u64, message: String) -> Self {
		Self {
			status,
			message,
			..Self::default()
		}
	}
}

/// A store that builds derivations it does not hold.
pub trait Peer {
	/// `Store::connect`.
	fn connect(&self) -> Result<()>;

	/// `Store::isTrustedClient`; `None` if the store cannot say.
	fn trusted(&self) -> Result<Option<bool>>;

	/// `Store::queryValidPaths`, letting the store substitute paths first.
	fn valid_paths(
		&self,
		paths: &BTreeSet<StorePath>,
		substitute: bool,
	) -> Result<BTreeSet<StorePath>>;

	fn path_info(&self, path: &StorePath) -> Result<Option<ValidPathInfo>>;

	fn nar(&self, path: &StorePath, sink: &mut dyn Write) -> Result<()>;

	/// `addMultipleToStore` without signature checks, if the store trusts
	/// this process. An untrusted daemon's checks are its own.
	fn add_paths(&self, infos: &[ValidPathInfo], nar: &mut NarSource<'_>) -> Result<()>;

	/// `Store::buildDerivation`.
	fn build_derivation(&self, drv_path: &StorePath, drv: &BuildDerivation) -> Result<BuildResult>;

	/// `Store::buildPathsWithResults` of one derivation's outputs.
	fn build_registered(&self, drv_path: &StorePath) -> Result<BuildResult>;
}

/// `LocalStore::buildDerivation`: one goal in a worker of its own.
pub fn build_locally(
	store: &LocalStore,
	settings: &BuildSettings,
	substituters: &[BinaryCache],
	cancelled: Option<&std::sync::atomic::AtomicBool>,
	mode: BuildMode,
	drv_path: &StorePath,
	drv: &BuildDerivation,
) -> BuildResult {
	use nix_compat::log::logger;
	let mut builder = LocalBuilder::new(store, settings)
		.with_substituters(substituters)
		.with_mode(mode);
	if let Some(cancelled) = cancelled {
		builder = builder.with_cancellation(cancelled);
	}
	let start = now();
	let worker = logger::WorkerGuard::new();
	logger::worker_update(|_| {});
	logger::worker_update(|c| c.expected_builds += 1);
	let mut built = false;
	let mut logs = BuildLogs::default();
	let result = builder.build_supplied_derivation(drv_path, drv, &mut |event| {
		built |= matches!(
			event,
			BuildEvent::Started { .. } | BuildEvent::StartedRemotely { .. }
		);
		logs.event(None, event);
	});
	drop(logs);
	drop(worker);
	match result {
		Ok(outputs) => BuildResult {
			status: if built { 0 } else { 2 },
			times_built: built.into(),
			start,
			stop: now(),
			outputs,
			..BuildResult::default()
		},
		Err(error) => BuildResult {
			start,
			stop: now(),
			..BuildResult::failure(
				failure_code(&error),
				match error {
					noxi_build::BuildError::Failed { message, .. } => message,
					// `Store::buildDerivation` catches what ends the worker.
					error => format!("error: {error}"),
				},
			)
		},
	}
}

impl Peer for LocalBackend {
	fn connect(&self) -> Result<()> {
		Ok(())
	}

	fn trusted(&self) -> Result<Option<bool>> {
		Ok(Some(true))
	}

	/// `LocalStore::queryValidPaths` does not substitute.
	fn valid_paths(
		&self,
		paths: &BTreeSet<StorePath>,
		_substitute: bool,
	) -> Result<BTreeSet<StorePath>> {
		Store::query_valid_paths(self, paths)
	}

	fn path_info(&self, path: &StorePath) -> Result<Option<ValidPathInfo>> {
		self.query_path_info(path)
	}

	fn nar(&self, path: &StorePath, sink: &mut dyn Write) -> Result<()> {
		self.nar_from_path(path, sink)
	}

	fn add_paths(&self, infos: &[ValidPathInfo], nar: &mut NarSource<'_>) -> Result<()> {
		self.add_multiple_to_store(infos, nar, false, false)
	}

	fn build_derivation(&self, drv_path: &StorePath, drv: &BuildDerivation) -> Result<BuildResult> {
		Ok(build_locally(
			&self.store,
			&self.settings,
			self.open_substituters(),
			None,
			BuildMode::Normal,
			drv_path,
			drv,
		))
	}

	fn build_registered(&self, drv_path: &StorePath) -> Result<BuildResult> {
		let start = now();
		let result = Store::build_paths(
			self,
			&[DerivedPath::Built {
				drv: drv_path.clone(),
				outputs: BTreeSet::new(),
			}],
		);
		Ok(match result {
			Ok(()) => BuildResult {
				start,
				stop: now(),
				outputs: self.query_derivation_outputs(drv_path)?,
				..BuildResult::default()
			},
			Err(error) => BuildResult::failure(9, error.to_string()),
		})
	}
}

fn method_prefix(method: FileIngestionMethod) -> &'static str {
	match method {
		FileIngestionMethod::Flat => "",
		FileIngestionMethod::NixArchive => "r:",
		FileIngestionMethod::Text => "text:",
		FileIngestionMethod::Git => "git:",
	}
}

/// A derivation's outputs as it declares them: the path, the method and
/// algorithm, and the hash, each empty if not declared.
fn declared_outputs(drv: &BuildDerivation) -> Vec<(String, String, String, String)> {
	let hex = |hash: &NixHash| {
		let text = hash.as_nix_lowerhex_string_fmt().to_string();
		text.split_once(':')
			.map_or(text.clone(), |(_, hex)| hex.to_owned())
	};
	let declared = |method: FileIngestionMethod, algo: HashAlgo| {
		format!("{}{}", method_prefix(method), algo.as_str())
	};
	let outputs = drv.basic.outputs();
	let fixed = outputs.as_fixed_output();
	outputs
		.names()
		.map(|name| {
			let name = name.to_string();
			let (path, algo, hash) = if let Some(spec) = drv.experimental.get(&name) {
				let method: FileIngestionMethod = ExperimentalMethod::ingestion_method(spec.method);
				let algo = declared(method, spec.hash_algo);
				match &spec.fixed {
					Some((path, hash)) => (path.to_absolute_path(), algo, hex(hash)),
					None if spec.impure => (String::new(), algo, "impure".to_owned()),
					None => (String::new(), algo, String::new()),
				}
			} else if let Some(spec) = drv.floating.get(&name) {
				(
					String::new(),
					declared(spec.method, spec.hash_algo),
					String::new(),
				)
			} else if let Some((hash, path)) = &fixed {
				let method = match hash.mode {
					OutputHashMode::Recursive => FileIngestionMethod::NixArchive,
					OutputHashMode::Flat => FileIngestionMethod::Flat,
				};
				(
					path.to_absolute_path(),
					declared(method, hash.hash.algo()),
					hex(&hash.hash),
				)
			} else {
				let path = outputs
					.iter()
					.find(|(other, _)| other.to_string() == name)
					.map(|(_, path)| path.to_absolute_path());
				(path.unwrap_or_default(), String::new(), String::new())
			};
			(name, path, algo, hash)
		})
		.collect()
}

/// `writeDerivation` of a `BasicDerivation` whose input sources are
/// `sources`.
pub fn write_basic_derivation<W: Write>(
	to: &mut Writer<W>,
	drv: &BuildDerivation,
	sources: &BTreeSet<StorePath>,
) {
	let mut outputs = declared_outputs(drv);
	outputs.sort();
	to.u64(outputs.len() as u64);
	for (name, path, algo, hash) in &outputs {
		to.string(name).string(path).string(algo).string(hash);
	}
	let basic = &drv.basic;
	to.store_paths(sources.iter())
		.bytes(basic.system())
		.bytes(basic.command())
		.u64(basic.arguments().len() as u64);
	for argument in basic.arguments() {
		to.bytes(argument);
	}
	to.u64(basic.environment().len() as u64);
	for (key, value) in basic.environment() {
		to.bytes(key).bytes(value);
	}
}

/// The `BasicDerivation` of `drv` with other input sources.
pub fn basic_derivation(
	drv_path: &StorePath,
	drv: &BuildDerivation,
	sources: &BTreeSet<StorePath>,
	features: noxi_store::build_derivation::OutputFeatures,
) -> crate::error::Result<BuildDerivation> {
	let mut bytes = Writer::new(Vec::new());
	write_basic_derivation(&mut bytes, drv, sources);
	let bytes = bytes.take();
	crate::connection::read_basic_derivation(&mut Reader::new(&bytes[..]), drv_path, features)
}

/// Read outputs as both protocols send them to clients that know
/// realisations by path.
pub fn read_outputs<R: Read>(
	from: &mut Reader<R>,
) -> crate::error::Result<BTreeMap<String, StorePath>> {
	let mut outputs = BTreeMap::new();
	for _ in 0..from.read_u64()? {
		let name = from.read_string()?;
		let path = from.read_store_path()?;
		from.read_strings()?; // signatures
		outputs.insert(name, path);
	}
	Ok(outputs)
}
