//! `noxi system` with an injected builder, fixture closures whose
//! `switch-to-configuration` is a logging shell script, and a profile,
//! running-system link and snapshot journal in a temporary directory.
//!
//! The environment is passed to `parse` as a function, so these tests do
//! not touch the process environment and can run in parallel.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use noxi_store::profiles;
use noxi_system::rebuild::{
	self, BuildError, BuildRequest, Builder, Invocation, RebuildOptions, RollbackRequest,
};
use noxi_system::snapshot::{Action, Phase, SnapshotStore, Status};

struct Fixture {
	dir: tempfile::TempDir,
}

impl Fixture {
	/// A profile at generation 1 pointing at closure `c0`, which is also the
	/// running system.
	fn new() -> Self {
		let fixture = Fixture {
			dir: tempfile::tempdir().unwrap(),
		};
		fs::create_dir(fixture.path("profiles")).unwrap();
		fs::write(fixture.path("configuration.nix"), "{ }").unwrap();
		let c0 = fixture.closure("c0", 0);
		symlink(&c0, fixture.path("profiles/system-1-link")).unwrap();
		symlink("system-1-link", fixture.profile()).unwrap();
		symlink(&c0, fixture.current()).unwrap();
		fixture
	}

	fn path(&self, name: &str) -> PathBuf {
		self.dir.path().join(name)
	}

	fn profile(&self) -> PathBuf {
		self.path("profiles/system")
	}

	fn current(&self) -> PathBuf {
		self.path("current-system")
	}

	fn log(&self) -> String {
		fs::read_to_string(self.path("activation.log")).unwrap_or_default()
	}

	fn journal(&self) -> SnapshotStore {
		SnapshotStore::open(self.path("snapshots")).unwrap()
	}

	/// A closure whose `switch-to-configuration` logs its action and
	/// `NIXOS_INSTALL_BOOTLOADER`, points the running-system link at the
	/// closure for `switch` and `test` as the real program does, and exits
	/// with `status`.
	fn closure(&self, name: &str, status: u8) -> PathBuf {
		let closure = self.path("closures").join(name);
		fs::create_dir_all(closure.join("bin")).unwrap();
		let program = closure.join("bin/switch-to-configuration");
		fs::write(
			&program,
			format!(
				"#!/bin/sh\necho \"$1 {name} NIXOS_INSTALL_BOOTLOADER=$NIXOS_INSTALL_BOOTLOADER\" >> '{log}'\ncase \"$1\" in switch|test) [ {status} = 0 ] && ln -sfn '{closure}' '{current}' ;; esac\nexit {status}\n",
				log = self.path("activation.log").display(),
				closure = closure.display(),
				current = self.current().display(),
			),
		)
		.unwrap();
		fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
		closure
	}

	fn env(&self) -> BTreeMap<&'static str, String> {
		BTreeMap::from([
			(
				"NOXI_SYSTEM_PROFILE",
				self.profile().to_string_lossy().into_owned(),
			),
			(
				"NOXI_CURRENT_SYSTEM",
				self.current().to_string_lossy().into_owned(),
			),
			(
				"NOXI_SNAPSHOT_DIR",
				self.path("snapshots").to_string_lossy().into_owned(),
			),
		])
	}

	fn parse(&self, args: &[&str], env: &BTreeMap<&'static str, String>) -> Invocation {
		let owned = args.iter().map(|arg| (*arg).to_owned()).collect();
		match rebuild::parse(owned, &|name| env.get(name).cloned()) {
			Ok(invocation) => invocation,
			Err(message) => panic!("{args:?}: {message}"),
		}
	}

	/// `noxi system ARGS --file configuration.nix` as root, building
	/// with `builder`.
	fn rebuild(&self, args: &[&str], builder: impl Builder + 'static) -> Run {
		let mut args = args.to_vec();
		let file = self.path("configuration.nix");
		let file = file.to_str().unwrap();
		if !args.contains(&"--flake") && !args.contains(&"--file") {
			args.extend(["--file", file]);
		}
		let Invocation::Rebuild(mut options) = self.parse(&args, &self.env()) else {
			panic!("not a rebuild");
		};
		options.euid = 0;
		options.builder = Box::new(builder);
		execute_rebuild(*options)
	}

	fn rollback(&self, args: &[&str]) -> Run {
		let mut args = args.to_vec();
		args.insert(0, "rollback");
		let Invocation::Rollback(mut request) = self.parse(&args, &self.env()) else {
			panic!("not a rollback");
		};
		request.euid = 0;
		execute_rollback(request)
	}

	fn generations(&self) -> (Vec<u64>, Option<u64>) {
		let (generations, current) = profiles::find_generations(&self.profile()).unwrap();
		(generations.iter().map(|g| g.number).collect(), current)
	}
}

struct Run {
	code: ExitCode,
	out: String,
	err: String,
}

fn execute_rebuild(options: RebuildOptions) -> Run {
	let (mut out, mut err) = (Vec::new(), Vec::new());
	let code = rebuild::rebuild(options, &mut out, &mut err);
	Run {
		code,
		out: String::from_utf8(out).unwrap(),
		err: String::from_utf8(err).unwrap(),
	}
}

fn execute_rollback(request: RollbackRequest) -> Run {
	let (mut out, mut err) = (Vec::new(), Vec::new());
	let code = rebuild::rollback(request, &mut out, &mut err);
	Run {
		code,
		out: String::from_utf8(out).unwrap(),
		err: String::from_utf8(err).unwrap(),
	}
}

/// A builder that returns `closure` and records the requests it saw.
fn returns(closure: &Path) -> (impl Builder + 'static, Rc<RefCell<Vec<BuildRequest>>>) {
	let seen = Rc::new(RefCell::new(Vec::new()));
	let closure = closure.to_owned();
	let record = seen.clone();
	let builder = move |request: &BuildRequest| -> Result<PathBuf, BuildError> {
		record.borrow_mut().push(request.clone());
		Ok(closure.clone())
	};
	(builder, seen)
}

/// The order phase lines appear in standard error.
fn phases(err: &str) -> Vec<&str> {
	err.lines()
		.filter_map(|line| line.split_once(": ").map(|(phase, _)| phase))
		.filter(|phase| {
			[
				"resolve",
				"update",
				"evaluate",
				"build",
				"prepare activation",
				"activate",
				"verify",
			]
			.contains(phase)
		})
		.collect()
}

#[test]
fn build_links_the_result_without_activating() {
	let f = Fixture::new();
	let c1 = f.closure("c1", 0);
	let (builder, seen) = returns(&c1);
	let link = f.path("result");
	let run = f.rebuild(
		&[
			"build",
			"--attr",
			"host",
			"--out-link",
			link.to_str().unwrap(),
		],
		builder,
	);
	assert_eq!(run.code, ExitCode::SUCCESS, "{}", run.err);
	let request = &seen.borrow()[0];
	assert_eq!(request.attribute, "host.config.system.build.toplevel");
	assert_eq!(request.link, link);
	assert_eq!(fs::read_link(&link).unwrap(), c1);
	assert_eq!(f.generations(), (vec![1], Some(1)));
	assert_eq!(f.log(), "");
	assert_eq!(
		phases(&run.err),
		[
			"resolve",
			"evaluate",
			"build",
			"prepare activation",
			"activate",
			"verify"
		]
	);
	assert!(
		run.err.contains("activate: no activation occurred"),
		"{}",
		run.err
	);
	assert!(run.out.contains("outcome: success"), "{}", run.out);
	assert!(run.out.contains("running system: unchanged"), "{}", run.out);
	assert!(run.out.contains("no activation occurred"), "{}", run.out);
	let record = f.journal().latest().unwrap().unwrap();
	assert_eq!(record.action, Action::Build);
	assert_eq!(record.outcome.status, Status::Success);
	assert_eq!(record.closure, Some(c1));
	assert_eq!(record.generation.before, Some(1));
	assert_eq!(record.generation.after, Some(1));
	assert_eq!(
		record.source.attribute.as_deref(),
		Some("host.config.system.build.toplevel")
	);
}

#[test]
fn switch_creates_a_generation_and_activates_it() {
	let f = Fixture::new();
	let c0 = f.path("closures/c0");
	let c1 = f.closure("c1", 0);
	let (builder, seen) = returns(&c1);
	let run = f.rebuild(&["switch"], builder);
	assert_eq!(run.code, ExitCode::SUCCESS, "{}", run.err);
	// Without --attr the file's own toplevel is built, through a temporary
	// link rather than ./result.
	assert_eq!(seen.borrow()[0].attribute, "config.system.build.toplevel");
	assert!(!seen.borrow()[0].link.starts_with(f.dir.path()));
	assert_eq!(f.generations(), (vec![1, 2], Some(2)));
	assert_eq!(
		fs::read_link(f.profile()).unwrap(),
		Path::new("system-2-link")
	);
	assert_eq!(fs::read_link(f.current()).unwrap(), c1);
	assert_eq!(f.log(), "switch c1 NIXOS_INSTALL_BOOTLOADER=0\n");
	assert_eq!(
		phases(&run.err),
		[
			"resolve",
			"evaluate",
			"build",
			"prepare activation",
			"activate",
			"verify"
		]
	);
	assert!(run.out.contains(&format!(
		"running system: changed, {} (generation 1) -> {} (generation 2)",
		c0.display(),
		c1.display()
	)));
	assert!(run.out.contains("boot default: changed"), "{}", run.out);
	assert!(run.out.contains("current generation: 2 (was 1)"));
	assert!(run.out.contains("previous generation: 1"));
	assert!(run.out.contains("snapshot: 0001"));
	assert!(
		run.out
			.contains("rollback: noxi system rollback --to 1 --profile")
	);
	let record = f.journal().get("1").unwrap();
	assert_eq!(record.action, Action::Switch);
	assert_eq!(record.outcome.status, Status::Success);
	assert_eq!(record.generation.before, Some(1));
	assert_eq!(record.generation.after, Some(2));
	assert_eq!(record.closure, Some(c1.clone()));
	assert_eq!(record.running_before, Some(c0));
	assert_eq!(record.running_after, Some(c1));
}

#[test]
fn install_bootloader_reaches_the_activation_program() {
	let f = Fixture::new();
	let c1 = f.closure("c1", 0);
	let (builder, _) = returns(&c1);
	let run = f.rebuild(&["boot", "--install-bootloader", "--no-snapshot"], builder);
	assert_eq!(run.code, ExitCode::SUCCESS, "{}", run.err);
	assert_eq!(f.log(), "boot c1 NIXOS_INSTALL_BOOTLOADER=1\n");
	assert!(run.out.contains("snapshot: none (--no-snapshot)"));
	assert!(!f.path("snapshots").exists());
}

#[test]
fn boot_moves_the_profile_but_not_the_running_system() {
	let f = Fixture::new();
	let c0 = f.path("closures/c0");
	let c1 = f.closure("c1", 0);
	let (builder, _) = returns(&c1);
	let run = f.rebuild(&["boot"], builder);
	assert_eq!(run.code, ExitCode::SUCCESS, "{}", run.err);
	assert_eq!(f.generations(), (vec![1, 2], Some(2)));
	assert_eq!(fs::read_link(f.current()).unwrap(), c0);
	assert_eq!(f.log(), "boot c1 NIXOS_INSTALL_BOOTLOADER=0\n");
	assert!(run.out.contains("running system: unchanged"), "{}", run.out);
	assert!(run.out.contains("boot default: changed"), "{}", run.out);
	assert!(
		run.out
			.contains("note: boot: the running system is unchanged")
	);
}

#[test]
fn test_and_dry_activate_leave_the_profile_alone() {
	let f = Fixture::new();
	let c1 = f.closure("c1", 0);
	let (builder, _) = returns(&c1);
	let run = f.rebuild(&["dry-activate"], builder);
	assert_eq!(run.code, ExitCode::SUCCESS, "{}", run.err);
	assert_eq!(f.generations(), (vec![1], Some(1)));
	assert!(run.out.contains("running system: unchanged"), "{}", run.out);
	assert!(run.out.contains("none were applied"), "{}", run.out);

	let (builder, _) = returns(&c1);
	let run = f.rebuild(&["test"], builder);
	assert_eq!(run.code, ExitCode::SUCCESS, "{}", run.err);
	assert_eq!(f.generations(), (vec![1], Some(1)));
	assert_eq!(fs::read_link(f.current()).unwrap(), c1);
	assert!(run.out.contains("running system: changed"), "{}", run.out);
	assert!(run.out.contains("boot default: unchanged"), "{}", run.out);
	assert_eq!(
		f.log(),
		"dry-activate c1 NIXOS_INSTALL_BOOTLOADER=0\ntest c1 NIXOS_INSTALL_BOOTLOADER=0\n"
	);
	let records = f.journal().list().unwrap();
	assert_eq!(records[0].action, Action::DryActivate);
	assert_eq!(records[1].action, Action::Test);
	assert!(records.iter().all(|r| r.outcome.status == Status::Success));
}

#[test]
fn failed_activation_reports_the_phase_and_the_state() {
	let f = Fixture::new();
	let c0 = f.path("closures/c0");
	let bad = f.closure("bad", 1);
	let (builder, _) = returns(&bad);
	let run = f.rebuild(&["switch"], builder);
	assert_eq!(run.code, ExitCode::FAILURE);
	assert_eq!(f.log(), "switch bad NIXOS_INSTALL_BOOTLOADER=0\n");
	let error = run
		.err
		.lines()
		.find(|line| line.starts_with("error: "))
		.unwrap();
	assert!(error.starts_with("error: activate: "), "{error}");
	assert!(
		error.contains("the profile now points at generation 2 (boot default changed)"),
		"{error}"
	);
	assert!(
		error.contains("the running system link is unchanged"),
		"{error}"
	);
	assert!(
		run.err
			.contains("hint: inspect the activation output above")
	);
	assert!(run.out.contains("outcome: failed in phase activate"));
	assert!(run.out.contains("running system: unchanged"));
	assert_eq!(fs::read_link(f.current()).unwrap(), c0);
	let record = f.journal().latest().unwrap().unwrap();
	assert_eq!(record.outcome.status, Status::Failed);
	assert_eq!(record.outcome.phase, Some(Phase::Activate));
	assert_eq!(record.generation.after, Some(2));
}

#[test]
fn failed_build_changes_nothing() {
	let f = Fixture::new();
	let builder = |_: &BuildRequest| -> Result<PathBuf, BuildError> {
		Err(BuildError::new(Phase::Build, "builder for 'x.drv' failed"))
	};
	let run = f.rebuild(&["switch"], builder);
	assert_eq!(run.code, ExitCode::FAILURE);
	assert!(
		run.err
			.contains("error: build: builder for 'x.drv' failed; the profile and boot default are unchanged; the running system link is unchanged"),
		"{}",
		run.err
	);
	assert_eq!(phases(&run.err), ["resolve", "evaluate"]);
	assert_eq!(f.generations(), (vec![1], Some(1)));
	assert_eq!(f.log(), "");
	assert!(run.out.contains("closure: none"));
	let record = f.journal().latest().unwrap().unwrap();
	assert_eq!(record.outcome.status, Status::Failed);
	assert_eq!(record.outcome.phase, Some(Phase::Build));
	assert_eq!(record.closure, None);
}

#[test]
fn a_closure_without_an_activation_program_is_refused_after_begin() {
	let f = Fixture::new();
	let empty = f.path("closures/empty");
	fs::create_dir_all(&empty).unwrap();
	let (builder, _) = returns(&empty);
	let run = f.rebuild(&["switch"], builder);
	assert_eq!(run.code, ExitCode::FAILURE);
	assert!(
		run.err
			.contains("error: prepare activation: activation program")
	);
	assert_eq!(f.generations(), (vec![1], Some(1)));
	let record = f.journal().latest().unwrap().unwrap();
	assert_eq!(record.outcome.status, Status::NotApplied);
	assert_eq!(record.outcome.phase, Some(Phase::PrepareActivation));
}

#[test]
fn unprivileged_runs_without_diversion_are_refused_up_front() {
	let f = Fixture::new();
	// Only the journal is diverted; the profile is the real default.
	let env = BTreeMap::from([(
		"NOXI_SNAPSHOT_DIR",
		f.path("snapshots").to_string_lossy().into_owned(),
	)]);
	let file = f.path("configuration.nix");
	for action in ["switch", "boot", "test"] {
		let Invocation::Rebuild(mut options) =
			f.parse(&[action, "--file", file.to_str().unwrap()], &env)
		else {
			panic!();
		};
		assert_eq!(options.profile, Path::new("/nix/var/nix/profiles/system"));
		options.euid = 1000;
		options.builder = Box::new(|_: &BuildRequest| -> Result<PathBuf, BuildError> {
			panic!("the builder must not run")
		});
		let run = execute_rebuild(*options);
		assert_eq!(run.code, ExitCode::FAILURE);
		assert!(
			run.err.contains("must run as root; nothing was changed"),
			"{}",
			run.err
		);
		assert!(run.err.contains("hint: run 'noxi system"), "{}", run.err);
		assert!(run.out.contains("system state: not read"), "{}", run.out);
	}
	assert!(!f.path("snapshots").exists());

	// Diverting only the activation program is not enough for switch.
	let mut env = env;
	env.insert(
		"NOXI_ACTIVATION_PROGRAM",
		f.path("closures/c0/bin/switch-to-configuration")
			.to_string_lossy()
			.into_owned(),
	);
	let Invocation::Rebuild(mut options) =
		f.parse(&["switch", "--file", file.to_str().unwrap()], &env)
	else {
		panic!();
	};
	options.euid = 1000;
	let run = execute_rebuild(*options);
	assert!(run.err.contains("must run as root"), "{}", run.err);

	let Invocation::Rollback(mut request) = f.parse(&["rollback"], &env) else {
		panic!();
	};
	request.euid = 1000;
	let run = execute_rollback(request);
	assert_eq!(run.code, ExitCode::FAILURE);
	assert!(run.err.contains("must run as root"), "{}", run.err);
}

#[test]
fn unprivileged_runs_with_full_diversion_proceed() {
	let f = Fixture::new();
	let c1 = f.closure("c1", 0);
	let mut env = f.env();
	env.insert(
		"NOXI_ACTIVATION_PROGRAM",
		c1.join("bin/switch-to-configuration")
			.to_string_lossy()
			.into_owned(),
	);
	let file = f.path("configuration.nix");
	let Invocation::Rebuild(mut options) =
		f.parse(&["switch", "--file", file.to_str().unwrap()], &env)
	else {
		panic!();
	};
	assert!(options.profile_diverted);
	options.euid = 1000;
	options.builder = Box::new(returns(&c1).0);
	let run = execute_rebuild(*options);
	assert_eq!(run.code, ExitCode::SUCCESS, "{}", run.err);
	assert_eq!(f.generations(), (vec![1, 2], Some(2)));
}

#[test]
fn rollback_selects_previous_or_named_generations() {
	let f = Fixture::new();
	let c0 = f.path("closures/c0");
	let c1 = f.closure("c1", 0);
	let c2 = f.closure("c2", 0);
	assert_eq!(
		f.rebuild(&["switch"], returns(&c1).0).code,
		ExitCode::SUCCESS
	);
	assert_eq!(
		f.rebuild(&["switch"], returns(&c2).0).code,
		ExitCode::SUCCESS
	);
	assert_eq!(f.generations(), (vec![1, 2, 3], Some(3)));

	let run = f.rollback(&[]);
	assert_eq!(run.code, ExitCode::SUCCESS, "{}", run.err);
	assert_eq!(f.generations(), (vec![1, 2, 3], Some(2)));
	assert_eq!(fs::read_link(f.current()).unwrap(), c1);
	assert!(
		run.err.contains("to generation 2") && run.err.contains("recorded by snapshot 0001"),
		"{}",
		run.err
	);
	assert!(run.out.contains("running system: changed"), "{}", run.out);
	let record = f.journal().latest().unwrap().unwrap();
	assert_eq!(record.id, "0003");
	assert_eq!(record.action, Action::Rollback);
	assert_eq!(record.outcome.status, Status::Success);
	assert_eq!(record.generation.before, Some(3));
	assert_eq!(record.generation.after, Some(2));
	assert_eq!(record.closure, Some(c1.clone()));

	let run = f.rollback(&["--to", "1"]);
	assert_eq!(run.code, ExitCode::SUCCESS, "{}", run.err);
	assert_eq!(f.generations().1, Some(1));
	assert_eq!(fs::read_link(f.current()).unwrap(), c0);
	assert!(run.err.contains("recorded by no snapshot"), "{}", run.err);
	assert_eq!(
		f.log(),
		"switch c1 NIXOS_INSTALL_BOOTLOADER=0\nswitch c2 NIXOS_INSTALL_BOOTLOADER=0\nswitch c1 NIXOS_INSTALL_BOOTLOADER=0\nswitch c0 NIXOS_INSTALL_BOOTLOADER=0\n"
	);

	let run = f.rollback(&["--to", "9"]);
	assert_eq!(run.code, ExitCode::FAILURE);
	assert!(
		run.err.contains("error: resolve: generation 9"),
		"{}",
		run.err
	);
	let run = f.rollback(&["--to", "1"]);
	assert_eq!(run.code, ExitCode::FAILURE);
	assert!(
		run.err.contains("already the current generation"),
		"{}",
		run.err
	);
}

#[test]
fn rollback_with_one_generation_fails_clearly() {
	let f = Fixture::new();
	let run = f.rollback(&[]);
	assert_eq!(run.code, ExitCode::FAILURE);
	assert!(
		run.err.contains("error: resolve: profile \"")
			&& run
				.err
				.contains("has no generation older than the current one (1)"),
		"{}",
		run.err
	);
	assert_eq!(f.generations(), (vec![1], Some(1)));
	assert_eq!(f.log(), "");
	assert!(f.journal().list().unwrap().is_empty());
}

#[test]
fn json_output_has_the_documented_shape() {
	let f = Fixture::new();
	let c1 = f.closure("c1", 0);
	let run = f.rebuild(&["switch", "--json"], returns(&c1).0);
	assert_eq!(run.code, ExitCode::SUCCESS, "{}", run.err);
	let value: serde_json::Value = serde_json::from_str(&run.out).unwrap();
	for key in [
		"action",
		"source",
		"lock",
		"update",
		"closure",
		"profile",
		"generation",
		"running",
		"boot",
		"snapshot",
		"outcome",
	] {
		assert!(value.get(key).is_some(), "missing {key}: {value}");
	}
	assert_eq!(value["action"], "switch");
	assert_eq!(value["source"]["kind"], "file");
	assert_eq!(value["lock"], serde_json::Value::Null);
	assert_eq!(value["update"], serde_json::Value::Null);
	assert_eq!(value["closure"], c1.to_str().unwrap());
	assert_eq!(value["generation"]["before"], 1);
	assert_eq!(value["generation"]["after"], 2);
	assert_eq!(value["running"]["after"], c1.to_str().unwrap());
	assert_eq!(value["boot"]["after"], c1.to_str().unwrap());
	assert_eq!(value["snapshot"], "0001");
	assert_eq!(value["outcome"]["status"], "success");
	// A success records the phase it finished in.
	assert_eq!(value["outcome"]["phase"], "verify");

	let builder = |_: &BuildRequest| -> Result<PathBuf, BuildError> {
		Err(BuildError::new(Phase::Evaluate, "undefined variable 'x'"))
	};
	let run = f.rebuild(&["boot", "--json"], builder);
	assert_eq!(run.code, ExitCode::FAILURE);
	let value: serde_json::Value = serde_json::from_str(&run.out).unwrap();
	assert_eq!(value["outcome"]["status"], "failed");
	assert_eq!(value["outcome"]["phase"], "evaluate");
	assert_eq!(value["closure"], serde_json::Value::Null);
}

#[test]
fn flake_sources_select_the_host_configuration() {
	let f = Fixture::new();
	let flake = f.path("flake");
	fs::create_dir(&flake).unwrap();
	fs::write(flake.join("flake.nix"), "{ outputs = { self }: { }; }").unwrap();
	let c1 = f.closure("c1", 0);
	let reference = format!("{}#machine", flake.display());
	let (builder, seen) = returns(&c1);
	let run = f.rebuild(
		&[
			"build",
			"--flake",
			&reference,
			"--json",
			"--out-link",
			f.path("result").to_str().unwrap(),
		],
		builder,
	);
	assert_eq!(run.code, ExitCode::SUCCESS, "{}", run.err);
	let request = seen.borrow()[0].clone();
	assert_eq!(
		request.attribute,
		"nixosConfigurations.\"machine\".config.system.build.toplevel"
	);
	assert_eq!(
		Path::new(&request.source),
		fs::canonicalize(&flake).unwrap()
	);
	let value: serde_json::Value = serde_json::from_str(&run.out).unwrap();
	assert_eq!(value["lock"]["exists"], false);
	assert!(
		value["lock"]["path"]
			.as_str()
			.unwrap()
			.ends_with("flake/flake.lock")
	);

	// Without a fragment the host name selects the configuration.
	let env = f.env();
	let Invocation::Rebuild(mut options) = f.parse(
		&[
			"build",
			"--flake",
			flake.to_str().unwrap(),
			"--out-link",
			f.path("result2").to_str().unwrap(),
		],
		&env,
	) else {
		panic!();
	};
	let (builder, seen) = returns(&c1);
	options.builder = Box::new(builder);
	options.hostname = Some("thishost".to_owned());
	assert_eq!(execute_rebuild(*options).code, ExitCode::SUCCESS);
	assert_eq!(
		seen.borrow()[0].attribute,
		"nixosConfigurations.\"thishost\".config.system.build.toplevel"
	);
}

#[test]
fn usage_errors_exit_two() {
	for args in [
		&[][..],
		&["bogus"][..],
		&["switch", "--flake", "a", "--file", "b"][..],
		&["switch", "--attr", "x"][..],
		&["switch", "--file", "x", "--update"][..],
		&["switch", "--out-link", "r"][..],
		&["rollback", "--to", "x"][..],
		&["rollback", "--flake", "a"][..],
		&["build", "extra"][..],
	] {
		let (mut out, mut err) = (Vec::new(), Vec::new());
		let code = rebuild::run(
			args.iter().map(|arg| (*arg).to_owned()).collect(),
			&mut out,
			&mut err,
		);
		assert_eq!(code, ExitCode::from(2), "{args:?}");
		assert!(
			String::from_utf8(err)
				.unwrap()
				.contains("usage: noxi system"),
			"{args:?}"
		);
	}
}
