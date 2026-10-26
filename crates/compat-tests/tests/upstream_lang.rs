//! Runs every copied CppNix language fixture against the nix-rs evaluator.
//!
//! This follows `tests/functional/lang.sh`: each `parse-*` and `eval-*` case
//! runs with the same environment, working directory, exit-code expectation,
//! `/pwd` substitution and post-processing, and its output is compared with
//! the expected files. Each case runs in a child process (this test binary,
//! re-executed) so hangs and stack overflows are contained and timed out.
//! `eval-*` children run nix-rs's `nix-instantiate` with the arguments
//! `lang.sh` passes, including each case's `.flags`; a case whose arguments
//! the command rejects is recorded as `unrun-flags`. `parse-*` children run
//! `nix-instantiate --parse -` with the fixture on standard input.
//!
//! Every file in the fixture directory must be a case, a companion of one, or
//! a listed support file. Every case must appear in `upstream-lang.txt` with
//! its observed outcome, plus the evaluator's error code for failures, so a gap that closes or a pass that regresses fails
//! this test. Set `NIXRS_COMPAT_BLESS=1` to rewrite the table after reviewing
//! the change. A JSON report of every case is written to the Cargo target
//! temporary directory.

use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use nix_compat::nixcpp::settings::Settings;
use nix_rs_commands::instantiate;
use serde_json::{Value as Json, json};

const FUNCTIONAL_DIR: &str = concat!(
	env!("CARGO_MANIFEST_DIR"),
	"/../../tests/nix/tests/functional"
);
const SNAPSHOT_LOCK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/nix-snapshot.lock");
const EXPECTATIONS: &str = concat!(
	env!("CARGO_MANIFEST_DIR"),
	"/expectations/upstream-lang.txt"
);
const REPORT: &str = concat!(env!("CARGO_TARGET_TMPDIR"), "/upstream-lang-report.json");

/// Selects child mode; its value is the operation to run on the file argument.
const CHILD_VAR: &str = "NIXRS_COMPAT_LANG_CHILD";
const BLESS_VAR: &str = "NIXRS_COMPAT_BLESS";
const TIMEOUT: Duration = Duration::from_secs(20);

/// The environment `lang.sh` exports for every case. `common/vars.sh` also
/// unsets `NIX_PATH`, points `HOME` and `NIX_STATE_DIR` at a per-run test
/// directory, and sets `NIX_CONF_DIR` to the configuration
/// [`write_test_config`] writes.
const SHARED_ENV: &[(&str, &str)] = &[
	("TEST_VAR", "foo"),
	("NIX_REMOTE", "dummy://"),
	("NIX_STORE_DIR", "/nix/store"),
];
/// Set by `lang.sh` only on the `eval-okay` invocation.
const EVAL_OKAY_ENV: &[(&str, &str)] =
	&[("NIX_PATH", "lang/dir3:lang/dir4"), ("HOME", "/fake-home")];

/// Files in the fixture directory that are not cases or their companions.
const SUPPORT: &[(&str, &str)] = &[
	("binary-data", "hashed by eval-okay-hashfile"),
	(
		"data",
		"read by eval-okay-hashfile, eval-okay-path and others",
	),
	("dir1", "search-path and path fixture"),
	("dir2", "search-path fixture"),
	("dir3", "search-path fixture on NIX_PATH"),
	("dir4", "search-path fixture on NIX_PATH"),
	("imported.nix", "imported by eval-okay-import"),
	("imported2.nix", "imported by imported.nix"),
	(
		"infinite-nesting.nix",
		"imported by eval-fail-genericClosure-deeply-nested-element",
	),
	("lib.nix", "library imported by many cases"),
	(
		"non-eval-fail-bad-drvPath.nix",
		"instantiated directly by lang.sh, not a fixture case",
	),
	(
		"non-eval-trivial-lambda-formals.nix",
		"imported by eval-fail-missing-arg-import and eval-fail-undeclared-arg-import",
	),
	("readDir", "directory read by the readDir cases"),
	(
		"symlink-resolution",
		"symlink fixture for path resolution cases",
	),
	(
		"eval-okay-hash.exp",
		"reference anomaly: expected output with no eval-okay-hash.nix case",
	),
];

const COMPANIONS: &[&str] = &[
	".err.exp",
	".exp",
	".exp.xml",
	".exp-disabled",
	".flags",
	".postprocess",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
	ParseOkay,
	ParseFail,
	EvalOkay,
	EvalFail,
}

impl Kind {
	fn of(name: &str) -> Option<Self> {
		[
			("parse-okay-", Kind::ParseOkay),
			("parse-fail-", Kind::ParseFail),
			("eval-okay-", Kind::EvalOkay),
			("eval-fail-", Kind::EvalFail),
		]
		.into_iter()
		.find_map(|(prefix, kind)| name.starts_with(prefix).then_some(kind))
	}

	fn should_fail(self) -> bool {
		matches!(self, Kind::ParseFail | Kind::EvalFail)
	}
}

/// The operation a child process performs, mirroring the `nix-instantiate`
/// invocation `lang.sh` uses for the case.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Operation {
	/// `nix-instantiate --parse - < FILE`
	Parse,
	/// `nix-instantiate --eval --strict FILE`, or the case's flags
	Eval,
	/// `nix-instantiate --eval --xml --no-location --strict FILE`
	EvalXml,
}

/// Child mode for an `eval-*` case: run `nix-instantiate` with the arguments.
const CHILD_INSTANTIATE: &str = "instantiate";

/// What happened when a case ran, compared with the reference expectations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outcome {
	/// Exit status and every compared output match the reference.
	Pass,
	/// Succeeded as expected, but stdout or stderr differs.
	Output,
	/// Failed as expected, but the diagnostic differs.
	ErrorMessage,
	/// Should succeed, but failed.
	UnexpectedFailure,
	/// Should fail, but succeeded.
	UnexpectedSuccess,
	Timeout,
	/// Ended by a signal or an exit status other than 0 or 1.
	Crash,
	/// Needs `nix-instantiate` arguments that nix-rs's command rejects.
	UnrunFlags,
	/// Upstream disables the case with an `.exp-disabled` file.
	Disabled,
}

impl Outcome {
	const ALL: [Outcome; 9] = [
		Outcome::Pass,
		Outcome::Output,
		Outcome::ErrorMessage,
		Outcome::UnexpectedFailure,
		Outcome::UnexpectedSuccess,
		Outcome::Timeout,
		Outcome::Crash,
		Outcome::UnrunFlags,
		Outcome::Disabled,
	];

	fn name(self) -> &'static str {
		match self {
			Outcome::Pass => "pass",
			Outcome::Output => "output",
			Outcome::ErrorMessage => "error-message",
			Outcome::UnexpectedFailure => "unexpected-failure",
			Outcome::UnexpectedSuccess => "unexpected-success",
			Outcome::Timeout => "timeout",
			Outcome::Crash => "crash",
			Outcome::UnrunFlags => "unrun-flags",
			Outcome::Disabled => "disabled",
		}
	}

	fn from_name(name: &str) -> Option<Self> {
		Outcome::ALL
			.into_iter()
			.find(|outcome| outcome.name() == name)
	}
}

struct Case {
	name: String,
	kind: Kind,
}

impl Case {
	fn companion(&self, suffix: &str) -> PathBuf {
		lang_dir().join(format!("{}{suffix}", self.name))
	}

	fn has(&self, suffix: &str) -> bool {
		self.companion(suffix).exists()
	}

	fn operation(&self) -> Operation {
		match self.kind {
			Kind::ParseOkay | Kind::ParseFail => Operation::Parse,
			Kind::EvalOkay if self.has(".exp.xml") => Operation::EvalXml,
			Kind::EvalOkay | Kind::EvalFail => Operation::Eval,
		}
	}

	/// The `nix-instantiate` arguments `lang.sh` passes for an `eval-*` case.
	/// An `eval-fail` case's `.flags` replace the defaults, with comments
	/// removed; an `eval-okay` case's first `.flags` line comes before them.
	fn instantiate_args(&self) -> Vec<String> {
		let flags = fs::read_to_string(self.companion(".flags")).ok();
		let file = lang_dir()
			.join(format!("{}.nix", self.name))
			.to_string_lossy()
			.into_owned();
		let words = |text: &str| {
			text.split_whitespace()
				.map(str::to_owned)
				.collect::<Vec<_>>()
		};
		let mut args = match (self.kind, self.operation(), flags) {
			(Kind::EvalFail, _, Some(flags)) => flags
				.lines()
				.flat_map(|line| words(line.split('#').next().unwrap_or_default()))
				.collect(),
			(Kind::EvalFail, _, None) => words("--eval --strict --show-trace"),
			(_, Operation::EvalXml, _) => words("--eval --xml --no-location --strict"),
			(_, _, flags) => {
				let mut args = words(
					flags
						.as_deref()
						.and_then(|f| f.lines().next())
						.unwrap_or(""),
				);
				args.extend(words("--eval --strict"));
				args
			}
		};
		args.push(file);
		args
	}
}

struct Run {
	outcome: Outcome,
	/// The evaluator's error code for failures, so a change of cause is visible.
	error_code: Option<String>,
	exit_code: Option<i32>,
	/// The signal that ended the run, which leaves no exit code.
	signal: Option<i32>,
	stdout: String,
	stderr: String,
	elapsed: Duration,
}

fn functional_dir() -> PathBuf {
	fs::canonicalize(FUNCTIONAL_DIR).expect("copied functional test directory must exist")
}

fn lang_dir() -> PathBuf {
	functional_dir().join("lang")
}

fn read_or_empty(path: &Path) -> String {
	fs::read_to_string(path).unwrap_or_default()
}

/// Lists the cases and fails on any file that is not a case, a companion of a
/// case, or a listed support file, and on any listed support file that is missing.
fn discover() -> Result<Vec<Case>, Vec<String>> {
	let mut names: Vec<String> = fs::read_dir(lang_dir())
		.expect("language fixture directory must be readable")
		.map(|entry| {
			entry
				.expect("fixture entry must be readable")
				.file_name()
				.into_string()
				.expect("fixture names are UTF-8")
		})
		.collect();
	names.sort();

	let cases: Vec<Case> = names
		.iter()
		.filter_map(|name| {
			let stem = name.strip_suffix(".nix")?;
			Kind::of(stem).map(|kind| Case {
				name: stem.to_owned(),
				kind,
			})
		})
		.collect();

	let mut unclassified: Vec<String> = names
		.into_iter()
		.filter(|name| {
			let is_case = name
				.strip_suffix(".nix")
				.is_some_and(|stem| Kind::of(stem).is_some());
			let is_companion = COMPANIONS.iter().any(|suffix| {
				name.strip_suffix(suffix)
					.is_some_and(|stem| cases.iter().any(|case| case.name == stem))
			});
			let is_support = SUPPORT.iter().any(|(support, _)| support == name);
			!(is_case || is_companion || is_support)
		})
		.collect();

	// A listed support file that no longer exists is a stale classification.
	unclassified.extend(
		SUPPORT
			.iter()
			.filter(|(support, _)| !lang_dir().join(support).exists())
			.map(|(support, _)| format!("{support} (listed as support, but missing)")),
	);

	if unclassified.is_empty() {
		Ok(cases)
	} else {
		Err(unclassified)
	}
}

/// Run nix-rs's `nix-instantiate` for an `eval-*` case.
fn child_instantiate(args: Vec<String>) -> ExitCode {
	let settings = match Settings::load(|name| env::var(name).ok()) {
		Ok(settings) => settings,
		Err(error) => {
			eprintln!("error: {error}");
			return ExitCode::FAILURE;
		}
	};
	let mut options =
		instantiate::parse_args(args, settings).expect("arguments checked by the parent");
	for warning in options.settings.finish() {
		eprintln!("warning: {warning}");
	}
	let cwd = env::current_dir().expect("fixture working directory");
	instantiate::run(&options, &cwd, &mut io::stdout().lock())
}

fn spawn(case: &Case, scratch: &Path, args: &[String]) -> io::Result<Child> {
	let mut command = Command::new(env::current_exe()?);
	command
		.env_clear()
		.envs(SHARED_ENV.iter().copied())
		.env("HOME", scratch.join("test-home"))
		.env("NIX_STATE_DIR", scratch.join("var/nix"))
		.env("NIX_CONF_DIR", scratch.join("etc"));
	if case.kind == Kind::EvalOkay {
		command.envs(EVAL_OKAY_ENV.iter().copied());
	}
	command.args(args).env(CHILD_VAR, CHILD_INSTANTIATE);
	if case.operation() == Operation::Parse {
		command.stdin(Stdio::from(fs::File::open(case.companion(".nix"))?));
	} else {
		command.stdin(Stdio::null());
	}
	command
		.current_dir(functional_dir())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
}

fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
	thread::spawn(move || {
		let mut buffer = Vec::new();
		let _ = pipe.read_to_end(&mut buffer);
		buffer
	})
}

/// Applies `lang.sh`'s `/pwd` substitution and any `.postprocess` script.
fn normalize(case: &Case, text: &str, scratch: &Path) -> String {
	let pwd = functional_dir();
	let mut text = text.replace(pwd.to_str().expect("fixture path is UTF-8"), "/pwd");
	let script = case.companion(".postprocess");
	if script.exists() {
		let base = scratch.join(&case.name);
		fs::write(base.with_extension("err"), &text).expect("write output for post-processing");
		let status = Command::new("bash")
			.arg(&script)
			.arg(&base)
			.current_dir(functional_dir())
			.status()
			.expect("run fixture post-processing with bash");
		assert!(status.success(), "{}.postprocess failed", case.name);
		text = fs::read_to_string(base.with_extension("err")).expect("read post-processed output");
	}
	text
}

fn run(case: &Case, scratch: &Path) -> Run {
	if case.has(".exp-disabled") {
		return skipped(Outcome::Disabled);
	}
	let args = match case.operation() {
		Operation::Parse => vec!["--parse".to_owned(), "-".to_owned()],
		Operation::Eval | Operation::EvalXml => case.instantiate_args(),
	};
	if instantiate::parse_args(args.clone(), Settings::default()).is_err() {
		return skipped(Outcome::UnrunFlags);
	}

	let started = Instant::now();
	let mut child = spawn(case, scratch, &args).expect("re-execute the test binary for a fixture");
	let stdout = drain(child.stdout.take().expect("piped stdout"));
	let stderr = drain(child.stderr.take().expect("piped stderr"));
	let status = loop {
		if let Some(status) = child.try_wait().expect("poll fixture child") {
			break Some(status);
		}
		if started.elapsed() > TIMEOUT {
			let _ = child.kill();
			let _ = child.wait();
			break None;
		}
		thread::sleep(Duration::from_millis(5));
	};
	let elapsed = started.elapsed();
	let stdout = String::from_utf8_lossy(&stdout.join().expect("stdout reader")).into_owned();
	let stderr = String::from_utf8_lossy(&stderr.join().expect("stderr reader")).into_owned();
	let exit_code = status.and_then(|status| status.code());
	let signal = status.and_then(|status| status.signal());

	let outcome = match (status, exit_code) {
		(None, _) => Outcome::Timeout,
		(Some(_), Some(0)) if case.kind.should_fail() => Outcome::UnexpectedSuccess,
		(Some(_), Some(1)) if !case.kind.should_fail() => Outcome::UnexpectedFailure,
		(Some(_), Some(0 | 1)) => {
			let matches = if case.kind.should_fail() {
				// `expectStderr` merges both streams; only stderr is expected.
				let got = normalize(case, &format!("{stdout}{stderr}"), scratch);
				got == read_or_empty(&case.companion(".err.exp"))
			} else {
				let expected_stdout = match case.operation() {
					Operation::EvalXml => case.companion(".exp.xml"),
					Operation::Parse | Operation::Eval => case.companion(".exp"),
				};
				// XML cases compare only stdout.
				normalize(case, &stdout, scratch) == read_or_empty(&expected_stdout)
					&& (case.operation() == Operation::EvalXml
						|| normalize(case, &stderr, scratch)
							== read_or_empty(&case.companion(".err.exp")))
			};
			match (matches, case.kind.should_fail()) {
				(true, _) => Outcome::Pass,
				(false, true) => Outcome::ErrorMessage,
				(false, false) => Outcome::Output,
			}
		}
		(Some(_), _) => Outcome::Crash,
	};

	let error_code = matches!(outcome, Outcome::ErrorMessage | Outcome::UnexpectedFailure)
		.then(|| error_code(&stderr));
	Run {
		outcome,
		error_code,
		exit_code,
		signal,
		stdout,
		stderr,
		elapsed,
	}
}

/// The first `error[E…]` code the evaluator printed, or `none`.
fn error_code(stderr: &str) -> String {
	stderr
		.lines()
		.find_map(|line| {
			let code = line.strip_prefix("error[")?.split_once(']')?.0;
			Some(code.to_owned())
		})
		.unwrap_or_else(|| "none".to_owned())
}

fn skipped(outcome: Outcome) -> Run {
	Run {
		outcome,
		error_code: None,
		exit_code: None,
		signal: None,
		stdout: String::new(),
		stderr: String::new(),
		elapsed: Duration::ZERO,
	}
}

/// Writes the configuration `common/init.sh` gives every functional test
/// outside NixOS into `scratch/etc`, for `NIX_CONF_DIR`.
fn write_test_config(scratch: &Path) -> io::Result<()> {
	let etc = scratch.join("etc");
	fs::create_dir_all(&etc)?;
	let user = env::var("USER").unwrap_or_else(|_| "nixbld".to_owned());
	fs::write(
		etc.join("nix.conf"),
		format!(
			"build-users-group =
keep-derivations = false
sandbox = false
experimental-features = nix-command
gc-reserved-space = 0
substituters =
flake-registry = {}/registry.json
show-trace = true
include nix.conf.extra
trusted-users = {user}
",
			scratch.display()
		),
	)?;
	fs::write(
		etc.join("nix.conf.extra"),
		"fsync-metadata = false
extra-experimental-features = flakes
!include nix.conf.extra.not-there
",
	)
}

fn run_all(cases: &[Case]) -> Vec<Run> {
	let scratch = tempfile::tempdir().expect("scratch directory for post-processing");
	write_test_config(scratch.path()).expect("write the test configuration");
	let next = AtomicUsize::new(0);
	let results = Mutex::new(BTreeMap::new());
	let workers = thread::available_parallelism().map_or(4, |count| count.get());
	thread::scope(|scope| {
		for _ in 0..workers {
			scope.spawn(|| {
				loop {
					let index = next.fetch_add(1, Ordering::Relaxed);
					let Some(case) = cases.get(index) else { break };
					let result = run(case, scratch.path());
					results.lock().unwrap().insert(index, result);
				}
			});
		}
	});
	results.into_inner().unwrap().into_values().collect()
}

impl Run {
	/// The table entry: the outcome name, followed by the error code if any.
	fn entry(&self) -> String {
		match &self.error_code {
			Some(code) => format!("{} {code}", self.outcome.name()),
			None => self.outcome.name().to_owned(),
		}
	}
}

fn read_expectations() -> BTreeMap<String, String> {
	read_or_empty(Path::new(EXPECTATIONS))
		.lines()
		.map(str::trim)
		.filter(|line| !line.is_empty() && !line.starts_with('#'))
		.map(|line| {
			let (name, entry) = line
				.split_once(' ')
				.unwrap_or_else(|| panic!("malformed expectation line: {line}"));
			let outcome = entry.split(' ').next().unwrap_or_default();
			assert!(
				Outcome::from_name(outcome).is_some(),
				"unknown outcome in expectation line: {line}"
			);
			(name.to_owned(), entry.trim().to_owned())
		})
		.collect()
}

fn write_expectations(cases: &[Case], runs: &[Run]) {
	let mut table = String::from(
		"# Observed outcome of each copied CppNix language fixture under nix-rs.\n\
		 # Generated by `NIXRS_COMPAT_BLESS=1 cargo nextest run -p nixrs-compat-tests --test upstream_lang`;\n\
		 # review every changed line. Outcomes are defined in tests/upstream_lang.rs.\n",
	);
	for (case, run) in cases.iter().zip(runs) {
		writeln!(table, "{} {}", case.name, run.entry()).unwrap();
	}
	fs::create_dir_all(Path::new(EXPECTATIONS).parent().unwrap()).unwrap();
	fs::write(EXPECTATIONS, table).expect("write expectation table");
}

fn write_report(cases: &[Case], runs: &[Run]) -> io::Result<()> {
	let reference_commit = read_or_empty(Path::new(SNAPSHOT_LOCK))
		.lines()
		.find_map(|line| line.strip_prefix("commit="))
		.unwrap_or_default()
		.to_owned();
	let mut totals: BTreeMap<&str, usize> = BTreeMap::new();
	for run in runs {
		*totals.entry(run.outcome.name()).or_default() += 1;
	}
	let entries: Vec<Json> = cases
		.iter()
		.zip(runs)
		.map(|(case, run)| {
			json!({
				"name": case.name,
				"kind": format!("{:?}", case.kind),
				"command": match case.operation() {
					Operation::Parse => vec!["--parse".to_owned(), "-".to_owned()],
					Operation::Eval | Operation::EvalXml => case.instantiate_args(),
				},
				"outcome": run.outcome.name(),
				"error_code": run.error_code,
				"exit_code": run.exit_code,
				"signal": run.signal,
				"stdout": run.stdout,
				"stderr": run.stderr,
				"elapsed_ms": run.elapsed.as_millis(),
			})
		})
		.collect();
	let report = json!({
		"reference": {"snapshot": "tests/nix/tests/functional/lang", "commit": reference_commit},
		"candidate": {"evaluator": "nix-rs-eval", "version": env!("CARGO_PKG_VERSION"), "io": "impure", "mode": "strict"},
		"platform": format!("{}-{}", env::consts::ARCH, env::consts::OS),
		"environment": {
			"all": SHARED_ENV.iter().map(|(key, value)| (key.to_string(), json!(value))).collect::<serde_json::Map<_, _>>(),
			"eval_okay": EVAL_OKAY_ENV.iter().map(|(key, value)| (key.to_string(), json!(value))).collect::<serde_json::Map<_, _>>(),
			"all_cases": {"NIX_STATE_DIR": "per-run test directory"},
			"other_cases": {"HOME": "per-run test directory", "NIX_PATH": null},
		},
		"timeout_seconds": TIMEOUT.as_secs(),
		"totals": totals,
		"cases": entries,
	});
	let mut file = fs::File::create(REPORT)?;
	file.write_all(&serde_json::to_vec_pretty(&report).map_err(io::Error::other)?)
}

fn main() -> ExitCode {
	if let Ok(mode) = env::var(CHILD_VAR) {
		return match mode.as_str() {
			CHILD_INSTANTIATE => child_instantiate(env::args().skip(1).collect()),
			_ => panic!("unknown child mode {mode}"),
		};
	}
	// Cargo and nextest pass libtest arguments such as `--list`; this harness
	// has one test, which is not ignored.
	if env::args().any(|arg| arg == "--list") {
		if !env::args().any(|arg| arg == "--ignored") {
			println!("upstream_lang: test");
		}
		return ExitCode::SUCCESS;
	}

	let cases = match discover() {
		Ok(cases) => cases,
		Err(unclassified) => {
			eprintln!("unclassified language fixture files: {unclassified:?}");
			return ExitCode::FAILURE;
		}
	};
	let runs = run_all(&cases);
	if let Err(error) = write_report(&cases, &runs) {
		eprintln!("cannot write {REPORT}: {error}");
		return ExitCode::FAILURE;
	}

	let mut totals: BTreeMap<&str, usize> = BTreeMap::new();
	for run in &runs {
		*totals.entry(run.outcome.name()).or_default() += 1;
	}
	println!("{} language fixtures: {totals:?}", cases.len());
	println!("report: {REPORT}");

	if env::var(BLESS_VAR).is_ok_and(|value| value == "1") {
		write_expectations(&cases, &runs);
		println!("rewrote {EXPECTATIONS}");
		return ExitCode::SUCCESS;
	}

	let mut expected = read_expectations();
	let mut differences = Vec::new();
	for (case, run) in cases.iter().zip(&runs) {
		match expected.remove(&case.name) {
			Some(entry) if entry == run.entry() => {}
			Some(entry) => differences.push(format!(
				"{}: recorded {entry}, observed {}",
				case.name,
				run.entry()
			)),
			None => differences.push(format!(
				"{}: not in the expectation table (observed {})",
				case.name,
				run.entry()
			)),
		}
	}
	differences.extend(
		expected
			.into_keys()
			.map(|name| format!("{name}: recorded but no longer a fixture")),
	);

	if differences.is_empty() {
		println!("all outcomes match {EXPECTATIONS}");
		ExitCode::SUCCESS
	} else {
		eprintln!(
			"{} outcome(s) differ from {EXPECTATIONS}:",
			differences.len()
		);
		for difference in differences {
			eprintln!("  {difference}");
		}
		ExitCode::FAILURE
	}
}
