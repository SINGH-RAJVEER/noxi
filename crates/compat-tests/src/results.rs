//! Machine-readable differential results, schema `noxi-compat-result/1`.
//!
//! Every differential runner writes one JSON object per case, one per line,
//! to `$NOXI_RESULTS_DIR/RUNNER.jsonl` (or the runner's default directory).
//! A record names the case, the compatibility matrix row and inventory
//! entries it is evidence for, the tier, both engines' command lines, the
//! settings both ran under, the revisions, executables and platform, what
//! each engine produced, and a verdict. Large outputs are written once to
//! `blobs/SHA256` beside the record file and referenced by path, so every
//! compared byte stays inspectable. `docs/compatibility-results.md`
//! documents the fields; `scripts/compat-results.py` aggregates them.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// The schema identifier every record carries. A change that removes or
/// renames a field, or changes a field's meaning, bumps the number.
pub const SCHEMA: &str = "noxi-compat-result/1";

/// Overrides every runner's default record directory.
pub const RESULTS_DIR_VAR: &str = "NOXI_RESULTS_DIR";
/// The tier a run belongs to (`fast`, `store`, `daemon`, `system`); set by
/// the `just test-*` recipes.
pub const TIER_VAR: &str = "NOXI_TIER";
/// Used when the source tree has no Git metadata, such as in a Nix build or
/// a VM: the noxi revision, and `1` when it had uncommitted changes.
pub const SOURCE_REV_VAR: &str = "NOXI_SOURCE_REV";
pub const SOURCE_DIRTY_VAR: &str = "NOXI_SOURCE_DIRTY";

/// Outputs up to this size are stored inline; larger ones go to a blob.
pub const INLINE_LIMIT: usize = 32 * 1024;

const REPOSITORY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const REFERENCE_LOCK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/nix-official.lock");

/// A case's outcome as compatibility evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
	/// Both engines agree on every compared field (or noxi matches the
	/// recorded reference output).
	Pass,
	/// A difference that no expectation records.
	Fail,
	/// A difference recorded in the runner's expectation table: a noxi gap or
	/// a reference anomaly. Never a pass.
	KnownDifference,
	/// The case exists but did not run under one or both engines.
	Unrun,
}

impl Verdict {
	pub fn as_str(self) -> &'static str {
		match self {
			Verdict::Pass => "pass",
			Verdict::Fail => "fail",
			Verdict::KnownDifference => "known-difference",
			Verdict::Unrun => "unrun",
		}
	}
}

pub fn sha256_hex(bytes: &[u8]) -> String {
	format!("{:x}", Sha256::digest(bytes))
}

fn file_sha256(path: &Path) -> io::Result<String> {
	let mut hasher = Sha256::new();
	io::copy(&mut fs::File::open(path)?, &mut hasher)?;
	Ok(format!("{:x}", hasher.finalize()))
}

/// An executable's path as invoked, the file it resolves to, and that file's
/// SHA-256, so a record names the exact binary it ran.
pub fn executable(path: &Path) -> Value {
	let resolved = fs::canonicalize(path).ok();
	let sha256 = resolved.as_deref().and_then(|path| file_sha256(path).ok());
	json!({"path": path, "resolved": resolved, "sha256": sha256})
}

fn git(args: &[&str]) -> Option<String> {
	let root =
		env::var_os("NOXI_SOURCE_DIR").map_or_else(|| PathBuf::from(REPOSITORY), PathBuf::from);
	let output = Command::new("git")
		.arg("-C")
		.arg(root)
		.args(args)
		.output()
		.ok()?;
	output
		.status
		.success()
		.then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The noxi source revision and whether tracked files differ from it.
pub fn noxi_revision() -> Value {
	if let Some(revision) = git(&["rev-parse", "HEAD"]) {
		let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
			.map(|status| !status.trim().is_empty());
		return json!({"revision": revision.trim(), "dirty": dirty, "source": "git"});
	}
	match env::var(SOURCE_REV_VAR) {
		Ok(revision) => json!({
			"revision": revision,
			"dirty": env::var(SOURCE_DIRTY_VAR).ok().map(|value| value == "1"),
			"source": SOURCE_REV_VAR,
		}),
		Err(_) => json!({"revision": "unknown", "dirty": null, "source": null}),
	}
}

/// The pinned reference release and commit from `tests/nix-official.lock`.
pub fn reference_revision() -> Value {
	let lock = fs::read_to_string(REFERENCE_LOCK).unwrap_or_default();
	let field = |name: &str| {
		lock.lines()
			.find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
			.map(str::to_owned)
	};
	json!({"release": field("ref"), "revision": field("commit"), "repository": field("repository")})
}

pub fn platform() -> String {
	format!("{}-{}", env::consts::ARCH, env::consts::OS)
}

fn now() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map_or(0, |elapsed| elapsed.as_secs())
}

/// `bytes` as a record field: its size and SHA-256, and the bytes themselves
/// inline (`text`, or `hex` when they are not UTF-8) or, when larger than
/// [`INLINE_LIMIT`], in `blob`, a path relative to the record file.
pub fn bytes_field(bytes: &[u8], blobs: Option<&Path>) -> Value {
	let sha256 = sha256_hex(bytes);
	let mut field = Map::new();
	field.insert("size".into(), bytes.len().into());
	field.insert("sha256".into(), sha256.clone().into());
	if bytes.len() <= INLINE_LIMIT {
		match std::str::from_utf8(bytes) {
			Ok(text) => field.insert("text".into(), text.into()),
			Err(_) => field.insert("hex".into(), data_hex(bytes).into()),
		};
	} else if let Some(blobs) = blobs {
		let path = blobs.join(&sha256);
		if !path.exists() && fs::create_dir_all(blobs).is_ok() {
			let _ = fs::write(&path, bytes);
		}
		field.insert("blob".into(), format!("blobs/{sha256}").into());
	}
	Value::Object(field)
}

fn data_hex(bytes: &[u8]) -> String {
	bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// What one engine did for one case.
#[derive(Clone, Debug, Default)]
pub struct Capture {
	pub argv: Vec<String>,
	pub exit_code: Option<i32>,
	pub signal: Option<i32>,
	pub timed_out: bool,
	pub duration: Duration,
	pub stdout: Vec<u8>,
	/// `None` when the runner captures both streams together in `stdout`.
	pub stderr: Option<Vec<u8>>,
	/// A [`store_snapshot`] of the store the case ran against, when it had one.
	pub store: Option<Value>,
	/// A runner's normalized daemon exchange, when the case ran a daemon.
	pub daemon_exchange: Option<Value>,
	/// Runner-specific fields, such as a script outcome or a log path.
	pub extra: Map<String, Value>,
}

impl Capture {
	/// Stdout parsed as JSON, when it is a JSON document.
	pub fn structured_output(&self) -> Option<Value> {
		let text = std::str::from_utf8(&self.stdout).ok()?.trim();
		if !(text.starts_with('{') || text.starts_with('[')) {
			return None;
		}
		serde_json::from_str(text).ok()
	}

	pub fn to_json(&self, blobs: Option<&Path>) -> Value {
		let structured = self
			.structured_output()
			.filter(|_| self.stdout.len() <= INLINE_LIMIT);
		let mut object = Map::new();
		object.insert("argv".into(), json!(self.argv));
		object.insert("exit_code".into(), json!(self.exit_code));
		object.insert("signal".into(), json!(self.signal));
		object.insert("timed_out".into(), self.timed_out.into());
		object.insert(
			"duration_ms".into(),
			(self.duration.as_millis() as u64).into(),
		);
		object.insert("stdout".into(), bytes_field(&self.stdout, blobs));
		object.insert(
			"stderr".into(),
			self.stderr
				.as_deref()
				.map_or(json!("combined with stdout"), |bytes| {
					bytes_field(bytes, blobs)
				}),
		);
		object.insert("structured_output".into(), json!(structured));
		object.insert("store".into(), json!(self.store));
		object.insert("daemon_exchange".into(), json!(self.daemon_exchange));
		for (key, value) in &self.extra {
			object.insert(key.clone(), value.clone());
		}
		Value::Object(object)
	}
}

/// The compared fields on which two captures differ, in a fixed order.
/// Store fields compare only when both captures recorded a store.
pub fn differences(reference: &Capture, noxi: &Capture) -> Vec<&'static str> {
	let mut fields = Vec::new();
	if reference.exit_code != noxi.exit_code {
		fields.push("exit_code");
	}
	if reference.signal != noxi.signal {
		fields.push("signal");
	}
	if reference.timed_out != noxi.timed_out {
		fields.push("timed_out");
	}
	if reference.stdout != noxi.stdout {
		fields.push("stdout");
	}
	if reference.stderr != noxi.stderr {
		fields.push("stderr");
	}
	if reference.structured_output() != noxi.structured_output() {
		fields.push("structured_output");
	}
	if let (Some(left), Some(right)) = (&reference.store, &noxi.store) {
		for (key, name) in [
			("paths", "produced_paths"),
			("derivations", "derivations"),
			("nars", "nar"),
			("metadata", "store_metadata"),
		] {
			if left.get(key) != right.get(key) {
				fields.push(name);
			}
		}
	}
	if reference.daemon_exchange != noxi.daemon_exchange {
		fields.push("daemon_exchange");
	}
	fields
}

/// The SHA-256 and size of `path`'s NAR serialization.
pub fn nar_digest(path: &Path) -> io::Result<(String, u64)> {
	struct Counting {
		hasher: Sha256,
		size: u64,
	}
	impl Write for Counting {
		fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
			self.hasher.update(bytes);
			self.size += bytes.len() as u64;
			Ok(bytes.len())
		}
		fn flush(&mut self) -> io::Result<()> {
			Ok(())
		}
	}
	fn write_node<W: Write>(
		path: &Path,
		node: nix_compat::nar::writer::Node<'_, W>,
	) -> io::Result<()> {
		let meta = fs::symlink_metadata(path)?;
		if meta.file_type().is_symlink() {
			let target = fs::read_link(path)?;
			node.symlink(target.as_os_str().as_encoded_bytes())
		} else if meta.is_dir() {
			let mut names: Vec<_> = fs::read_dir(path)?
				.map(|entry| entry.map(|entry| entry.file_name()))
				.collect::<io::Result<_>>()?;
			names.sort_by(|a, b| a.as_encoded_bytes().cmp(b.as_encoded_bytes()));
			let mut directory = node.directory()?;
			for name in names {
				let entry = directory.entry(name.as_encoded_bytes())?;
				write_node(&path.join(&name), entry)?;
			}
			directory.close()
		} else {
			let executable = meta.permissions().mode() & 0o100 != 0;
			let mut reader = BufReader::new(fs::File::open(path)?);
			node.file(executable, meta.len(), &mut reader)
		}
	}
	let mut sink = Counting {
		hasher: Sha256::new(),
		size: 0,
	};
	write_node(path, nix_compat::nar::writer::open(&mut sink)?)?;
	Ok((format!("{:x}", sink.hasher.finalize()), sink.size))
}

/// The paths in a store directory and their content: each path's NAR
/// SHA-256 and size, every derivation's bytes, and the `ValidPaths` and
/// `Refs` rows of `db` (registration times excluded). `None` when the store
/// directory does not exist.
pub fn store_snapshot(store_dir: &Path, db: Option<&Path>, blobs: Option<&Path>) -> Option<Value> {
	let entries = fs::read_dir(store_dir).ok()?;
	let mut names: Vec<String> = entries
		.flatten()
		.map(|entry| entry.file_name().to_string_lossy().into_owned())
		.filter(|name| name != ".links" && !name.ends_with(".lock") && !name.starts_with(".tmp"))
		.collect();
	names.sort();
	let logical = |name: &str| format!("{}/{name}", store_dir.display());
	let mut nars = Map::new();
	let mut derivations = Map::new();
	for name in &names {
		let path = store_dir.join(name);
		nars.insert(
			logical(name),
			match nar_digest(&path) {
				Ok((sha256, size)) => json!({"sha256": sha256, "size": size}),
				Err(error) => json!({"error": error.to_string()}),
			},
		);
		if name.ends_with(".drv")
			&& let Ok(bytes) = fs::read(&path)
		{
			derivations.insert(logical(name), bytes_field(&bytes, blobs));
		}
	}
	let metadata = db.map(|db| match valid_paths(db) {
		Ok(rows) => rows,
		Err(error) => json!({"error": error}),
	});
	Some(json!({
		"store_dir": store_dir,
		"paths": names.iter().map(|name| logical(name)).collect::<Vec<_>>(),
		"nars": nars,
		"derivations": derivations,
		"metadata": metadata,
	}))
}

fn valid_paths(db: &Path) -> Result<Value, String> {
	if !db.exists() {
		return Ok(Value::Null);
	}
	let connection =
		rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
			.map_err(|error| error.to_string())?;
	let mut references: BTreeMap<i64, Vec<String>> = BTreeMap::new();
	{
		let mut statement = connection
			.prepare("SELECT Refs.referrer, ValidPaths.path FROM Refs JOIN ValidPaths ON Refs.reference = ValidPaths.id ORDER BY ValidPaths.path")
			.map_err(|error| error.to_string())?;
		let rows = statement
			.query_map([], |row| {
				Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
			})
			.map_err(|error| error.to_string())?;
		for row in rows {
			let (referrer, path) = row.map_err(|error| error.to_string())?;
			references.entry(referrer).or_default().push(path);
		}
	}
	let mut statement = connection
		.prepare(
			"SELECT id, path, hash, narSize, deriver, ultimate, sigs, ca FROM ValidPaths ORDER BY path",
		)
		.map_err(|error| error.to_string())?;
	let rows = statement
		.query_map([], |row| {
			let id: i64 = row.get(0)?;
			Ok(json!({
				"path": row.get::<_, String>(1)?,
				"hash": row.get::<_, String>(2)?,
				"nar_size": row.get::<_, Option<i64>>(3)?,
				"deriver": row.get::<_, Option<String>>(4)?,
				"ultimate": row.get::<_, Option<i64>>(5)?,
				"sigs": row.get::<_, Option<String>>(6)?,
				"ca": row.get::<_, Option<String>>(7)?,
				"references": references.get(&id).cloned().unwrap_or_default(),
			}))
		})
		.map_err(|error| error.to_string())?;
	rows.collect::<Result<Vec<_>, _>>()
		.map(Value::from)
		.map_err(|error| error.to_string())
}

/// One case's record before it is written.
#[derive(Clone, Debug)]
pub struct Case {
	/// Unique within the runner, for example a fixture or script path.
	pub id: String,
	/// The `docs/compatibility-matrix.md` row this case is evidence for, as
	/// `scripts/compat-results.py rows` lists them.
	pub matrix_row: String,
	/// Reference interface inventory entries the case exercises, as
	/// `KIND:NAME` (for example `executable:nix-instantiate`).
	pub inventory: Vec<String>,
	/// The environment, configuration, experimental features, inputs and
	/// timeout both engines ran with.
	pub settings: Value,
	/// `reference` and `noxi` captures; a missing engine did not run.
	pub reference: Option<Capture>,
	pub noxi: Option<Capture>,
	pub verdict: Verdict,
	/// The runner's own classification, such as `gap` or `anomaly`.
	pub classification: Option<String>,
	pub reason: Option<String>,
	/// Compared fields that differ; [`differences`] when both engines ran.
	pub differences: Vec<String>,
}

/// Writes the records of one runner process.
pub struct Recorder {
	runner: String,
	path: PathBuf,
	blobs: PathBuf,
	tier: String,
	run: Value,
	file: Mutex<fs::File>,
}

impl Recorder {
	/// Truncates `RUNNER.jsonl` in `$NOXI_RESULTS_DIR`, or in `default_dir`.
	/// `reference` describes the reference executables, `noxi` the
	/// candidate's, and `tier` is the runner's tier unless `NOXI_TIER` names
	/// another.
	pub fn create(
		runner: &str,
		default_dir: &Path,
		tier: &str,
		reference: Value,
		noxi: Value,
	) -> io::Result<Self> {
		let dir =
			env::var_os(RESULTS_DIR_VAR).map_or_else(|| default_dir.to_path_buf(), PathBuf::from);
		fs::create_dir_all(&dir)?;
		let path = dir.join(format!("{runner}.jsonl"));
		let file = fs::File::create(&path)?;
		let tier = env::var(TIER_VAR).unwrap_or_else(|_| tier.to_owned());
		let mut reference_run = reference_revision();
		reference_run["executables"] = reference;
		let mut noxi_run = noxi_revision();
		noxi_run["executables"] = noxi;
		let run = json!({
			"runner": runner,
			"started": now(),
			"platform": platform(),
			"kernel": fs::read_to_string("/proc/sys/kernel/osrelease").ok().map(|text| text.trim().to_owned()),
			"invocation": env::args().collect::<Vec<_>>(),
			"reference": reference_run,
			"noxi": noxi_run,
		});
		Ok(Recorder {
			runner: runner.to_owned(),
			blobs: dir.join("blobs"),
			path,
			tier,
			run,
			file: Mutex::new(file),
		})
	}

	pub fn path(&self) -> &Path {
		&self.path
	}

	/// Where [`bytes_field`] and [`store_snapshot`] put large outputs.
	pub fn blobs(&self) -> &Path {
		&self.blobs
	}

	pub fn record(&self, case: &Case) -> io::Result<()> {
		let engine = |capture: &Option<Capture>| {
			capture
				.as_ref()
				.map_or(Value::Null, |capture| capture.to_json(Some(&self.blobs)))
		};
		let record = json!({
			"schema": SCHEMA,
			"case": format!("{}:{}", self.runner, case.id),
			"runner": self.runner,
			"tier": self.tier,
			"matrix_row": case.matrix_row,
			"inventory": case.inventory,
			"run": self.run,
			"settings": case.settings,
			"engines": {"reference": engine(&case.reference), "noxi": engine(&case.noxi)},
			"verdict": case.verdict.as_str(),
			"classification": case.classification,
			"reason": case.reason,
			"differences": case.differences,
		});
		let mut line = serde_json::to_vec(&record).map_err(io::Error::other)?;
		line.push(b'\n');
		let mut file = self
			.file
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner());
		file.write_all(&line)?;
		file.flush()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn large_outputs_go_to_blobs_and_small_ones_inline() {
		let dir = tempfile::tempdir().unwrap();
		let small = bytes_field(b"hello\n", Some(dir.path()));
		assert_eq!(small["text"], "hello\n");
		assert_eq!(small["size"], 6);
		let binary = bytes_field(&[0xff, 0x0a], Some(dir.path()));
		assert_eq!(binary["hex"], "ff0a");
		let large = vec![b'x'; INLINE_LIMIT + 1];
		let field = bytes_field(&large, Some(&dir.path().join("blobs")));
		let blob = field["blob"].as_str().unwrap();
		assert_eq!(fs::read(dir.path().join(blob)).unwrap(), large);
		assert!(field.get("text").is_none());
	}

	#[test]
	fn nar_digest_matches_the_nar_format() {
		let dir = tempfile::tempdir().unwrap();
		let file = dir.path().join("file");
		fs::write(&file, b"").unwrap();
		// nix-archive-1 ( type regular contents "" ), each string padded.
		let mut expected = Vec::new();
		for token in ["nix-archive-1", "(", "type", "regular", "contents", "", ")"] {
			expected.extend((token.len() as u64).to_le_bytes());
			expected.extend(token.as_bytes());
			expected.resize(expected.len().next_multiple_of(8), 0);
		}
		let (sha256, size) = nar_digest(&file).unwrap();
		assert_eq!(size, expected.len() as u64);
		assert_eq!(sha256, sha256_hex(&expected));
	}

	#[test]
	fn snapshots_and_differences_cover_store_fields() {
		let dir = tempfile::tempdir().unwrap();
		let store = dir.path().join("store");
		fs::create_dir_all(store.join("aaa-out")).unwrap();
		fs::write(store.join("bbb-x.drv"), b"Derive([])").unwrap();
		let snapshot = store_snapshot(&store, None, None).unwrap();
		assert_eq!(snapshot["paths"].as_array().unwrap().len(), 2);
		let drv = format!("{}/bbb-x.drv", store.display());
		assert_eq!(snapshot["derivations"][&drv]["text"], "Derive([])");
		assert!(snapshot["nars"][&drv]["sha256"].is_string());
		assert!(store_snapshot(&dir.path().join("missing"), None, None).is_none());

		let reference = Capture {
			stdout: b"{\"a\":1}".to_vec(),
			stderr: Some(Vec::new()),
			store: Some(snapshot.clone()),
			..Capture::default()
		};
		let mut noxi = reference.clone();
		assert!(differences(&reference, &noxi).is_empty());
		noxi.stdout = b"{\"a\": 2}".to_vec();
		let mut changed = snapshot;
		changed["paths"] = json!([]);
		noxi.store = Some(changed);
		assert_eq!(
			differences(&reference, &noxi),
			["stdout", "structured_output", "produced_paths"]
		);
	}

	#[test]
	fn records_are_one_json_object_per_line() {
		let dir = tempfile::tempdir().unwrap();
		let recorder = Recorder::create("unit", dir.path(), "fast", json!({}), json!({})).unwrap();
		let case = Case {
			id: "one".into(),
			matrix_row: "nix-language-and-evaluator".into(),
			inventory: vec!["executable:nix-instantiate".into()],
			settings: json!({"timeout_ms": 1}),
			reference: None,
			noxi: Some(Capture {
				stderr: Some(Vec::new()),
				..Capture::default()
			}),
			verdict: Verdict::Unrun,
			classification: None,
			reason: Some("reference not run".into()),
			differences: Vec::new(),
		};
		recorder.record(&case).unwrap();
		recorder.record(&case).unwrap();
		let text = fs::read_to_string(recorder.path()).unwrap();
		let lines: Vec<Value> = text
			.lines()
			.map(|line| serde_json::from_str(line).unwrap())
			.collect();
		assert_eq!(lines.len(), 2);
		assert_eq!(lines[0]["schema"], SCHEMA);
		assert_eq!(lines[0]["case"], "unit:one");
		assert_eq!(lines[0]["verdict"], "unrun");
		assert_eq!(lines[0]["engines"]["reference"], Value::Null);
		assert_eq!(lines[0]["engines"]["noxi"]["stderr"]["size"], 0);
		assert!(lines[0]["run"]["noxi"]["revision"].is_string());
		assert!(lines[0]["run"]["reference"]["revision"].is_string());
	}
}
