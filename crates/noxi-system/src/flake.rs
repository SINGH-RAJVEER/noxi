//! `noxi flake update` and `noxi flake lock`: native flake input updates.
//!
//! The lock algorithm is the one behind the compatibility `nix flake update`
//! and `nix flake lock` commands: this module calls that implementation
//! in-process, then compares the lock file before and after and reports
//! which inputs were added, removed or changed.

use std::{
	collections::{BTreeMap, HashSet, VecDeque},
	fmt,
	io::Write,
	path::{Path, PathBuf},
	process::ExitCode,
	sync::Mutex,
};

use nix_compat::nixcpp::settings::Settings;
use serde_json::{Value, json};

pub const USAGE: &str = "usage: noxi flake update [INPUT...] [--flake PATH] [--commit-lock-file] [--override-input NAME REF]... [--json]\n       noxi flake lock [--flake PATH] [--override-input NAME REF]... [--json]";

/// Experimental features the native command enables for itself.
const FEATURES: &str = "nix-command flakes";

/// Stack size for the evaluator thread, matching the compatibility commands.
const STACK_SIZE: usize = 64 * 1024 * 1024;

/// The shared implementation uses process-wide logging and fetcher state, so
/// lock operations in one process run one at a time.
pub(crate) static LOCK: Mutex<()> = Mutex::new(());

/// Which lock operation to perform.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Operation {
	/// Re-resolve the named inputs, or all inputs when none are named.
	#[default]
	Update,
	/// Create or complete the lock without changing existing entries.
	Lock,
}

impl Operation {
	fn name(self) -> &'static str {
		match self {
			Operation::Update => "update",
			Operation::Lock => "lock",
		}
	}
}

/// Options for [`update`].
#[derive(Clone, Debug, Default)]
pub struct UpdateOptions {
	pub operation: Operation,
	/// The local flake directory; the current directory when `None`.
	pub flake: Option<PathBuf>,
	/// Input paths to update, such as `nixpkgs` or `home-manager/nixpkgs`.
	/// Empty updates every input. Only valid for [`Operation::Update`].
	pub inputs: Vec<String>,
	/// Commit the lock file in Git, as `nix flake update --commit-lock-file`.
	pub commit_lock_file: bool,
	/// `--override-input NAME REF` pairs.
	pub override_inputs: Vec<(String, String)>,
	/// Settings to use; loaded from the environment and configuration files
	/// when `None`. `nix-command` and `flakes` are always enabled.
	pub settings: Option<Settings>,
}

/// The result of a successful [`update`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateReport {
	/// The canonical flake directory.
	pub flake: PathBuf,
	/// The lock file path, whether or not it exists.
	pub lock_path: PathBuf,
	/// The lock file existed before the operation.
	pub existed: bool,
	/// The lock file bytes differ from before (including creation).
	pub rewritten: bool,
	pub diff: LockDiff,
}

/// The phase an [`UpdateError`] occurred in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
	/// Finding the flake and reading its existing lock file.
	Resolve,
	/// Running the shared lock algorithm, including fetching inputs and
	/// writing or committing the lock file.
	Lock,
	/// Reading back the lock file the lock phase wrote.
	Write,
}

impl fmt::Display for Phase {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Phase::Resolve => "resolve",
			Phase::Lock => "lock",
			Phase::Write => "write",
		})
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateError {
	pub phase: Phase,
	pub message: String,
}

impl fmt::Display for UpdateError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}: {}", self.phase, self.message)
	}
}

impl std::error::Error for UpdateError {}

fn error(phase: Phase, message: impl Into<String>) -> UpdateError {
	UpdateError {
		phase,
		message: message.into(),
	}
}

/// The identifying attributes of one locked input node.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LockedInput {
	/// The locked reference rendered as a flake URL, or the `locked` JSON when
	/// it cannot be rendered.
	pub url: String,
	pub rev: Option<String>,
	pub nar_hash: Option<String>,
	/// Unix time of the source's last modification.
	pub last_modified: Option<u64>,
	/// The `original` reference rendered as a flake URL.
	pub original: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
	Added,
	Removed,
	Changed,
}

impl ChangeKind {
	pub fn as_str(self) -> &'static str {
		match self {
			ChangeKind::Added => "added",
			ChangeKind::Removed => "removed",
			ChangeKind::Changed => "changed",
		}
	}
}

/// One input whose lock entry differs between two lock files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputChange {
	/// The input path from the root, such as `nixpkgs` or `utils/systems`.
	pub input: String,
	pub kind: ChangeKind,
	/// The old entry; `None` when added.
	pub old: Option<LockedInput>,
	/// The new entry; `None` when removed.
	pub new: Option<LockedInput>,
}

/// The input-level difference between two version-7 lock files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LockDiff {
	/// Added and changed inputs in breadth-first order of the new lock,
	/// followed by removed inputs in breadth-first order of the old lock.
	pub changes: Vec<InputChange>,
	/// Input paths present in both locks with identical entries.
	pub unchanged: Vec<String>,
}

impl LockDiff {
	pub fn is_empty(&self) -> bool {
		self.changes.is_empty()
	}
}

/// Every input node reachable from the root, keyed by its first input path in
/// breadth-first order. `follows` edges are resolved to their target node; a
/// node reachable by several paths appears once.
fn input_nodes(lock: &Value) -> Vec<(String, String)> {
	let root = lock["root"].as_str().unwrap_or("root").to_owned();
	let mut seen = HashSet::from([root.clone()]);
	let mut found = Vec::new();
	let mut queue = VecDeque::from([(Vec::<String>::new(), root)]);
	while let Some((path, key)) = queue.pop_front() {
		let Some(inputs) = lock["nodes"][&key]["inputs"].as_object() else {
			continue;
		};
		for (name, edge) in inputs {
			let Some(target) = resolve_edge(lock, edge, 0) else {
				continue;
			};
			if !seen.insert(target.clone()) {
				continue;
			}
			let mut child = path.clone();
			child.push(name.clone());
			found.push((child.join("/"), target.clone()));
			queue.push_back((child, target));
		}
	}
	found
}

/// The node an input edge points at: a node name, or a `follows` path from the
/// root.
fn resolve_edge(lock: &Value, edge: &Value, depth: usize) -> Option<String> {
	match edge {
		Value::String(node) => Some(node.clone()),
		Value::Array(path) if depth < 64 => {
			let mut key = lock["root"].as_str().unwrap_or("root").to_owned();
			for segment in path {
				let next = &lock["nodes"][&key]["inputs"][segment.as_str()?];
				key = resolve_edge(lock, next, depth + 1)?;
			}
			Some(key)
		}
		_ => None,
	}
}

fn render_ref(attrs: &Value) -> String {
	let rendered = noxi_flakeref::input::json_to_attrs(attrs)
		.ok()
		.and_then(|attrs| {
			noxi_flakeref::flake_ref::FlakeRef::from_attrs(
				&noxi_flakeref::input::FetchSettings {
					flakes: true,
					..Default::default()
				},
				&attrs,
			)
			.ok()
		});
	match rendered {
		Some(reference) => reference.to_string(),
		None => attrs.to_string(),
	}
}

fn summarize(node: &Value) -> LockedInput {
	let locked = &node["locked"];
	LockedInput {
		url: render_ref(locked),
		rev: locked["rev"].as_str().map(str::to_owned),
		nar_hash: locked["narHash"].as_str().map(str::to_owned),
		last_modified: locked["lastModified"].as_u64(),
		original: render_ref(&node["original"]),
	}
}

/// Compare two version-7 lock files by input path. Inputs are identified by
/// their path from the root rather than by node name, because node names such
/// as `nixpkgs_2` may be renumbered between locks. An entry is changed when
/// its `locked` or `original` attributes, or its `flake` flag, differ.
pub fn diff_locks(old: &Value, new: &Value) -> LockDiff {
	let old_nodes: BTreeMap<String, String> = input_nodes(old).into_iter().collect();
	let mut diff = LockDiff::default();
	let mut present = HashSet::new();
	for (path, key) in input_nodes(new) {
		let new_node = &new["nodes"][&key];
		present.insert(path.clone());
		match old_nodes.get(&path) {
			None => diff.changes.push(InputChange {
				input: path,
				kind: ChangeKind::Added,
				old: None,
				new: Some(summarize(new_node)),
			}),
			Some(old_key) => {
				let old_node = &old["nodes"][old_key];
				let same = ["locked", "original", "flake"]
					.iter()
					.all(|field| old_node[field] == new_node[field]);
				if same {
					diff.unchanged.push(path);
				} else {
					diff.changes.push(InputChange {
						input: path,
						kind: ChangeKind::Changed,
						old: Some(summarize(old_node)),
						new: Some(summarize(new_node)),
					});
				}
			}
		}
	}
	for (path, key) in input_nodes(old) {
		if !present.contains(&path) {
			diff.changes.push(InputChange {
				input: path,
				kind: ChangeKind::Removed,
				old: Some(summarize(&old["nodes"][&key])),
				new: None,
			});
		}
	}
	diff
}

fn read_lock(path: &Path, phase: Phase) -> Result<Option<(Vec<u8>, Value)>, UpdateError> {
	match std::fs::read(path) {
		Ok(bytes) => {
			let value = serde_json::from_slice(&bytes).map_err(|e| {
				error(
					phase,
					format!("cannot parse lock file '{}': {e}", path.display()),
				)
			})?;
			Ok(Some((bytes, value)))
		}
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
		Err(e) => Err(error(
			phase,
			format!("cannot read lock file '{}': {e}", path.display()),
		)),
	}
}

/// Lock or update the inputs of a local flake through the same implementation
/// as `nix flake lock` and `nix flake update`, then report the difference.
///
/// The lock file is written atomically, and only when it changes. The shared
/// implementation's own diagnostics, including its lock-change warning, go to
/// the process's standard error.
pub fn update(options: UpdateOptions) -> Result<UpdateReport, UpdateError> {
	if options.operation == Operation::Lock && !options.inputs.is_empty() {
		return Err(error(
			Phase::Resolve,
			"'noxi flake lock' does not take input names; use 'noxi flake update'",
		));
	}
	let requested = options.flake.clone().unwrap_or_else(|| PathBuf::from("."));
	let flake = std::fs::canonicalize(&requested).map_err(|e| {
		error(
			Phase::Resolve,
			format!("cannot open flake directory '{}': {e}", requested.display()),
		)
	})?;
	if !flake.is_dir() {
		return Err(error(
			Phase::Resolve,
			format!("flake path '{}' is not a directory", flake.display()),
		));
	}
	if !flake.join("flake.nix").is_file() {
		return Err(error(
			Phase::Resolve,
			format!(
				"directory '{}' does not contain a flake.nix",
				flake.display()
			),
		));
	}
	let lock_path = flake.join("flake.lock");
	let flake_arg = flake
		.to_str()
		.ok_or_else(|| {
			error(
				Phase::Resolve,
				format!("flake path '{}' is not valid UTF-8", flake.display()),
			)
		})?
		.to_owned();

	let mut settings = match options.settings {
		Some(settings) => settings,
		// Enabling the features ahead of `NIX_CONFIG` keeps flake settings
		// given there, such as `flake-registry`, from being ignored.
		None => Settings::load(|name| {
			let value = std::env::var(name).ok();
			if name == "NIX_CONFIG" {
				Some(format!(
					"extra-experimental-features = {FEATURES}\n{}",
					value.unwrap_or_default()
				))
			} else {
				value
			}
		})
		.map_err(|e| error(Phase::Resolve, format!("cannot load settings: {e}")))?,
	};
	settings
		.set("extra-experimental-features", FEATURES)
		.map_err(|e| error(Phase::Resolve, e.to_string()))?;

	let mut args = vec!["flake".to_owned(), options.operation.name().to_owned()];
	match options.operation {
		Operation::Update => {
			args.extend(["--flake".to_owned(), flake_arg.clone()]);
			args.extend(options.inputs.iter().cloned());
		}
		Operation::Lock => args.push(flake_arg.clone()),
	}
	if options.commit_lock_file {
		args.push("--commit-lock-file".to_owned());
	}
	for (name, reference) in &options.override_inputs {
		args.extend([
			"--override-input".to_owned(),
			name.clone(),
			reference.clone(),
		]);
	}

	let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
	let before = read_lock(&lock_path, Phase::Resolve)?;
	let result = std::thread::Builder::new()
		.stack_size(STACK_SIZE)
		.spawn(move || noxi_commands::nix::main(settings, args))
		.map_err(|e| error(Phase::Lock, format!("cannot start the lock thread: {e}")))?
		.join()
		.map_err(|_| error(Phase::Lock, "the lock implementation panicked"))?;
	match result {
		Ok(code) if code == ExitCode::SUCCESS => {}
		Ok(_) => {
			return Err(error(
				Phase::Lock,
				format!("locking flake '{flake_arg}' failed; the cause is reported above"),
			));
		}
		Err(
			noxi_commands::cli::Failure::Usage(message)
			| noxi_commands::cli::Failure::Error(message)
			| noxi_commands::cli::Failure::Exit(_, message),
		) => return Err(error(Phase::Lock, message)),
	}
	let after = read_lock(&lock_path, Phase::Write)?;

	let empty = json!({ "nodes": {}, "root": "root", "version": 7 });
	let diff = diff_locks(
		before.as_ref().map_or(&empty, |(_, value)| value),
		after.as_ref().map_or(&empty, |(_, value)| value),
	);
	let rewritten =
		before.as_ref().map(|(bytes, _)| bytes) != after.as_ref().map(|(bytes, _)| bytes);
	Ok(UpdateReport {
		flake,
		lock_path,
		existed: before.is_some(),
		rewritten,
		diff,
	})
}

fn locked_json(input: &Option<LockedInput>) -> Value {
	match input {
		None => Value::Null,
		Some(input) => {
			let mut value = json!({ "url": input.url, "original": input.original });
			if let Some(rev) = &input.rev {
				value["rev"] = json!(rev);
			}
			if let Some(hash) = &input.nar_hash {
				value["narHash"] = json!(hash);
			}
			if let Some(time) = input.last_modified {
				value["lastModified"] = json!(time);
			}
			value
		}
	}
}

/// The `--json` report:
/// `{ "flake": PATH, "lock": LOCKPATH, "changes": [...], "unchanged": N }`.
pub fn report_json(report: &UpdateReport) -> Value {
	let changes: Vec<Value> = report
		.diff
		.changes
		.iter()
		.map(|change| {
			json!({
				"input": change.input,
				"kind": change.kind.as_str(),
				"old": locked_json(&change.old),
				"new": locked_json(&change.new),
			})
		})
		.collect();
	json!({
		"flake": report.flake.to_string_lossy(),
		"lock": report.lock_path.to_string_lossy(),
		"changes": changes,
		"unchanged": report.diff.unchanged.len(),
	})
}

fn short_rev(rev: &str) -> &str {
	rev.get(..7).unwrap_or(rev)
}

fn date(time: Option<u64>) -> String {
	time.map(noxi_flakeref::lockfile::format_date)
		.unwrap_or_else(|| "unknown".to_owned())
}

/// The identity of one entry: its short revision, or its NAR hash when the
/// revision is absent, plus its last-modified date.
fn describe(input: &LockedInput) -> String {
	let id = match (&input.rev, &input.nar_hash) {
		(Some(rev), _) => format!("rev {}", short_rev(rev)),
		(None, Some(hash)) => format!("narHash {hash}"),
		(None, None) => format!("url {}", input.url),
	};
	match input.last_modified {
		Some(time) => format!("{id}, last modified {}", date(Some(time))),
		None => id,
	}
}

fn describe_change(old: &LockedInput, new: &LockedInput) -> String {
	let mut parts = Vec::new();
	match (&old.rev, &new.rev) {
		(Some(a), Some(b)) if a != b => {
			parts.push(format!("rev {} -> {}", short_rev(a), short_rev(b)))
		}
		(Some(a), Some(_)) => parts.push(format!("rev {}", short_rev(a))),
		_ if old.nar_hash != new.nar_hash => parts.push(format!(
			"narHash {} -> {}",
			old.nar_hash.as_deref().unwrap_or("none"),
			new.nar_hash.as_deref().unwrap_or("none")
		)),
		_ => {}
	}
	if old.last_modified != new.last_modified {
		parts.push(format!(
			"last modified {} -> {}",
			date(old.last_modified),
			date(new.last_modified)
		));
	} else if old.last_modified.is_some() {
		parts.push(format!("last modified {}", date(old.last_modified)));
	}
	if old.original != new.original {
		parts.push(format!("source {} -> {}", old.original, new.original));
	}
	if old.rev.is_none() && new.rev.is_none() && old.nar_hash == new.nar_hash && old.url != new.url
	{
		parts.push(format!("url {} -> {}", old.url, new.url));
	}
	parts.join(", ")
}

/// Write the human-readable report.
pub fn write_report(report: &UpdateReport, out: &mut dyn Write) -> std::io::Result<()> {
	if !report.rewritten {
		writeln!(out, "no changes")?;
		return Ok(());
	}
	let state = if report.existed { "updated" } else { "created" };
	writeln!(out, "{state} {}", report.lock_path.display())?;
	let width = report
		.diff
		.changes
		.iter()
		.map(|change| change.input.len())
		.max()
		.unwrap_or(0);
	for change in &report.diff.changes {
		let detail = match (&change.old, &change.new) {
			(None, Some(new)) => describe(new),
			(Some(old), None) => describe(old),
			(Some(old), Some(new)) => describe_change(old, new),
			(None, None) => String::new(),
		};
		writeln!(
			out,
			"{:<8} {:<width$}  {detail}",
			change.kind.as_str(),
			change.input
		)?;
	}
	if report.diff.changes.is_empty() {
		writeln!(out, "no input entries changed")?;
	}
	let unchanged = &report.diff.unchanged;
	if !unchanged.is_empty() {
		writeln!(
			out,
			"unchanged: {} input{} ({})",
			unchanged.len(),
			if unchanged.len() == 1 { "" } else { "s" },
			unchanged.join(", ")
		)?;
	}
	Ok(())
}

fn usage_error(err: &mut dyn Write, message: &str) -> ExitCode {
	let _ = writeln!(err, "error: {message}\n{USAGE}");
	ExitCode::from(2)
}

/// Run `noxi flake ARGS`, writing the report to `out` and diagnostics to `err`.
pub fn run(args: Vec<String>, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
	let mut args = args.into_iter();
	let operation = match args.next().as_deref() {
		Some("update") => Operation::Update,
		Some("lock") => Operation::Lock,
		Some("--help" | "-h") => {
			let _ = writeln!(out, "{USAGE}");
			return ExitCode::SUCCESS;
		}
		Some(other) => return usage_error(err, &format!("unknown flake subcommand '{other}'")),
		None => return usage_error(err, "missing flake subcommand"),
	};
	let mut options = UpdateOptions {
		operation,
		..Default::default()
	};
	let mut json = false;
	let mut operands_only = false;
	while let Some(arg) = args.next() {
		if operands_only || !arg.starts_with('-') || arg == "-" {
			options.inputs.push(arg);
			continue;
		}
		match arg.as_str() {
			"--" => operands_only = true,
			"--help" | "-h" => {
				let _ = writeln!(out, "{USAGE}");
				return ExitCode::SUCCESS;
			}
			"--json" => json = true,
			"--commit-lock-file" if operation == Operation::Update => {
				options.commit_lock_file = true
			}
			"--flake" => match args.next() {
				Some(path) => options.flake = Some(PathBuf::from(path)),
				None => return usage_error(err, "'--flake' requires a path"),
			},
			"--override-input" => match (args.next(), args.next()) {
				(Some(name), Some(reference)) => options.override_inputs.push((name, reference)),
				_ => return usage_error(err, "'--override-input' requires a name and a reference"),
			},
			_ => return usage_error(err, &format!("unknown flag '{arg}'")),
		}
	}
	if operation == Operation::Lock && !options.inputs.is_empty() {
		return usage_error(err, "'noxi flake lock' does not take input names");
	}
	match update(options) {
		Ok(report) => {
			let written = if json {
				writeln!(out, "{}", report_json(&report))
			} else {
				write_report(&report, out)
			};
			match written {
				Ok(()) => ExitCode::SUCCESS,
				Err(e) => {
					let _ = writeln!(err, "error: write: cannot print the report: {e}");
					ExitCode::FAILURE
				}
			}
		}
		Err(e) => {
			let _ = writeln!(err, "error: {e}");
			ExitCode::FAILURE
		}
	}
}

pub fn main(args: Vec<String>) -> ExitCode {
	run(args, &mut std::io::stdout(), &mut std::io::stderr())
}
