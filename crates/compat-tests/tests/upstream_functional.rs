//! Runs CppNix 2.35.2's functional shell tests against noxi's executables.
//!
//! Each test script registered in a `meson.build` under
//! `tests/nix/tests/functional` runs the way upstream's meson harness runs
//! it: `bash -x -e -u -o pipefail SCRIPT` in the script's directory, with
//! `_NIX_TEST_SOURCE_DIR`, `_NIX_TEST_BUILD_DIR`, `TEST_SUITE_NAME`,
//! `TEST_NAME`, an empty `NIX_REMOTE` and upstream's `PS4`. The build
//! directory holds `common/subst-vars.sh` and `config.nix`, rendered from
//! their `.in` templates as meson's `configure_file` does.
//!
//! Every script gets its own scratch root: a copy of the functional tree
//! (scripts may write next to themselves, and the snapshot must stay
//! unchanged), the build directory, and `TMPDIR`, under which
//! `common/vars.sh` places `TEST_ROOT` and the diverted store, state, log
//! and configuration directories. The environment is cleared; `PATH` holds
//! the engine's executables first, then only the package directories of
//! the host tools listed in [`TOOLS`]. Before any script runs, the harness
//! checks that no directory on that `PATH` other than the engine's own
//! holds a `nix*` executable, that `nix` resolves to the engine's binary,
//! and that the `coreutils` directory builders get as `PATH` holds no
//! `nix*` executable either, so a noxi result never comes from an installed
//! CppNix. Where unprivileged user namespaces are available, scripts run in
//! a private network namespace, so none can reach the network. Each script
//! runs in its own process group and is killed with it after
//! `NOXI_FUNCTIONAL_TIMEOUT` seconds (default 120).
//!
//! The noxi executables come from `NOXI_CANDIDATE_BIN`, a directory holding
//! the binaries built from `crates/noxi-commands` (default: this test's own
//! Cargo profile directory, so build them first with `cargo build -p
//! noxi-commands --bins`). The engine's bin directory mirrors the
//! replacement package's layout: each command CppNix installs is that
//! command's own binary when one was built, and otherwise a link to `nix`,
//! which dispatches on its name. The pinned reference runs the same scripts
//! only when `NOXI_REFERENCE_BIN` is set; otherwise its column is reported
//! as not checked.
//!
//! `expectations/upstream-functional.txt` records each script's observed
//! outcome under both engines and the class derived from them: `pass` when
//! both pass, `gap` when the reference passes and noxi does not, `anomaly`
//! when the reference itself fails or skips in this harness, and `unrun` for
//! the suites that need C++ consumers built against the reference
//! libraries. Every other file in the functional tree outside `lang/`
//! (owned by `upstream_lang`) is listed as `support`. A changed outcome, a
//! new file or a stale entry fails this test; `NOXI_COMPAT_BLESS=1` rewrites
//! the table. `NOXI_FUNCTIONAL_FILTER` (comma-separated substrings of the
//! script path) restricts the run, and `NOXI_FUNCTIONAL_JOBS` sets the
//! number of concurrent scripts (default 8). Logs and a JSON report are
//! written under the Cargo target temporary directory.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};

const SNAPSHOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/nix");
const FUNCTIONAL: &str = "tests/functional";
const EXPECTATIONS: &str = concat!(
	env!("CARGO_MANIFEST_DIR"),
	"/expectations/upstream-functional.txt"
);
const OUTPUT_DIR: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/upstream-functional");

const BLESS_VAR: &str = "NOXI_COMPAT_BLESS";
const FILTER_VAR: &str = "NOXI_FUNCTIONAL_FILTER";
const JOBS_VAR: &str = "NOXI_FUNCTIONAL_JOBS";
const TIMEOUT_VAR: &str = "NOXI_FUNCTIONAL_TIMEOUT";
const KEEP_VAR: &str = "NOXI_FUNCTIONAL_KEEP";
const CANDIDATE_VAR: &str = "NOXI_CANDIDATE_BIN";
const REFERENCE_VAR: &str = "NOXI_REFERENCE_BIN";

/// The executables CppNix 2.35.2 installs in its `bin` directory.
const COMMANDS: &[&str] = &[
	"nix",
	"nix-build",
	"nix-channel",
	"nix-collect-garbage",
	"nix-copy-closure",
	"nix-daemon",
	"nix-env",
	"nix-hash",
	"nix-instantiate",
	"nix-prefetch-url",
	"nix-shell",
	"nix-store",
];

/// Host tools the scripts call. Each one's package `bin` directory is put
/// on `PATH`; a missing tool is left out, and scripts that need it fail or
/// skip under both engines.
const TOOLS: &[&str] = &[
	"bash", "ls", "grep", "sed", "awk", "find", "xargs", "diff", "cmp", "patch", "tar", "gzip",
	"bzip2", "xz", "zstd", "git", "jq", "sqlite3", "curl", "unshare", "python3", "perl", "file",
	"which", "ps", "ssh", "openssl", "script", "mount", "umount", "man", "hg", "dot", "unzip",
	"lsof", "expect",
];

/// Suites upstream builds only with C++ consumers of the reference
/// libraries; this harness cannot build them for either engine.
const NOT_RUN: &[(&str, &str)] = &[
	(
		"libstoreconsumer",
		"needs the C++ libstore consumer built against the engine's libraries (TODO section 6)",
	),
	(
		"plugins",
		"needs C++ evaluator plugins built against the engine's libraries (TODO section 6)",
	),
];

#[derive(Clone, Debug)]
struct Script {
	/// Relative to `tests/nix`, for example `tests/functional/ca/build.sh`.
	path: String,
	suite: String,
	/// Directory relative to the functional tree, `""` for the top level.
	dir: String,
	file: String,
}

impl Script {
	fn name(&self) -> &str {
		self.file.strip_suffix(".sh").unwrap_or(&self.file)
	}

	fn not_run(&self) -> Option<&'static str> {
		NOT_RUN
			.iter()
			.find(|(suite, _)| *suite == self.suite)
			.map(|(_, reason)| *reason)
	}
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outcome {
	Pass,
	/// Exit status 77, upstream's skip.
	Skip,
	Fail,
	Timeout,
	NotRun,
}

impl Outcome {
	fn name(self) -> &'static str {
		match self {
			Outcome::Pass => "pass",
			Outcome::Skip => "skip",
			Outcome::Fail => "fail",
			Outcome::Timeout => "timeout",
			Outcome::NotRun => "not-run",
		}
	}

	fn parse(text: &str) -> Option<Self> {
		[
			Outcome::Pass,
			Outcome::Skip,
			Outcome::Fail,
			Outcome::Timeout,
			Outcome::NotRun,
		]
		.into_iter()
		.find(|outcome| outcome.name() == text)
	}
}

/// The fixture class a pair of outcomes gives.
fn classify(noxi: Outcome, reference: Outcome) -> &'static str {
	match (noxi, reference) {
		(Outcome::NotRun, _) | (_, Outcome::NotRun) => "unrun",
		(_, Outcome::Skip | Outcome::Fail | Outcome::Timeout) => "anomaly",
		(Outcome::Pass, Outcome::Pass) => "pass",
		(_, Outcome::Pass) => "gap",
	}
}

#[test]
fn skipped_reference_never_qualifies_a_script() {
	assert_eq!(classify(Outcome::Pass, Outcome::Skip), "anomaly");
	assert_eq!(classify(Outcome::Skip, Outcome::Pass), "gap");
	assert_eq!(classify(Outcome::Pass, Outcome::Fail), "anomaly");
	assert_eq!(classify(Outcome::Pass, Outcome::Pass), "pass");
	assert_eq!(classify(Outcome::NotRun, Outcome::Pass), "unrun");
}

fn executable_evidence(engine: &Engine) -> Json {
	let executables: BTreeMap<_, _> = COMMANDS
		.iter()
		.map(|command| {
			let path = fs::canonicalize(engine.bin.join(command)).unwrap();
			let bytes = fs::read(&path).unwrap();
			(
				*command,
				json!({
					"path": path,
					"sha256": format!("{:x}", Sha256::digest(bytes)),
				}),
			)
		})
		.collect();
	json!(executables)
}

#[derive(Clone, Debug)]
struct Run {
	outcome: Outcome,
	seconds: f64,
	detail: String,
	log: Option<PathBuf>,
}

struct Engine {
	name: &'static str,
	bin: PathBuf,
	path: OsString,
	bash: PathBuf,
	coreutils: PathBuf,
	busybox: String,
	/// Runs scripts in a private network namespace.
	unshare: Option<PathBuf>,
}

/// An expectation table line for a script.
#[derive(Clone, Debug)]
struct Expected {
	class: String,
	noxi: Outcome,
	reference: Option<Outcome>,
	note: String,
}

fn snapshot() -> PathBuf {
	fs::canonicalize(SNAPSHOT).expect("tests/nix snapshot")
}

fn functional_dir() -> PathBuf {
	snapshot().join(FUNCTIONAL)
}

/// Reads every `'name' : ...` suite and its `'tests' : [...]` list from the
/// functional tree's `meson.build` files.
fn scripts() -> Vec<Script> {
	let root = functional_dir();
	let mut files = vec![(String::new(), root.join("meson.build"))];
	for entry in fs::read_dir(&root).unwrap() {
		let entry = entry.unwrap();
		let meson = entry.path().join("meson.build");
		if entry.file_type().unwrap().is_dir() && meson.exists() {
			files.push((entry.file_name().to_string_lossy().into_owned(), meson));
		}
	}
	let mut scripts = Vec::new();
	for (dir, meson) in files {
		let text = fs::read_to_string(&meson).unwrap();
		let mut rest = text.as_str();
		while let Some(start) = rest.find("'name' : '") {
			rest = &rest[start + "'name' : '".len()..];
			let suite = &rest[..rest.find('\'').unwrap()];
			let tests = rest.find("'tests' : [").expect("suite without tests");
			rest = &rest[tests + "'tests' : [".len()..];
			let list = &rest[..rest.find(']').unwrap()];
			for item in list.split(',') {
				let item = item.trim().trim_matches('\'');
				if item.is_empty() {
					continue;
				}
				let relative = if dir.is_empty() {
					item.to_owned()
				} else {
					format!("{dir}/{item}")
				};
				assert!(
					root.join(&relative).is_file(),
					"{} lists missing {relative}",
					meson.display()
				);
				scripts.push(Script {
					path: format!("{FUNCTIONAL}/{relative}"),
					suite: suite.to_owned(),
					dir: dir.clone(),
					file: item.to_owned(),
				});
			}
		}
	}
	scripts.sort_by(|a, b| a.path.cmp(&b.path));
	scripts.dedup_by(|a, b| a.path == b.path);
	assert!(scripts.len() > 200, "too few scripts: {}", scripts.len());
	scripts
}

/// Every file in the functional tree outside `lang/`, walked like
/// `fixture_inventory` walks it: directory symlinks count as files.
fn functional_files() -> Vec<String> {
	fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
		for entry in fs::read_dir(dir).unwrap() {
			let entry = entry.unwrap();
			let path = entry.path();
			let relative = path
				.strip_prefix(root)
				.unwrap()
				.to_string_lossy()
				.into_owned();
			if relative == format!("{FUNCTIONAL}/lang") {
				continue;
			}
			if entry.file_type().unwrap().is_dir() {
				walk(&path, root, out);
			} else {
				out.push(relative);
			}
		}
	}
	let root = snapshot();
	let mut files = Vec::new();
	walk(&root.join(FUNCTIONAL), &root, &mut files);
	files.sort();
	files
}

fn find_on_path(name: &str) -> Option<PathBuf> {
	env::split_paths(&env::var_os("PATH")?)
		.map(|dir| dir.join(name))
		.find(|path| {
			path.metadata()
				.is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
		})
}

fn holds_nix(dir: &Path) -> Vec<String> {
	fs::read_dir(dir)
		.map(|entries| {
			entries
				.filter_map(Result::ok)
				.map(|entry| entry.file_name().to_string_lossy().into_owned())
				.filter(|name| name.starts_with("nix"))
				.collect()
		})
		.unwrap_or_default()
}

/// The `PATH` after the engine's own directory: each tool's package
/// directory, or for a tool whose directory also holds a `nix*` executable,
/// a private link to the tool alone.
fn tool_path(private: &Path) -> (Vec<PathBuf>, Vec<&'static str>) {
	let mut dirs: Vec<PathBuf> = Vec::new();
	let mut missing = Vec::new();
	fs::create_dir_all(private).unwrap();
	for tool in TOOLS {
		let Some(found) = find_on_path(tool) else {
			missing.push(*tool);
			continue;
		};
		let real = fs::canonicalize(&found).unwrap();
		let dir = real.parent().unwrap().to_path_buf();
		if holds_nix(&dir).is_empty() {
			if !dirs.contains(&dir) {
				dirs.push(dir);
			}
		} else {
			let link = private.join(tool);
			let _ = fs::remove_file(&link);
			std::os::unix::fs::symlink(&real, &link).unwrap();
		}
	}
	dirs.push(private.to_path_buf());
	(dirs, missing)
}

fn candidate_dir() -> PathBuf {
	if let Some(dir) = env::var_os(CANDIDATE_VAR) {
		return PathBuf::from(dir);
	}
	// The test binary lives below target/<profile>, next to the workspace's
	// binaries.
	let exe = env::current_exe().unwrap();
	exe.ancestors()
		.skip(1)
		.find(|dir| dir.join("nix").is_file())
		.unwrap_or_else(|| exe.parent().unwrap())
		.to_path_buf()
}

/// Lays out the candidate's commands like the replacement package does.
fn candidate_bin(into: &Path) -> PathBuf {
	let source = candidate_dir();
	let nix = source.join("nix");
	assert!(
		nix.is_file(),
		"{} is missing; build it with `cargo build -p noxi-commands --bins` or set {CANDIDATE_VAR}",
		nix.display()
	);
	let modified = nix.metadata().and_then(|meta| meta.modified()).ok();
	eprintln!(
		"noxi executables: {} (nix modified {:?})",
		source.display(),
		modified
	);
	fs::create_dir_all(into).unwrap();
	for command in COMMANDS {
		let own = source.join(command);
		let target = if own.is_file() { own } else { nix.clone() };
		let link = into.join(command);
		let _ = fs::remove_file(&link);
		// Keep this run on one artifact even if another task rebuilds the
		// source directory while scripts are running.
		let target = fs::canonicalize(target).unwrap();
		if fs::hard_link(&target, &link).is_err() {
			fs::copy(&target, &link).unwrap();
		}
	}
	into.to_path_buf()
}

fn busybox() -> String {
	let path = env::var_os("NOXI_TEST_BUSYBOX")
		.map(|dir| PathBuf::from(dir).join("bin/busybox"))
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-busybox/bin/busybox")
		});
	fs::canonicalize(path)
		.map(|path| path.to_string_lossy().into_owned())
		.unwrap_or_default()
}

fn network_isolation() -> Option<PathBuf> {
	let unshare = find_on_path("unshare")?;
	let works = Command::new(&unshare)
		.args(["--map-current-user", "--net", "true"])
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.status()
		.is_ok_and(|status| status.success());
	works.then_some(unshare)
}

impl Engine {
	fn new(name: &'static str, bin: PathBuf, scratch: &Path) -> Self {
		let bin = fs::canonicalize(&bin).unwrap_or(bin);
		for command in COMMANDS {
			assert!(
				bin.join(command).exists(),
				"{name}: {} has no {command}",
				bin.display()
			);
		}
		let (tools, missing) = tool_path(&scratch.join("tools"));
		if !missing.is_empty() {
			eprintln!("{name}: host tools not found: {}", missing.join(" "));
		}
		let mut dirs = vec![bin.clone()];
		dirs.extend(tools);
		let path = env::join_paths(&dirs).unwrap();
		// No installed CppNix may be reachable through PATH.
		for dir in &dirs[1..] {
			let found = holds_nix(dir);
			assert!(
				found.is_empty(),
				"{name}: {} on PATH holds {found:?}",
				dir.display()
			);
		}
		let bash = fs::canonicalize(find_on_path("bash").expect("bash")).unwrap();
		let coreutils = fs::canonicalize(find_on_path("ls").expect("coreutils"))
			.unwrap()
			.parent()
			.unwrap()
			.to_path_buf();
		assert!(
			holds_nix(&coreutils).is_empty(),
			"{name}: the coreutils directory builders get holds a nix executable"
		);
		let engine = Engine {
			name,
			bin,
			path,
			bash,
			coreutils,
			busybox: busybox(),
			unshare: network_isolation(),
		};
		engine.check_resolution();
		engine
	}

	/// Each command on the scripts' `PATH` resolves into the engine's
	/// directory, to the binary laid out there.
	fn check_resolution(&self) {
		let probe = COMMANDS
			.iter()
			.map(|command| format!("type -P {command}"))
			.collect::<Vec<_>>()
			.join("; ");
		let output = Command::new(&self.bash)
			.args(["-c", &probe])
			.env_clear()
			.env("PATH", &self.path)
			.output()
			.unwrap();
		let resolved = String::from_utf8(output.stdout).unwrap();
		let resolved: Vec<&str> = resolved.lines().collect();
		assert_eq!(
			resolved.len(),
			COMMANDS.len(),
			"{}: {resolved:?}",
			self.name
		);
		for (command, path) in COMMANDS.iter().zip(resolved) {
			assert_eq!(
				fs::canonicalize(path).unwrap(),
				fs::canonicalize(self.bin.join(command)).unwrap(),
				"{}: {command} resolves outside the engine",
				self.name
			);
		}
	}

	fn substitute(&self, template: &str) -> String {
		template
			.replace("@bash@", &self.bash.to_string_lossy())
			.replace("@bindir@", &self.bin.to_string_lossy())
			.replace("@coreutils@", &self.coreutils.to_string_lossy())
			.replace("@dot@", "")
			.replace("@sandbox_shell@", &self.busybox)
			.replace("@PACKAGE_VERSION@", "2.35.2")
			.replace("@system@", "x86_64-linux")
	}
}

/// Copies the functional tree, keeping links inside it and copying the
/// files outside ones point at.
fn copy_tree(from: &Path, to: &Path, top: &Path) {
	fs::create_dir_all(to).unwrap();
	for entry in fs::read_dir(from).unwrap() {
		let entry = entry.unwrap();
		let source = entry.path();
		let target = to.join(entry.file_name());
		let kind = entry.file_type().unwrap();
		if kind.is_dir() {
			copy_tree(&source, &target, top);
		} else if kind.is_symlink() {
			// Resolved lexically, so links inside the tree are kept even
			// when they are deliberately broken.
			let link = fs::read_link(&source).unwrap();
			let mut resolved = PathBuf::new();
			for part in from.join(&link).components() {
				match part {
					std::path::Component::ParentDir => {
						resolved.pop();
					}
					std::path::Component::CurDir => {}
					part => resolved.push(part),
				}
			}
			if resolved.starts_with(top) {
				std::os::unix::fs::symlink(&link, &target).unwrap();
			} else if let Ok(real) = fs::canonicalize(&source)
				&& real.is_file()
			{
				fs::copy(&real, &target).unwrap();
			}
		} else {
			fs::copy(&source, &target).unwrap();
		}
		if !kind.is_symlink()
			&& let Ok(meta) = fs::metadata(&target)
		{
			let mut perms = meta.permissions();
			perms.set_mode(perms.mode() | 0o200);
			fs::set_permissions(&target, perms).unwrap();
		}
	}
}

fn make_writable(path: &Path) {
	let Ok(meta) = fs::symlink_metadata(path) else {
		return;
	};
	if meta.file_type().is_symlink() {
		return;
	}
	let mut perms = meta.permissions();
	perms.set_mode(perms.mode() | 0o700);
	let _ = fs::set_permissions(path, perms);
	if meta.is_dir()
		&& let Ok(entries) = fs::read_dir(path)
	{
		for entry in entries.flatten() {
			make_writable(&entry.path());
		}
	}
}

/// Store paths below the scratch root hash that root (`NIX_STORE_DIR`), so
/// their hash parts change from run to run; notes show them as `HASH`.
fn mask_store_hashes(line: &str) -> String {
	const NIX32: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
	let bytes = line.as_bytes();
	let mut out = String::with_capacity(line.len());
	let mut i = 0;
	while i < bytes.len() {
		if line[i..].starts_with("/store/")
			&& bytes.len() >= i + 7 + 32
			&& bytes[i + 7..i + 39].iter().all(|b| NIX32.contains(b))
		{
			out.push_str("/store/HASH");
			i += 39;
		} else {
			let c = line[i..].chars().next().unwrap();
			out.push(c);
			i += c.len_utf8();
		}
	}
	out
}

/// The last `error:` line and the innermost frame of upstream's
/// `test failed at:` report, or the skip message.
fn summarize(log: &str, root: &str, outcome: Outcome) -> String {
	let clean = |line: &str| {
		mask_store_hashes(
			&line
				.replace(&format!("{root}/src/"), "")
				.replace(root, "$ROOT"),
		)
		.trim()
		.to_owned()
	};
	let lines: Vec<&str> = log.lines().collect();
	let summary = match outcome {
		Outcome::Pass | Outcome::NotRun => String::new(),
		Outcome::Skip => lines
			.iter()
			.rev()
			.find(|line| line.contains("skipping this test"))
			.map(|line| clean(line))
			.unwrap_or_default(),
		Outcome::Fail => {
			let failed = lines
				.iter()
				.rposition(|line| line.ends_with("test failed at:"));
			let end = failed.unwrap_or(lines.len());
			let error = lines[..end]
				.iter()
				.rev()
				.find(|line| {
					let line = line.trim_start();
					line.starts_with("error:") || line.starts_with("test failed:")
				})
				.or_else(|| {
					lines[..end]
						.iter()
						.rev()
						.find(|line| !line.starts_with('+') && !line.trim().is_empty())
				})
				.map(|line| clean(line))
				.unwrap_or_default();
			let frame = failed
				.and_then(|index| lines.get(index + 1))
				.map(|line| clean(line))
				.unwrap_or_default();
			format!("{frame}: {error}")
		}
		Outcome::Timeout => lines
			.iter()
			.rev()
			.find(|line| line.starts_with('+'))
			.map(|line| format!("timed out at {}", clean(line)))
			.unwrap_or_else(|| "timed out".to_owned()),
	};
	let mut summary: String = summary.chars().filter(|c| !c.is_control()).collect();
	if summary.len() > 200 {
		let mut cut = 200;
		while !summary.is_char_boundary(cut) {
			cut -= 1;
		}
		summary.truncate(cut);
	}
	summary
}

fn run_script(engine: &Engine, script: &Script, timeout: Duration) -> Run {
	if script.not_run().is_some() {
		return Run {
			outcome: Outcome::NotRun,
			seconds: 0.0,
			detail: String::new(),
			log: None,
		};
	}
	// Short, so that TEST_ROOT/dSocket stays under the socket path limit.
	let root = tempfile::Builder::new()
		.prefix("nf")
		.tempdir_in("/tmp")
		.unwrap();
	let root_path = root.path().to_path_buf();
	let source = root_path.join("src");
	let build = root_path.join("build");
	let functional = functional_dir();
	copy_tree(&functional, &source, &functional);
	fs::create_dir_all(build.join("common")).unwrap();
	fs::create_dir_all(root_path.join("tmp")).unwrap();
	let template = |name: &str| fs::read_to_string(functional.join(name)).unwrap();
	fs::write(
		build.join("common/subst-vars.sh"),
		engine.substitute(&template("common/subst-vars.sh.in")),
	)
	.unwrap();
	fs::write(
		build.join("config.nix"),
		engine.substitute(&template("config.nix.in")),
	)
	.unwrap();

	let log_path = root_path.join("log");
	let log = fs::File::create(&log_path).unwrap();
	let mut command = match &engine.unshare {
		Some(unshare) => {
			let mut command = Command::new(unshare);
			command
				.args(["--map-current-user", "--net", "--"])
				.arg(&engine.bash);
			command
		}
		None => Command::new(&engine.bash),
	};
	command
		.args(["-x", "-e", "-u", "-o", "pipefail", &script.file])
		.current_dir(source.join(&script.dir))
		.env_clear()
		.env("PATH", &engine.path)
		.env("HOME", "/homeless-shelter")
		.env("TMPDIR", root_path.join("tmp"))
		// Read under `set -u` by common/vars.sh; set only inside a build.
		.env("NIX_STORE", "")
		.env("_NIX_TEST_SOURCE_DIR", &source)
		.env("_NIX_TEST_BUILD_DIR", &build)
		.env("TEST_SUITE_NAME", &script.suite)
		.env("TEST_NAME", script.name())
		.env("NIX_REMOTE", "")
		// Upstream runs the suite in a stdenv build, which exports the
		// builder's shell; several scripts read it.
		.env("shell", &engine.bash)
		.env("PS4", "+(${BASH_SOURCE[0]-$0}:$LINENO) ")
		.stdin(Stdio::null())
		.stdout(log.try_clone().unwrap())
		.stderr(log)
		.process_group(0);
	for var in ["USER", "LOGNAME", "LOCALE_ARCHIVE", "TZDIR"] {
		if let Some(value) = env::var_os(var) {
			command.env(var, value);
		}
	}
	let start = Instant::now();
	let mut child = command.spawn().unwrap();
	let group = child.id() as libc::pid_t;
	let status = loop {
		if let Some(status) = child.try_wait().unwrap() {
			break Some(status);
		}
		if start.elapsed() > timeout {
			break None;
		}
		thread::sleep(Duration::from_millis(50));
	};
	// Background daemons and helpers die with the script's group.
	unsafe {
		libc::killpg(group, libc::SIGKILL);
	}
	if status.is_none() {
		let _ = child.wait();
	}
	let seconds = start.elapsed().as_secs_f64();
	let outcome = match status {
		None => Outcome::Timeout,
		Some(status) if status.code() == Some(0) => Outcome::Pass,
		Some(status) if status.code() == Some(77) => Outcome::Skip,
		Some(status) => {
			debug_assert!(status.code().is_some() || status.signal().is_some());
			Outcome::Fail
		}
	};
	let text = String::from_utf8_lossy(&fs::read(&log_path).unwrap_or_default()).into_owned();
	let detail = summarize(&text, &root_path.to_string_lossy(), outcome);
	let saved = Path::new(OUTPUT_DIR)
		.join(engine.name)
		.join(format!("{}.log", script.path.replace('/', "_")));
	fs::create_dir_all(saved.parent().unwrap()).unwrap();
	fs::write(&saved, &text).unwrap();
	if env::var_os(KEEP_VAR).is_some() {
		let kept = root.keep();
		eprintln!("kept {} for {}", kept.display(), script.path);
	} else {
		make_writable(&root_path);
		drop(root);
	}
	Run {
		outcome,
		seconds,
		detail,
		log: Some(saved),
	}
}

fn parse_expectations() -> (BTreeMap<String, Expected>, BTreeMap<String, String>) {
	let mut scripts = BTreeMap::new();
	let mut support = BTreeMap::new();
	let text = fs::read_to_string(EXPECTATIONS).unwrap_or_default();
	for (index, raw) in text.lines().enumerate() {
		if raw.starts_with('#') || raw.trim().is_empty() {
			continue;
		}
		let (line, note) = raw.split_once(" # ").unwrap_or((raw, ""));
		let fields: Vec<&str> = line.split_whitespace().collect();
		match fields.as_slice() {
			[path, "support"] => {
				support.insert((*path).to_owned(), note.trim().to_owned());
			}
			[path, class, noxi, reference] => {
				let outcome = |text: &str| {
					Outcome::parse(text)
						.unwrap_or_else(|| panic!("line {}: unknown outcome {text:?}", index + 1))
				};
				let reference = (*reference != "-").then(|| outcome(reference));
				scripts.insert(
					(*path).to_owned(),
					Expected {
						class: (*class).to_owned(),
						noxi: outcome(noxi),
						reference,
						note: note.trim().to_owned(),
					},
				);
			}
			_ => panic!("line {}: malformed {raw:?}", index + 1),
		}
	}
	(scripts, support)
}

fn write_expectations(scripts: &BTreeMap<String, Expected>, support: &BTreeMap<String, String>) {
	let mut out = String::new();
	out.push_str(
		"# Observed outcome of each CppNix 2.35.2 functional test script under noxi and the\n\
		 # pinned reference, and the fixture class they give; every other file in\n\
		 # tests/functional outside lang/ is listed as support. Generated by\n\
		 # `NOXI_COMPAT_BLESS=1 NOXI_REFERENCE_BIN=... cargo nextest run -p noxi-compat-tests --test upstream_functional`;\n\
		 # review every changed line. Format: `<path> <class> <noxi> <reference> # note`.\n\
		 # Classes and outcomes are defined in tests/upstream_functional.rs.\n",
	);
	let mut lines: BTreeMap<&str, String> = BTreeMap::new();
	for (path, expected) in scripts {
		let mut line = format!(
			"{path} {} {} {}",
			expected.class,
			expected.noxi.name(),
			expected.reference.map_or("-", Outcome::name)
		);
		if !expected.note.is_empty() {
			let _ = write!(line, " # {}", expected.note);
		}
		lines.insert(path, line);
	}
	for (path, note) in support {
		let mut line = format!("{path} support");
		if !note.is_empty() {
			let _ = write!(line, " # {note}");
		}
		lines.insert(path, line);
	}
	for line in lines.values() {
		out.push_str(line);
		out.push('\n');
	}
	fs::write(EXPECTATIONS, out).unwrap();
}

fn selected(script: &Script) -> bool {
	match env::var(FILTER_VAR) {
		Ok(filter) if !filter.is_empty() => filter
			.split(',')
			.any(|part| !part.is_empty() && script.path.contains(part)),
		_ => true,
	}
}

#[test]
fn functional_scripts_match_expectations() {
	let bless = env::var_os(BLESS_VAR).is_some();
	let all = scripts();
	let (mut expected, mut support) = parse_expectations();

	// Every file in the tree is a script or listed support.
	let script_paths: BTreeSet<&str> = all.iter().map(|script| script.path.as_str()).collect();
	let files = functional_files();
	let files_set: BTreeSet<&str> = files.iter().map(String::as_str).collect();
	let mut problems = Vec::new();
	if bless {
		support = files
			.iter()
			.filter(|file| !script_paths.contains(file.as_str()))
			.map(|file| (file.clone(), support.get(file).cloned().unwrap_or_default()))
			.collect();
		expected.retain(|path, _| script_paths.contains(path.as_str()));
	}
	for file in &files {
		if !script_paths.contains(file.as_str()) && !support.contains_key(file) {
			problems.push(format!("unlisted file {file}"));
		}
	}
	for path in support.keys().chain(expected.keys()) {
		if !files_set.contains(path.as_str()) {
			problems.push(format!("stale entry {path}"));
		}
	}
	for path in support.keys() {
		if script_paths.contains(path.as_str()) {
			problems.push(format!("{path} is a test script, not support"));
		}
	}

	let chosen: Vec<&Script> = all.iter().filter(|script| selected(script)).collect();
	let scratch = tempfile::tempdir().unwrap();
	let noxi = Engine::new(
		"noxi",
		candidate_bin(&scratch.path().join("noxi-bin")),
		scratch.path(),
	);
	let reference = env::var_os(REFERENCE_VAR).map(|dir| {
		Engine::new(
			"reference",
			PathBuf::from(dir),
			&scratch.path().join("reference"),
		)
	});
	match &reference {
		Some(engine) => eprintln!("reference: {}", engine.bin.display()),
		None => eprintln!(
			"reference: NOT RUN ({REFERENCE_VAR} is unset); its recorded column is not checked"
		),
	}
	eprintln!(
		"network: {}",
		if noxi.unshare.is_some() {
			"private network namespace"
		} else {
			"NOT ISOLATED (unprivileged user namespaces unavailable)"
		}
	);

	let timeout = Duration::from_secs(
		env::var(TIMEOUT_VAR)
			.ok()
			.and_then(|value| value.parse().ok())
			.unwrap_or(120),
	);
	let jobs = env::var(JOBS_VAR)
		.ok()
		.and_then(|value| value.parse().ok())
		.unwrap_or(8usize)
		.max(1);
	let mut queue: Vec<(&Engine, &Script)> = Vec::new();
	for script in &chosen {
		queue.push((&noxi, script));
		if let Some(engine) = &reference {
			queue.push((engine, script));
		}
	}
	let next = AtomicUsize::new(0);
	let results: Mutex<BTreeMap<(String, &'static str), Run>> = Mutex::new(BTreeMap::new());
	let started = Instant::now();
	thread::scope(|scope| {
		for _ in 0..jobs.min(queue.len().max(1)) {
			scope.spawn(|| {
				loop {
					let index = next.fetch_add(1, Ordering::SeqCst);
					let Some((engine, script)) = queue.get(index) else {
						break;
					};
					let run = run_script(engine, script, timeout);
					eprintln!(
						"{:>9} {:<8} {:>6.1}s {}",
						engine.name,
						run.outcome.name(),
						run.seconds,
						script.path
					);
					results
						.lock()
						.unwrap()
						.insert((script.path.clone(), engine.name), run);
				}
			});
		}
	});
	let results = results.into_inner().unwrap();
	eprintln!(
		"ran {} script runs in {:.0}s",
		queue.len(),
		started.elapsed().as_secs_f64()
	);

	let mut report = Vec::new();
	let mut counts: BTreeMap<String, usize> = BTreeMap::new();
	for script in &chosen {
		let noxi_run = &results[&(script.path.clone(), "noxi")];
		let reference_run = results.get(&(script.path.clone(), "reference"));
		let recorded = expected.get(&script.path).cloned();
		if noxi_run.outcome != recorded.as_ref().map_or(Outcome::NotRun, |e| e.noxi)
			|| recorded.is_none()
		{
			problems.push(format!(
				"{}: noxi {} (recorded {}) {}",
				script.path,
				noxi_run.outcome.name(),
				recorded.as_ref().map_or("nothing", |e| e.noxi.name()),
				noxi_run.detail
			));
		}
		if let Some(run) = reference_run
			&& recorded.as_ref().and_then(|e| e.reference) != Some(run.outcome)
		{
			problems.push(format!(
				"{}: reference {} (recorded {}) {}",
				script.path,
				run.outcome.name(),
				recorded
					.as_ref()
					.and_then(|e| e.reference)
					.map_or("nothing", Outcome::name),
				run.detail
			));
		}
		let reference_outcome = reference_run
			.map(|run| run.outcome)
			.or_else(|| recorded.as_ref().and_then(|e| e.reference));
		let class = reference_outcome
			.map(|reference| classify(noxi_run.outcome, reference))
			.unwrap_or("unclassified");
		*counts.entry(class.to_owned()).or_default() += 1;
		if let Some(recorded) = &recorded
			&& recorded.class != class
			&& class != "unclassified"
		{
			problems.push(format!(
				"{}: class {class} (recorded {})",
				script.path, recorded.class
			));
		}
		report.push(json!({
			"script": script.path,
			"suite": script.suite,
			"class": class,
			"noxi": run_json(noxi_run),
			"reference": reference_run.map(run_json),
		}));
		if bless {
			let note = match (script.not_run(), class) {
				(Some(reason), _) => reason.to_owned(),
				(None, "gap") => noxi_run.detail.clone(),
				(None, "anomaly") => reference_run.map_or(String::new(), |run| {
					format!("reference {}: {}", run.outcome.name(), run.detail)
				}),
				_ => String::new(),
			};
			expected.insert(
				script.path.clone(),
				Expected {
					class: class.to_owned(),
					noxi: noxi_run.outcome,
					reference: reference_outcome,
					// A failure can move to a later command without changing
					// its exit status. Always report the current failure.
					note,
				},
			);
		}
	}
	fs::create_dir_all(OUTPUT_DIR).unwrap();
	let report_path = Path::new(OUTPUT_DIR).join("report.json");
	fs::write(
		&report_path,
		serde_json::to_string_pretty(&json!({
			"snapshot": "CppNix 2.35.2",
			"reference": reference.as_ref().map(|engine| engine.bin.to_string_lossy().into_owned()),
			"noxi": noxi.bin.to_string_lossy(),
			"executables": {
				"noxi": executable_evidence(&noxi),
				"reference": reference.as_ref().map(executable_evidence),
			},
			"network_isolated": noxi.unshare.is_some(),
			"timeout_seconds": timeout.as_secs(),
			"scripts": report,
		}))
		.unwrap(),
	)
	.unwrap();
	eprintln!("classes of the selected scripts: {counts:?}");
	eprintln!("report: {}", report_path.display());

	if bless {
		write_expectations(&expected, &support);
		eprintln!("blessed {EXPECTATIONS}");
		return;
	}
	assert!(
		problems.is_empty(),
		"{} differences from expectations/upstream-functional.txt (set {BLESS_VAR}=1 to accept after review):\n{}",
		problems.len(),
		problems.join("\n")
	);
}

fn run_json(run: &Run) -> Json {
	json!({
		"outcome": run.outcome.name(),
		"seconds": (run.seconds * 10.0).round() / 10.0,
		"detail": run.detail,
		"log": run.log.as_ref().map(|path| path.to_string_lossy().into_owned()),
	})
}
