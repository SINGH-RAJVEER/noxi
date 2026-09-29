//! Compare a small pure-evaluation corpus with a selected CppNix binary.
//! This is development evidence, not the complete upstream language suite.

use std::env;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const CASES: &[(&str, &str)] = &[
	("integer", "1 + 2 * 3"),
	("boolean", "true && !false"),
	("string", "\"hello\" + \" world\""),
	("list", "[ 1 2 3 ]"),
	("attributes", "{ a = 1; b = [ 2 3 ]; }"),
	("inherit", "let x = 42; in { inherit x; }"),
	("function", "(x: x + 1) 41"),
	("lazy-branch", "if true then 1 else (1 / 0)"),
	("recursive-attributes", "rec { a = 1; b = a + 1; }"),
	("builtin", "builtins.length [ 1 2 3 ]"),
];
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

struct Captured {
	status: ExitStatus,
	stdout: Vec<u8>,
	stderr: Vec<u8>,
	timed_out: bool,
}

fn run(program: &Path, args: &[&str], home: &Path, timeout: Duration) -> Result<Captured, String> {
	let mut stdout = tempfile::tempfile().map_err(|error| format!("temporary stdout: {error}"))?;
	let mut stderr = tempfile::tempfile().map_err(|error| format!("temporary stderr: {error}"))?;
	let mut child = Command::new(program)
		.args(args)
		.current_dir(home)
		.env_clear()
		.env("PATH", env::var_os("PATH").unwrap_or_default())
		.env("HOME", home)
		.env("NIX_PATH", "")
		.env("NIX_CONFIG", "")
		.env("NIX_REMOTE", "dummy://")
		.env("NIX_STORE_DIR", "/nix/store")
		.env("NIX_CONF_DIR", home.join("config"))
		.env("NIX_USER_CONF_FILES", home.join("empty-user.conf"))
		.env("XDG_CONFIG_HOME", home.join("config"))
		.env("XDG_CONFIG_DIRS", home.join("config"))
		.env("XDG_CACHE_HOME", home.join("cache"))
		.env("XDG_DATA_HOME", home.join("data"))
		.env("LC_ALL", "C")
		.env("TZ", "UTC")
		.stdout(Stdio::from(
			stdout.try_clone().map_err(|error| error.to_string())?,
		))
		.stderr(Stdio::from(
			stderr.try_clone().map_err(|error| error.to_string())?,
		))
		.spawn()
		.map_err(|error| format!("cannot run {}: {error}", program.display()))?;
	let deadline = Instant::now()
		.checked_add(timeout)
		.ok_or("timeout is too large")?;
	let (status, timed_out) = loop {
		if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
			break (status, false);
		}
		if Instant::now() >= deadline {
			child
				.kill()
				.map_err(|error| format!("kill timed-out child: {error}"))?;
			break (child.wait().map_err(|error| error.to_string())?, true);
		}
		thread::sleep(Duration::from_millis(10));
	};
	stdout
		.seek(SeekFrom::Start(0))
		.map_err(|error| error.to_string())?;
	stderr
		.seek(SeekFrom::Start(0))
		.map_err(|error| error.to_string())?;
	let mut stdout_bytes = Vec::new();
	let mut stderr_bytes = Vec::new();
	stdout
		.read_to_end(&mut stdout_bytes)
		.map_err(|error| error.to_string())?;
	stderr
		.read_to_end(&mut stderr_bytes)
		.map_err(|error| error.to_string())?;
	Ok(Captured {
		status,
		stdout: stdout_bytes,
		stderr: stderr_bytes,
		timed_out,
	})
}

fn captured(output: &Captured) -> Value {
	json!({
		"exit_code": output.status.code(),
		"stdout": String::from_utf8_lossy(&output.stdout),
		"stderr": String::from_utf8_lossy(&output.stderr),
		"timed_out": output.timed_out,
	})
}

fn absolute_program(path: &str) -> Result<PathBuf, String> {
	let path = Path::new(path);
	let absolute = if path.is_absolute() {
		path.to_path_buf()
	} else {
		env::current_dir()
			.map_err(|error| error.to_string())?
			.join(path)
	};
	// Keep the final symlink intact: CppNix selects legacy commands by argv[0].
	fs::metadata(&absolute).map_err(|error| format!("{}: {error}", absolute.display()))?;
	Ok(absolute)
}

fn execute() -> Result<bool, String> {
	let mut args = env::args().skip(1);
	let (Some(reference), Some(candidate), Some(report), None) =
		(args.next(), args.next(), args.next(), args.next())
	else {
		return Err("usage: differential-eval REFERENCE_NIX_INSTANTIATE NIXRS REPORT.json".into());
	};
	let reference = absolute_program(&reference)?;
	let candidate = absolute_program(&candidate)?;
	let reference = Path::new(&reference);
	let candidate = Path::new(&candidate);
	let timeout = match env::var("NIXRS_DIFF_TIMEOUT_MS") {
		Ok(value) => Duration::from_millis(
			value
				.parse::<u64>()
				.map_err(|error| format!("NIXRS_DIFF_TIMEOUT_MS: {error}"))?,
		),
		Err(_) => DEFAULT_TIMEOUT,
	};
	let home = tempfile::tempdir().map_err(|error| format!("temporary HOME: {error}"))?;
	fs::create_dir_all(home.path().join("config"))
		.map_err(|error| format!("temporary Nix configuration: {error}"))?;
	let reference_version = run(reference, &["--version"], home.path(), timeout)?;
	let candidate_version = run(candidate, &["--version"], home.path(), timeout)?;
	if !reference_version.status.success() || !candidate_version.status.success() {
		return Err("cannot read reference or candidate version".into());
	}

	let mut all_match = true;
	let mut cases = Vec::with_capacity(CASES.len());
	for (name, expression) in CASES {
		let reference_output = run(
			reference,
			&["--eval", "--strict", "--expr", expression],
			home.path(),
			timeout,
		)?;
		let candidate_output = run(
			candidate,
			&["eval", "--expr", expression],
			home.path(),
			timeout,
		)?;
		let matches = !reference_output.timed_out
			&& !candidate_output.timed_out
			&& reference_output.status.code() == candidate_output.status.code()
			&& reference_output.stdout == candidate_output.stdout
			&& reference_output.stderr == candidate_output.stderr;
		all_match &= matches;
		cases.push(json!({
			"name": name,
			"expression": expression,
			"matches": matches,
			"reference": captured(&reference_output),
			"candidate": captured(&candidate_output),
		}));
	}
	let report_json = json!({
		"platform": env::consts::ARCH.to_owned() + "-" + env::consts::OS,
		"reference": {"path": reference, "version": String::from_utf8_lossy(&reference_version.stdout).trim()},
		"candidate": {"path": candidate, "version": String::from_utf8_lossy(&candidate_version.stdout).trim()},
		"reference_argv": ["--eval", "--strict", "--expr", "EXPRESSION"],
		"candidate_argv": ["eval", "--expr", "EXPRESSION"],
		"environment": {"HOME": "isolated temporary directory", "NIX_PATH": "", "NIX_CONFIG": "", "NIX_REMOTE": "dummy://", "NIX_STORE_DIR": "/nix/store", "NIX_CONF_DIR": "temporary empty directory", "NIX_USER_CONF_FILES": "temporary nonexistent file", "XDG_CONFIG_DIRS": "temporary empty directory", "PATH": env::var("PATH").unwrap_or_default(), "LC_ALL": "C", "TZ": "UTC"},
		"timeout_ms": timeout.as_millis(),
		"all_match": all_match,
		"cases": cases,
	});
	let serialized = serde_json::to_vec_pretty(&report_json)
		.map_err(|error| format!("serialize report: {error}"))?;
	fs::write(&report, serialized).map_err(|error| format!("write {report}: {error}"))?;
	println!(
		"{}: {}/{} cases match",
		report,
		cases.iter().filter(|case| case["matches"] == true).count(),
		cases.len()
	);
	Ok(all_match)
}

fn main() -> ExitCode {
	match execute() {
		Ok(true) => ExitCode::SUCCESS,
		Ok(false) => ExitCode::FAILURE,
		Err(error) => {
			eprintln!("error: {error}");
			ExitCode::from(2)
		}
	}
}
