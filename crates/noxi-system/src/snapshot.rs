//! `noxi snapshot`: the journal of system generations and the source and
//! lock revisions they were built from.
//!
//! A profile generation records only a store path. A snapshot ties that
//! generation to the configuration source, its revision and the flake lock
//! it was built from, the requested action, the actor, and the observed
//! outcome, so that a rollback can restore the recorded lock as well as the
//! generation.
//!
//! The rebuilder records an operation in two steps:
//!
//! ```no_run
//! use noxi_system::snapshot::{Action, Observed, Outcome, Request, SnapshotStore, SourceKind};
//!
//! # fn main() -> Result<(), noxi_system::snapshot::Error> {
//! let store = SnapshotStore::default_location()?;
//! let mut request = Request::new(Action::Switch, SourceKind::Flake, "/etc/nixos");
//! request.attribute = Some("nixosConfigurations.host".into());
//! request.lock = Some("/etc/nixos/flake.lock".into());
//! let pending = store.begin(request)?;
//! // ... evaluate, build, switch the profile and activate ...
//! let closure = Some("/nix/store/...-nixos-system-host".into());
//! let snapshot = pending.finish(Observed { closure }, Outcome::success(None))?;
//! # let _ = snapshot;
//! # Ok(())
//! # }
//! ```
//!
//! `begin` writes a record whose outcome is `not-applied`, so a rebuild that
//! stops part way still leaves a record behind; `finish` rewrites it with
//! the observed closure, generations, running and boot states and outcome.

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use noxi_store::lock::PathLocks;
use noxi_store::profiles;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The record format this version writes.
pub const SCHEMA_VERSION: u32 = 1;
/// Environment variable that overrides the snapshot directory.
pub const DIR_ENV: &str = "NOXI_SNAPSHOT_DIR";
/// The NixOS system profile.
pub const DEFAULT_PROFILE: &str = "/nix/var/nix/profiles/system";
/// The link to the running NixOS system.
pub const CURRENT_SYSTEM: &str = "/run/current-system";

pub const USAGE: &str = "usage: noxi snapshot list [--json]\n       noxi snapshot show ID [--json]\n       noxi snapshot diff A B [--json]\n       noxi snapshot rollback ID [--no-activate] [--restore-lock] [--force] [--dry-run] [--profile PATH]";

/// Result of snapshot operations.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The action a snapshot records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
	Build,
	Test,
	Boot,
	Switch,
	DryActivate,
	Rollback,
}

impl Action {
	pub fn as_str(self) -> &'static str {
		match self {
			Action::Build => "build",
			Action::Test => "test",
			Action::Boot => "boot",
			Action::Switch => "switch",
			Action::DryActivate => "dry-activate",
			Action::Rollback => "rollback",
		}
	}
}

impl fmt::Display for Action {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}

/// The rebuild phase an outcome refers to, as docs/logging.md names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
	Resolve,
	/// `noxi system --update`: re-locking flake inputs before evaluation.
	Update,
	Evaluate,
	Build,
	PrepareActivation,
	Activate,
	Verify,
}

impl Phase {
	pub fn as_str(self) -> &'static str {
		match self {
			Phase::Resolve => "resolve",
			Phase::Update => "update",
			Phase::Evaluate => "evaluate",
			Phase::Build => "build",
			Phase::PrepareActivation => "prepare-activation",
			Phase::Activate => "activate",
			Phase::Verify => "verify",
		}
	}
}

impl fmt::Display for Phase {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}

/// Whether the recorded operation took effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
	Success,
	Failed,
	NotApplied,
}

impl Status {
	pub fn as_str(self) -> &'static str {
		match self {
			Status::Success => "success",
			Status::Failed => "failed",
			Status::NotApplied => "not-applied",
		}
	}
}

impl fmt::Display for Status {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}

/// The observed outcome of an operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
	pub status: Status,
	/// The last phase reached: where it failed, or where it completed.
	#[serde(default)]
	pub phase: Option<Phase>,
	#[serde(default)]
	pub message: Option<String>,
}

impl Outcome {
	pub fn success(message: Option<String>) -> Self {
		Outcome {
			status: Status::Success,
			phase: Some(Phase::Verify),
			message,
		}
	}

	pub fn failed(phase: Phase, message: impl Into<String>) -> Self {
		Outcome {
			status: Status::Failed,
			phase: Some(phase),
			message: Some(message.into()),
		}
	}

	pub fn not_applied(phase: Option<Phase>, message: impl Into<String>) -> Self {
		Outcome {
			status: Status::NotApplied,
			phase,
			message: Some(message.into()),
		}
	}
}

/// Who requested the operation, and where.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
	#[serde(default)]
	pub user: Option<String>,
	#[serde(default)]
	pub uid: Option<u32>,
	#[serde(default)]
	pub hostname: Option<String>,
}

impl Actor {
	/// The current process's user and the machine's host name.
	pub fn current() -> Self {
		// SAFETY: `getuid` has no preconditions.
		let uid = unsafe { libc::getuid() };
		Actor {
			user: user_name(uid).or_else(|| std::env::var("USER").ok()),
			uid: Some(uid),
			hostname: host_name(),
		}
	}
}

/// The kind of configuration source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
	Flake,
	File,
}

impl SourceKind {
	pub fn as_str(self) -> &'static str {
		match self {
			SourceKind::Flake => "flake",
			SourceKind::File => "file",
		}
	}
}

/// The recorded configuration source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
	pub kind: SourceKind,
	/// A local path or flake reference.
	pub path: String,
	/// The flake fragment or `--attr` value.
	#[serde(default)]
	pub attribute: Option<String>,
	/// The git HEAD commit or jj change id of the checkout holding `path`.
	#[serde(default)]
	pub revision: Option<String>,
	/// Whether that checkout had uncommitted changes, when known.
	#[serde(default)]
	pub dirty: Option<bool>,
}

/// The recorded flake lock. Its verbatim copy is `<id>.flake.lock`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockRecord {
	pub path: PathBuf,
	pub sha256: String,
}

/// Generation numbers of the profile before and after the operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Generations {
	#[serde(default)]
	pub before: Option<u64>,
	#[serde(default)]
	pub after: Option<u64>,
}

/// One journal record. Unknown fields are ignored on load, so records
/// written by a newer version still load.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
	pub schema_version: u32,
	pub id: String,
	/// RFC 3339 UTC.
	pub created_at: String,
	#[serde(default)]
	pub finished_at: Option<String>,
	pub action: Action,
	#[serde(default)]
	pub actor: Actor,
	pub source: Source,
	#[serde(default)]
	pub lock: Option<LockRecord>,
	/// The system toplevel; null when evaluation or build failed.
	#[serde(default)]
	pub closure: Option<PathBuf>,
	pub profile: PathBuf,
	#[serde(default)]
	pub generation: Generations,
	#[serde(default)]
	pub running_before: Option<PathBuf>,
	#[serde(default)]
	pub running_after: Option<PathBuf>,
	#[serde(default)]
	pub boot_before: Option<PathBuf>,
	#[serde(default)]
	pub boot_after: Option<PathBuf>,
	pub outcome: Outcome,
}

/// What the rebuilder asks to record when an operation starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
	pub action: Action,
	pub kind: SourceKind,
	/// A local path or flake reference.
	pub path: String,
	pub attribute: Option<String>,
	/// The flake lock file; it is copied when it exists.
	pub lock: Option<PathBuf>,
	pub profile: PathBuf,
	/// The running-system link, normally `/run/current-system`.
	pub current_system: PathBuf,
}

impl Request {
	/// A request against the system profile and `/run/current-system`.
	pub fn new(action: Action, kind: SourceKind, path: impl Into<String>) -> Self {
		Request {
			action,
			kind,
			path: path.into(),
			attribute: None,
			lock: None,
			profile: PathBuf::from(DEFAULT_PROFILE),
			current_system: PathBuf::from(CURRENT_SYSTEM),
		}
	}
}

/// What the rebuilder observed; generations and running and boot states
/// are read from the filesystem by [`Pending::finish`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observed {
	/// The built system toplevel, or `None` if evaluation or build failed.
	pub closure: Option<PathBuf>,
}

/// One input of a lock file, as the diff reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockedInput {
	pub name: String,
	pub rev: Option<String>,
	pub nar_hash: Option<String>,
	pub last_modified: Option<i64>,
}

/// An input present in both locks whose locked state differs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputChange {
	pub name: String,
	/// The differing fields among `rev`, `narHash` and `lastModified`.
	pub fields: Vec<String>,
	pub before: LockedInput,
	pub after: LockedInput,
}

/// The input-by-input difference between two flake locks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockDiff {
	pub added: Vec<LockedInput>,
	pub removed: Vec<LockedInput>,
	pub changed: Vec<InputChange>,
}

impl LockDiff {
	pub fn is_empty(&self) -> bool {
		self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
	}

	/// Compare two `flake.lock` documents. Nodes are compared by name,
	/// excluding the root node.
	pub fn between(before: &[u8], after: &[u8]) -> Result<LockDiff> {
		let before = lock_inputs(before, "the first lock")?;
		let mut after = lock_inputs(after, "the second lock")?;
		let mut diff = LockDiff::default();
		for (name, old) in before {
			match after.remove(&name) {
				None => diff.removed.push(old),
				Some(new) => {
					let mut fields = Vec::new();
					if old.rev != new.rev {
						fields.push("rev".to_owned());
					}
					if old.nar_hash != new.nar_hash {
						fields.push("narHash".to_owned());
					}
					if old.last_modified != new.last_modified {
						fields.push("lastModified".to_owned());
					}
					if !fields.is_empty() {
						diff.changed.push(InputChange {
							name,
							fields,
							before: old,
							after: new,
						});
					}
				}
			}
		}
		diff.added.extend(after.into_values());
		Ok(diff)
	}
}

fn lock_inputs(bytes: &[u8], what: &str) -> Result<BTreeMap<String, LockedInput>> {
	let invalid = |message: String| Error::LockFormat {
		what: what.to_owned(),
		message,
	};
	let value: Value = serde_json::from_slice(bytes).map_err(|e| invalid(e.to_string()))?;
	let nodes = value
		.get("nodes")
		.and_then(Value::as_object)
		.ok_or_else(|| invalid("it has no \"nodes\" object".to_owned()))?;
	let root = value.get("root").and_then(Value::as_str).unwrap_or("root");
	let mut inputs = BTreeMap::new();
	for (name, node) in nodes {
		if name == root {
			continue;
		}
		let locked = node.get("locked");
		let field = |key: &str| locked.and_then(|locked| locked.get(key));
		inputs.insert(
			name.clone(),
			LockedInput {
				name: name.clone(),
				rev: field("rev").and_then(Value::as_str).map(str::to_owned),
				nar_hash: field("narHash").and_then(Value::as_str).map(str::to_owned),
				last_modified: field("lastModified").and_then(Value::as_i64),
			},
		);
	}
	Ok(inputs)
}

/// Options for [`SnapshotStore::rollback`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RollbackOptions {
	/// Run `switch-to-configuration switch` after switching the profile.
	pub activate: bool,
	/// Write the snapshot's stored lock back to its recorded path.
	pub restore_lock: bool,
	/// Overwrite a lock edited since the most recent snapshot, and allow a
	/// target whose outcome was not `success`.
	pub force: bool,
	/// Check everything and report the plan without changing anything or
	/// recording a snapshot.
	pub dry_run: bool,
	/// The profile the snapshot's generation belongs to.
	pub profile: PathBuf,
	/// Defaults to `<generation link>/bin/switch-to-configuration`.
	pub activation_program: Option<PathBuf>,
	/// The running-system link, normally `/run/current-system`.
	pub current_system: PathBuf,
}

impl Default for RollbackOptions {
	fn default() -> Self {
		RollbackOptions {
			activate: true,
			restore_lock: false,
			force: false,
			dry_run: false,
			profile: PathBuf::from(DEFAULT_PROFILE),
			activation_program: None,
			current_system: PathBuf::from(CURRENT_SYSTEM),
		}
	}
}

/// What a rollback did, with the state observed afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RollbackReport {
	/// The snapshot rolled back to.
	pub target: Snapshot,
	/// The generation selected.
	pub generation: u64,
	pub generation_before: Option<u64>,
	/// The profile's current generation as observed afterwards.
	pub generation_after: Option<u64>,
	pub activation_program: PathBuf,
	/// Whether activation was requested (`RollbackOptions::activate`).
	pub activation_requested: bool,
	/// The lock path chosen for restoring, also on a dry run.
	pub lock_to_restore: Option<PathBuf>,
	/// Whether the activation program was run (whatever its exit status).
	pub activated: bool,
	/// The lock path written back, if any.
	pub lock_restored: Option<PathBuf>,
	pub dry_run: bool,
	pub running_after: Option<PathBuf>,
	pub boot_after: Option<PathBuf>,
	/// The `rollback` snapshot recording this operation; `None` on a dry run.
	pub record: Option<Snapshot>,
}

impl RollbackReport {
	/// Whether the rollback was carried out (or, on a dry run, would be).
	pub fn succeeded(&self) -> bool {
		self.record
			.as_ref()
			.is_none_or(|record| record.outcome.status == Status::Success)
	}
}

/// Snapshot journal errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("{context}: {source}")]
	Io {
		context: String,
		#[source]
		source: io::Error,
	},
	#[error("snapshot record \"{}\" is not valid: {source}", path.display())]
	Record {
		path: PathBuf,
		#[source]
		source: serde_json::Error,
	},
	#[error("{0}")]
	Store(#[from] noxi_store::Error),
	#[error("'{0}' is not a snapshot id; ids are numbers such as 0001")]
	InvalidId(String),
	#[error("snapshot {0} does not exist")]
	NotFound(String),
	#[error("snapshot {0} has no stored flake lock")]
	NoLock(String),
	#[error("{what} is not a valid flake lock: {message}")]
	LockFormat { what: String, message: String },
	#[error("snapshot {0} recorded no system generation and closure")]
	NoGeneration(String),
	#[error("snapshot {id} recorded outcome {status}, not success")]
	NotSuccessful { id: String, status: Status },
	#[error(
		"snapshot {id} belongs to profile \"{}\", not \"{}\"",
		recorded.display(),
		requested.display()
	)]
	ProfileMismatch {
		id: String,
		recorded: PathBuf,
		requested: PathBuf,
	},
	#[error("generation {generation} of profile \"{}\" no longer exists", profile.display())]
	GenerationMissing { profile: PathBuf, generation: u64 },
	#[error(
		"generation {generation} points at \"{}\", but snapshot {id} recorded \"{}\"",
		actual.display(),
		expected.display()
	)]
	GenerationMismatch {
		id: String,
		generation: u64,
		expected: PathBuf,
		actual: PathBuf,
	},
	#[error("the closure \"{}\" of generation {generation} no longer exists", closure.display())]
	ClosureMissing { closure: PathBuf, generation: u64 },
	#[error(
		"flake lock \"{}\" changed after snapshot {recorded_by} recorded it",
		path.display()
	)]
	LockEdited { path: PathBuf, recorded_by: String },
}

impl Error {
	fn io(context: impl Into<String>, source: io::Error) -> Self {
		Error::Io {
			context: context.into(),
			source,
		}
	}

	/// The rollback phase this error stops.
	pub fn phase(&self) -> Phase {
		match self {
			Error::GenerationMissing { .. }
			| Error::GenerationMismatch { .. }
			| Error::ClosureMissing { .. }
			| Error::LockEdited { .. }
			| Error::Store(_) => Phase::PrepareActivation,
			_ => Phase::Resolve,
		}
	}

	/// A justified next step, when there is one.
	pub fn hint(&self) -> Option<String> {
		Some(match self {
			Error::NotFound(_) | Error::InvalidId(_) => {
				"run `noxi snapshot list` to see the recorded ids".to_owned()
			}
			Error::NoLock(_) => {
				"roll back without --restore-lock, or choose a snapshot that recorded a lock".to_owned()
			}
			Error::NoGeneration(_) => {
				"this operation produced no generation; choose another snapshot with `noxi snapshot list`".to_owned()
			}
			Error::NotSuccessful { .. } => {
				"choose a successful snapshot, or pass --force to select its generation anyway".to_owned()
			}
			Error::ProfileMismatch { recorded, .. } => {
				format!("pass --profile {}", recorded.display())
			}
			Error::GenerationMissing { .. } | Error::ClosureMissing { .. } => {
				"the generation was collected; choose another snapshot with `noxi snapshot list`".to_owned()
			}
			Error::GenerationMismatch { .. } => {
				"the generation number was reused for another closure; choose another snapshot".to_owned()
			}
			Error::LockEdited { recorded_by, .. } => format!(
				"inspect the edit; pass --force to overwrite it with this snapshot's lock, or roll back without --restore-lock (the lock recorded by snapshot {recorded_by} differs from the file now)"
			),
			Error::Store(_) => "check permissions on the profile; the system profile needs root".to_owned(),
			Error::Io { .. } | Error::Record { .. } | Error::LockFormat { .. } => return None,
		})
	}
}

/// A record written by [`SnapshotStore::begin`] and not yet finished.
#[derive(Debug)]
pub struct Pending {
	dir: PathBuf,
	snapshot: Snapshot,
	current_system: PathBuf,
	/// Re-read the lock at this path when finishing.
	refresh_lock: Option<PathBuf>,
}

impl Pending {
	pub fn id(&self) -> &str {
		&self.snapshot.id
	}

	/// The record as written so far.
	pub fn snapshot(&self) -> &Snapshot {
		&self.snapshot
	}

	/// Record the lock at `path` as it is when the operation finishes rather
	/// than as it was at [`SnapshotStore::begin`], for operations that may
	/// rewrite the lock before building from it.
	pub fn refresh_lock_at_finish(&mut self, path: PathBuf) {
		self.refresh_lock = Some(path);
	}

	/// Fill in the observed closure, the profile's current generation, the
	/// running and boot states and the outcome, and rewrite the record.
	pub fn finish(mut self, observed: Observed, outcome: Outcome) -> Result<Snapshot> {
		if let Some(path) = self.refresh_lock.take() {
			let id = self.snapshot.id.clone();
			self.snapshot.lock = match read_optional(&path)? {
				Some(bytes) => Some(store_lock_copy(&self.dir, &id, &path, &bytes)?),
				None => {
					remove_if_exists(&lock_copy_path(&self.dir, &id))?;
					None
				}
			};
		}
		let (generation, boot) = profile_state(&self.snapshot.profile);
		let snapshot = &mut self.snapshot;
		snapshot.finished_at = Some(rfc3339(SystemTime::now()));
		snapshot.closure = observed.closure;
		snapshot.generation.after = generation;
		snapshot.running_after = read_link(&self.current_system);
		snapshot.boot_after = boot;
		snapshot.outcome = outcome;
		write_record(&self.dir, snapshot)?;
		Ok(self.snapshot)
	}
}

/// The snapshot directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotStore {
	dir: PathBuf,
}

impl SnapshotStore {
	/// The journal in `dir`. The directory is created by the first
	/// [`begin`](Self::begin), not here.
	pub fn open(dir: PathBuf) -> Result<Self> {
		match fs::metadata(&dir) {
			Ok(metadata) if !metadata.is_dir() => Err(Error::io(
				format!("opening snapshot directory \"{}\"", dir.display()),
				io::Error::new(io::ErrorKind::NotADirectory, "not a directory"),
			)),
			_ => Ok(SnapshotStore { dir }),
		}
	}

	/// `$NOXI_SNAPSHOT_DIR`, or `<nix state dir>/noxi/snapshots` with the
	/// state directory resolved as the store does (`NIX_STATE_DIR`).
	pub fn default_location() -> Result<Self> {
		let dir = match std::env::var_os(DIR_ENV).filter(|dir| !dir.is_empty()) {
			Some(dir) => PathBuf::from(dir),
			None => noxi_store::LocalStoreConfig::system()
				.state_dir
				.join("noxi/snapshots"),
		};
		Self::open(dir)
	}

	pub fn dir(&self) -> &Path {
		&self.dir
	}

	/// Where the lock copy of snapshot `id` is kept.
	pub fn lock_copy(&self, id: &str) -> PathBuf {
		lock_copy_path(&self.dir, id)
	}

	/// Allocate an id and write an initial record with outcome
	/// `not-applied`, recording the actor, source revision, lock and the
	/// profile, running and boot state before the operation.
	pub fn begin(&self, request: Request) -> Result<Pending> {
		let (revision, dirty) = source_revision(&request.path);
		let source = Source {
			kind: request.kind,
			path: request.path,
			attribute: request.attribute,
			revision,
			dirty,
		};
		let lock = match &request.lock {
			Some(path) => read_optional(path)?.map(|bytes| (path.clone(), bytes)),
			None => None,
		};
		self.begin_with(
			request.action,
			source,
			lock,
			request.profile,
			request.current_system,
			None,
		)
	}

	fn begin_with(
		&self,
		action: Action,
		source: Source,
		lock: Option<(PathBuf, Vec<u8>)>,
		profile: PathBuf,
		current_system: PathBuf,
		refresh_lock: Option<PathBuf>,
	) -> Result<Pending> {
		fs::create_dir_all(&self.dir).map_err(|e| {
			Error::io(
				format!("creating snapshot directory \"{}\"", self.dir.display()),
				e,
			)
		})?;
		let _lock = PathLocks::lock([self.dir.join("journal")]).map_err(|e| {
			Error::io(
				format!("locking snapshot directory \"{}\"", self.dir.display()),
				e,
			)
		})?;
		let id = format_id(self.max_id()? + 1);
		let lock = match lock {
			Some((path, bytes)) => Some(store_lock_copy(&self.dir, &id, &path, &bytes)?),
			None => None,
		};
		let (generation, boot) = profile_state(&profile);
		let snapshot = Snapshot {
			schema_version: SCHEMA_VERSION,
			id,
			created_at: rfc3339(SystemTime::now()),
			finished_at: None,
			action,
			actor: Actor::current(),
			source,
			lock,
			closure: None,
			profile,
			generation: Generations {
				before: generation,
				after: None,
			},
			running_before: read_link(&current_system),
			running_after: None,
			boot_before: boot,
			boot_after: None,
			outcome: Outcome::not_applied(None, "operation started; no outcome was recorded"),
		};
		write_record(&self.dir, &snapshot)?;
		Ok(Pending {
			dir: self.dir.clone(),
			snapshot,
			current_system,
			refresh_lock,
		})
	}

	/// The highest id with a record or lock copy, or 0.
	fn max_id(&self) -> Result<u64> {
		Ok(self
			.entries()?
			.into_iter()
			.chain(self.lock_copy_ids()?)
			.max()
			.unwrap_or(0))
	}

	fn read_dir(&self) -> Result<Vec<String>> {
		let entries = match fs::read_dir(&self.dir) {
			Ok(entries) => entries,
			Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
			Err(e) => {
				return Err(Error::io(
					format!("reading snapshot directory \"{}\"", self.dir.display()),
					e,
				));
			}
		};
		let mut names = Vec::new();
		for entry in entries {
			let entry = entry.map_err(|e| {
				Error::io(
					format!("reading snapshot directory \"{}\"", self.dir.display()),
					e,
				)
			})?;
			names.push(entry.file_name().to_string_lossy().into_owned());
		}
		Ok(names)
	}

	/// Numeric ids of `<id>.json` records.
	fn entries(&self) -> Result<Vec<u64>> {
		Ok(self
			.read_dir()?
			.iter()
			.filter_map(|name| parse_stem(name.strip_suffix(".json")?))
			.collect())
	}

	fn lock_copy_ids(&self) -> Result<Vec<u64>> {
		Ok(self
			.read_dir()?
			.iter()
			.filter_map(|name| parse_stem(name.strip_suffix(".flake.lock")?))
			.collect())
	}

	/// Every record, oldest first.
	pub fn list(&self) -> Result<Vec<Snapshot>> {
		let mut ids = self.entries()?;
		ids.sort_unstable();
		ids.into_iter()
			.map(|number| self.load(&format_id(number)))
			.collect()
	}

	/// The record with `id`; `1` and `0001` name the same record.
	pub fn get(&self, id: &str) -> Result<Snapshot> {
		self.load(&normalize_id(id)?)
	}

	/// The newest record.
	pub fn latest(&self) -> Result<Option<Snapshot>> {
		let ids = self.entries()?;
		match ids.into_iter().max() {
			Some(number) => self.load(&format_id(number)).map(Some),
			None => Ok(None),
		}
	}

	fn load(&self, id: &str) -> Result<Snapshot> {
		let path = self.dir.join(format!("{id}.json"));
		let bytes = match fs::read(&path) {
			Ok(bytes) => bytes,
			Err(e) if e.kind() == io::ErrorKind::NotFound => {
				return Err(Error::NotFound(id.to_owned()));
			}
			Err(e) => return Err(Error::io(format!("reading \"{}\"", path.display()), e)),
		};
		serde_json::from_slice(&bytes).map_err(|source| Error::Record { path, source })
	}

	/// The stored lock copy of a record.
	pub fn lock_bytes(&self, snapshot: &Snapshot) -> Result<Vec<u8>> {
		if snapshot.lock.is_none() {
			return Err(Error::NoLock(snapshot.id.clone()));
		}
		read_optional(&self.lock_copy(&snapshot.id))?
			.ok_or_else(|| Error::NoLock(snapshot.id.clone()))
	}

	/// Compare the lock copies of snapshots `a` and `b` input by input.
	pub fn diff_locks(&self, a: &str, b: &str) -> Result<LockDiff> {
		let a = self.lock_bytes(&self.get(a)?)?;
		let b = self.lock_bytes(&self.get(b)?)?;
		LockDiff::between(&a, &b)
	}

	/// Select the generation snapshot `id` recorded, optionally activate it
	/// and restore its lock, and record the rollback as a new snapshot.
	///
	/// Refusals (missing or reused generation, edited lock) happen before
	/// anything changes and are returned as errors. Once the profile is
	/// touched, failures are recorded in the new snapshot's outcome and the
	/// report is returned; check [`RollbackReport::succeeded`].
	pub fn rollback(&self, id: &str, options: RollbackOptions) -> Result<RollbackReport> {
		let target = self.get(id)?;
		if target.profile != options.profile {
			return Err(Error::ProfileMismatch {
				id: target.id,
				recorded: target.profile,
				requested: options.profile,
			});
		}
		let (Some(generation), Some(closure)) = (target.generation.after, target.closure.clone())
		else {
			return Err(Error::NoGeneration(target.id));
		};
		if target.outcome.status != Status::Success && !options.force {
			return Err(Error::NotSuccessful {
				id: target.id,
				status: target.outcome.status,
			});
		}
		let profile = options.profile.clone();
		let missing = || Error::GenerationMissing {
			profile: profile.clone(),
			generation,
		};
		let (generations, current) = profiles::find_generations(&profile).map_err(|_| missing())?;
		let link = generations
			.iter()
			.find(|g| g.number == generation)
			.ok_or_else(missing)?
			.path
			.clone();
		let actual = fs::read_link(&link).map_err(|_| missing())?;
		if actual != closure {
			return Err(Error::GenerationMismatch {
				id: target.id,
				generation,
				expected: closure,
				actual,
			});
		}
		if fs::metadata(&link).is_err() {
			return Err(Error::ClosureMissing {
				closure,
				generation,
			});
		}

		let lock_path = target.lock.as_ref().map(|lock| lock.path.clone());
		let restore = if options.restore_lock {
			let bytes = self.lock_bytes(&target)?;
			let path = lock_path.clone().expect("lock_bytes checked the record");
			if !options.force {
				self.check_lock_unchanged(&path)?;
			}
			Some((path, bytes))
		} else {
			None
		};

		let program = options
			.activation_program
			.clone()
			.unwrap_or_else(|| link.join("bin/switch-to-configuration"));
		if options.dry_run {
			let (_, boot) = profile_state(&profile);
			return Ok(RollbackReport {
				target,
				generation,
				generation_before: current,
				generation_after: current,
				activation_program: program,
				activation_requested: options.activate,
				lock_to_restore: restore.as_ref().map(|(path, _)| path.clone()),
				activated: false,
				lock_restored: None,
				dry_run: true,
				running_after: read_link(&options.current_system),
				boot_after: boot,
				record: None,
			});
		}

		let current_lock = match &lock_path {
			Some(path) => read_optional(path)?.map(|bytes| (path.clone(), bytes)),
			None => None,
		};
		let pending = self.begin_with(
			Action::Rollback,
			target.source.clone(),
			current_lock,
			profile.clone(),
			options.current_system.clone(),
			lock_path,
		)?;
		let observed = Observed {
			closure: Some(closure),
		};
		let mut report = RollbackReport {
			target,
			generation,
			generation_before: current,
			generation_after: None,
			activation_program: program.clone(),
			activation_requested: options.activate,
			lock_to_restore: restore.as_ref().map(|(path, _)| path.clone()),
			activated: false,
			lock_restored: None,
			dry_run: false,
			running_after: None,
			boot_after: None,
			record: None,
		};
		let outcome = self.apply_rollback(&mut report, &options, restore);
		let record = pending.finish(observed, outcome)?;
		report.generation_after = record.generation.after;
		report.running_after = record.running_after.clone();
		report.boot_after = record.boot_after.clone();
		report.record = Some(record);
		Ok(report)
	}

	fn check_lock_unchanged(&self, path: &Path) -> Result<()> {
		let snapshots = self.list()?;
		let recorded = snapshots.iter().rev().find_map(|snapshot| {
			snapshot
				.lock
				.as_ref()
				.filter(|lock| lock.path == path)
				.map(|lock| (snapshot.id.clone(), lock.sha256.clone()))
		});
		let Some((recorded_by, sha256)) = recorded else {
			return Ok(());
		};
		let current = read_optional(path)?.map(|bytes| sha256_hex(&bytes));
		if current.as_deref() != Some(sha256.as_str()) {
			return Err(Error::LockEdited {
				path: path.to_owned(),
				recorded_by,
			});
		}
		Ok(())
	}

	fn apply_rollback(
		&self,
		report: &mut RollbackReport,
		options: &RollbackOptions,
		restore: Option<(PathBuf, Vec<u8>)>,
	) -> Outcome {
		let generation = report.generation;
		let mut log = |_: &str| {};
		if let Err(e) =
			profiles::switch_generation(&options.profile, Some(generation), false, &mut log)
		{
			return Outcome::failed(
				Phase::PrepareActivation,
				format!("could not switch the profile to generation {generation}: {e}"),
			);
		}
		if let Some((path, bytes)) = restore {
			if let Err(e) = write_atomic(&path, &bytes) {
				return Outcome::failed(
					Phase::PrepareActivation,
					format!(
						"the profile now points at generation {generation}, but the lock was not restored: {e}"
					),
				);
			}
			report.lock_restored = Some(path);
		}
		if options.activate {
			report.activated = true;
			let program = &report.activation_program;
			match Command::new(program).arg("switch").status() {
				Ok(status) if status.success() => {}
				Ok(status) => {
					return Outcome::failed(
						Phase::Activate,
						format!(
							"\"{}\" {status}; the profile now points at generation {generation}",
							program.display()
						),
					);
				}
				Err(e) => {
					return Outcome::failed(
						Phase::Activate,
						format!(
							"could not run \"{}\": {e}; the profile now points at generation {generation}",
							program.display()
						),
					);
				}
			}
		}
		let (now, _) = profile_state(&options.profile);
		if now != Some(generation) {
			return Outcome::failed(
				Phase::Verify,
				format!(
					"the profile points at generation {} after the rollback, not {generation}",
					now.map_or("none".to_owned(), |n| n.to_string())
				),
			);
		}
		Outcome::success(Some(if options.activate {
			format!("selected and activated generation {generation}")
		} else {
			format!(
				"selected generation {generation}; switch-to-configuration was not run, so the running system and boot entries are unchanged"
			)
		}))
	}
}

fn parse_stem(stem: &str) -> Option<u64> {
	if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
		return None;
	}
	stem.parse().ok()
}

fn format_id(number: u64) -> String {
	format!("{number:04}")
}

fn normalize_id(id: &str) -> Result<String> {
	parse_stem(id.trim())
		.filter(|&number| number > 0)
		.map(format_id)
		.ok_or_else(|| Error::InvalidId(id.to_owned()))
}

fn lock_copy_path(dir: &Path, id: &str) -> PathBuf {
	dir.join(format!("{id}.flake.lock"))
}

fn store_lock_copy(dir: &Path, id: &str, path: &Path, bytes: &[u8]) -> Result<LockRecord> {
	write_atomic(&lock_copy_path(dir, id), bytes)?;
	Ok(LockRecord {
		path: path.to_owned(),
		sha256: sha256_hex(bytes),
	})
}

fn write_record(dir: &Path, snapshot: &Snapshot) -> Result<()> {
	let mut json = serde_json::to_vec_pretty(snapshot).map_err(|source| Error::Record {
		path: dir.join(format!("{}.json", snapshot.id)),
		source,
	})?;
	json.push(b'\n');
	write_atomic(&dir.join(format!("{}.json", snapshot.id)), &json)
}

/// Write `path` by renaming a synced temporary file in the same directory.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
	let dir = path.parent().unwrap_or(Path::new("."));
	let name = path.file_name().unwrap_or_default().to_string_lossy();
	let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
	let write = || -> io::Result<()> {
		let mut file = File::create(&tmp)?;
		file.write_all(bytes)?;
		file.sync_all()?;
		fs::rename(&tmp, path)?;
		File::open(dir)?.sync_all()
	};
	write().map_err(|e| {
		let _ = fs::remove_file(&tmp);
		Error::io(format!("writing \"{}\"", path.display()), e)
	})
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
	match fs::read(path) {
		Ok(bytes) => Ok(Some(bytes)),
		Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
		Err(e) => Err(Error::io(format!("reading \"{}\"", path.display()), e)),
	}
}

fn remove_if_exists(path: &Path) -> Result<()> {
	match fs::remove_file(path) {
		Err(e) if e.kind() != io::ErrorKind::NotFound => {
			Err(Error::io(format!("removing \"{}\"", path.display()), e))
		}
		_ => Ok(()),
	}
}

fn sha256_hex(bytes: &[u8]) -> String {
	Sha256::digest(bytes)
		.iter()
		.map(|byte| format!("{byte:02x}"))
		.collect()
}

fn read_link(path: &Path) -> Option<PathBuf> {
	fs::read_link(path).ok()
}

/// The profile's current generation number and link target.
fn profile_state(profile: &Path) -> (Option<u64>, Option<PathBuf>) {
	let generation = profiles::find_generations(profile)
		.ok()
		.and_then(|(_, current)| current);
	(generation, read_link(profile))
}

/// The revision and dirtiness of the checkout holding a local source, or
/// nulls. Neither git nor jj is allowed to modify the repository.
pub fn source_revision(source: &str) -> (Option<String>, Option<bool>) {
	let local = source
		.strip_prefix("git+file://")
		.or_else(|| source.strip_prefix("path:"))
		.or_else(|| source.strip_prefix("file://"))
		.unwrap_or(source);
	let local = Path::new(local.split(['?', '#']).next().unwrap_or(local));
	let Ok(metadata) = fs::metadata(local) else {
		return (None, None);
	};
	let start = if metadata.is_dir() {
		local
	} else {
		local.parent().unwrap_or(local)
	};
	for dir in start.ancestors() {
		if dir.join(".git").exists() {
			let revision = run_tool(
				Command::new("git")
					.arg("-C")
					.arg(dir)
					.args(["rev-parse", "HEAD"]),
			);
			let dirty = run_tool(Command::new("git").arg("-C").arg(dir).args([
				"--no-optional-locks",
				"status",
				"--porcelain",
			]))
			.map(|status| !status.is_empty());
			return (revision.filter(|rev| !rev.is_empty()), dirty);
		}
		if dir.join(".jj").is_dir() {
			// Without a working-copy snapshot, jj cannot tell whether the
			// files changed, so dirtiness stays unknown.
			let revision = run_tool(Command::new("jj").arg("-R").arg(dir).args([
				"--ignore-working-copy",
				"log",
				"--no-graph",
				"-r",
				"@",
				"-T",
				"change_id",
			]));
			return (revision.filter(|rev| !rev.is_empty()), None);
		}
	}
	(None, None)
}

fn run_tool(command: &mut Command) -> Option<String> {
	let output = command
		.env("GIT_OPTIONAL_LOCKS", "0")
		.stdin(Stdio::null())
		.stderr(Stdio::null())
		.output()
		.ok()?;
	output
		.status
		.success()
		.then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn user_name(uid: u32) -> Option<String> {
	let mut buf = vec![0 as libc::c_char; 4096];
	// SAFETY: an all-zero `passwd` is a valid out parameter.
	let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
	let mut result = std::ptr::null_mut();
	// SAFETY: every pointer refers to live, correctly sized storage.
	let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
	if rc != 0 || result.is_null() || pwd.pw_name.is_null() {
		return None;
	}
	// SAFETY: `getpwuid_r` succeeded, so `pw_name` is a C string in `buf`.
	let name = unsafe { CStr::from_ptr(pwd.pw_name) };
	Some(name.to_string_lossy().into_owned())
}

pub(crate) fn host_name() -> Option<String> {
	let mut buf = [0u8; 256];
	// SAFETY: the buffer is valid for `buf.len()` bytes.
	let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
	if rc != 0 {
		return None;
	}
	let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
	let name = String::from_utf8_lossy(&buf[..end]).into_owned();
	(!name.is_empty()).then_some(name)
}

/// `time` as an RFC 3339 UTC timestamp with second precision.
fn rfc3339(time: SystemTime) -> String {
	let secs = time
		.duration_since(UNIX_EPOCH)
		.map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
	let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
	// Howard Hinnant's days-to-civil conversion.
	let z = days + 719_468;
	let era = z.div_euclid(146_097);
	let doe = z.rem_euclid(146_097);
	let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let day = doy - (153 * mp + 2) / 5 + 1;
	let month = if mp < 10 { mp + 3 } else { mp - 9 };
	let year = yoe + era * 400 + i64::from(month <= 2);
	format!(
		"{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
		rem / 3600,
		rem % 3600 / 60,
		rem % 60
	)
}

// Command line.

/// Run `noxi snapshot` with the arguments after the subcommand name.
pub fn main(args: Vec<String>) -> ExitCode {
	run(args, &mut io::stdout().lock(), &mut io::stderr().lock())
}

/// [`main`] writing to `out` and `err`, with the journal at
/// [`SnapshotStore::default_location`].
pub fn run(args: Vec<String>, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
	match parse(args) {
		Ok(Invocation::Help) => {
			let _ = writeln!(out, "{USAGE}");
			ExitCode::SUCCESS
		}
		Ok(command) => match execute(command, out, err) {
			Ok(code) => code,
			Err(message) => {
				let _ = writeln!(err, "{}", nix_compat::log::format_error(&message));
				ExitCode::FAILURE
			}
		},
		Err(message) => {
			let _ = writeln!(err, "{message}\n{USAGE}");
			ExitCode::from(2)
		}
	}
}

enum Invocation {
	Help,
	List {
		json: bool,
	},
	Show {
		id: String,
		json: bool,
	},
	Diff {
		a: String,
		b: String,
		json: bool,
	},
	Rollback {
		id: String,
		options: RollbackOptions,
	},
}

fn parse(args: Vec<String>) -> Result<Invocation, String> {
	let mut args = args.into_iter();
	let Some(subcommand) = args.next() else {
		return Err("noxi snapshot: missing subcommand".to_owned());
	};
	let mut json = false;
	let mut positional = Vec::new();
	let mut options = RollbackOptions::default();
	let rollback = subcommand == "rollback";
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--json" if !rollback => json = true,
			"--no-activate" if rollback => options.activate = false,
			"--restore-lock" if rollback => options.restore_lock = true,
			"--force" if rollback => options.force = true,
			"--dry-run" if rollback => options.dry_run = true,
			"--profile" if rollback => {
				let path = args
					.next()
					.ok_or_else(|| "flag '--profile' requires an argument".to_owned())?;
				options.profile = PathBuf::from(path);
			}
			flag if flag.starts_with("--") => return Err(format!("unknown flag '{flag}'")),
			_ => positional.push(arg),
		}
	}
	let expect = |count: usize, what: &str| {
		if positional.len() == count {
			Ok(())
		} else {
			Err(format!("noxi snapshot {subcommand}: expected {what}"))
		}
	};
	match subcommand.as_str() {
		"help" | "--help" | "-h" => Ok(Invocation::Help),
		"list" => {
			expect(0, "no arguments")?;
			Ok(Invocation::List { json })
		}
		"show" => {
			expect(1, "one snapshot id")?;
			Ok(Invocation::Show {
				id: positional.remove(0),
				json,
			})
		}
		"diff" => {
			expect(2, "two snapshot ids")?;
			let b = positional.pop().unwrap_or_default();
			let a = positional.pop().unwrap_or_default();
			Ok(Invocation::Diff { a, b, json })
		}
		"rollback" => {
			expect(1, "one snapshot id")?;
			Ok(Invocation::Rollback {
				id: positional.remove(0),
				options,
			})
		}
		other => Err(format!("noxi snapshot: unknown subcommand '{other}'")),
	}
}

fn render_error(action: &str, error: &Error) -> String {
	let mut message = format!("{action} stopped in phase {}: {error}", error.phase());
	if let Some(hint) = error.hint() {
		message.push_str(&format!("\nnext: {hint}"));
	}
	message
}

fn to_json<T: Serialize>(value: &T) -> Result<String, String> {
	serde_json::to_string_pretty(value).map_err(|e| e.to_string())
}

fn execute(
	command: Invocation,
	out: &mut dyn Write,
	err: &mut dyn Write,
) -> Result<ExitCode, String> {
	let store =
		SnapshotStore::default_location().map_err(|e| render_error("opening the journal", &e))?;
	let write = |e: io::Error| format!("writing output: {e}");
	match command {
		Invocation::Help => unreachable!("handled by run"),
		Invocation::List { json } => {
			let snapshots = store
				.list()
				.map_err(|e| render_error("listing snapshots", &e))?;
			if json {
				writeln!(out, "{}", to_json(&snapshots)?).map_err(write)?;
			} else {
				render_list(out, &snapshots).map_err(write)?;
			}
		}
		Invocation::Show { id, json } => {
			let snapshot = store
				.get(&id)
				.map_err(|e| render_error("showing the snapshot", &e))?;
			if json {
				writeln!(out, "{}", to_json(&snapshot)?).map_err(write)?;
			} else {
				render_show(out, &store, &snapshot).map_err(write)?;
			}
		}
		Invocation::Diff { a, b, json } => {
			let diff = store
				.diff_locks(&a, &b)
				.map_err(|e| render_error("comparing locks", &e))?;
			if json {
				writeln!(out, "{}", to_json(&diff)?).map_err(write)?;
			} else {
				render_diff(out, &diff).map_err(write)?;
			}
		}
		Invocation::Rollback { id, options } => {
			let report = store
				.rollback(&id, options)
				.map_err(|e| render_error("rollback", &e))?;
			return render_rollback(out, err, &report).map_err(write);
		}
	}
	Ok(ExitCode::SUCCESS)
}

fn or_dash<T: fmt::Display>(value: Option<T>) -> String {
	value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

fn short_revision(source: &Source) -> String {
	let Some(revision) = &source.revision else {
		return "-".to_owned();
	};
	let mut short: String = revision.chars().take(12).collect();
	if source.dirty == Some(true) {
		short.push_str("-dirty");
	}
	short
}

fn render_list(out: &mut dyn Write, snapshots: &[Snapshot]) -> io::Result<()> {
	if snapshots.is_empty() {
		return writeln!(out, "no snapshots recorded");
	}
	let rows: Vec<[String; 7]> = snapshots
		.iter()
		.map(|s| {
			[
				s.id.clone(),
				s.created_at.clone(),
				s.action.to_string(),
				or_dash(s.generation.after),
				s.outcome.status.to_string(),
				short_revision(&s.source),
				or_dash(
					s.closure
						.as_ref()
						.and_then(|closure| closure.file_name())
						.map(|name| name.to_string_lossy()),
				),
			]
		})
		.collect();
	let header = [
		"ID",
		"CREATED",
		"ACTION",
		"GENERATION",
		"STATUS",
		"REVISION",
		"CLOSURE",
	];
	let mut widths = header.map(str::len);
	for row in &rows {
		for (width, cell) in widths.iter_mut().zip(row) {
			*width = (*width).max(cell.len());
		}
	}
	let line = |cells: &[&str]| {
		let mut line = String::new();
		for (i, cell) in cells.iter().enumerate() {
			if i + 1 == cells.len() {
				line.push_str(cell);
			} else {
				line.push_str(&format!("{cell:<width$}  ", width = widths[i]));
			}
		}
		line
	};
	writeln!(out, "{}", line(&header))?;
	for row in &rows {
		let cells: Vec<&str> = row.iter().map(String::as_str).collect();
		writeln!(out, "{}", line(&cells))?;
	}
	Ok(())
}

fn path_or_dash(path: &Option<PathBuf>) -> String {
	or_dash(path.as_ref().map(|path| path.display()))
}

fn render_show(out: &mut dyn Write, store: &SnapshotStore, s: &Snapshot) -> io::Result<()> {
	writeln!(out, "snapshot {}", s.id)?;
	writeln!(out, "  schema version: {}", s.schema_version)?;
	writeln!(out, "  action:         {}", s.action)?;
	writeln!(out, "  created:        {}", s.created_at)?;
	writeln!(out, "  finished:       {}", or_dash(s.finished_at.as_ref()))?;
	let actor = match (&s.actor.user, s.actor.uid) {
		(Some(user), Some(uid)) => format!("{user} (uid {uid})"),
		(Some(user), None) => user.clone(),
		(None, Some(uid)) => format!("uid {uid}"),
		(None, None) => "-".to_owned(),
	};
	writeln!(
		out,
		"  actor:          {actor} on {}",
		or_dash(s.actor.hostname.as_ref())
	)?;
	let attribute = s
		.source
		.attribute
		.as_ref()
		.map_or_else(String::new, |attribute| match s.source.kind {
			SourceKind::Flake => format!("#{attribute}"),
			SourceKind::File => format!(" --attr {attribute}"),
		});
	writeln!(
		out,
		"  source:         {} {}{attribute}",
		s.source.kind.as_str(),
		s.source.path
	)?;
	let dirty = match s.source.dirty {
		Some(true) => " (uncommitted changes)",
		Some(false) => " (clean)",
		None => "",
	};
	writeln!(
		out,
		"  revision:       {}{dirty}",
		or_dash(s.source.revision.as_ref())
	)?;
	match &s.lock {
		Some(lock) => writeln!(
			out,
			"  lock:           {} (sha256 {})",
			lock.path.display(),
			lock.sha256
		)?,
		None => writeln!(out, "  lock:           -")?,
	}
	writeln!(out, "  closure:        {}", path_or_dash(&s.closure))?;
	writeln!(out, "  profile:        {}", s.profile.display())?;
	writeln!(
		out,
		"  generation:     {} -> {}",
		or_dash(s.generation.before),
		or_dash(s.generation.after)
	)?;
	writeln!(
		out,
		"  running:        {} -> {}",
		path_or_dash(&s.running_before),
		path_or_dash(&s.running_after)
	)?;
	writeln!(
		out,
		"  boot:           {} -> {}",
		path_or_dash(&s.boot_before),
		path_or_dash(&s.boot_after)
	)?;
	writeln!(out, "  outcome:        {}", s.outcome.status)?;
	writeln!(out, "  phase:          {}", or_dash(s.outcome.phase))?;
	writeln!(
		out,
		"  message:        {}",
		or_dash(s.outcome.message.as_ref())
	)?;

	let previous = store.list().ok().and_then(|snapshots| {
		snapshots
			.into_iter()
			.take_while(|other| other.id != s.id)
			.last()
	});
	if let Some(previous) = previous
		&& let (Ok(before), Ok(after)) = (store.lock_bytes(&previous), store.lock_bytes(s))
		&& let Ok(diff) = LockDiff::between(&before, &after)
	{
		writeln!(out, "lock changes since snapshot {}:", previous.id)?;
		render_diff(out, &diff)?;
	}
	Ok(())
}

fn describe_input(input: &LockedInput) -> String {
	let mut parts = Vec::new();
	if let Some(rev) = &input.rev {
		parts.push(format!("rev {rev}"));
	}
	if let Some(hash) = &input.nar_hash {
		parts.push(format!("narHash {hash}"));
	}
	if let Some(time) = input.last_modified {
		parts.push(format!("lastModified {time}"));
	}
	parts.join(", ")
}

fn render_diff(out: &mut dyn Write, diff: &LockDiff) -> io::Result<()> {
	if diff.is_empty() {
		return writeln!(out, "  no input changes");
	}
	for input in &diff.added {
		writeln!(out, "  added   {}: {}", input.name, describe_input(input))?;
	}
	for input in &diff.removed {
		writeln!(out, "  removed {}: {}", input.name, describe_input(input))?;
	}
	for change in &diff.changed {
		writeln!(out, "  changed {}:", change.name)?;
		for field in &change.fields {
			let (before, after) = match field.as_str() {
				"rev" => (change.before.rev.clone(), change.after.rev.clone()),
				"narHash" => (
					change.before.nar_hash.clone(),
					change.after.nar_hash.clone(),
				),
				_ => (
					change.before.last_modified.map(|time| time.to_string()),
					change.after.last_modified.map(|time| time.to_string()),
				),
			};
			writeln!(
				out,
				"    {field}: {} -> {}",
				or_dash(before),
				or_dash(after)
			)?;
		}
	}
	Ok(())
}

fn render_rollback(
	out: &mut dyn Write,
	err: &mut dyn Write,
	report: &RollbackReport,
) -> io::Result<ExitCode> {
	let profile = report.target.profile.display();
	if report.dry_run {
		writeln!(
			out,
			"dry run: would switch profile {profile} from generation {} to {} (snapshot {})",
			or_dash(report.generation_before),
			report.generation,
			report.target.id
		)?;
		if report.activation_requested {
			writeln!(
				out,
				"dry run: would run {} switch",
				report.activation_program.display()
			)?;
		} else {
			writeln!(out, "dry run: would not run switch-to-configuration")?;
		}
		if let Some(path) = &report.lock_to_restore {
			writeln!(
				out,
				"dry run: would restore lock {} from snapshot {}",
				path.display(),
				report.target.id
			)?;
		}
		writeln!(
			out,
			"nothing was changed and no snapshot was recorded; running {}, boot {}",
			path_or_dash(&report.running_after),
			path_or_dash(&report.boot_after)
		)?;
		return Ok(ExitCode::SUCCESS);
	}
	let state = format!(
		"generation: {} (was {})\nrunning:    {}\nboot:       {}",
		or_dash(report.generation_after),
		or_dash(report.generation_before),
		path_or_dash(&report.running_after),
		path_or_dash(&report.boot_after)
	);
	let record = report.record.as_ref();
	let record_id = or_dash(record.map(|record| record.id.as_str()));
	if report.succeeded() {
		writeln!(
			out,
			"rolled back profile {profile} to generation {} from snapshot {}",
			report.generation, report.target.id
		)?;
		if let Some(message) = record.and_then(|record| record.outcome.message.as_ref()) {
			writeln!(out, "{message}")?;
		}
		if let Some(path) = &report.lock_restored {
			writeln!(out, "restored lock {}", path.display())?;
		}
		writeln!(out, "{state}")?;
		writeln!(out, "recorded as snapshot {record_id}")?;
		return Ok(ExitCode::SUCCESS);
	}
	let (phase, message) = record.map_or(("-".to_owned(), String::new()), |record| {
		(
			or_dash(record.outcome.phase),
			record.outcome.message.clone().unwrap_or_default(),
		)
	});
	let next = if report.activated {
		"inspect the activation output above, then retry or roll back to another snapshot"
	} else {
		"fix the cause above and rerun the rollback"
	};
	writeln!(
		err,
		"{}",
		nix_compat::log::format_error(&format!(
			"rollback failed in phase {phase}: {message}\n{state}\nrecorded as snapshot {record_id}\nnext: {next}"
		))
	)?;
	Ok(ExitCode::FAILURE)
}
