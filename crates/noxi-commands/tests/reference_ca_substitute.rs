//! Content-addressed derivation outputs copied to and substituted from
//! `file://` binary caches, compared with the pinned CppNix 2.35.2.
//!
//! The reference builds a floating content-addressed derivation and a
//! floating parent that uses it in a diverted store. Both engines' `nix copy
//! --to file://` then upload the outputs, which must give byte-identical
//! caches, including the `build-trace-v2/<drv>/<output>.doi` realisations.
//! For the parent these are the input's realisation and the resolved
//! parent's, as `toRealisedPaths` collects them, not one for the unresolved
//! parent. Both engines' `nix-store --realise` then substitute the parent
//! from that cache into fresh stores holding only the derivations: neither
//! may build, and the stores must end up with the same valid paths, path
//! metadata and build trace.
//!
//! The reference executables and the busybox come from `just reference`,
//! or from `NOXI_REFERENCE_BIN` and `NOXI_TEST_BUSYBOX`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

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

fn reference(program: &str) -> PathBuf {
	from_env_or_target("NOXI_REFERENCE_BIN", "nix-reference/bin", "nix-store").join(program)
}

fn busybox() -> String {
	let path = from_env_or_target("NOXI_TEST_BUSYBOX", "test-busybox", "bin/busybox");
	fs::canonicalize(path)
		.unwrap()
		.to_string_lossy()
		.into_owned()
}

struct Env {
	dir: tempfile::TempDir,
}

impl Env {
	fn new() -> Self {
		let dir = tempfile::tempdir().unwrap();
		for sub in ["conf", "home"] {
			fs::create_dir(dir.path().join(sub)).unwrap();
		}
		Env { dir }
	}

	fn path(&self, name: &str) -> PathBuf {
		self.dir.path().join(name)
	}

	fn store(&self, name: &str) -> String {
		self.path(name).display().to_string()
	}

	/// Run `program` with a controlled environment, `ca-derivations`
	/// enabled and the extra configuration lines in `config`.
	fn run(&self, program: &Path, args: &[&str], config: &str) -> Output {
		Command::new(program)
			.args(args)
			.env_clear()
			.env("HOME", self.path("home"))
			.env("NIX_CONF_DIR", self.path("conf"))
			.env("NIX_USER_CONF_FILES", "")
			.env(
				"NIX_CONFIG",
				format!(
					"experimental-features = nix-command ca-derivations\nbuild-users-group =\nsubstituters =\nsandbox = true\nsandbox-fallback = false\nsandbox-paths =\ncores = 1\n{config}"
				),
			)
			.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin")
			.output()
			.unwrap()
	}

	fn ok(&self, program: &Path, args: &[&str], config: &str) -> String {
		let output = self.run(program, args, config);
		assert!(
			output.status.success(),
			"{} {args:?} failed:\n{}",
			program.display(),
			String::from_utf8_lossy(&output.stderr)
		);
		String::from_utf8(output.stdout).unwrap()
	}

	/// `nix path-info --json` of every valid path in `store`, without the
	/// registration times.
	fn path_infos(&self, store: &str) -> Value {
		let mut json: Value = serde_json::from_str(&self.ok(
			&reference("nix"),
			&["path-info", "--json", "--all", "--store", store],
			"",
		))
		.unwrap();
		for info in json.as_object_mut().unwrap().values_mut() {
			info.as_object_mut().unwrap().remove("registrationTime");
		}
		json
	}

	fn verify(&self, store: &str) {
		let output = self.run(
			&reference("nix-store"),
			&["--store", store, "--verify", "--check-contents"],
			"",
		);
		let stderr = String::from_utf8_lossy(&output.stderr);
		assert!(output.status.success(), "verifying {store}:\n{stderr}");
		assert!(!stderr.contains("error"), "verifying {store}:\n{stderr}");
	}
}

/// The `BuildTraceV3` rows of the store at `root`.
fn build_trace(root: &Path) -> Vec<(String, String, String, String)> {
	let db = rusqlite::Connection::open_with_flags(
		root.join("nix/var/nix/db/db.sqlite"),
		rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
	)
	.unwrap();
	db.prepare("select drvPath, outputName, outputPath, ifnull(signatures, '') from BuildTraceV3 order by drvPath, outputName")
		.unwrap()
		.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
		.unwrap()
		.collect::<Result<_, _>>()
		.unwrap()
}

/// Every file and directory under `dir`, with file contents.
fn tree(dir: &Path) -> Vec<(String, Option<Vec<u8>>)> {
	let mut entries = Vec::new();
	let mut pending = vec![dir.to_path_buf()];
	while let Some(current) = pending.pop() {
		for entry in fs::read_dir(&current).unwrap() {
			let path = entry.unwrap().path();
			let name = path.strip_prefix(dir).unwrap().display().to_string();
			if path.is_dir() {
				entries.push((name, None));
				pending.push(path);
			} else {
				entries.push((name, Some(fs::read(&path).unwrap())));
			}
		}
	}
	entries.sort();
	entries
}

const EXPR: &str = r#"let
	bb = builtins.storePath @busybox@;
	input = derivation {
		name = "ca-input"; system = "x86_64-linux"; builder = "${bb}/bin/sh";
		args = [ "-c" "echo input > $out" ];
		__contentAddressed = true; outputHashAlgo = "sha256"; outputHashMode = "recursive";
	};
in {
	inherit input;
	parent = derivation {
		name = "ca-parent"; system = "x86_64-linux"; builder = "${bb}/bin/sh";
		args = [ "-c" "echo $input > $out" ]; inherit input;
		__contentAddressed = true; outputHashAlgo = "sha256"; outputHashMode = "recursive";
	};
}"#;

#[test]
fn ca_outputs_copy_to_and_substitute_from_file_caches_like_the_reference() {
	let env = Env::new();
	let busybox = busybox();
	let expr = env.path("expr.nix");
	fs::write(&expr, EXPR.replace("@busybox@", &busybox)).unwrap();
	// The derivations refer to the busybox, so every store needs it.
	let copy_busybox = |store: &str| {
		env.ok(
			&reference("nix"),
			&[
				"copy",
				"--no-check-sigs",
				"--to",
				&format!("local?root={store}"),
				&busybox,
			],
			"",
		);
	};
	let src = env.store("src");
	copy_busybox(&src);
	let instantiate = |store: &str, attr: &str| {
		env.ok(
			&reference("nix-instantiate"),
			&["--store", store, expr.to_str().unwrap(), "-A", attr],
			"",
		)
		.trim()
		.to_owned()
	};
	let input = instantiate(&src, "input");
	let parent = instantiate(&src, "parent");
	env.ok(
		&reference("nix-store"),
		&["--store", &src, "--realise", &parent],
		"",
	);

	let cache = env.path("cache");
	let uri = format!("file://{}?compression=none", cache.display());
	let noxi_nix = PathBuf::from(env!("CARGO_BIN_EXE_nix"));
	for installable in [
		format!("{input}^out"),
		format!("{parent}^out"),
		format!("{parent}^*"),
	] {
		let mut results = Vec::new();
		for bin in [reference("nix"), noxi_nix.clone()] {
			let _ = fs::remove_dir_all(&cache);
			let output = env.run(
				&bin,
				&[
					"copy",
					"--store",
					&format!("local?root={src}"),
					"--to",
					&uri,
					&installable,
				],
				"",
			);
			// The reference uploads in parallel, so the order of its
			// per-path messages varies.
			let mut lines: Vec<String> = String::from_utf8_lossy(&output.stderr)
				.lines()
				.map(str::to_owned)
				.collect();
			lines.sort();
			results.push((output.status.code(), output.stdout, lines, tree(&cache)));
		}
		assert_eq!(
			results[1].0, results[0].0,
			"{installable}: {:?}",
			results[1].2
		);
		assert_eq!(results[1].1, results[0].1, "{installable}");
		assert_eq!(results[1].2, results[0].2, "{installable}");
		assert!(
			results[1].3 == results[0].3,
			"{installable}: cache files differ:\nreference: {:?}\nnoxi: {:?}",
			results[0]
				.3
				.iter()
				.map(|(name, _)| name)
				.collect::<Vec<_>>(),
			results[1]
				.3
				.iter()
				.map(|(name, _)| name)
				.collect::<Vec<_>>()
		);
		let doi: Vec<&String> = results[0]
			.3
			.iter()
			.filter(|(name, contents)| contents.is_some() && name.ends_with(".doi"))
			.map(|(name, _)| name)
			.collect();
		assert_eq!(
			doi.len(),
			2 - usize::from(installable.starts_with(&input)),
			"{doi:?}"
		);
	}

	// The cache now holds the parent's closure, written by noxi. Substitute
	// the parent into fresh stores that hold only its derivations.
	let substituters = format!("substituters = file://{}?trusted=1\n", cache.display());
	let mut stores = Vec::new();
	for (name, bin) in [
		("dest-reference", reference("nix-store")),
		("dest-noxi", PathBuf::from(env!("CARGO_BIN_EXE_nix-store"))),
	] {
		let store = env.store(name);
		copy_busybox(&store);
		assert_eq!(instantiate(&store, "parent"), parent);
		let output = env.run(
			&bin,
			&["--store", &store, "--realise", &parent],
			&substituters,
		);
		let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
		assert!(output.status.success(), "{name}:\n{stderr}");
		assert!(!stderr.contains("building"), "{name}:\n{stderr}");
		stores.push((
			String::from_utf8(output.stdout).unwrap(),
			env.path_infos(&store),
			build_trace(&env.path(name)),
			stderr.replace(&store, "STORE"),
		));
		env.verify(&store);
	}
	assert_eq!(stores[1].0, stores[0].0);
	assert_eq!(stores[1].1, stores[0].1);
	assert_eq!(stores[1].2, stores[0].2);
	assert_eq!(stores[1].3, stores[0].3);
	// The input's realisation and the resolved parent's are substituted;
	// the unresolved parent gets none.
	let drvs: Vec<&str> = stores[0].2.iter().map(|row| row.0.as_str()).collect();
	assert_eq!(drvs.len(), 2, "{drvs:?}");
	assert!(drvs.iter().all(|drv| !parent.ends_with(drv)), "{drvs:?}");
	assert!(drvs.iter().any(|drv| input.ends_with(drv)), "{drvs:?}");
}

/// A local store as the substituter, `local?root=SRC&trusted=1`: CppNix
/// answers the realisation queries from the source store's build trace, so
/// the parent is substituted without building.
#[test]
fn ca_outputs_substitute_from_a_local_store_like_the_reference() {
	let env = Env::new();
	let busybox = busybox();
	let expr = env.path("expr.nix");
	fs::write(&expr, EXPR.replace("@busybox@", &busybox)).unwrap();
	let copy_busybox = |store: &str| {
		env.ok(
			&reference("nix"),
			&[
				"copy",
				"--no-check-sigs",
				"--to",
				&format!("local?root={store}"),
				&busybox,
			],
			"",
		);
	};
	let instantiate = |store: &str, attr: &str| {
		env.ok(
			&reference("nix-instantiate"),
			&["--store", store, expr.to_str().unwrap(), "-A", attr],
			"",
		)
		.trim()
		.to_owned()
	};
	let src = env.store("src");
	copy_busybox(&src);
	let input = instantiate(&src, "input");
	let parent = instantiate(&src, "parent");
	env.ok(
		&reference("nix-store"),
		&["--store", &src, "--realise", &parent],
		"",
	);
	let source_trace = build_trace(&env.path("src"));
	assert_eq!(source_trace.len(), 2, "{source_trace:?}");

	let substituters = format!("substituters = local?root={src}&trusted=1\n");
	let mut stores = Vec::new();
	for (name, bin) in [
		("dest-reference", reference("nix-store")),
		("dest-noxi", PathBuf::from(env!("CARGO_BIN_EXE_nix-store"))),
	] {
		let store = env.store(name);
		copy_busybox(&store);
		assert_eq!(instantiate(&store, "parent"), parent);
		// The destination is named `local?root=`, which the copy messages
		// show as `local://`.
		let output = env.run(
			&bin,
			&[
				"--store",
				&format!("local?root={store}"),
				"--realise",
				&parent,
			],
			&substituters,
		);
		let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
		assert!(output.status.success(), "{name}:\n{stderr}");
		assert!(!stderr.contains("building"), "{name}:\n{stderr}");
		stores.push((
			String::from_utf8(output.stdout).unwrap(),
			env.path_infos(&store),
			build_trace(&env.path(name)),
			stderr.replace(&store, "STORE"),
		));
		env.verify(&store);
	}
	assert_eq!(stores[1].0, stores[0].0);
	assert_eq!(stores[1].1, stores[0].1);
	assert_eq!(stores[1].2, stores[0].2);
	assert_eq!(stores[1].3, stores[0].3);
	// The source store's realisations are copied as they are.
	assert_eq!(stores[0].2, source_trace);
	assert!(
		stores[0].2.iter().any(|row| input.ends_with(&row.0)),
		"{:?}",
		stores[0].2
	);
}
