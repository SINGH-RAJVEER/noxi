//! `noxi system build|test|boot|switch|dry-activate|rollback`: native system
//! generation builds and activation.
//!
//! A rebuild runs the phases of docs/logging.md in order: resolve the
//! configuration source, optionally update its flake inputs, evaluate and
//! build the system toplevel through the in-process compatibility
//! implementation, prepare activation (register the generation for `switch`
//! and `boot`), run `switch-to-configuration`, and verify the observed
//! profile and running-system links. Every run that gets past resolve is
//! recorded in the [snapshot journal](crate::snapshot).
//!
//! The three side-effecting pieces are injectable for tests: the
//! [`Builder`], the activation program, and the profile, running-system
//! and journal locations. [`parse`] takes the environment as a function, so
//! tests build [`RebuildOptions`] without touching the process environment.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use nix_compat::nixcpp::settings::Settings;
use nix_compat::store_path::StorePath;
use noxi_commands::cli::Failure;
use noxi_store::{DerivedPath, profiles};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::flake;
use crate::snapshot::{
	self, Action, CURRENT_SYSTEM, DEFAULT_PROFILE, Observed, Outcome, Pending, Phase, Request,
	SnapshotStore, SourceKind, Status,
};

pub const USAGE: &str = "usage: noxi system {build|test|boot|switch|dry-activate} [--flake PATH[#HOST] | --file PATH [--attr ATTR]] [--update [INPUT...]] [--profile-name NAME | --profile PATH] [--install-bootloader] [--no-snapshot] [--json] [--out-link PATH]\n       noxi system rollback [--to GENERATION] [--profile-name NAME | --profile PATH] [--no-snapshot] [--json]";

/// Environment variable naming an explicit system profile path.
pub const PROFILE_ENV: &str = "NOXI_SYSTEM_PROFILE";
/// Environment variable naming the running-system link.
pub const CURRENT_SYSTEM_ENV: &str = "NOXI_CURRENT_SYSTEM";
/// Environment variable naming the activation program to run instead of
/// `<toplevel>/bin/switch-to-configuration`.
pub const ACTIVATION_ENV: &str = "NOXI_ACTIVATION_PROGRAM";
/// Where `--profile-name NAME` profiles live, as in nixos-rebuild.
pub const SYSTEM_PROFILES_DIR: &str = "/nix/var/nix/profiles/system-profiles";
/// The flake used when no source is given and it has a `flake.nix`.
pub const DEFAULT_FLAKE_DIR: &str = "/etc/nixos";
/// The file used when no source is given and there is no default flake.
pub const DEFAULT_FILE: &str = "<nixpkgs/nixos>";
/// The attribute path of the system toplevel inside a NixOS configuration.
pub const TOPLEVEL: &str = "config.system.build.toplevel";

/// Experimental features enabled for flake builds.
const FEATURES: &str = "nix-command flakes";
/// Stack size for the evaluator thread, matching the compatibility commands.
const STACK_SIZE: usize = 64 * 1024 * 1024;

// Options.

/// Where the configuration comes from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SourceChoice {
	/// `--flake /etc/nixos` when `/etc/nixos/flake.nix` exists, otherwise
	/// `--file <nixpkgs/nixos>`.
	#[default]
	Default,
	/// `--flake PATH[#HOST]`.
	Flake(String),
	/// `--file PATH [--attr ATTR]`.
	File {
		path: String,
		attribute: Option<String>,
	},
}

/// Where the snapshot journal is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Journal {
	/// `--no-snapshot`: write no record.
	Disabled,
	/// [`SnapshotStore::default_location`].
	#[default]
	Default,
	/// An explicit directory, such as `NOXI_SNAPSHOT_DIR`.
	Dir(PathBuf),
}

/// What [`Builder::build`] is asked to realize.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildRequest {
	pub kind: SourceKind,
	/// The flake reference without its fragment, or the file path.
	pub source: String,
	/// The attribute path of the toplevel.
	pub attribute: String,
	/// The link the build should leave pointing at the result. It is the
	/// out-link for `build` and a temporary link otherwise.
	pub link: PathBuf,
}

impl BuildRequest {
	/// The installable as `nix build` takes it, or `FILE` with `--attr`.
	pub fn installable(&self) -> String {
		match self.kind {
			SourceKind::Flake => format!("{}#{}", self.source, self.attribute),
			SourceKind::File => format!("{} --attr {}", self.source, self.attribute),
		}
	}
}

/// A failed evaluation or build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildError {
	/// [`Phase::Evaluate`] or [`Phase::Build`].
	pub phase: Phase,
	pub message: String,
}

impl BuildError {
	pub fn new(phase: Phase, message: impl Into<String>) -> Self {
		BuildError {
			phase,
			message: message.into(),
		}
	}
}

/// Evaluates and realizes the toplevel, and registers profile generations.
pub trait Builder {
	/// Realize the toplevel and return its path.
	fn build(&self, request: &BuildRequest) -> Result<PathBuf, BuildError>;

	/// Create (or reuse) the profile generation pointing at `closure` and
	/// return its link. The caller holds the profile lock and switches the
	/// profile link afterwards. The default creates a plain symbolic link,
	/// numbered as [`profiles::create_generation`] numbers it.
	fn register_generation(&self, profile: &Path, closure: &Path) -> Result<PathBuf, String> {
		link_generation(profile, closure)
	}
}

impl<F> Builder for F
where
	F: Fn(&BuildRequest) -> Result<PathBuf, BuildError>,
{
	fn build(&self, request: &BuildRequest) -> Result<PathBuf, BuildError> {
		self(request)
	}
}

/// Options for one `noxi system ACTION` run.
pub struct RebuildOptions {
	/// One of build, test, boot, switch and dry-activate.
	pub action: Action,
	pub source: SourceChoice,
	/// `--update [INPUT...]`: `Some(empty)` updates every input.
	pub update: Option<Vec<String>>,
	pub profile: PathBuf,
	/// The profile was chosen with `--profile` or `NOXI_SYSTEM_PROFILE`.
	pub profile_diverted: bool,
	pub current_system: PathBuf,
	/// Run this instead of `<toplevel>/bin/switch-to-configuration`.
	pub activation_program: Option<PathBuf>,
	pub install_bootloader: bool,
	pub journal: Journal,
	pub json: bool,
	/// The out-link for `build`; `./result` when `None`.
	pub out_link: Option<PathBuf>,
	/// The effective user id the privilege check uses.
	pub euid: u32,
	/// The host name selecting `nixosConfigurations.HOST` when a flake
	/// reference has no fragment.
	pub hostname: Option<String>,
	pub builder: Box<dyn Builder>,
}

/// Options for `noxi system rollback`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RollbackRequest {
	/// The generation to select; the newest one older than the current
	/// when `None`.
	pub to: Option<u64>,
	pub profile: PathBuf,
	pub profile_diverted: bool,
	pub current_system: PathBuf,
	pub activation_program: Option<PathBuf>,
	pub journal: Journal,
	pub json: bool,
	pub euid: u32,
}

/// A parsed command line.
pub enum Invocation {
	Help,
	Rebuild(Box<RebuildOptions>),
	Rollback(RollbackRequest),
}

fn effective_uid() -> u32 {
	// SAFETY: `geteuid` has no preconditions.
	unsafe { libc::geteuid() }
}

/// Parse the arguments after `noxi system`, reading `NOXI_SYSTEM_PROFILE`,
/// `NOXI_CURRENT_SYSTEM`, `NOXI_ACTIVATION_PROGRAM` and
/// `NOXI_SNAPSHOT_DIR` through `env`. The builder is [`CompatBuilder`].
pub fn parse(
	args: Vec<String>,
	env: &dyn Fn(&str) -> Option<String>,
) -> Result<Invocation, String> {
	let env = |name: &str| env(name).filter(|value| !value.is_empty());
	let mut args = args.into_iter().peekable();
	let action = match args.next().as_deref() {
		Some("build") => Action::Build,
		Some("test") => Action::Test,
		Some("boot") => Action::Boot,
		Some("switch") => Action::Switch,
		Some("dry-activate") => Action::DryActivate,
		Some("rollback") => Action::Rollback,
		Some("--help" | "-h" | "help") => return Ok(Invocation::Help),
		Some(other) => return Err(format!("unknown system action '{other}'")),
		None => return Err("missing system action".to_owned()),
	};
	let rollback = action == Action::Rollback;
	let mut flake = None;
	let mut file = None;
	let mut attribute = None;
	let mut update = None::<Vec<String>>;
	let mut profile_name = None;
	let mut profile = None;
	let mut install_bootloader = false;
	let mut no_snapshot = false;
	let mut json = false;
	let mut out_link = None;
	let mut to = None;
	let value = |flag: &str, args: &mut dyn Iterator<Item = String>| {
		args.next()
			.ok_or_else(|| format!("flag '{flag}' requires an argument"))
	};
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--help" | "-h" => return Ok(Invocation::Help),
			"--json" => json = true,
			"--no-snapshot" => no_snapshot = true,
			"--profile-name" | "-p" => profile_name = Some(value(&arg, &mut args)?),
			"--profile" => profile = Some(PathBuf::from(value(&arg, &mut args)?)),
			"--to" if rollback => {
				let number = value(&arg, &mut args)?;
				to = Some(
					number
						.parse::<u64>()
						.map_err(|_| format!("'{number}' is not a generation number"))?,
				);
			}
			"--flake" if !rollback => flake = Some(value(&arg, &mut args)?),
			"--file" | "-f" if !rollback => file = Some(value(&arg, &mut args)?),
			"--attr" | "-A" if !rollback => attribute = Some(value(&arg, &mut args)?),
			"--install-bootloader" if !rollback => install_bootloader = true,
			"--out-link" | "-o" if action == Action::Build => {
				out_link = Some(PathBuf::from(value(&arg, &mut args)?))
			}
			"--update" if !rollback => {
				let mut inputs = Vec::new();
				while let Some(next) = args.next_if(|next| !next.starts_with('-')) {
					inputs.push(next);
				}
				update = Some(inputs);
			}
			flag if flag.starts_with('-') => return Err(format!("unknown flag '{flag}'")),
			operand => return Err(format!("unexpected argument '{operand}'")),
		}
	}
	if profile.is_some() && profile_name.is_some() {
		return Err("'--profile' and '--profile-name' are mutually exclusive".to_owned());
	}
	let profile_diverted =
		profile.is_some() || (profile_name.is_none() && env(PROFILE_ENV).is_some());
	let profile = match (profile, profile_name) {
		(Some(path), _) => path,
		(None, Some(name)) if name == "system" => PathBuf::from(DEFAULT_PROFILE),
		(None, Some(name)) => {
			if name.is_empty() || name.contains('/') {
				return Err(format!("'{name}' is not a profile name"));
			}
			Path::new(SYSTEM_PROFILES_DIR).join(name)
		}
		(None, None) => {
			env(PROFILE_ENV).map_or_else(|| PathBuf::from(DEFAULT_PROFILE), PathBuf::from)
		}
	};
	let current_system =
		env(CURRENT_SYSTEM_ENV).map_or_else(|| PathBuf::from(CURRENT_SYSTEM), PathBuf::from);
	let activation_program = env(ACTIVATION_ENV).map(PathBuf::from);
	let journal = if no_snapshot {
		Journal::Disabled
	} else {
		match env(snapshot::DIR_ENV) {
			Some(dir) => Journal::Dir(PathBuf::from(dir)),
			None => Journal::Default,
		}
	};
	if rollback {
		return Ok(Invocation::Rollback(RollbackRequest {
			to,
			profile,
			profile_diverted,
			current_system,
			activation_program,
			journal,
			json,
			euid: effective_uid(),
		}));
	}
	let source = match (flake, file, attribute) {
		(Some(_), Some(_), _) => {
			return Err("'--flake' and '--file' are mutually exclusive".to_owned());
		}
		(Some(_), None, Some(_)) => {
			return Err(
				"'--attr' selects an attribute of '--file'; with '--flake' use PATH#HOST"
					.to_owned(),
			);
		}
		(None, None, Some(_)) => return Err("'--attr' requires '--file'".to_owned()),
		(Some(reference), None, None) => SourceChoice::Flake(reference),
		(None, Some(path), attribute) => SourceChoice::File { path, attribute },
		(None, None, None) => SourceChoice::Default,
	};
	if update.is_some() && matches!(source, SourceChoice::File { .. }) {
		return Err("'--update' re-locks flake inputs and needs a flake source".to_owned());
	}
	Ok(Invocation::Rebuild(Box::new(RebuildOptions {
		action,
		source,
		update,
		profile,
		profile_diverted,
		current_system,
		activation_program,
		install_bootloader,
		journal,
		json,
		out_link,
		euid: effective_uid(),
		hostname: snapshot::host_name(),
		builder: Box::new(CompatBuilder),
	})))
}

// The in-process compatibility builder.

/// Builds through the compatibility `nix-build` (for `--file`) and
/// `nix build` (for `--flake`) implementations in this process, and
/// registers generations as `nix-env --set` does.
#[derive(Clone, Copy, Debug, Default)]
pub struct CompatBuilder;

fn load_settings(flakes: bool) -> Result<Settings, String> {
	let mut settings = Settings::load(|name| {
		let value = std::env::var(name).ok();
		if flakes && name == "NIX_CONFIG" {
			// Enabling the features ahead of `NIX_CONFIG` keeps flake
			// settings given there from being ignored.
			Some(format!(
				"extra-experimental-features = {FEATURES}\n{}",
				value.unwrap_or_default()
			))
		} else {
			value
		}
	})
	.map_err(|e| format!("cannot load settings: {e}"))?;
	if flakes {
		settings
			.set("extra-experimental-features", FEATURES)
			.map_err(|e| e.to_string())?;
	}
	Ok(settings)
}

fn failure_message(failure: Failure) -> String {
	match failure {
		Failure::Usage(message) | Failure::Error(message) | Failure::Exit(_, message) => message,
	}
}

/// `nix-build FILE --attr ATTR --out-link LINK`, split into evaluation and
/// build so that a failure names its phase.
fn build_file(request: &BuildRequest) -> Result<PathBuf, BuildError> {
	let evaluate = |message: String| BuildError::new(Phase::Evaluate, message);
	let build = |message: String| BuildError::new(Phase::Build, message);
	let settings = load_settings(false).map_err(evaluate)?;
	let options = noxi_commands::instantiate::parse_args_with(
		[
			request.source.clone(),
			"--attr".to_owned(),
			request.attribute.clone(),
		],
		settings,
		|_, _| None,
	)
	.map_err(evaluate)?;
	let cwd = std::env::current_dir()
		.map_err(|e| evaluate(format!("cannot read the current directory: {e}")))?;
	let evaluated = noxi_commands::instantiate::evaluate(&options, &cwd).map_err(|_| {
		evaluate(format!(
			"evaluating '{}' failed; the cause is reported above",
			request.installable()
		))
	})?;
	let derivations =
		noxi_commands::instantiate::derivations(&evaluated.results).map_err(evaluate)?;
	let [(drv, output)] = &derivations[..] else {
		return Err(evaluate(format!(
			"'{}' evaluated to {} derivations, not one system toplevel",
			request.installable(),
			derivations.len()
		)));
	};
	let store = evaluated
		.store
		.ok_or_else(|| build("the selected store cannot build derivations".to_owned()))?;
	let paths = [DerivedPath::Built {
		drv: drv.clone(),
		outputs: BTreeSet::from([output.clone()]),
	}];
	let missing = store
		.query_missing(&paths)
		.map_err(|e| build(e.to_string()))?;
	noxi_commands::paths::print_missing(store.as_ref(), &missing)
		.map_err(|e| build(failure_message(e)))?;
	store
		.build_paths(&paths)
		.map_err(|e| build(e.to_string()))?;
	let out = store
		.query_derivation_outputs(drv)
		.map_err(|e| build(e.to_string()))?
		.remove(output)
		.ok_or_else(|| {
			build(format!(
				"derivation '{}' does not have an output named '{output}'",
				drv.to_absolute_path()
			))
		})?;
	store
		.add_perm_root(&out, &request.link)
		.map_err(|e| build(e.to_string()))?;
	Ok(PathBuf::from(out.to_absolute_path()))
}

/// `nix build FLAKE#ATTR --out-link LINK`. Evaluation and build run as one
/// call, so a failure in either is reported as a build failure.
fn build_flake(request: &BuildRequest) -> Result<PathBuf, BuildError> {
	let build = |message: String| BuildError::new(Phase::Build, message);
	let settings = load_settings(true).map_err(build)?;
	let link = request.link.to_str().ok_or_else(|| {
		build(format!(
			"out-link '{}' is not valid UTF-8",
			request.link.display()
		))
	})?;
	let args = vec![
		"build".to_owned(),
		request.installable(),
		"--out-link".to_owned(),
		link.to_owned(),
	];
	match noxi_commands::nix::main(settings, args) {
		Ok(code) if code == ExitCode::SUCCESS => {}
		Ok(_) => {
			return Err(build(format!(
				"evaluating or building '{}' failed; the cause is reported above",
				request.installable()
			)));
		}
		Err(failure) => return Err(build(failure_message(failure))),
	}
	fs::read_link(&request.link).map_err(|e| {
		build(format!(
			"the build did not leave '{}' pointing at a result: {e}",
			request.link.display()
		))
	})
}

impl Builder for CompatBuilder {
	fn build(&self, request: &BuildRequest) -> Result<PathBuf, BuildError> {
		// The shared implementation uses process-wide logging and fetcher
		// state, so its operations in one process run one at a time.
		let _guard = flake::LOCK
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner());
		let request = request.clone();
		std::thread::Builder::new()
			.stack_size(STACK_SIZE)
			.spawn(move || match request.kind {
				SourceKind::Flake => build_flake(&request),
				SourceKind::File => build_file(&request),
			})
			.map_err(|e| {
				BuildError::new(
					Phase::Evaluate,
					format!("cannot start the evaluator thread: {e}"),
				)
			})?
			.join()
			.map_err(|_| BuildError::new(Phase::Build, "the build implementation panicked"))?
	}

	/// `nix-env --set`: a generation that is also a garbage-collector root
	/// in the store, when `closure` is a store path.
	fn register_generation(&self, profile: &Path, closure: &Path) -> Result<PathBuf, String> {
		let Ok(path) = StorePath::from_absolute_path(closure.as_os_str().as_encoded_bytes()) else {
			return link_generation(profile, closure);
		};
		let settings = load_settings(false)?;
		let uri = noxi_commands::store::store_uri(None, &settings);
		let opened = noxi_commands::store::open(&uri, &settings)?
			.ok_or_else(|| format!("the store '{uri}' cannot hold profile generations"))?;
		profiles::create_generation(opened.store.as_ref(), profile, &path)
			.map_err(|e| e.to_string())
	}
}

/// The link of generation `number` of `profile`.
fn generation_link(profile: &Path, number: u64) -> PathBuf {
	PathBuf::from(format!("{}-{number}-link", profile.display()))
}

/// The generation number [`link_generation`] would use for `closure`, and
/// whether it already exists.
fn proposed_generation(profile: &Path, closure: &Path) -> (u64, bool) {
	let generations = profiles::find_generations(profile)
		.map(|(generations, _)| generations)
		.unwrap_or_default();
	match generations.last() {
		Some(last) if fs::read_link(&last.path).is_ok_and(|target| target == closure) => {
			(last.number, true)
		}
		Some(last) => (last.number + 1, false),
		None => (1, false),
	}
}

/// A generation link to `closure` without a store: the last generation if
/// it already points there, else a new one numbered after it.
pub fn link_generation(profile: &Path, closure: &Path) -> Result<PathBuf, String> {
	let (number, exists) = proposed_generation(profile, closure);
	let link = generation_link(profile, number);
	if !exists {
		std::os::unix::fs::symlink(closure, &link)
			.map_err(|e| format!("cannot create generation link \"{}\": {e}", link.display()))?;
	}
	Ok(link)
}

// Observed state.

/// The profile's current generation, its link target (the boot default) and
/// the running-system link.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct State {
	generation: Option<u64>,
	boot: Option<PathBuf>,
	running: Option<PathBuf>,
}

fn read_link(path: &Path) -> Option<PathBuf> {
	fs::read_link(path).ok()
}

/// The target of a profile link: the generation link's own target, so that
/// the boot default compares with closures.
fn profile_target(profile: &Path) -> Option<PathBuf> {
	let link = read_link(profile)?;
	let link = if link.is_relative() {
		profile.parent().unwrap_or(Path::new("/")).join(link)
	} else {
		link
	};
	read_link(&link).or(Some(link))
}

fn observe(profile: &Path, current_system: &Path) -> State {
	State {
		generation: profiles::find_generations(profile)
			.ok()
			.and_then(|(_, current)| current),
		boot: profile_target(profile),
		running: read_link(current_system),
	}
}

/// The generation of `profile` whose link points at `closure`.
fn generation_of(profile: &Path, closure: &Path) -> Option<u64> {
	let (generations, _) = profiles::find_generations(profile).ok()?;
	generations
		.iter()
		.rev()
		.find(|generation| read_link(&generation.path).as_deref() == Some(closure))
		.map(|generation| generation.number)
}

// Reporting.

/// What the run observed; rendered as text or as the `--json` object.
struct Report {
	action: Action,
	source: Option<Value>,
	lock: Option<Value>,
	update: Option<Value>,
	closure: Option<PathBuf>,
	out_link: Option<PathBuf>,
	profile: PathBuf,
	current_system: PathBuf,
	before: Option<State>,
	after: Option<State>,
	snapshot: Option<String>,
	/// Why no snapshot id is reported.
	snapshot_note: Option<String>,
	outcome: Outcome,
	hint: Option<String>,
	rollback: Option<String>,
	notes: Vec<String>,
}

impl Report {
	fn new(action: Action, profile: &Path, current_system: &Path) -> Self {
		Report {
			action,
			source: None,
			lock: None,
			update: None,
			closure: None,
			out_link: None,
			profile: profile.to_owned(),
			current_system: current_system.to_owned(),
			before: None,
			after: None,
			snapshot: None,
			snapshot_note: None,
			outcome: Outcome::not_applied(None, "the operation did not start"),
			hint: None,
			rollback: None,
			notes: Vec::new(),
		}
	}

	fn json(&self) -> Value {
		let path = |path: &Option<PathBuf>| {
			path.as_ref()
				.map(|path| path.to_string_lossy().into_owned())
		};
		let pair = |get: &dyn Fn(&State) -> Value| {
			json!({
				"before": self.before.as_ref().map_or(Value::Null, get),
				"after": self.after.as_ref().map_or(Value::Null, get),
			})
		};
		json!({
			"action": self.action.as_str(),
			"source": self.source.clone().unwrap_or(Value::Null),
			"lock": self.lock.clone().unwrap_or(Value::Null),
			"update": self.update.clone().unwrap_or(Value::Null),
			"closure": path(&self.closure),
			"out_link": path(&self.out_link),
			"profile": self.profile.to_string_lossy(),
			"current_system": self.current_system.to_string_lossy(),
			"generation": pair(&|state| json!(state.generation)),
			"running": pair(&|state| json!(path(&state.running))),
			"boot": pair(&|state| json!(path(&state.boot))),
			"snapshot": self.snapshot,
			"outcome": {
				"status": self.outcome.status.as_str(),
				"phase": self.outcome.phase.map(Phase::as_str),
				"message": self.outcome.message,
			},
			"rollback": self.rollback,
		})
	}

	fn write_text(&self, out: &mut dyn Write) -> io::Result<()> {
		let show = |path: &Option<PathBuf>| {
			path.as_ref()
				.map_or_else(|| "none".to_owned(), |path| path.display().to_string())
		};
		writeln!(out, "action: {}", self.action)?;
		let status = self.outcome.status.as_str();
		match self.outcome.phase {
			Some(phase) if self.outcome.status != Status::Success => {
				writeln!(out, "outcome: {status} in phase {}", label(phase))?
			}
			_ => writeln!(out, "outcome: {status}")?,
		}
		writeln!(out, "closure: {}", show(&self.closure))?;
		if let Some(link) = &self.out_link {
			writeln!(out, "out link: {}", link.display())?;
		}
		writeln!(out, "profile: {}", self.profile.display())?;
		match (&self.before, &self.after) {
			(Some(before), Some(after)) => {
				let number =
					|n: Option<u64>| n.map_or_else(|| "none".to_owned(), |n| n.to_string());
				let generation_of = |path: &Option<PathBuf>| {
					path.as_ref()
						.and_then(|path| generation_of(&self.profile, path))
						.map_or_else(String::new, |n| format!(" (generation {n})"))
				};
				let change = |what: &str, a: &Option<PathBuf>, b: &Option<PathBuf>| {
					if a == b {
						format!("{what}: unchanged, {}{}", show(b), generation_of(b))
					} else {
						format!(
							"{what}: changed, {}{} -> {}{}",
							show(a),
							generation_of(a),
							show(b),
							generation_of(b)
						)
					}
				};
				writeln!(
					out,
					"{}",
					change("running system", &before.running, &after.running)
				)?;
				writeln!(out, "{}", change("boot default", &before.boot, &after.boot))?;
				if before.generation == after.generation {
					writeln!(
						out,
						"current generation: {} (unchanged)",
						number(after.generation)
					)?;
				} else {
					writeln!(
						out,
						"current generation: {} (was {})",
						number(after.generation),
						number(before.generation)
					)?;
				}
				writeln!(out, "previous generation: {}", number(before.generation))?;
			}
			_ => writeln!(out, "system state: not read; nothing was changed")?,
		}
		match (&self.snapshot, &self.snapshot_note) {
			(Some(id), _) => writeln!(out, "snapshot: {id}")?,
			(None, Some(note)) => writeln!(out, "snapshot: none ({note})")?,
			(None, None) => writeln!(out, "snapshot: none")?,
		}
		if let Some(rollback) = &self.rollback {
			writeln!(out, "rollback: {rollback}")?;
		}
		for note in &self.notes {
			writeln!(out, "note: {note}")?;
		}
		Ok(())
	}
}

/// The phase name as the phase lines print it.
fn label(phase: Phase) -> &'static str {
	match phase {
		Phase::PrepareActivation => "prepare activation",
		other => other.as_str(),
	}
}

fn phase_line(err: &mut dyn Write, phase: Phase, text: impl AsRef<str>) {
	let _ = writeln!(err, "{}: {}", label(phase), text.as_ref());
}

fn sha256_hex(bytes: &[u8]) -> String {
	Sha256::digest(bytes)
		.iter()
		.map(|byte| format!("{byte:02x}"))
		.collect()
}

fn mutates(action: Action) -> bool {
	matches!(
		action,
		Action::Test | Action::Boot | Action::Switch | Action::Rollback
	)
}

/// Why an unprivileged run must stop before touching anything, if it must.
/// A run is diverted when everything it would mutate is overridden: the
/// activation program for `test`, and also the profile for the others.
fn refusal(
	action: Action,
	euid: u32,
	profile_diverted: bool,
	activation_diverted: bool,
) -> Option<String> {
	if !mutates(action) || euid == 0 {
		return None;
	}
	let diverted = match action {
		Action::Test => activation_diverted,
		_ => activation_diverted && profile_diverted,
	};
	(!diverted).then(|| {
		format!(
			"'noxi system {action}' changes the running system or its profile and must run as root; nothing was changed"
		)
	})
}

fn open_journal(journal: &Journal) -> Option<Result<SnapshotStore, snapshot::Error>> {
	match journal {
		Journal::Disabled => None,
		Journal::Default => Some(SnapshotStore::default_location()),
		Journal::Dir(dir) => Some(SnapshotStore::open(dir.clone())),
	}
}

/// The rollback hint: back to the previous generation with
/// `noxi system rollback`, and the snapshot that recorded it, if any.
fn rollback_hint(
	report: &Report,
	journal: Option<&SnapshotStore>,
	default_profile: bool,
) -> Option<String> {
	let (before, after) = (report.before.as_ref()?, report.after.as_ref()?);
	let profile_flag = if default_profile {
		String::new()
	} else {
		format!(" --profile {}", report.profile.display())
	};
	if before.generation != after.generation {
		let previous = before.generation?;
		let mut hint = format!("noxi system rollback --to {previous}{profile_flag}");
		let recorded = journal
			.and_then(|journal| journal.list().ok())
			.and_then(|records| {
				records.into_iter().rev().find(|record| {
					record.profile == report.profile
						&& record.generation.after == Some(previous)
						&& record.outcome.status == Status::Success
						&& record.closure.is_some()
				})
			});
		if let Some(record) = recorded {
			hint.push_str(&format!(", or noxi snapshot rollback {}", record.id));
		}
		return Some(hint);
	}
	if before.running != after.running {
		return Some(format!(
			"run {}/bin/switch-to-configuration switch, or reboot, to return to the boot default",
			report.profile.display()
		));
	}
	None
}

/// Print the error block, if any, and the final report; return the exit
/// status.
fn conclude(report: &Report, out: &mut dyn Write, err: &mut dyn Write, json: bool) -> ExitCode {
	let failed = report.outcome.status != Status::Success;
	if failed {
		let phase = report.outcome.phase.map_or("", label);
		let message = report
			.outcome
			.message
			.as_deref()
			.unwrap_or("the operation did not complete");
		let _ = writeln!(err, "error: {phase}: {message}");
		if let Some(hint) = &report.hint {
			let _ = writeln!(err, "hint: {hint}");
		}
	}
	let written = if json {
		writeln!(out, "{}", report.json())
	} else {
		report.write_text(out)
	};
	if let Err(e) = written {
		let _ = writeln!(err, "error: verify: cannot print the final report: {e}");
		return ExitCode::FAILURE;
	}
	if failed {
		ExitCode::FAILURE
	} else {
		ExitCode::SUCCESS
	}
}

/// A sentence on how the profile and running system differ from before.
fn impact(before: &State, after: &State) -> String {
	let profile = if before.generation == after.generation && before.boot == after.boot {
		"the profile and boot default are unchanged".to_owned()
	} else {
		format!(
			"the profile now points at generation {} (boot default changed)",
			after
				.generation
				.map_or_else(|| "none".to_owned(), |n| n.to_string())
		)
	};
	let running = if before.running == after.running {
		"the running system link is unchanged"
	} else {
		"the running system link changed"
	};
	format!("{profile}; {running}")
}

// Resolve.

struct Resolved {
	kind: SourceKind,
	/// Flake reference without fragment, or file path.
	source: String,
	attribute: String,
	/// The local flake directory, for `--update`.
	flake_dir: Option<PathBuf>,
	lock: Option<PathBuf>,
}

/// The local directory a flake reference names, if it is local.
fn local_flake_dir(reference: &str) -> Option<&str> {
	if let Some(path) = reference.strip_prefix("path:") {
		return Some(path.split('?').next().unwrap_or(path));
	}
	if let Some(path) = reference.strip_prefix("git+file://") {
		return Some(path.split('?').next().unwrap_or(path));
	}
	(!reference.contains(':')).then_some(reference)
}

fn resolve(source: &SourceChoice, hostname: Option<&str>) -> Result<Resolved, String> {
	let source = match source {
		SourceChoice::Default => {
			let flake = Path::new(DEFAULT_FLAKE_DIR).join("flake.nix");
			if flake.exists() {
				// It can be a symbolic link to the actual flake.
				let flake = fs::canonicalize(&flake)
					.map_err(|e| format!("cannot resolve \"{}\": {e}", flake.display()))?;
				SourceChoice::Flake(
					flake
						.parent()
						.unwrap_or(Path::new("/"))
						.to_string_lossy()
						.into_owned(),
				)
			} else {
				SourceChoice::File {
					path: DEFAULT_FILE.to_owned(),
					attribute: None,
				}
			}
		}
		other => other.clone(),
	};
	match source {
		SourceChoice::Flake(reference) => {
			let (path, host) = reference.split_once('#').unwrap_or((&reference, ""));
			if host.contains('"') {
				return Err(format!("'{host}' is not a host name"));
			}
			let host = if host.is_empty() {
				hostname.unwrap_or("default")
			} else {
				host
			};
			let attribute = format!("nixosConfigurations.\"{host}\".{TOPLEVEL}");
			let path = if path.is_empty() { "." } else { path };
			let (source, flake_dir) = match local_flake_dir(path) {
				Some(local) => {
					let dir = fs::canonicalize(local)
						.map_err(|e| format!("cannot open flake directory '{local}': {e}"))?;
					if !dir.join("flake.nix").is_file() {
						return Err(format!(
							"directory '{}' does not contain a flake.nix",
							dir.display()
						));
					}
					let source = if path == local {
						dir.to_string_lossy().into_owned()
					} else {
						path.to_owned()
					};
					(source, Some(dir))
				}
				None => (path.to_owned(), None),
			};
			let lock = flake_dir.as_ref().map(|dir| dir.join("flake.lock"));
			Ok(Resolved {
				kind: SourceKind::Flake,
				source,
				attribute,
				flake_dir,
				lock,
			})
		}
		SourceChoice::File { path, attribute } => {
			let attribute = match attribute {
				Some(attribute) => format!("{attribute}.{TOPLEVEL}"),
				None => TOPLEVEL.to_owned(),
			};
			let source = if path.starts_with('<') {
				path
			} else {
				let cwd = std::env::current_dir()
					.map_err(|e| format!("cannot read the current directory: {e}"))?;
				let mut file = noxi_commands::instantiate::absolute_path(&cwd, Path::new(&path));
				if file.is_dir() {
					// As nixos-rebuild: a directory means its system.nix,
					// or its default.nix.
					if file.join("system.nix").exists() {
						file = file.join("system.nix");
					} else if file.join("default.nix").exists() {
						file = file.join("default.nix");
					}
				}
				if !file.exists() {
					return Err(format!(
						"configuration file '{}' does not exist",
						file.display()
					));
				}
				file.to_string_lossy().into_owned()
			};
			Ok(Resolved {
				kind: SourceKind::File,
				source,
				attribute,
				flake_dir: None,
				lock: None,
			})
		}
		SourceChoice::Default => unreachable!("the default source was resolved above"),
	}
}

// The rebuild.

/// Run one `noxi system ACTION`.
pub fn rebuild(options: RebuildOptions, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
	let action = options.action;
	let mut report = Report::new(action, &options.profile, &options.current_system);
	let default_profile = options.profile == Path::new(DEFAULT_PROFILE);
	if action == Action::Rollback {
		report.outcome = Outcome::not_applied(None, "use noxi system rollback");
		return conclude(&report, out, err, options.json);
	}

	// The privilege check runs before anything reads the profile.
	if let Some(message) = refusal(
		action,
		options.euid,
		options.profile_diverted,
		options.activation_program.is_some(),
	) {
		report.outcome = Outcome::not_applied(Some(Phase::PrepareActivation), message);
		report.hint = Some(format!("run 'noxi system {action}' as root"));
		report.snapshot_note = Some("refused before recording".to_owned());
		return conclude(&report, out, err, options.json);
	}

	// Resolve.
	let resolved = match resolve(&options.source, options.hostname.as_deref()) {
		Ok(resolved) => resolved,
		Err(message) => {
			report.outcome = Outcome::failed(Phase::Resolve, message);
			report.hint = Some(
				"check the --flake or --file argument; nothing was built or activated".to_owned(),
			);
			report.snapshot_note = Some("failed before recording".to_owned());
			return conclude(&report, out, err, options.json);
		}
	};
	if options.update.is_some() && resolved.flake_dir.is_none() {
		report.outcome = Outcome::failed(
			Phase::Resolve,
			format!(
				"'--update' needs a local flake directory, and '{}' is not one",
				resolved.source
			),
		);
		report.hint =
			Some("pass --flake with a local directory, or rebuild without --update".to_owned());
		report.snapshot_note = Some("failed before recording".to_owned());
		return conclude(&report, out, err, options.json);
	}
	let revision = resolved
		.flake_dir
		.as_ref()
		.map(|dir| dir.to_string_lossy().into_owned())
		.or_else(|| (!resolved.source.starts_with('<')).then(|| resolved.source.clone()))
		.map(|path| snapshot::source_revision(&path))
		.unwrap_or((None, None));
	report.source = Some(json!({
		"kind": resolved.kind.as_str(),
		"path": resolved.source,
		"attribute": resolved.attribute,
		"revision": revision.0,
		"dirty": revision.1,
	}));
	let lock_bytes = resolved.lock.as_ref().and_then(|lock| fs::read(lock).ok());
	let lock_text = match (&resolved.lock, &lock_bytes) {
		(Some(path), Some(bytes)) => {
			let sha256 = sha256_hex(bytes);
			report.lock =
				Some(json!({ "path": path.to_string_lossy(), "exists": true, "sha256": sha256 }));
			format!("lock {} sha256 {sha256}", path.display())
		}
		(Some(path), None) => {
			report.lock =
				Some(json!({ "path": path.to_string_lossy(), "exists": false, "sha256": null }));
			format!("lock {} (not present yet)", path.display())
		}
		(None, _) => "no lock file".to_owned(),
	};
	let revision_text = match &revision {
		(Some(rev), Some(true)) => format!("revision {rev} (uncommitted changes)"),
		(Some(rev), _) => format!("revision {rev}"),
		(None, _) => "revision unknown".to_owned(),
	};
	phase_line(
		err,
		Phase::Resolve,
		format!(
			"{action} {} {}#{}, {lock_text}, {revision_text}",
			resolved.kind.as_str(),
			resolved.source,
			resolved.attribute
		),
	);

	// Begin the snapshot record.
	let mutating = mutates(action);
	let mut journal_error = None;
	let journal = match open_journal(&options.journal) {
		None => {
			report.snapshot_note = Some("--no-snapshot".to_owned());
			let _ = writeln!(err, "note: --no-snapshot: no snapshot record is written");
			None
		}
		Some(Ok(journal)) => Some(journal),
		Some(Err(e)) => {
			journal_error = Some(e.to_string());
			None
		}
	};
	let mut pending: Option<Pending> = None;
	if let Some(journal) = &journal {
		let mut request = Request::new(action, resolved.kind, resolved.source.clone());
		request.attribute = Some(resolved.attribute.clone());
		request.lock = resolved.lock.clone();
		request.profile = options.profile.clone();
		request.current_system = options.current_system.clone();
		match journal.begin(request) {
			Ok(mut record) => {
				if let Some(lock) = &resolved.lock {
					// The update or the build may write the lock.
					record.refresh_lock_at_finish(lock.clone());
				}
				pending = Some(record);
			}
			Err(e) => journal_error = Some(e.to_string()),
		}
	}
	if let Some(message) = &journal_error {
		report.snapshot_note = Some("the journal could not be written".to_owned());
		if !mutating {
			let _ = writeln!(
				err,
				"warning: cannot write the snapshot journal: {message}; continuing without a record"
			);
		}
	}

	report.before = Some(observe(&options.profile, &options.current_system));
	let outcome = run_phases(
		&options,
		&resolved,
		&mut report,
		journal_error.as_deref(),
		err,
	);
	report.outcome = outcome;
	report.after = Some(observe(&options.profile, &options.current_system));

	if report.outcome.status != Status::Success
		&& let (Some(before), Some(after)) = (&report.before, &report.after)
	{
		let impact = impact(before, after);
		let message = report.outcome.message.take().unwrap_or_default();
		report.outcome.message = Some(format!("{message}; {impact}"));
	}

	if let Some(record) = pending {
		match record.finish(
			Observed {
				closure: report.closure.clone(),
			},
			report.outcome.clone(),
		) {
			Ok(record) => report.snapshot = Some(record.id),
			Err(e) if mutating && report.outcome.status == Status::Success => {
				report.snapshot_note = Some("the record could not be finished".to_owned());
				report.outcome = Outcome::failed(
					Phase::Verify,
					format!(
						"the system was activated, but its snapshot record could not be finished: {e}"
					),
				);
				report.hint = Some(
					"check the snapshot directory; the system state above is accurate".to_owned(),
				);
			}
			Err(e) => {
				report.snapshot_note = Some("the record could not be finished".to_owned());
				let _ = writeln!(err, "warning: cannot finish the snapshot record: {e}");
			}
		}
	}
	report.rollback = rollback_hint(&report, journal.as_ref(), default_profile);
	conclude(&report, out, err, options.json)
}

/// Update, evaluate, build, prepare activation, activate and verify,
/// returning the outcome. The report collects what each phase found.
fn run_phases(
	options: &RebuildOptions,
	resolved: &Resolved,
	report: &mut Report,
	journal_error: Option<&str>,
	err: &mut dyn Write,
) -> Outcome {
	let action = options.action;

	// Update.
	if let (Some(inputs), Some(dir)) = (&options.update, &resolved.flake_dir) {
		let result = flake::update(flake::UpdateOptions {
			flake: Some(dir.clone()),
			inputs: inputs.clone(),
			..Default::default()
		});
		match result {
			Ok(update) => {
				report.update = Some(flake::report_json(&update));
				let summary = if update.rewritten {
					format!(
						"{} {}: {} input{} changed, {} unchanged",
						if update.existed { "updated" } else { "created" },
						update.lock_path.display(),
						update.diff.changes.len(),
						if update.diff.changes.len() == 1 {
							""
						} else {
							"s"
						},
						update.diff.unchanged.len()
					)
				} else {
					format!("{}: no changes", update.lock_path.display())
				};
				phase_line(err, Phase::Update, summary);
				if update.rewritten {
					let mut text = Vec::new();
					let _ = flake::write_report(&update, &mut text);
					for line in String::from_utf8_lossy(&text).lines().skip(1) {
						let _ = writeln!(err, "  {line}");
					}
				}
			}
			Err(e) => {
				report.hint = Some("fix the input reference or rebuild without --update; nothing was built or activated".to_owned());
				return Outcome::failed(Phase::Update, e.to_string());
			}
		}
	}

	// Evaluate and build.
	phase_line(
		err,
		Phase::Evaluate,
		format!("{} of {}", resolved.attribute, resolved.source),
	);
	let temp = if action == Action::Build {
		None
	} else {
		match tempfile::Builder::new().prefix("noxi-system-").tempdir() {
			Ok(dir) => Some(dir),
			Err(e) => {
				report.hint = Some("check TMPDIR; nothing was built or activated".to_owned());
				return Outcome::failed(
					Phase::Build,
					format!("cannot create a temporary directory for the build result: {e}"),
				);
			}
		}
	};
	let link = match &temp {
		Some(dir) => dir.path().join("result"),
		None => {
			let link = options
				.out_link
				.clone()
				.unwrap_or_else(|| PathBuf::from("result"));
			match std::env::current_dir() {
				Ok(cwd) => noxi_commands::instantiate::absolute_path(&cwd, &link),
				Err(_) => link,
			}
		}
	};
	let request = BuildRequest {
		kind: resolved.kind,
		source: resolved.source.clone(),
		attribute: resolved.attribute.clone(),
		link: link.clone(),
	};
	let closure = match options.builder.build(&request) {
		Ok(closure) => closure,
		Err(e) => {
			report.hint = Some(
				"fix the error reported above and run the command again; nothing was activated"
					.to_owned(),
			);
			return Outcome::failed(e.phase, e.message);
		}
	};
	report.closure = Some(closure.clone());
	phase_line(err, Phase::Build, closure.display().to_string());

	if action == Action::Build {
		// An injected builder may not have created the out-link.
		if read_link(&link).as_deref() != Some(closure.as_path()) {
			if fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()) {
				let _ = fs::remove_file(&link);
			}
			if let Err(e) = std::os::unix::fs::symlink(&closure, &link) {
				report.hint = Some("remove the existing file or pass --out-link".to_owned());
				return Outcome::failed(
					Phase::Build,
					format!("cannot create out-link \"{}\": {e}", link.display()),
				);
			}
		}
		report.out_link = Some(link);
		phase_line(
			err,
			Phase::PrepareActivation,
			"skipped; build stops after realization",
		);
		phase_line(err, Phase::Activate, "no activation occurred");
		report
			.notes
			.push("build stops after realization; no activation occurred".to_owned());
		phase_line(err, Phase::Verify, "profile and running system untouched");
		return Outcome::success(Some(format!("built {}", closure.display())));
	}

	// Prepare activation.
	if let Some(message) = journal_error.filter(|_| mutates(action)) {
		report.hint = Some(
			"set NOXI_SNAPSHOT_DIR to a writable directory, run as root, or pass --no-snapshot"
				.to_owned(),
		);
		return Outcome::not_applied(
			Some(Phase::PrepareActivation),
			format!("cannot write the snapshot journal: {message}; nothing was activated"),
		);
	}
	let program = options
		.activation_program
		.clone()
		.unwrap_or_else(|| closure.join("bin/switch-to-configuration"));
	if !program.is_file() {
		report.hint = Some(
			"the closure is not a NixOS system toplevel; check the selected attribute".to_owned(),
		);
		return Outcome::not_applied(
			Some(Phase::PrepareActivation),
			format!(
				"activation program \"{}\" does not exist; nothing was activated",
				program.display()
			),
		);
	}
	let before = report.before.clone().unwrap_or_default();
	let current = before
		.generation
		.map_or_else(|| "no generation".to_owned(), |n| format!("generation {n}"));
	let running = before
		.running
		.as_ref()
		.map_or_else(|| "unknown".to_owned(), |path| path.display().to_string());
	let registers = matches!(action, Action::Switch | Action::Boot);
	if registers {
		let (number, exists) = proposed_generation(&options.profile, &closure);
		phase_line(
			err,
			Phase::PrepareActivation,
			format!(
				"{action}: profile {} at {current} -> generation {number}{}, running {running}",
				options.profile.display(),
				if exists { " (existing)" } else { "" }
			),
		);
		if let Err(message) = set_profile(options, &closure) {
			report.hint = Some("check permissions on the profile directory".to_owned());
			return Outcome::failed(Phase::PrepareActivation, message);
		}
	} else {
		phase_line(
			err,
			Phase::PrepareActivation,
			format!(
				"{action}: profile {} stays at {current}, running {running}",
				options.profile.display()
			),
		);
	}

	// Activate.
	phase_line(
		err,
		Phase::Activate,
		format!("running {} {action}", program.display()),
	);
	let mut command = Command::new(&program);
	command.arg(action.as_str()).env(
		"NIXOS_INSTALL_BOOTLOADER",
		if options.install_bootloader { "1" } else { "0" },
	);
	// As nixos-rebuild, keep the program's output off standard output.
	if let Ok(stderr) = io::stderr().as_fd().try_clone_to_owned() {
		command.stdout(Stdio::from(stderr));
	}
	match command.status() {
		Ok(status) if status.success() => {}
		Ok(status) => {
			report.hint = Some(if registers {
				"inspect the activation output above; `noxi system rollback` returns the profile to the previous generation".to_owned()
			} else {
				"inspect the activation output above".to_owned()
			});
			return Outcome::failed(
				Phase::Activate,
				format!(
					"\"{} {action}\" {status}; activation may have applied part of its changes",
					program.display()
				),
			);
		}
		Err(e) => {
			report.hint = Some("check that the activation program is executable".to_owned());
			return Outcome::failed(
				Phase::Activate,
				format!("cannot run \"{}\": {e}", program.display()),
			);
		}
	}

	// Verify.
	let after = observe(&options.profile, &options.current_system);
	let changed = |a: bool| if a { "changed" } else { "unchanged" };
	phase_line(
		err,
		Phase::Verify,
		format!(
			"running system {}, boot default {}",
			changed(before.running != after.running),
			changed(before.boot != after.boot)
		),
	);
	if registers && after.boot.as_deref() != Some(closure.as_path()) {
		report.hint = Some(format!("inspect {}", options.profile.display()));
		return Outcome::failed(
			Phase::Verify,
			format!(
				"the profile does not point at {} after activation",
				closure.display()
			),
		);
	}
	if matches!(action, Action::Switch | Action::Test)
		&& after.running.as_deref() != Some(closure.as_path())
	{
		report.hint = Some(format!(
			"inspect {} and the activation output above",
			options.current_system.display()
		));
		return Outcome::failed(
			Phase::Verify,
			format!(
				"the activation program succeeded, but {} does not point at {}",
				options.current_system.display(),
				closure.display()
			),
		);
	}
	match action {
		Action::Boot => report
			.notes
			.push("boot: the running system is unchanged until the next boot".to_owned()),
		Action::Test => report
			.notes
			.push("test: the boot default is unchanged".to_owned()),
		Action::DryActivate => report.notes.push(
			"dry-activate: the intended changes are listed above; none were applied".to_owned(),
		),
		_ => {}
	}
	Outcome::success(None)
}

/// `nix-env -p PROFILE --set CLOSURE` under the profile lock.
fn set_profile(options: &RebuildOptions, closure: &Path) -> Result<(), String> {
	let profile = &options.profile;
	if let Some(parent) = profile.parent()
		&& !parent.exists()
	{
		fs::create_dir_all(parent)
			.map_err(|e| format!("cannot create \"{}\": {e}", parent.display()))?;
	}
	let _lock = profiles::lock_profile(profile).map_err(|e| e.to_string())?;
	let generation = options.builder.register_generation(profile, closure)?;
	profiles::switch_link(profile, &generation).map_err(|e| {
		format!(
			"generation \"{}\" was created, but the profile could not be switched to it: {e}",
			generation.display()
		)
	})
}

// Rollback.

/// Run `noxi system rollback`.
pub fn rollback(request: RollbackRequest, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
	let action = Action::Rollback;
	let profile = request.profile.clone();
	let default_profile = profile == Path::new(DEFAULT_PROFILE);
	let mut report = Report::new(action, &profile, &request.current_system);
	if let Some(message) = refusal(
		action,
		request.euid,
		request.profile_diverted,
		request.activation_program.is_some(),
	) {
		report.outcome = Outcome::not_applied(Some(Phase::PrepareActivation), message);
		report.hint = Some("run 'noxi system rollback' as root".to_owned());
		report.snapshot_note = Some("refused before recording".to_owned());
		return conclude(&report, out, err, request.json);
	}

	// Resolve: the target generation.
	let fail_resolve = |report: &mut Report, message: String, hint: &str| {
		report.outcome = Outcome::failed(Phase::Resolve, message);
		report.hint = Some(hint.to_owned());
		report.snapshot_note = Some("failed before recording".to_owned());
	};
	let (generations, current) = match profiles::find_generations(&profile) {
		Ok(found) => found,
		Err(e) => {
			fail_resolve(
				&mut report,
				format!("cannot read profile \"{}\": {e}", profile.display()),
				"check the --profile argument",
			);
			return conclude(&report, out, err, request.json);
		}
	};
	let target = match request.to {
		Some(number) => generations.iter().find(|g| g.number == number),
		None => generations
			.iter()
			.rev()
			.find(|g| current.is_some_and(|current| g.number < current)),
	};
	let Some(target) = target else {
		let message = match (request.to, current) {
			(Some(number), _) => format!(
				"generation {number} of profile \"{}\" does not exist",
				profile.display()
			),
			(None, Some(current)) => format!(
				"profile \"{}\" has no generation older than the current one ({current})",
				profile.display()
			),
			(None, None) => format!(
				"profile \"{}\" has no current generation",
				profile.display()
			),
		};
		fail_resolve(
			&mut report,
			message,
			"list the generations with `nix-env -p PROFILE --list-generations`; nothing was changed",
		);
		return conclude(&report, out, err, request.json);
	};
	if Some(target.number) == current {
		fail_resolve(
			&mut report,
			format!(
				"generation {} is already the current generation",
				target.number
			),
			"choose another generation with --to",
		);
		return conclude(&report, out, err, request.json);
	}
	let number = target.number;
	let link = target.path.clone();
	let Some(closure) = read_link(&link) else {
		fail_resolve(
			&mut report,
			format!("cannot read generation link \"{}\"", link.display()),
			"choose another generation with --to",
		);
		return conclude(&report, out, err, request.json);
	};
	report.closure = Some(closure.clone());

	// The snapshot that recorded this generation, if any.
	let journal = open_journal(&request.journal);
	let mut journal_error = None;
	let journal = match journal {
		None => {
			report.snapshot_note = Some("--no-snapshot".to_owned());
			let _ = writeln!(err, "note: --no-snapshot: no snapshot record is written");
			None
		}
		Some(Ok(journal)) => Some(journal),
		Some(Err(e)) => {
			journal_error = Some(e.to_string());
			None
		}
	};
	let recorded = journal
		.as_ref()
		.and_then(|journal| journal.list().ok())
		.and_then(|records| {
			records.into_iter().rev().find(|record| {
				record.profile == profile
					&& record.generation.after == Some(number)
					&& record.closure.as_deref() == Some(closure.as_path())
					&& record.outcome.status == Status::Success
			})
		});
	report.source = recorded.as_ref().map(|record| {
		json!({
			"kind": record.source.kind.as_str(),
			"path": record.source.path,
			"attribute": record.source.attribute,
			"revision": record.source.revision,
			"dirty": record.source.dirty,
		})
	});
	phase_line(
		err,
		Phase::Resolve,
		format!(
			"rollback profile {} from {} to generation {number} ({}){}",
			profile.display(),
			current.map_or_else(|| "no generation".to_owned(), |n| format!("generation {n}")),
			closure.display(),
			recorded.as_ref().map_or_else(
				|| ", recorded by no snapshot".to_owned(),
				|record| format!(", recorded by snapshot {}", record.id)
			)
		),
	);

	let mut pending = None;
	if let Some(journal) = &journal {
		let (kind, path, attribute) = match &recorded {
			Some(record) => (
				record.source.kind,
				record.source.path.clone(),
				record.source.attribute.clone(),
			),
			None => (SourceKind::File, link.to_string_lossy().into_owned(), None),
		};
		let mut snapshot_request = Request::new(action, kind, path);
		snapshot_request.attribute = attribute;
		snapshot_request.profile = profile.clone();
		snapshot_request.current_system = request.current_system.clone();
		match journal.begin(snapshot_request) {
			Ok(record) => pending = Some(record),
			Err(e) => journal_error = Some(e.to_string()),
		}
	}
	if journal_error.is_some() {
		report.snapshot_note = Some("the journal could not be written".to_owned());
	}

	report.before = Some(observe(&profile, &request.current_system));
	let outcome = rollback_phases(
		&request,
		number,
		&closure,
		&link,
		journal_error.as_deref(),
		&mut report,
		err,
	);
	report.outcome = outcome;
	report.after = Some(observe(&profile, &request.current_system));
	if report.outcome.status != Status::Success
		&& let (Some(before), Some(after)) = (&report.before, &report.after)
	{
		let impact = impact(before, after);
		let message = report.outcome.message.take().unwrap_or_default();
		report.outcome.message = Some(format!("{message}; {impact}"));
	}
	if let Some(record) = pending {
		match record.finish(
			Observed {
				closure: Some(closure.clone()),
			},
			report.outcome.clone(),
		) {
			Ok(record) => report.snapshot = Some(record.id),
			Err(e) => {
				report.snapshot_note = Some("the record could not be finished".to_owned());
				if report.outcome.status == Status::Success {
					report.outcome = Outcome::failed(
						Phase::Verify,
						format!(
							"the rollback was applied, but its snapshot record could not be finished: {e}"
						),
					);
					report.hint = Some(
						"check the snapshot directory; the system state above is accurate"
							.to_owned(),
					);
				}
			}
		}
	}
	report.rollback = rollback_hint(&report, journal.as_ref(), default_profile);
	conclude(&report, out, err, request.json)
}

fn rollback_phases(
	request: &RollbackRequest,
	number: u64,
	closure: &Path,
	link: &Path,
	journal_error: Option<&str>,
	report: &mut Report,
	err: &mut dyn Write,
) -> Outcome {
	if let Some(message) = journal_error {
		report.hint = Some(
			"set NOXI_SNAPSHOT_DIR to a writable directory, run as root, or pass --no-snapshot"
				.to_owned(),
		);
		return Outcome::not_applied(
			Some(Phase::PrepareActivation),
			format!("cannot write the snapshot journal: {message}; nothing was changed"),
		);
	}
	let program = request
		.activation_program
		.clone()
		.unwrap_or_else(|| link.join("bin/switch-to-configuration"));
	if !program.is_file() {
		report.hint = Some(
			"the generation's closure may have been collected; choose another generation with --to"
				.to_owned(),
		);
		return Outcome::not_applied(
			Some(Phase::PrepareActivation),
			format!(
				"activation program \"{}\" does not exist; nothing was changed",
				program.display()
			),
		);
	}
	phase_line(
		err,
		Phase::PrepareActivation,
		format!(
			"switching profile {} to generation {number}",
			request.profile.display()
		),
	);
	let mut log = |_: &str| {};
	if let Err(e) = profiles::switch_generation(&request.profile, Some(number), false, &mut log) {
		report.hint = Some("check permissions on the profile directory".to_owned());
		return Outcome::failed(
			Phase::PrepareActivation,
			format!("cannot switch the profile to generation {number}: {e}"),
		);
	}
	phase_line(
		err,
		Phase::Activate,
		format!("running {} switch", program.display()),
	);
	let mut command = Command::new(&program);
	command.arg("switch").env("NIXOS_INSTALL_BOOTLOADER", "0");
	if let Ok(stderr) = io::stderr().as_fd().try_clone_to_owned() {
		command.stdout(Stdio::from(stderr));
	}
	match command.status() {
		Ok(status) if status.success() => {}
		Ok(status) => {
			report.hint = Some("inspect the activation output above".to_owned());
			return Outcome::failed(
				Phase::Activate,
				format!(
					"\"{} switch\" {status}; activation may have applied part of its changes",
					program.display()
				),
			);
		}
		Err(e) => {
			report.hint = Some("check that the activation program is executable".to_owned());
			return Outcome::failed(
				Phase::Activate,
				format!("cannot run \"{}\": {e}", program.display()),
			);
		}
	}
	let before = report.before.clone().unwrap_or_default();
	let after = observe(&request.profile, &request.current_system);
	phase_line(
		err,
		Phase::Verify,
		format!(
			"running system {}, boot default {}",
			if before.running != after.running {
				"changed"
			} else {
				"unchanged"
			},
			if before.boot != after.boot {
				"changed"
			} else {
				"unchanged"
			}
		),
	);
	if after.generation != Some(number) {
		report.hint = Some(format!("inspect {}", request.profile.display()));
		return Outcome::failed(
			Phase::Verify,
			format!("the profile is not at generation {number} after the rollback"),
		);
	}
	if after.running.as_deref() != Some(closure) {
		report.hint = Some(format!(
			"inspect {} and the activation output above",
			request.current_system.display()
		));
		return Outcome::failed(
			Phase::Verify,
			format!(
				"the activation program succeeded, but {} does not point at {}",
				request.current_system.display(),
				closure.display()
			),
		);
	}
	Outcome::success(Some(format!("selected and activated generation {number}")))
}

// Command line.

/// Run a parsed invocation.
pub fn execute(invocation: Invocation, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
	match invocation {
		Invocation::Help => {
			let _ = writeln!(out, "{USAGE}");
			ExitCode::SUCCESS
		}
		Invocation::Rebuild(options) => rebuild(*options, out, err),
		Invocation::Rollback(request) => rollback(request, out, err),
	}
}

/// Run `noxi system ARGS` with the process environment, writing the final
/// report to `out` and phase lines and diagnostics to `err`.
pub fn run(args: Vec<String>, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
	match parse(args, &|name| std::env::var(name).ok()) {
		Ok(invocation) => execute(invocation, out, err),
		Err(message) => {
			let _ = writeln!(err, "error: {message}\n{USAGE}");
			ExitCode::from(2)
		}
	}
}

pub fn main(args: Vec<String>) -> ExitCode {
	match parse(args, &|name| std::env::var(name).ok()) {
		// Unlocked handles: the build thread writes to standard error
		// while this one waits for it.
		Ok(invocation) => execute(invocation, &mut io::stdout(), &mut io::stderr()),
		Err(message) => crate::usage(&format!("error: {message}\n{USAGE}")),
	}
}
