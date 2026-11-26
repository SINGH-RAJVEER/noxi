//! Garbage collection checked against the pinned CppNix 2.35.2 reference.
//!
//! The reference builds a small graph in a temporary store: built outputs
//! that refer to each other and to the test busybox, an unbuilt
//! derivation, added sources, and roots of each kind (a direct link, an
//! indirect `--add-root` link, and a `nix-env` profile). The store is then
//! copied twice, and the reference collects one copy while noxi collects
//! the other.
//!
//! The busybox is rooted directly because sandboxed builds elsewhere may map
//! it, which would make it a runtime root for one collector and not the
//! other.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nix_compat::store_path::StorePath;
use noxi_store::gc::{GcAction, GcOptions};
use noxi_store::{LocalStore, LocalStoreConfig};

fn from_env_or_target(var: &str, target: &str, probe: &str) -> PathBuf {
	if let Some(path) = std::env::var_os(var) {
		return PathBuf::from(path);
	}
	let path = Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../target")
		.join(target);
	assert!(
		path.join(probe).exists(),
		"{target} is missing; run `just reference` or set {var}"
	);
	path
}

struct Env {
	dir: noxi_test_support::TempDir,
	busybox: String,
}

impl Env {
	fn command(&self, program: &str, args: &[&str]) -> Command {
		let bin = from_env_or_target("NOXI_REFERENCE_BIN", "nix-reference/bin", "nix-store");
		let mut command = Command::new(bin.join(program));
		command
			.args(args)
			.env_clear()
			.env("HOME", self.dir.path().join("home"))
			.env("NIX_CONF_DIR", self.dir.path().join("conf"))
			.env(
				"NIX_CONFIG",
				"experimental-features = nix-command\nbuild-users-group =\nsubstituters =\nsandbox = true\nsandbox-fallback = false\nsandbox-paths =\ncores = 1\n",
			)
			.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin");
		command
	}

	fn reference(&self, program: &str, args: &[&str]) -> Output {
		self.command(program, args).output().unwrap()
	}

	fn reference_ok(&self, program: &str, args: &[&str]) -> String {
		let output = self.reference(program, args);
		assert!(
			output.status.success(),
			"{program} {args:?} failed:\n{}",
			String::from_utf8_lossy(&output.stderr)
		);
		String::from_utf8(output.stdout).unwrap()
	}

	fn root(&self, name: &str) -> PathBuf {
		self.dir.path().join(name)
	}

	fn store_arg(&self, name: &str) -> String {
		self.root(name).to_string_lossy().into_owned()
	}

	/// Build the graph in store "src" and return the paths of interest.
	fn new() -> (Self, Graph) {
		let dir = noxi_test_support::tempdir().unwrap();
		for sub in ["conf", "home", "links"] {
			fs::create_dir(dir.path().join(sub)).unwrap();
		}
		let busybox = fs::canonicalize(from_env_or_target(
			"NOXI_TEST_BUSYBOX",
			"test-busybox",
			"bin/busybox",
		))
		.unwrap()
		.to_string_lossy()
		.into_owned();
		let env = Env { dir, busybox };
		let src = env.store_arg("src");
		env.reference_ok(
			"nix",
			&[
				"copy",
				"--no-check-sigs",
				"--to",
				&format!("local?root={src}"),
				&env.busybox,
			],
		);
		let expr = env.dir.path().join("graph.nix");
		fs::write(
			&expr,
			format!(
				r#"let
					bb = builtins.storePath {busybox};
					mk = name: script: derivation {{
						inherit name;
						system = "x86_64-linux";
						builder = "${{bb}}/bin/sh";
						args = [ "-c" script ];
					}};
					a = mk "a" "echo ${{bb}} > $out";
					b = mk "b" "echo b > $out";
					c = mk "c" "echo ${{a}} ${{builtins.toFile "c-source" "c"}} > $out";
					unbuilt = mk "unbuilt" "echo ${{builtins.toFile "unbuilt-source" "u"}} > $out";
				in {{ inherit a b c unbuilt; }}"#,
				busybox = env.busybox
			),
		)
		.unwrap();
		let expr = expr.to_str().unwrap();
		let drv = |attr: &str| {
			StorePath::from_absolute_path(
				env.reference_ok("nix-instantiate", &["--store", &src, expr, "-A", attr])
					.trim()
					.as_bytes(),
			)
			.unwrap()
		};
		let graph = Graph {
			a: drv("a"),
			b: drv("b"),
			c: drv("c"),
			unbuilt: drv("unbuilt"),
		};
		// c through an indirect root, b through a profile, a's output
		// unrooted except through c.
		let link = env.root("links/c");
		env.reference_ok(
			"nix-store",
			&[
				"--store",
				&src,
				"--realise",
				"--add-root",
				link.to_str().unwrap(),
				&graph.c.to_absolute_path(),
			],
		);
		let b_out = env
			.reference_ok(
				"nix-store",
				&["--store", &src, "--realise", &graph.b.to_absolute_path()],
			)
			.trim()
			.to_owned();
		env.reference_ok(
			"nix-env",
			&[
				"--store",
				&src,
				"-p",
				&format!("{src}/nix/var/nix/profiles/test"),
				"--set",
				&b_out,
			],
		);
		// Unrooted garbage: an added file and a built output.
		let extra = env.dir.path().join("extra-file");
		fs::write(&extra, "extra").unwrap();
		env.reference_ok(
			"nix-store",
			&["--store", &src, "--add", extra.to_str().unwrap()],
		);
		let lonely = env.dir.path().join("lonely.nix");
		fs::write(
			&lonely,
			format!(
				r#"derivation {{ name = "lonely"; system = "x86_64-linux"; builder = "${{builtins.storePath {}}}/bin/sh"; args = [ "-c" "echo lonely > $out" ]; }}"#,
				env.busybox
			),
		)
		.unwrap();
		env.reference_ok(
			"nix-build",
			&["--store", &src, "--no-out-link", lonely.to_str().unwrap()],
		);
		std::os::unix::fs::symlink(&env.busybox, env.root("src/nix/var/nix/gcroots/busybox"))
			.unwrap();
		(env, graph)
	}

	/// Copy the source store to `name`.
	fn copy(&self, name: &str) {
		let status = Command::new("cp")
			.args(["-a", &self.store_arg("src"), &self.store_arg(name)])
			.status()
			.unwrap();
		assert!(status.success());
		// The profile links are relative, but the copy's own gcroots link
		// into the copy's profiles directory by absolute path.
		let profiles = self.root(name).join("nix/var/nix/gcroots/profiles");
		if fs::symlink_metadata(&profiles).is_ok() {
			fs::remove_file(&profiles).unwrap();
			std::os::unix::fs::symlink(self.root(name).join("nix/var/nix/profiles"), &profiles)
				.unwrap();
		}
	}

	fn open(&self, name: &str) -> LocalStore {
		let mut config = LocalStoreConfig::with_root(&self.root(name));
		config.require_sigs = false;
		LocalStore::open(config).unwrap()
	}

	fn normalise(&self, name: &str, s: &str) -> String {
		s.replace(&self.store_arg(name), "ROOT")
	}

	fn valid_paths(&self, name: &str) -> BTreeSet<String> {
		self.reference_ok(
			"nix",
			&["path-info", "--all", "--store", &self.store_arg(name)],
		)
		.lines()
		.map(str::to_owned)
		.collect()
	}

	fn verify(&self, name: &str) {
		let output = self.reference(
			"nix-store",
			&[
				"--store",
				&self.store_arg(name),
				"--verify",
				"--check-contents",
			],
		);
		let stderr = String::from_utf8_lossy(&output.stderr);
		assert!(output.status.success(), "verify failed:\n{stderr}");
		for bad in ["error", "warning", "modified", "disappeared"] {
			assert!(!stderr.contains(bad), "verify reported problems:\n{stderr}");
		}
	}
}

#[allow(dead_code)]
struct Graph {
	a: StorePath,
	b: StorePath,
	c: StorePath,
	unbuilt: StorePath,
}

fn deleted_lines(stderr: &str) -> BTreeSet<String> {
	stderr
		.lines()
		.filter_map(|l| l.strip_prefix("deleting '"))
		.filter_map(|l| l.strip_suffix('\''))
		.map(str::to_owned)
		.collect()
}

#[test]
fn roots_match_the_reference() {
	let (env, _) = Env::new();
	env.copy("reference");
	env.copy("noxi");
	let reference: BTreeSet<String> = env
		.reference_ok(
			"nix-store",
			&[
				"--store",
				&env.store_arg("reference"),
				"--gc",
				"--print-roots",
			],
		)
		.lines()
		.map(|l| env.normalise("reference", l))
		.collect();
	let store = env.open("noxi");
	let mut noxi = BTreeSet::new();
	for (path, links) in store.find_roots(false, &mut |_| {}).unwrap() {
		for link in links {
			noxi.insert(env.normalise(
				"noxi",
				&format!("\"{link}\" -> {}", path.to_absolute_path()),
			));
		}
	}
	assert_eq!(noxi, reference);
	assert!(
		reference.iter().any(|l| l.contains("links/c")),
		"{reference:?}"
	);
	assert!(
		reference.iter().any(|l| l.contains("profiles/test")),
		"{reference:?}"
	);
}

#[test]
fn live_and_dead_paths_match_the_reference() {
	let (env, _) = Env::new();
	env.copy("reference");
	env.copy("noxi");
	let store = env.open("noxi");
	for (flag, action) in [
		("--print-live", GcAction::ReturnLive),
		("--print-dead", GcAction::ReturnDead),
	] {
		let reference: BTreeSet<String> = env
			.reference_ok(
				"nix-store",
				&["--store", &env.store_arg("reference"), "--gc", flag],
			)
			.lines()
			.map(str::to_owned)
			.collect();
		let noxi = store
			.collect_garbage(&GcOptions::new(action), &mut |_| {})
			.unwrap()
			.paths;
		assert_eq!(noxi, reference, "{flag}");
	}
}

#[test]
fn collection_deletes_what_the_reference_deletes() {
	let (env, _) = Env::new();
	env.copy("reference");
	env.copy("noxi");
	let output = env.reference(
		"nix-store",
		&["--store", &env.store_arg("reference"), "--gc"],
	);
	assert!(output.status.success());
	let stderr = String::from_utf8_lossy(&output.stderr);
	let reference_deleted = deleted_lines(&stderr);
	assert!(!reference_deleted.is_empty());

	let store = env.open("noxi");
	let mut log = Vec::new();
	let results = store
		.collect_garbage(&GcOptions::new(GcAction::DeleteDead), &mut |l| {
			log.push(l.to_owned())
		})
		.unwrap();
	drop(store);
	assert_eq!(results.paths, reference_deleted);
	assert_eq!(deleted_lines(&log.join("\n")), reference_deleted);
	assert_eq!(env.valid_paths("noxi"), env.valid_paths("reference"));
	let freed = String::from_utf8_lossy(&output.stdout);
	assert!(
		freed.contains(&format!("{} store paths deleted", reference_deleted.len())),
		"{freed}"
	);
	env.verify("noxi");
	env.verify("reference");
}

#[test]
fn deleting_specific_paths_matches_the_reference() {
	let (env, graph) = Env::new();
	env.copy("reference");
	env.copy("noxi");
	let store = env.open("noxi");

	// A live path: c's output is rooted.
	let c_out = env
		.reference_ok(
			"nix-store",
			&[
				"--store",
				&env.store_arg("src"),
				"-q",
				"--outputs",
				&graph.c.to_absolute_path(),
			],
		)
		.trim()
		.to_owned();
	let output = env.reference(
		"nix-store",
		&["--store", &env.store_arg("reference"), "--delete", &c_out],
	);
	assert!(!output.status.success());
	let stderr = String::from_utf8_lossy(&output.stderr);
	let mut options = GcOptions::new(GcAction::DeleteSpecific);
	options.paths = Some(BTreeSet::from([StorePath::from_absolute_path(
		c_out.as_bytes(),
	)
	.unwrap()]));
	let error = store
		.collect_garbage(&options, &mut |_| {})
		.unwrap_err()
		.to_string();
	assert!(
		stderr.contains(&error),
		"noxi: {error}\nreference: {stderr}"
	);

	// A dead path: the unbuilt derivation.
	let output = env.reference(
		"nix-store",
		&[
			"--store",
			&env.store_arg("reference"),
			"--delete",
			&graph.unbuilt.to_absolute_path(),
		],
	);
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	options.paths = Some(BTreeSet::from([graph.unbuilt.clone()]));
	let results = store.collect_garbage(&options, &mut |_| {}).unwrap();
	assert_eq!(
		results.paths,
		deleted_lines(&String::from_utf8_lossy(&output.stderr))
	);
	drop(store);
	assert_eq!(env.valid_paths("noxi"), env.valid_paths("reference"));
	env.verify("noxi");
}

/// A temporary root added by the reference while noxi collects reaches
/// noxi's GC socket and keeps the path.
#[test]
fn collection_honours_roots_from_the_gc_socket() {
	let (env, _) = Env::new();
	env.copy("noxi");
	// The added extra file is garbage; keep it alive through a client that
	// connects while the collector holds the lock.
	let store = env.open("noxi");
	let dead = store
		.collect_garbage(&GcOptions::new(GcAction::ReturnDead), &mut |_| {})
		.unwrap()
		.paths;
	let target = dead
		.iter()
		.find(|p| p.ends_with("-extra-file"))
		.unwrap()
		.clone();
	let target = StorePath::from_absolute_path(target.as_bytes()).unwrap();

	// A second handle in another thread adds the root once collection has
	// started, through the socket.
	let root = env.root("noxi");
	let mut options = GcOptions::new(GcAction::DeleteDead);
	options.paths = Some(
		dead.iter()
			.map(|p| StorePath::from_absolute_path(p.as_bytes()).unwrap())
			.collect(),
	);
	let gc_lock = root.join("nix/var/nix/gc.lock");
	let handle = std::thread::spawn({
		let target = target.clone();
		let root = root.clone();
		move || {
			// Wait until the collector holds the lock exclusively.
			let file = fs::File::open(&gc_lock).unwrap();
			while noxi_store::lock::lock_file(&file, noxi_store::lock::LockType::Read, false)
				.unwrap()
			{
				noxi_store::lock::lock_file(&file, noxi_store::lock::LockType::None, false)
					.unwrap();
				std::thread::sleep(std::time::Duration::from_millis(1));
			}
			let mut config = LocalStoreConfig::with_root(&root);
			config.require_sigs = false;
			let other = LocalStore::open(config).unwrap();
			other.add_temp_root(&target).unwrap();
			other
		}
	});
	// Hold up the collector until the root has been sent: the first log
	// line comes after the lock is taken.
	let mut waited = false;
	let results = store
		.collect_garbage(&options, &mut |line| {
			if !waited && line == "finding garbage collector roots..." {
				waited = true;
				while !handle.is_finished() {
					std::thread::sleep(std::time::Duration::from_millis(1));
				}
			}
		})
		.unwrap();
	let _other = handle.join().unwrap();
	assert!(
		!results.paths.contains(&target.to_absolute_path()),
		"{results:?}"
	);
	assert!(store.is_valid_path(&target).unwrap());
}

/// A derivation being built, the derivations waiting for it, their input
/// sources and the outputs of built inputs are temporary roots of the
/// building process, which neither collector may delete while the build
/// runs; nothing else roots them. The builder is held either in the input
/// derivation or in the derivation that uses it, with one job (the
/// sequential builder) and two (the scheduler).
#[test]
fn collection_spares_a_running_build_and_its_inputs() {
	let (env, _) = Env::new();
	for held in ["top", "dep"] {
		for jobs in [1, 2] {
			for collector in ["noxi", "reference"] {
				collect_during_build(&env, held, jobs, collector);
			}
		}
	}
}

fn collect_during_build(env: &Env, held: &str, jobs: u64, collector: &str) {
	let case = format!("{held}-{jobs}-{collector}");
	let name = format!("build-{case}");
	env.copy(&name);
	let sync = env.root(&format!("sync-{case}"));
	fs::create_dir(&sync).unwrap();
	let wait = format!(
		"; ${{bb}}/bin/touch {sync}/started; while [ ! -e {sync}/go ]; do ${{bb}}/bin/sleep 0.05; done",
		sync = sync.display()
	);
	let (wait_dep, wait_top) = if held == "dep" {
		(wait.as_str(), "")
	} else {
		("", wait.as_str())
	};
	let expr = env.dir.path().join(format!("{name}.nix"));
	fs::write(
		&expr,
		format!(
			r#"let
				bb = builtins.storePath {busybox};
				mk = name: script: derivation {{
					inherit name;
					system = "x86_64-linux";
					builder = "${{bb}}/bin/sh";
					args = [ "-c" script ];
				}};
				dep = mk "running-dep" "${{bb}}/bin/cat ${{builtins.toFile "dep-source" "dep"}} > $out{wait_dep}";
			in mk "running" "${{bb}}/bin/cat ${{dep}} ${{builtins.toFile "running-source" "top"}} > $out{wait_top}""#,
			busybox = env.busybox,
		),
	)
	.unwrap();
	let store_arg = env.store_arg(&name);
	let drv = env
		.reference_ok(
			"nix-instantiate",
			&["--store", &store_arg, expr.to_str().unwrap()],
		)
		.trim()
		.to_owned();
	let drv = StorePath::from_absolute_path(drv.as_bytes()).unwrap();
	let store = env.open(&name);
	let references = |path: &StorePath| -> Vec<StorePath> {
		store
			.query_path_info(path)
			.unwrap()
			.unwrap()
			.references
			.into_iter()
			.collect()
	};
	// The busybox, the source and the input derivation.
	let inputs = references(&drv);
	assert_eq!(inputs.len(), 3, "{inputs:?}");
	let dep_drv = inputs
		.iter()
		.find(|p| p.name().ends_with(".drv"))
		.unwrap()
		.clone();
	let mut alive = vec![drv.clone()];
	alive.extend(inputs.iter().cloned());
	alive.extend(references(&dep_drv));

	let root = env.root(&name);
	let builder = std::thread::spawn({
		let drv = drv.clone();
		let sync = sync.clone();
		move || {
			let mut config = LocalStoreConfig::with_root(&root);
			config.require_sigs = false;
			let store = LocalStore::open(config).unwrap();
			let settings = noxi_build::settings::BuildSettings {
				// A diverted store builds in the sandbox; the builder
				// signals through a bound directory.
				sandbox_paths: vec![noxi_build::settings::SandboxPath::parse(
					&sync.display().to_string(),
				)],
				build_cores: 1,
				max_build_jobs: jobs,
				..noxi_build::settings::BuildSettings::default()
			};
			noxi_build::local::LocalBuilder::new(&store, &settings)
				.realise(&drv, &mut |_| {})
				.map_err(|e| e.to_string())
		}
	});
	let started = std::time::Instant::now();
	while !sync.join("started").exists() {
		assert!(!builder.is_finished(), "{case}: {:?}", builder.join());
		assert!(
			started.elapsed().as_secs() < 60,
			"{case}: the build did not start"
		);
		std::thread::sleep(std::time::Duration::from_millis(20));
	}
	if held == "top" {
		// The input was built before the held build started.
		alive.extend(store.query_derivation_outputs(&dep_drv).unwrap().into_values());
	}
	match collector {
		"noxi" => {
			store
				.collect_garbage(&GcOptions::new(GcAction::DeleteDead), &mut |_| {})
				.unwrap();
		}
		_ => {
			env.reference_ok("nix-store", &["--store", &store_arg, "--gc"]);
		}
	}
	for path in &alive {
		assert!(
			store.is_valid_path(path).unwrap() && store.real_path(path).exists(),
			"{case}: {collector} collected {path} during the build"
		);
	}
	fs::write(sync.join("go"), "").unwrap();
	let outputs = builder.join().unwrap().unwrap();
	assert_eq!(
		fs::read_to_string(store.real_path(&outputs["out"])).unwrap(),
		"deptop",
		"{case}"
	);
	env.verify(&name);
}

/// A temporary root that a live noxi process recorded before the
/// collection started keeps an otherwise dead path, and what it refers to,
/// from either collector.
#[test]
fn collection_honours_temporary_roots_of_live_processes() {
	let (env, graph) = Env::new();
	for collector in ["noxi", "reference"] {
		let name = format!("temp-{collector}");
		env.copy(&name);
		let store = env.open(&name);
		let dead = store
			.collect_garbage(&GcOptions::new(GcAction::ReturnDead), &mut |_| {})
			.unwrap()
			.paths;
		// The unbuilt derivation refers to a source only it keeps alive.
		let target = graph.unbuilt.clone();
		let source = store
			.query_path_info(&target)
			.unwrap()
			.unwrap()
			.references
			.into_iter()
			.find(|p| p.name() == "unbuilt-source")
			.unwrap();
		for path in [&target, &source] {
			assert!(dead.contains(&path.to_absolute_path()), "{dead:?}");
		}
		// Another handle, as another client would, records the root and
		// stays open.
		let holder = env.open(&name);
		holder.add_temp_root(&target).unwrap();
		match collector {
			"noxi" => {
				store
					.collect_garbage(&GcOptions::new(GcAction::DeleteDead), &mut |_| {})
					.unwrap();
			}
			_ => {
				env.reference_ok("nix-store", &["--store", &env.store_arg(&name), "--gc"]);
			}
		}
		for path in [&target, &source] {
			assert!(
				store.is_valid_path(path).unwrap() && store.real_path(path).exists(),
				"{collector} collected {path} although it is a temporary root"
			);
		}
		// Other dead paths went.
		assert!(
			dead.iter()
				.filter(|p| p.ends_with("-extra-file"))
				.all(|p| !Path::new(&env.root(&name).join(&p[1..])).exists()
					&& !store
						.is_valid_path(&StorePath::from_absolute_path(p.as_bytes()).unwrap())
						.unwrap()),
			"{collector}: {dead:?}"
		);
		drop(holder);
		env.verify(&name);
	}
}
