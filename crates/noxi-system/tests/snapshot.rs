//! The snapshot journal against temporary directories and a fake profile.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use nix_compat::derivation::UnverifiedDerivation;
use nix_compat::nixhash::HashAlgo;
use nix_compat::store_path::StorePath;
use noxi_store::api::DerivedPath;
use noxi_store::{FileIngestionMethod, Store, ValidPathInfo, profiles};
use noxi_system::snapshot::{
	Action, Error, LockDiff, Observed, Outcome, Phase, Request, RollbackOptions, SnapshotStore,
	SourceKind, Status,
};

/// A store whose roots point at closures in a temporary directory, so
/// `profiles::create_generation` can be used without `/nix`.
struct FakeStore {
	closures: PathBuf,
}

impl FakeStore {
	fn closure(&self, path: &StorePath) -> PathBuf {
		self.closures
			.join(path.to_absolute_path().rsplit('/').next().unwrap())
	}
}

impl Store for FakeStore {
	fn real_path(&self, path: &StorePath) -> PathBuf {
		self.closure(path)
	}
	fn is_valid_path(&self, path: &StorePath) -> noxi_store::Result<bool> {
		Ok(self.closure(path).exists())
	}
	fn query_path_info(&self, _: &StorePath) -> noxi_store::Result<Option<ValidPathInfo>> {
		unimplemented!()
	}
	fn query_referrers(&self, _: &StorePath) -> noxi_store::Result<BTreeSet<StorePath>> {
		unimplemented!()
	}
	fn query_valid_derivers(&self, _: &StorePath) -> noxi_store::Result<BTreeSet<StorePath>> {
		unimplemented!()
	}
	fn query_derivation_outputs(
		&self,
		_: &StorePath,
	) -> noxi_store::Result<BTreeMap<String, StorePath>> {
		unimplemented!()
	}
	fn read_derivation(&self, _: &StorePath) -> noxi_store::Result<UnverifiedDerivation> {
		unimplemented!()
	}
	fn add_text_to_store(
		&self,
		_: &str,
		_: &[u8],
		_: &BTreeSet<StorePath>,
	) -> noxi_store::Result<StorePath> {
		unimplemented!()
	}
	fn add_to_store_from_dump(
		&self,
		_: &mut (dyn Read + Send),
		_: &str,
		_: FileIngestionMethod,
		_: FileIngestionMethod,
		_: HashAlgo,
		_: &BTreeSet<StorePath>,
	) -> noxi_store::Result<StorePath> {
		unimplemented!()
	}
	fn add_temp_root(&self, _: &StorePath) -> noxi_store::Result<()> {
		Ok(())
	}
	fn add_perm_root(&self, path: &StorePath, gc_root: &Path) -> noxi_store::Result<PathBuf> {
		std::os::unix::fs::symlink(self.closure(path), gc_root).unwrap();
		Ok(gc_root.to_owned())
	}
	fn build_paths(&self, _: &[DerivedPath]) -> noxi_store::Result<()> {
		unimplemented!()
	}
}

const NIXBASE32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";

struct Fixture {
	_tmp: tempfile::TempDir,
	root: PathBuf,
	fake: FakeStore,
	journal: SnapshotStore,
	profile: PathBuf,
	source: PathBuf,
	lock: PathBuf,
	activation_log: PathBuf,
	current_system: PathBuf,
}

impl Fixture {
	fn new() -> Self {
		let tmp = tempfile::tempdir().unwrap();
		let root = tmp.path().to_owned();
		for dir in ["closures", "profiles", "source"] {
			fs::create_dir(root.join(dir)).unwrap();
		}
		let journal = SnapshotStore::open(root.join("snapshots")).unwrap();
		Fixture {
			fake: FakeStore {
				closures: root.join("closures"),
			},
			journal,
			profile: root.join("profiles/system"),
			source: root.join("source"),
			lock: root.join("source/flake.lock"),
			activation_log: root.join("activation.log"),
			current_system: root.join("current-system"),
			root,
			_tmp: tmp,
		}
	}

	/// A closure with a `switch-to-configuration` that logs its argument
	/// and exits with `status`, linked as a new generation and made current.
	fn generation(&self, n: usize, status: i32) -> (u64, PathBuf) {
		let digest = String::from_utf8(vec![NIXBASE32[n]; 32]).unwrap();
		let path =
			StorePath::from_absolute_path(format!("/nix/store/{digest}-system-{n}").as_bytes())
				.unwrap();
		let closure = self.fake.closure(&path);
		fs::create_dir_all(closure.join("bin")).unwrap();
		let script = closure.join("bin/switch-to-configuration");
		fs::write(
			&script,
			format!(
				"#!/bin/sh\necho \"$1\" >> '{}'\nexit {status}\n",
				self.activation_log.display()
			),
		)
		.unwrap();
		fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
		let link = profiles::create_generation(&self.fake, &self.profile, &path).unwrap();
		profiles::switch_link(&self.profile, &link).unwrap();
		let (_, current) = profiles::find_generations(&self.profile).unwrap();
		(current.unwrap(), closure)
	}

	fn request(&self, action: Action) -> Request {
		let mut request =
			Request::new(action, SourceKind::Flake, self.source.display().to_string());
		request.attribute = Some("nixosConfigurations.host".to_owned());
		request.lock = Some(self.lock.clone());
		request.profile = self.profile.clone();
		request.current_system = self.current_system.clone();
		request
	}

	/// Build a generation from the current lock and record a successful switch.
	fn record_switch(&self, n: usize) -> String {
		let pending = self.journal.begin(self.request(Action::Switch)).unwrap();
		let (_, closure) = self.generation(n, 0);
		let observed = Observed {
			closure: Some(closure),
		};
		pending.finish(observed, Outcome::success(None)).unwrap().id
	}

	fn options(&self) -> RollbackOptions {
		RollbackOptions {
			profile: self.profile.clone(),
			current_system: self.current_system.clone(),
			..RollbackOptions::default()
		}
	}

	fn profile_target(&self) -> PathBuf {
		fs::read_link(&self.profile).unwrap()
	}

	fn activations(&self) -> String {
		fs::read_to_string(&self.activation_log).unwrap_or_default()
	}
}

fn lock_json(inputs: &[(&str, &str, &str, i64)]) -> String {
	let mut nodes = serde_json::Map::new();
	let mut root_inputs = serde_json::Map::new();
	for (name, rev, nar_hash, last_modified) in inputs {
		root_inputs.insert((*name).to_owned(), serde_json::json!(name));
		nodes.insert(
			(*name).to_owned(),
			serde_json::json!({
				"locked": {
					"lastModified": last_modified,
					"narHash": nar_hash,
					"owner": "example",
					"repo": name,
					"rev": rev,
					"type": "github"
				},
				"original": { "owner": "example", "repo": name, "type": "github" }
			}),
		);
	}
	nodes.insert(
		"root".to_owned(),
		serde_json::json!({ "inputs": root_inputs }),
	);
	serde_json::to_string_pretty(&serde_json::json!({
		"nodes": nodes,
		"root": "root",
		"version": 7
	}))
	.unwrap()
}

fn lock_a() -> String {
	lock_json(&[
		(
			"nixpkgs",
			"1111111111111111111111111111111111111111",
			"sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
			1_700_000_000,
		),
		(
			"home-manager",
			"2222222222222222222222222222222222222222",
			"sha256-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=",
			1_700_000_100,
		),
		(
			"flake-utils",
			"3333333333333333333333333333333333333333",
			"sha256-CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC=",
			1_700_000_200,
		),
	])
}

fn lock_b() -> String {
	lock_json(&[
		(
			"nixpkgs",
			"4444444444444444444444444444444444444444",
			"sha256-DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD=",
			1_700_100_000,
		),
		(
			"flake-utils",
			"3333333333333333333333333333333333333333",
			"sha256-CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC=",
			1_700_000_200,
		),
		(
			"sops-nix",
			"5555555555555555555555555555555555555555",
			"sha256-EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE=",
			1_700_000_300,
		),
	])
}

#[test]
fn begin_and_finish_write_records_and_lock_copies() {
	let f = Fixture::new();
	fs::write(&f.lock, lock_a()).unwrap();
	assert!(f.journal.latest().unwrap().is_none());
	assert!(f.journal.list().unwrap().is_empty());

	let pending = f.journal.begin(f.request(Action::Switch)).unwrap();
	assert_eq!(pending.id(), "0001");
	let raw: serde_json::Value =
		serde_json::from_slice(&fs::read(f.journal.dir().join("0001.json")).unwrap()).unwrap();
	assert_eq!(raw["schema_version"], 1);
	assert_eq!(raw["outcome"]["status"], "not-applied");
	assert_eq!(raw["action"], "switch");
	assert_eq!(raw["source"]["kind"], "flake");
	assert!(raw["finished_at"].is_null());
	assert!(raw["closure"].is_null());
	assert_eq!(
		fs::read_to_string(f.journal.lock_copy("0001")).unwrap(),
		lock_a()
	);

	let (generation, closure) = f.generation(1, 0);
	let first = pending
		.finish(
			Observed {
				closure: Some(closure.clone()),
			},
			Outcome::success(None),
		)
		.unwrap();
	assert_eq!(first.outcome.status, Status::Success);
	assert_eq!(first.generation.before, None);
	assert_eq!(first.generation.after, Some(generation));
	assert_eq!(first.boot_after, Some(PathBuf::from("system-1-link")));
	assert_eq!(first.running_after, None);
	assert_eq!(first.closure.as_deref(), Some(closure.as_path()));
	assert_eq!(first.lock.as_ref().unwrap().path, f.lock);
	assert_eq!(first.lock.as_ref().unwrap().sha256.len(), 64);
	assert!(first.finished_at.as_deref().unwrap().ends_with('Z'));
	let raw: serde_json::Value =
		serde_json::from_slice(&fs::read(f.journal.dir().join("0001.json")).unwrap()).unwrap();
	assert_eq!(raw["outcome"]["status"], "success");
	assert_eq!(raw["generation"]["after"], 1);

	// A failed build records no closure and leaves the profile alone.
	let pending = f.journal.begin(f.request(Action::Boot)).unwrap();
	assert_eq!(pending.id(), "0002");
	let second = pending
		.finish(
			Observed::default(),
			Outcome::failed(Phase::Build, "builder exited with status 1"),
		)
		.unwrap();
	assert_eq!(second.generation.before, Some(1));
	assert_eq!(second.generation.after, Some(1));
	assert_eq!(second.closure, None);

	let ids: Vec<String> = f
		.journal
		.list()
		.unwrap()
		.into_iter()
		.map(|s| s.id)
		.collect();
	assert_eq!(ids, ["0001", "0002"]);
	assert_eq!(f.journal.latest().unwrap().unwrap().id, "0002");
	assert_eq!(f.journal.get("1").unwrap(), first);
	assert!(matches!(f.journal.get("7"), Err(Error::NotFound(id)) if id == "0007"));
	assert!(matches!(f.journal.get("x"), Err(Error::InvalidId(_))));
}

#[test]
fn records_from_a_newer_schema_load() {
	let f = Fixture::new();
	f.record_switch(1);
	let mut raw: serde_json::Value =
		serde_json::from_slice(&fs::read(f.journal.dir().join("0001.json")).unwrap()).unwrap();
	raw["schema_version"] = 2.into();
	raw["id"] = "0010".into();
	raw["future_field"] = serde_json::json!({ "anything": [1, 2, 3] });
	raw["source"]["future_nested"] = true.into();
	raw["outcome"]["future_nested"] = "x".into();
	fs::write(
		f.journal.dir().join("0010.json"),
		serde_json::to_vec(&raw).unwrap(),
	)
	.unwrap();

	let newer = f.journal.get("0010").unwrap();
	assert_eq!(newer.schema_version, 2);
	assert_eq!(f.journal.latest().unwrap().unwrap().id, "0010");
	// Ids continue after the highest existing one.
	let pending = f.journal.begin(f.request(Action::Build)).unwrap();
	assert_eq!(pending.id(), "0011");
}

#[test]
fn lock_diff_reports_added_removed_and_changed_inputs() {
	let diff = LockDiff::between(lock_a().as_bytes(), lock_b().as_bytes()).unwrap();
	let names = |inputs: &[noxi_system::snapshot::LockedInput]| {
		inputs.iter().map(|i| i.name.clone()).collect::<Vec<_>>()
	};
	assert_eq!(names(&diff.added), ["sops-nix"]);
	assert_eq!(names(&diff.removed), ["home-manager"]);
	assert_eq!(diff.changed.len(), 1);
	let change = &diff.changed[0];
	assert_eq!(change.name, "nixpkgs");
	assert_eq!(change.fields, ["rev", "narHash", "lastModified"]);
	assert_eq!(
		change.after.rev.as_deref(),
		Some("4444444444444444444444444444444444444444")
	);
	assert_eq!(change.before.last_modified, Some(1_700_000_000));
	assert!(
		LockDiff::between(lock_a().as_bytes(), lock_a().as_bytes())
			.unwrap()
			.is_empty()
	);
	assert!(matches!(
		LockDiff::between(b"{}", lock_a().as_bytes()),
		Err(Error::LockFormat { .. })
	));

	// The same comparison through the journal's stored copies.
	let f = Fixture::new();
	fs::write(&f.lock, lock_a()).unwrap();
	f.record_switch(1);
	fs::write(&f.lock, lock_b()).unwrap();
	f.record_switch(2);
	assert_eq!(f.journal.diff_locks("1", "2").unwrap(), diff);
}

#[test]
fn rollback_switches_activates_and_records() {
	let f = Fixture::new();
	let first = f.record_switch(1);
	f.record_switch(2);
	assert_eq!(f.profile_target(), Path::new("system-2-link"));

	let report = f.journal.rollback(&first, f.options()).unwrap();
	assert!(report.succeeded());
	assert!(report.activated);
	assert_eq!(report.generation, 1);
	assert_eq!(report.generation_before, Some(2));
	assert_eq!(report.generation_after, Some(1));
	assert_eq!(report.boot_after, Some(PathBuf::from("system-1-link")));
	assert_eq!(f.profile_target(), Path::new("system-1-link"));
	assert_eq!(f.activations(), "switch\n");

	let record = report.record.unwrap();
	assert_eq!(record.id, "0003");
	assert_eq!(record.action, Action::Rollback);
	assert_eq!(record.generation.before, Some(2));
	assert_eq!(record.generation.after, Some(1));
	assert_eq!(record.outcome.status, Status::Success);
	assert_eq!(f.journal.latest().unwrap().unwrap(), record);
}

#[test]
fn dry_run_and_no_activate_do_not_run_the_activation_program() {
	let f = Fixture::new();
	let first = f.record_switch(1);
	f.record_switch(2);

	let report = f
		.journal
		.rollback(
			&first,
			RollbackOptions {
				dry_run: true,
				..f.options()
			},
		)
		.unwrap();
	assert!(report.dry_run && report.record.is_none() && !report.activated);
	assert!(report.activation_requested && report.lock_to_restore.is_none());
	assert_eq!(f.profile_target(), Path::new("system-2-link"));
	assert_eq!(f.journal.list().unwrap().len(), 2);
	assert_eq!(f.activations(), "");

	let report = f
		.journal
		.rollback(
			&first,
			RollbackOptions {
				activate: false,
				..f.options()
			},
		)
		.unwrap();
	assert!(report.succeeded() && !report.activated);
	assert_eq!(f.profile_target(), Path::new("system-1-link"));
	assert_eq!(f.activations(), "");
	assert_eq!(report.record.unwrap().action, Action::Rollback);
}

#[test]
fn restore_lock_writes_the_copy_back_and_refuses_edited_locks() {
	let f = Fixture::new();
	fs::write(&f.lock, lock_a()).unwrap();
	let first = f.record_switch(1);
	fs::write(&f.lock, lock_b()).unwrap();
	let second = f.record_switch(2);

	let restore = RollbackOptions {
		restore_lock: true,
		..f.options()
	};
	let report = f.journal.rollback(&first, restore.clone()).unwrap();
	assert!(report.succeeded());
	assert_eq!(report.lock_restored.as_deref(), Some(f.lock.as_path()));
	assert_eq!(fs::read_to_string(&f.lock).unwrap(), lock_a());
	// The rollback record holds the lock now on disk.
	let record = report.record.unwrap();
	assert_eq!(
		fs::read_to_string(f.journal.lock_copy(&record.id)).unwrap(),
		lock_a()
	);

	// An edit made after the most recent snapshot is not overwritten.
	fs::write(&f.lock, "{\"edited\": true}").unwrap();
	let error = f.journal.rollback(&second, restore.clone()).unwrap_err();
	assert!(
		matches!(&error, Error::LockEdited { recorded_by, .. } if *recorded_by == record.id),
		"{error}"
	);
	assert!(error.hint().unwrap().contains("--force"));
	assert_eq!(f.profile_target(), Path::new("system-1-link"));
	assert_eq!(fs::read_to_string(&f.lock).unwrap(), "{\"edited\": true}");

	let report = f
		.journal
		.rollback(
			&second,
			RollbackOptions {
				force: true,
				..restore
			},
		)
		.unwrap();
	assert!(report.succeeded());
	assert_eq!(fs::read_to_string(&f.lock).unwrap(), lock_b());
	assert_eq!(f.profile_target(), Path::new("system-2-link"));
}

#[test]
fn rollback_refuses_missing_generations_and_records_failed_activation() {
	let f = Fixture::new();
	let first = f.record_switch(1);
	f.record_switch(2);

	let error = f
		.journal
		.rollback(
			&first,
			RollbackOptions {
				profile: f.root.join("profiles/other"),
				..f.options()
			},
		)
		.unwrap_err();
	assert!(matches!(error, Error::ProfileMismatch { .. }));

	profiles::delete_generation(&f.profile, 1).unwrap();
	let error = f.journal.rollback(&first, f.options()).unwrap_err();
	assert!(matches!(
		error,
		Error::GenerationMissing { generation: 1, .. }
	));
	assert_eq!(f.journal.list().unwrap().len(), 2);

	// A failing activation program is recorded with the observed state.
	let pending = f.journal.begin(f.request(Action::Switch)).unwrap();
	let (_, closure) = f.generation(3, 1);
	let third = pending
		.finish(
			Observed {
				closure: Some(closure),
			},
			Outcome::success(None),
		)
		.unwrap();
	f.record_switch(4);
	let report = f.journal.rollback(&third.id, f.options()).unwrap();
	assert!(!report.succeeded());
	let record = report.record.unwrap();
	assert_eq!(record.outcome.status, Status::Failed);
	assert_eq!(record.outcome.phase, Some(Phase::Activate));
	assert_eq!(record.generation.after, Some(3));
	assert_eq!(f.activations(), "switch\n");
}

#[test]
fn concurrent_writers_get_distinct_ids() {
	let f = Fixture::new();
	let mut ids: Vec<String> = std::thread::scope(|scope| {
		let workers: Vec<_> = (0..8)
			.map(|_| {
				scope.spawn(|| {
					f.journal
						.begin(f.request(Action::Build))
						.unwrap()
						.id()
						.to_owned()
				})
			})
			.collect();
		workers.into_iter().map(|w| w.join().unwrap()).collect()
	});
	ids.sort();
	let expected: Vec<String> = (1..=8).map(|n| format!("{n:04}")).collect();
	assert_eq!(ids, expected);
}
