//! Checks what noxi can do with CppNix 2.35.2's `tests/repl-completion.nix`.
//!
//! The fixture is a `runCommand` that drives `nix repl` through `expect`,
//! pressing Tab after expressions whose evaluation fails, and requires a
//! fresh prompt instead of a crash. It needs a terminal and `expect`, which
//! the development shell does not provide, and a `nix repl`. This test
//! reads the fixture, takes the command its `expect` script spawns, and
//! checks expression evaluation and error recovery through standard input.
//! This smoke check does not send terminal Tab events. The copied fixture
//! remains a known gap until a terminal-driven completion comparison runs.
//!
//! noxi's executables come from `NOXI_CANDIDATE_BIN` (default: this test's
//! Cargo profile directory).

use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const FIXTURE: &str = include_str!("../../../tests/nix/tests/repl-completion.nix");

fn candidate_nix() -> PathBuf {
	if let Some(dir) = env::var_os("NOXI_CANDIDATE_BIN") {
		return PathBuf::from(dir).join("nix");
	}
	let exe = env::current_exe().unwrap();
	exe.ancestors()
		.skip(1)
		.map(|dir| dir.join("nix"))
		.find(|nix| nix.is_file())
		.expect("build noxi-commands' binaries first or set NOXI_CANDIDATE_BIN")
}

#[test]
fn noxi_repl_recovers_from_fixture_expression_errors() {
	let spawn = FIXTURE
		.lines()
		.map(str::trim)
		.find_map(|line| line.strip_prefix("spawn nix "))
		.expect("repl-completion.nix spawns nix");
	assert_eq!(
		spawn,
		"repl --offline --extra-experimental-features nix-command"
	);
	let scratch = tempfile::tempdir().unwrap();
	let mut child = Command::new(candidate_nix())
		.args(spawn.split_whitespace())
		.env_clear()
		.env("HOME", scratch.path())
		.env("NIX_REMOTE", "dummy://")
		.current_dir(scratch.path())
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	// Exercise the fixture's missing import, force it, then evaluate again.
	child
		.stdin
		.take()
		.unwrap()
		.write_all(b"foo = import ./does-not-exist.nix\nfoo\n1 + 2\n:q\n")
		.unwrap();
	let output = child.wait_with_output().unwrap();
	let stderr = String::from_utf8_lossy(&output.stderr);
	let stdout = String::from_utf8_lossy(&output.stdout);
	assert!(
		output.status.success()
			// Values are coloured even off a terminal, as the reference prints them.
			&& stdout
				.lines()
				.any(|line| line.trim() == "3" || line.trim() == "\x1b[36;1m3\x1b[0m")
			&& stderr.contains("error:"),
		"stdin expression/error recovery smoke failed; terminal completion remains unqualified\n\
		 status: {:?}\nstdout: {stdout}\nstderr: {stderr}",
		output.status
	);
}
