//! `nix __build-remote`, CppNix 2.35.2's build hook (`build-remote.cc`):
//! find a machine for each derivation the worker offers, and build the one
//! it accepts there.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use nix_compat::log::logger::{self, Activity};
use nix_compat::log::{ActivityType, VerbosityLevel};
use nix_compat::nixcpp::machines::Machine;
use nix_compat::nixcpp::settings::Settings;
use nix_compat::nixcpp::store_reference::{StoreReference, Variant};
use nix_compat::store_path::StorePath;
use noxi_build::backend::LocalBackend;
use noxi_build::settings::BuildSettings;
use noxi_daemon::peer::{Peer, basic_derivation};
use noxi_daemon::ssh::close_on_exec;
use noxi_daemon::wire::Reader;
use noxi_store::build_derivation::OutputFeatures;
use noxi_store::lock::{LockType, lock_file, open_lock_file};
use noxi_store::{Realisation, Store, ValidPathInfo};

use crate::cli::Failure;
use crate::machine;

/// Where the hook's parent reads the builder's output, and the other end of
/// that pipe, from which messages of a failed `ssh` are read back.
const BUILDER_OUT: i32 = 4;
const BUILDER_OUT_READ: i32 = 5;

/// The store the worker builds for.
enum Primary {
	Local(Box<LocalBackend>),
	Other(Rc<dyn Store>),
}

impl Primary {
	fn store(&self) -> &dyn Store {
		match self {
			Primary::Local(store) => store.as_ref(),
			Primary::Other(store) => store.as_ref(),
		}
	}
}

fn escape_uri(uri: &str) -> String {
	uri.replace('/', "_")
}

fn message(error: impl std::fmt::Display) -> Failure {
	Failure::Error(error.to_string())
}

/// `printError`.
fn print(level: VerbosityLevel, text: &str) {
	logger::log(level, text);
}

fn reply(text: &str) {
	let _ = io::stderr()
		.lock()
		.write_all(format!("{text}\n").as_bytes());
}

fn activity(text: String) -> Activity {
	Activity::new(
		VerbosityLevel::Talkative,
		ActivityType::Unknown,
		text,
		vec![],
		0,
	)
}

/// A request to build one derivation.
struct Request {
	willing: bool,
	system: String,
	drv_path: StorePath,
	features: BTreeSet<String>,
}

struct Hook {
	settings: Settings,
	/// `max-jobs` as the worker has it.
	max_jobs: u64,
	build: BuildSettings,
	store: Primary,
	current_load: PathBuf,
	machines: Vec<(Machine, bool)>,
}

pub fn main(mut settings: Settings, args: Vec<String>) -> Result<ExitCode, Failure> {
	logger::set_log_format(logger::LogFormat::InternalJson);
	// No passphrase or host key popups from `ssh`.
	// SAFETY: the command's other thread only waits for this one.
	unsafe {
		std::env::remove_var("DISPLAY");
		std::env::remove_var("SSH_ASKPASS");
	}
	let args = match args.split_first() {
		Some((first, rest)) if first == "__build-remote" => rest,
		_ => &args[..],
	};
	let [verbosity] = args else {
		return Err(Failure::Usage("called without required arguments".into()));
	};
	nix_compat::log::set_verbosity(
		verbosity
			.parse()
			.map_err(|_| Failure::Error("stoll".into()))?,
	);
	// Only `ssh` is meant to have them.
	close_on_exec(BUILDER_OUT);
	close_on_exec(BUILDER_OUT_READ);

	let mut source = Reader::new(io::BufReader::new(io::stdin().lock()));
	while source.read_int().map_err(message)? != 0 {
		let name = source.read_string().map_err(message)?;
		let value = source.read_string().map_err(message)?;
		settings
			.set(&name, &value)
			.map_err(|e| Failure::Error(e.to_string()))?;
	}
	let max_jobs = BuildSettings::from_settings(&settings).max_build_jobs;
	// A machine that is a local store builds in this process.
	settings
		.set("max-jobs", "1")
		.map_err(|e| Failure::Error(e.to_string()))?;

	let uri = crate::store::store_uri(None, &settings);
	let cwd = std::env::current_dir()?;
	let reference = parse_reference(&uri, &cwd)?;
	let (store, state_dir) = match machine::local(&reference, &settings)? {
		Some(store) => {
			let state_dir = store.store.config().state_dir.clone();
			(Primary::Local(Box::new(store)), state_dir)
		}
		None => {
			let opened = crate::store::open(&uri, &settings)?
				.ok_or_else(|| Failure::Error(format!("cannot open store '{uri}'")))?;
			(
				Primary::Other(opened.store),
				noxi_store::LocalStoreConfig::system().state_dir,
			)
		}
	};
	let build = BuildSettings::from_settings(&settings);
	let mut warnings = Vec::new();
	let machines = Machine::parse_config(
		&BTreeSet::from([build.system.clone()]),
		settings.get("builders").unwrap_or_default(),
		&cwd,
		&mut warnings,
	)
	.map_err(|e| match e {
		nix_compat::nixcpp::machines::Error::Usage(text) => Failure::Usage(text),
		e => message(e),
	})?;
	for warning in warnings {
		logger::warn(&warning);
	}
	print(
		VerbosityLevel::Debug,
		&format!("got {} remote builders", machines.len()),
	);
	if machines.is_empty() {
		reply("# decline-permanently");
		return Ok(ExitCode::SUCCESS);
	}
	let mut hook = Hook {
		settings,
		max_jobs,
		build,
		store,
		current_load: state_dir.join("current-load"),
		machines: machines.into_iter().map(|m| (m, true)).collect(),
	};
	loop {
		match source.read_string() {
			Ok(request) if request == "try" => {}
			Ok(_) | Err(noxi_daemon::Error::EndOfFile) => return Ok(ExitCode::SUCCESS),
			Err(error) => return Err(message(error)),
		}
		let request = Request {
			willing: source.read_int().map_err(message)? != 0,
			system: source.read_string().map_err(message)?,
			drv_path: source.read_store_path().map_err(message)?,
			features: source
				.read_strings()
				.map_err(message)?
				.into_iter()
				.collect(),
		};
		if let Some((index, peer, _slot)) = hook.find_machine(&request)? {
			return hook.build_on(index, peer.as_ref(), &request.drv_path, &mut source);
		}
	}
}

fn parse_reference(uri: &str, cwd: &Path) -> Result<StoreReference, Failure> {
	StoreReference::parse(uri, &Default::default(), cwd, &mut Vec::new()).map_err(message)
}

/// `drainFD` of a descriptor without blocking, then `chomp`.
fn drain_available(fd: i32) -> String {
	let mut out = Vec::new();
	// SAFETY: the descriptor was given to this process; its flags are only
	// changed for this process's copy of the pipe end. It stays open.
	let mut file = std::mem::ManuallyDrop::new(unsafe {
		let flags = libc::fcntl(fd, libc::F_GETFL);
		if flags == -1 {
			return String::new();
		}
		libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
		File::from_raw_fd(fd)
	});
	let mut chunk = [0; 4096];
	while let Ok(n) = file.read(&mut chunk) {
		if n == 0 {
			break;
		}
		out.extend(&chunk[..n]);
	}
	String::from_utf8_lossy(out.trim_ascii_end()).into_owned()
}

impl Hook {
	/// Answer a request. An accepted one returns the machine, its store and
	/// the lock on its build slot.
	fn find_machine(
		&mut self,
		request: &Request,
	) -> Result<Option<(usize, Box<dyn Peer>, File)>, Failure> {
		let experimental = self.settings.experimental_features();
		let available = |feature: &String| {
			self.build.system_features.contains(feature)
				|| matches!(feature.as_str(), "ca-derivations" | "recursive-nix")
					&& experimental.contains(feature.as_str())
		};
		let could_build_locally = self.max_jobs > 0
			&& self.build.can_build_platform(&request.system)
			&& request.features.iter().all(available);
		let can_build_locally = request.willing && could_build_locally;
		let _ = std::fs::create_dir(&self.current_load);
		loop {
			let main_lock = open_lock(&self.current_load.join("main-lock"))?;
			lock_file(&main_lock, LockType::Write, true)?;
			let mut right_type = false;
			let mut best: Option<(usize, f32, File)> = None;
			for (index, (machine, enabled)) in self.machines.iter().enumerate() {
				print(
					VerbosityLevel::Debug,
					&format!(
						"considering building on remote machine '{}'",
						machine.store_uri.render(true)
					),
				);
				if !*enabled
					|| !machine.system_supported(&request.system)
					|| !machine.all_supported(&request.features)
					|| !machine.mandatory_met(&request.features)
				{
					continue;
				}
				right_type = true;
				let mut free = None;
				let mut load = 0u64;
				for slot in 0..machine.max_jobs {
					let lock = open_lock(&self.current_load.join(format!(
						"{}-{slot}",
						escape_uri(&machine.store_uri.render(true))
					)))?;
					if !lock_file(&lock, LockType::Write, false)? {
						load += 1;
					} else if free.is_none() {
						free = Some(lock);
					}
				}
				let Some(free) = free else {
					continue;
				};
				let load = load as f32;
				let better = best.as_ref().is_none_or(|(best, best_load, _)| {
					let speed = machine.speed_factor;
					let best_speed = self.machines[*best].0.speed_factor;
					let (relative, best_relative) = (load / speed, best_load / best_speed);
					relative < best_relative
						|| relative == best_relative
							&& (speed > best_speed || speed == best_speed && load < *best_load)
				});
				if better {
					best = Some((index, load, free));
				}
			}
			let Some((index, _, slot)) = best else {
				if right_type && !can_build_locally {
					reply("# postpone");
				} else {
					print(
						if could_build_locally {
							VerbosityLevel::Chatty
						} else {
							VerbosityLevel::Warn
						},
						&self.no_machine(request),
					);
					reply("# decline");
				}
				return Ok(None);
			};
			let _ = slot.set_modified(SystemTime::now());
			drop(main_lock);
			let machine = &self.machines[index].0;
			let uri = machine.store_uri.render(true);
			let connected = {
				let _act = activity(format!("connecting to '{uri}'"));
				machine::open(&machine::complete_reference(machine), &self.settings)
					.and_then(|peer| peer.connect().map(|()| peer).map_err(|e| e.to_string()))
			};
			match connected {
				Ok(peer) => return Ok(Some((index, peer, slot))),
				Err(error) => {
					let said = drain_available(BUILDER_OUT_READ);
					let said = if said.is_empty() {
						said
					} else {
						format!(": {said}")
					};
					print(
						VerbosityLevel::Error,
						&format!("cannot build on '{uri}': error: {error}{said}"),
					);
					self.machines[index].1 = false;
				}
			}
		}
	}

	fn no_machine(&self, request: &Request) -> String {
		let join = |set: &BTreeSet<String>| set.iter().cloned().collect::<Vec<_>>().join(", ");
		let mut text = format!(
			"Failed to find a machine for remote build!\nderivation: {}\nrequired (system, features): ({}, [{}])\n{} available machines:\n(systems, maxjobs, supportedFeatures, mandatoryFeatures)",
			request.drv_path,
			request.system,
			join(&request.features),
			self.machines.len()
		);
		for (machine, _) in &self.machines {
			text += &format!(
				"\n([{}], {}, [{}], [{}])",
				join(&machine.system_types),
				machine.max_jobs,
				join(&machine.supported_features),
				join(&machine.mandatory_features)
			);
		}
		text
	}

	fn build_on(
		&self,
		index: usize,
		peer: &dyn Peer,
		drv_path: &StorePath,
		source: &mut Reader<impl Read>,
	) -> Result<ExitCode, Failure> {
		// SAFETY: nothing else in this process uses the descriptor.
		unsafe { libc::close(BUILDER_OUT_READ) };
		let machine = &self.machines[index].0;
		let uri = machine.store_uri.render(true);
		reply(&format!("# accept\n{uri}"));
		let inputs = source.read_store_paths().map_err(message)?;
		let wanted = source.read_strings().map_err(message)?;
		let store = self.store.store();

		let upload_lock = self.upload_lock(&uri)?;
		{
			let _act = activity(format!("waiting for the upload lock to '{uri}'"));
			let deadline = Instant::now() + Duration::from_secs(15 * 60);
			while !lock_file(&upload_lock, LockType::Write, false)? {
				if Instant::now() > deadline {
					// The reference's message has no argument for its `%s`.
					print(
						VerbosityLevel::Error,
						"somebody is hogging the upload lock for '%s', continuing...",
					);
					break;
				}
				std::thread::sleep(Duration::from_millis(50));
			}
		}
		let substitute = self.settings.get_bool("builders-use-substitutes", false);
		let names = (
			Name::of(&self.build_uri()),
			Name::of(&machine::complete_reference(machine)),
		);
		{
			let _act = activity(format!("copying dependencies to '{uri}'"));
			self.copy_to(peer, &inputs, substitute, &names)?;
		}
		drop(upload_lock);

		let drv = store.read_build_derivation(drv_path)?;
		let content_addressed = drv.is_fixed() || drv.has_floating_outputs() || drv.is_impure();
		let result = if peer.trusted()?.unwrap_or(true) || content_addressed {
			// The outputs of the input derivations are on the machine as
			// sources. Changing the sources of a derivation without input
			// derivations would change the outputs of a content-addressed
			// one.
			let sources = if drv.basic.input_derivations().is_empty() {
				drv.basic.input_sources().clone()
			} else {
				inputs.clone()
			};
			let basic =
				basic_derivation(drv_path, &drv, &sources, self.features()).map_err(message)?;
			let result = peer.build_derivation(drv_path, &basic)?;
			if !result.success() {
				if self.build.keep_failed {
					logger::warn(&format!(
						"The failed build directory was kept on the remote builder due to `--keep-failed`.{}",
						if self
							.build
							.can_build_platform(&String::from_utf8_lossy(drv.basic.system()))
						{
							" You can re-run the command with `--builders ''` to disable remote building for this invocation."
						} else {
							""
						}
					));
				}
				return Err(Failure::Error(format!(
					"build of '{}' on '{uri}' failed: {}",
					drv_path.to_absolute_path(),
					result.message
				)));
			}
			result
		} else {
			// The machine believes the derivation's closure, not the plan.
			let closure = store.compute_closure(&BTreeSet::from([drv_path.clone()]))?;
			self.copy_to(peer, &closure, substitute, &names)?;
			peer.build_registered(drv_path)?
		};

		let mut missing = BTreeSet::new();
		let mut realisations = BTreeMap::new();
		if self.features().ca_derivations && !noxi_build::local::has_known_output_paths(&drv) {
			for output in &wanted {
				if store.query_realisation(drv_path, output)?.is_some() || !result.success() {
					continue;
				}
				print(VerbosityLevel::Debug, &format!("missing output {output}"));
				let path = result.outputs.get(output).ok_or_else(|| {
					Failure::Error(format!(
						"'{uri}' did not return the output '{output}' of '{}'",
						drv_path.to_absolute_path()
					))
				})?;
				missing.insert(path.clone());
				realisations.insert(output.clone(), path.clone());
			}
		} else {
			for path in store
				.query_partial_derivation_output_map(drv_path)?
				.into_values()
				.flatten()
			{
				if !store.is_valid_path(&path)? {
					missing.insert(path);
				}
			}
		}
		if !missing.is_empty() {
			let _act = activity(format!("copying outputs from '{uri}'"));
			if let Primary::Local(local) = &self.store {
				for path in &missing {
					local.store.assume_locked(path);
				}
			}
			self.copy_from(peer, &missing, &names)?;
		}
		for (output, path) in realisations {
			store.register_drv_output(
				drv_path,
				&output,
				&Realisation {
					out_path: path,
					signatures: BTreeSet::new(),
				},
			)?;
		}
		Ok(ExitCode::SUCCESS)
	}

	fn features(&self) -> OutputFeatures {
		let features = self.settings.experimental_features();
		OutputFeatures {
			ca_derivations: features.contains("ca-derivations"),
			impure_derivations: features.contains("impure-derivations"),
			dynamic_derivations: features.contains("dynamic-derivations"),
			git_hashing: features.contains("git-hashing"),
		}
	}

	fn build_uri(&self) -> StoreReference {
		let uri = crate::store::store_uri(None, &self.settings);
		parse_reference(&uri, Path::new("/")).unwrap_or(StoreReference {
			variant: Variant::Auto,
			params: Default::default(),
		})
	}

	/// Uploads to a machine happen one at a time. A URI too long for a file
	/// name is replaced by its hash.
	fn upload_lock(&self, uri: &str) -> Result<File, Failure> {
		let path = |name: &str| {
			self.current_load
				.join(format!("{}.upload-lock", escape_uri(name)))
		};
		if uri.len() + ".upload-lock".len() > 255 {
			use nix_compat::nixhash::{HashAlgo, NixHashDigester};
			let mut hash = NixHashDigester::new(HashAlgo::Md5);
			hash.update(uri.as_bytes());
			let hash = hash.finalize().to_sri_string();
			let hash = hash.split_once('-').map_or(hash.as_str(), |(_, hash)| hash);
			return open_lock(&path(hash));
		}
		open_lock(&path(uri))
	}

	fn copy_to(
		&self,
		peer: &dyn Peer,
		paths: &BTreeSet<StorePath>,
		substitute: bool,
		(here, there): &(Name, Name),
	) -> Result<(), Failure> {
		let valid = peer.valid_paths(paths, substitute)?;
		let missing: BTreeSet<StorePath> = paths.difference(&valid).cloned().collect();
		let store = self.store.store();
		copy_paths(
			&missing,
			&|path| store.query_path_info(path),
			&mut |path, sink| store.nar_from_path(path, sink),
			&mut |infos, nar| peer.add_paths(infos, nar),
			here,
			there,
		)
	}

	fn copy_from(
		&self,
		peer: &dyn Peer,
		paths: &BTreeSet<StorePath>,
		(here, there): &(Name, Name),
	) -> Result<(), Failure> {
		let store = self.store.store();
		copy_paths(
			paths,
			&|path| peer.path_info(path),
			&mut |path, sink| peer.nar(path, sink),
			&mut |infos, nar| store.add_multiple_to_store(infos, nar, false, false),
			there,
			here,
		)
	}
}

fn open_lock(path: &Path) -> Result<File, Failure> {
	open_lock_file(path, true)?
		.ok_or_else(|| Failure::Error(format!("opening lock file \"{}\"", path.display())))
}

/// A store in the message of a copied path.
struct Name {
	uri: String,
	/// `local` or the daemon, without a path.
	shorthand: bool,
}

impl Name {
	/// The store `reference` names. `LocalStore` is called `local` whatever
	/// its root, and `local://` if it has parameters.
	fn of(reference: &StoreReference) -> Self {
		let scheme = match &reference.variant {
			Variant::Specified { scheme, .. } => scheme.as_str(),
			Variant::Daemon => "unix",
			Variant::Auto | Variant::Local => "local",
		};
		let parameters = !reference.params.is_empty();
		match scheme {
			"local" => Self {
				uri: if parameters { "local://" } else { "local" }.to_owned(),
				shorthand: true,
			},
			"unix" => Self {
				shorthand: reference.render(false) == "unix://"
					|| reference.variant == Variant::Daemon,
				uri: reference.render(false),
			},
			_ => Self {
				uri: reference.render(false),
				shorthand: false,
			},
		}
	}
}

type Nar<'a> = dyn FnMut(&StorePath, &mut dyn Write) -> noxi_store::Result<()> + 'a;

/// `copyPaths` of paths the destination does not have, references first.
fn copy_paths(
	paths: &BTreeSet<StorePath>,
	info: &dyn Fn(&StorePath) -> noxi_store::Result<Option<ValidPathInfo>>,
	nar: &mut Nar<'_>,
	add: &mut dyn FnMut(&[ValidPathInfo], &mut Nar<'_>) -> noxi_store::Result<()>,
	from: &Name,
	to: &Name,
) -> Result<(), Failure> {
	if paths.is_empty() {
		return Ok(());
	}
	let _act = Activity::new(
		VerbosityLevel::Info,
		ActivityType::CopyPaths,
		format!("copying {} paths", paths.len()),
		vec![],
		0,
	);
	let mut known: BTreeMap<StorePath, ValidPathInfo> = BTreeMap::new();
	for path in paths {
		let info = info(path)?.ok_or_else(|| {
			Failure::Error(format!("path '{}' is not valid", path.to_absolute_path()))
		})?;
		known.insert(path.clone(), info);
	}
	let sorted = crate::paths::topo_sort_by(paths, |path| Ok(known[path].references.clone()))?;
	let infos: Vec<ValidPathInfo> = sorted
		.into_iter()
		.rev()
		.map(|path| {
			let mut info = known[&path].clone();
			info.ultimate = false;
			info
		})
		.collect();
	add(&infos, &mut |path, sink| {
		let text = path.to_absolute_path();
		let message = if from.shorthand {
			format!("copying path '{text}' to '{}'", to.uri)
		} else if to.shorthand {
			format!("copying path '{text}' from '{}'", from.uri)
		} else {
			format!("copying path '{text}' from '{}' to '{}'", from.uri, to.uri)
		};
		let _act = Activity::new(
			VerbosityLevel::Info,
			ActivityType::CopyPath,
			message,
			vec![
				text.into(),
				from.uri.as_str().into(),
				to.uri.as_str().into(),
			],
			0,
		);
		nar(path, sink)
	})?;
	Ok(())
}
