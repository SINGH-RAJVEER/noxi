//! `nix-instantiate --eval --strict` against results recorded from the
//! CppNix 2.35.2 reference (`just reference`), run in read-only mode as the
//! reference does by default.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;

fn reference_instantiate() -> std::path::PathBuf {
	std::env::var_os("NIXRS_REFERENCE_BIN")
		.map(std::path::PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate")
}

#[test]
fn parse_output_matches_the_reference() {
	let dir = tempfile::tempdir().unwrap();
	let reference = reference_instantiate();
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	for expression in [
		"{ z.b = 1; z.a = 2; z = { c = 3; }; a = rec { inherit (builtins) map; }; }",
		"let b = 2; c = {}; in { f = 3; inherit (c) e d; inherit b; a = 1; }",
		"args@{ z ? 1, a, ... }: assert a; with args; if a then z else null",
		"[ (1 + 2 + 3) (1 - 2) (2 * 3) (4 / 2) (-1) (!true) (1 < 2) (1 <= 2) (2 > 1) (2 >= 1) ]",
		"[ (true && false) (true || false) (true -> false) (1 == 2) (1 != 2) ([] ++ []) ({} // {}) ]",
		"[ (builtins.map (x: x) []) ({} ? a.b) ({}.a.b or 1) <nix/a> __curPos ]",
		"[ \"a${1}b\" \"${1}\" ''\n  foo\n  bar\n'' http://example.com ]",
		"[ ./foo/../bar /foo/../bar ~/foo ./foo/${\"bar\"}/baz ]",
		"{ \"a b\" = 1; ${\"static\"} = 2; ${1 + 2} = 3; \"${\"dynamic\"}\" = 4; }",
		"let { body = 1; x = 2; }",
		"[ 0.0 1.23456789 1.0e6 0.0000001 ]",
	] {
		let run = |program: &Path| {
			Command::new(program)
				.env_clear()
				.env("HOME", dir.path())
				.env("NIX_CONF_DIR", dir.path())
				.env("NIX_USER_CONF_FILES", "")
				.env("NIX_CONFIG", "")
				.current_dir(dir.path())
				.args(["--parse", "--expr", expression])
				.output()
				.unwrap()
		};
		let expected = run(&reference);
		let actual = run(candidate);
		assert!(
			expected.status.success(),
			"reference {expression}: {expected:?}"
		);
		assert_eq!(
			actual.status.code(),
			expected.status.code(),
			"{expression}: {actual:?}"
		);
		assert_eq!(actual.stdout, expected.stdout, "{expression}");
		assert_eq!(actual.stderr, expected.stderr, "{expression}");
	}
}

#[test]
fn parser_lints_match_the_reference() {
	let dir = tempfile::tempdir().unwrap();
	let reference = reference_instantiate();
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	fs::write(
		dir.path().join("child.nix"),
		"[ /tmp/foo foo/bar http://example.com ]\n",
	)
	.unwrap();
	for expression in [
		"/tmp/foo",
		"builtins.isPath ~/foo",
		"[ foo/bar ./foo ../foo ]",
		"http://example.com",
		"[ \"http://example.com\" ./foo/${\"bar\"} ]",
		"let or = 1; in [ (x: x) or ]",
		"let or = 1; f = x: x; in [ (f or) (f (f or)) ]",
		"import ./child.nix",
	] {
		for severity in ["ignore", "warn", "fatal"] {
			let flags = [
				"--impure",
				"--lint-absolute-path-literals",
				severity,
				"--lint-short-path-literals",
				severity,
				"--lint-url-literals",
				severity,
			];
			// Imported fatal errors also acquire evaluation trace frames, owned by the diagnostic work.
			if expression.starts_with("import") && severity == "fatal" {
				continue;
			}
			let expected = message_evaluation(&reference, dir.path(), expression, "", &flags, "");
			let actual = message_evaluation(candidate, dir.path(), expression, "", &flags, "");
			assert_eq!(
				actual.status.code(),
				expected.status.code(),
				"{expression}, {severity}: {actual:?}"
			);
			assert_eq!(actual.stdout, expected.stdout, "{expression}, {severity}");
			assert_eq!(
				String::from_utf8_lossy(&actual.stderr),
				String::from_utf8_lossy(&expected.stderr),
				"{expression}, {severity}"
			);
		}
	}
	for flags in [
		vec!["--quiet", "--lint-absolute-path-literals", "warn"],
		vec!["--pure-eval", "--impure"],
	] {
		let expression = if flags[0] == "--quiet" {
			"/tmp/foo"
		} else {
			"builtins.isPath ~/foo"
		};
		let expected = message_evaluation(&reference, dir.path(), expression, "", &flags, "");
		let actual = message_evaluation(candidate, dir.path(), expression, "", &flags, "");
		assert_eq!(actual.status.code(), expected.status.code());
		assert_eq!(actual.stdout, expected.stdout);
		assert_eq!(actual.stderr, expected.stderr);
	}
}

/// Read-only, isolated evaluation with either engine, including the
/// configuration and environment that control warning behavior.
fn message_evaluation(
	program: &Path,
	dir: &Path,
	expression: &str,
	config: &str,
	flags: &[&str],
	abort_env: &str,
) -> std::process::Output {
	Command::new(program)
		.env_clear()
		.env("HOME", dir)
		.env("NIX_CONF_DIR", dir)
		.env("NIX_USER_CONF_FILES", "")
		.env("NIX_CONFIG", config)
		.env("NIX_PATH", "")
		.env("NIX_ABORT_ON_WARN", abort_env)
		.current_dir(dir)
		.args([
			"--store", "dummy://", "--eval", "--strict", "--expr", expression,
		])
		.args(flags)
		.output()
		.unwrap()
}

#[test]
fn warning_and_verbose_trace_behavior_matches_the_reference() {
	let dir = tempfile::tempdir().unwrap();
	fs::write(dir.path().join("ansi-message"), b"\x1b[31mred\x1b[0m").unwrap();
	let reference = reference_instantiate();
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	let cases: Vec<(&str, &str, Vec<&str>, &str)> = vec![
		(
			r#"builtins.warn (builtins.readFile ./ansi-message) 123"#,
			"",
			vec![],
			"",
		),
		(
			r#"builtins.trace (builtins.readFile ./ansi-message) 123"#,
			"",
			vec![],
			"",
		),
		(
			r#"builtins.traceVerbose (builtins.readFile ./ansi-message) 123"#,
			"",
			vec!["--trace-verbose"],
			"",
		),
		(r#"builtins.warn "Hello %" 123"#, "", vec![], ""),
		(
			r#"builtins.warn "line one\n\nline three  \n" 123"#,
			"",
			vec![],
			"",
		),
		(r#"builtins.warn "" 123"#, "", vec![], ""),
		(
			r#"builtins.warn "hidden" 123"#,
			"",
			vec!["--quiet", "--quiet", "--quiet"],
			"",
		),
		(
			r#"builtins.warn "shown" 123"#,
			"",
			vec!["--quiet", "--quiet", "--quiet", "-v"],
			"",
		),
		(
			r#"builtins.warn "not aborted" 123"#,
			"abort-on-warn = false\n",
			vec![],
			"yes",
		),
		(
			r#"builtins.warn "not aborted" 123"#,
			"",
			vec!["--no-abort-on-warn"],
			"true",
		),
		(r#"builtins.warn "not aborted" 123"#, "", vec![], "TRUE"),
		(
			r#"builtins.traceVerbose (throw "unused") 123"#,
			"",
			vec![],
			"",
		),
		(
			r#"builtins.traceVerbose "Hello" 123"#,
			"",
			vec!["--trace-verbose"],
			"",
		),
		(
			r#"builtins.traceVerbose "Hello" 123"#,
			"trace-verbose = true\n",
			vec![],
			"",
		),
		(
			r#"builtins.traceVerbose (throw "unused") 123"#,
			"trace-verbose = true\n",
			vec!["--no-trace-verbose"],
			"",
		),
		(
			r#"__traceVerbose "alias" 123"#,
			"",
			vec!["--trace-verbose"],
			"",
		),
		(r#"__warn "alias" 123"#, "", vec![], ""),
		(
			r#"builtins.tryEval (builtins.warn (throw "message") (throw "value"))"#,
			"",
			vec![],
			"",
		),
	];
	for (expression, config, flags, abort_env) in cases {
		let expected = message_evaluation(
			&reference,
			dir.path(),
			expression,
			config,
			&flags,
			abort_env,
		);
		assert!(
			expected.status.success(),
			"reference {expression}: {:?}",
			expected
		);
		let actual =
			message_evaluation(candidate, dir.path(), expression, config, &flags, abort_env);
		assert_eq!(
			(actual.status.code(), actual.stdout, actual.stderr),
			(expected.status.code(), expected.stdout, expected.stderr),
			"{expression} {flags:?}"
		);
	}
	// The legacy diagnostic renderer remains a separate gate. Check the
	// failure semantics here without treating its text as a matching result.
	for (expression, config, flags, abort_env) in [
		(
			r#"builtins.tryEval (builtins.warn "abort" (builtins.trace "unforced" 123))"#,
			"abort-on-warn = true\n",
			vec![],
			"",
		),
		(
			r#"builtins.warn "abort" 123"#,
			"",
			vec!["--abort-on-warn"],
			"",
		),
		(r#"builtins.warn "abort" 123"#, "", vec![], "1"),
		(r#"builtins.warn "abort" 123"#, "", vec![], "yes"),
		(r#"builtins.warn "abort" 123"#, "", vec![], "true"),
		(r#"builtins.tryEval (builtins.warn 1 123)"#, "", vec![], ""),
	] {
		let expected = message_evaluation(
			&reference,
			dir.path(),
			expression,
			config,
			&flags,
			abort_env,
		);
		let actual =
			message_evaluation(candidate, dir.path(), expression, config, &flags, abort_env);
		assert_eq!(expected.status.code(), Some(1), "reference {expression}");
		assert_eq!(actual.status.code(), expected.status.code(), "{expression}");
		assert!(actual.stdout.is_empty());
		if expression.contains("\"abort\"") {
			assert!(String::from_utf8_lossy(&actual.stderr).contains("abort-on-warn is set"));
			assert!(!String::from_utf8_lossy(&actual.stderr).contains("trace: unforced"));
		}
	}
}

#[test]
fn sequencing_matches_the_reference() {
	let dir = tempfile::tempdir().unwrap();
	let reference = reference_instantiate();
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	for expression in [
		r#"builtins.seq (builtins.trace "first" 1) (builtins.trace "second" 2)"#,
		r#"builtins.tryEval (builtins.seq (throw "first") (abort "second"))"#,
		r#"builtins.seq [ (throw "unforced element") ] 2"#,
		r#"builtins.seq { x = throw "unforced attribute"; } 2"#,
	] {
		let expected = message_evaluation(&reference, dir.path(), expression, "", &[], "");
		let actual = message_evaluation(candidate, dir.path(), expression, "", &[], "");
		assert!(
			expected.status.success(),
			"reference {expression}: {expected:?}"
		);
		assert_eq!(
			(actual.status.code(), actual.stdout, actual.stderr),
			(expected.status.code(), expected.stdout, expected.stderr),
			"{expression}"
		);
	}
}

#[test]
fn scoped_import_matches_the_reference() {
	let dir = tempfile::tempdir().unwrap();
	fs::create_dir(dir.path().join("nested")).unwrap();
	fs::write(
		dir.path().join("nested/default.nix"),
		r#"[ x (f 5) (import ./child.nix) ]"#,
	)
	.unwrap();
	fs::write(dir.path().join("nested/child.nix"), "7").unwrap();
	fs::write(
		dir.path().join("shadow.nix"),
		"[ builtins.x true null import ]",
	)
	.unwrap();
	fs::write(dir.path().join("memo.nix"), r#"builtins.trace "scoped" x"#).unwrap();
	fs::write(
		dir.path().join("plain.nix"),
		r#"builtins.trace "ordinary" 3"#,
	)
	.unwrap();
	fs::write(dir.path().join("empty.nix"), "1").unwrap();
	fs::write(dir.path().join("nested/base.nix"), "./.").unwrap();
	symlink("nested/base.nix", dir.path().join("file-link.nix")).unwrap();
	symlink("nested", dir.path().join("dir-link")).unwrap();
	symlink("file-link.nix", dir.path().join("chain.nix")).unwrap();
	symlink("plain.nix", dir.path().join("plain-link.nix")).unwrap();
	fs::create_dir(dir.path().join("default-link")).unwrap();
	symlink(
		"../nested/base.nix",
		dir.path().join("default-link/default.nix"),
	)
	.unwrap();
	let reference = reference_instantiate();
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	for expression in [
		r#"[ (import ./file-link.nix) (import ./chain.nix) (import ./dir-link/base.nix) (import ./default-link) ]"#,
		r#"[ (scopedImport {} ./file-link.nix) (scopedImport {} ./chain.nix) (scopedImport {} ./dir-link/base.nix) (scopedImport {} ./default-link) ]"#,
		r#"[ (import ./plain.nix) (import ./plain-link.nix) ]"#,
		r#"scopedImport { x = 4; f = v: v + 1; } ./dir-link"#,
		r#"scopedImport { x = 4; f = v: v + 1; unused = throw "unused"; } ./nested"#,
		r#"builtins.scopedImport { x = 4; f = v: v + 1; } ./nested/default.nix"#,
		r#"scopedImport { builtins = { x = 9; }; true = 10; null = 11; import = 12; } ./shadow.nix"#,
		r#"[ (scopedImport { x = 1; } ./memo.nix) (scopedImport { x = 2; } ./memo.nix) ]"#,
		r#"[ (import ./plain.nix) (import ./plain.nix) ]"#,
		r#"scopedImport (builtins.trace "scope" {}) (builtins.trace "path" ./empty.nix)"#,
		r#"builtins.tryEval (scopedImport (throw "unused scope") (throw "path"))"#,
	] {
		let expected = message_evaluation(&reference, dir.path(), expression, "", &[], "");
		let actual = message_evaluation(candidate, dir.path(), expression, "", &[], "");
		assert!(
			expected.status.success(),
			"reference {expression}: {:?}",
			expected
		);
		assert_eq!(
			(actual.status.code(), actual.stdout, actual.stderr),
			(expected.status.code(), expected.stdout, expected.stderr),
			"{expression}"
		);
	}
}

#[test]
fn mapped_store_imports_match_the_reference() {
	let source = tempfile::tempdir().unwrap();
	let directory = source.path().join("expression");
	fs::create_dir(&directory).unwrap();
	fs::write(directory.join("default.nix"), "import ./child.nix").unwrap();
	fs::write(directory.join("child.nix"), "[ 17 ./. ]").unwrap();
	fs::write(directory.join("scope.nix"), "x + import ./value.nix").unwrap();
	fs::write(directory.join("value.nix"), "4").unwrap();
	symlink("child.nix", directory.join("link.nix")).unwrap();
	let reference_root = tempfile::tempdir().unwrap();
	let candidate_root = tempfile::tempdir().unwrap();
	let reference = reference_instantiate();
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	for expression in [
		r#"let p = builtins.path { path = ./expression; }; in import p"#,
		r#"let p = builtins.path { path = ./expression; }; in import "${p}/link.nix""#,
		r#"let p = builtins.path { path = ./expression; }; in scopedImport { x = 8; } "${p}/scope.nix""#,
	] {
		let run = |program: &Path, root: &Path| {
			Command::new(program)
				.env_clear()
				.env("HOME", source.path())
				.env("NIX_CONF_DIR", source.path())
				.env("NIX_USER_CONF_FILES", "")
				.env("NIX_PATH", "")
				.current_dir(source.path())
				.args([
					"--store",
					&format!("local?root={}", root.display()),
					"--read-write-mode",
					"--eval",
					"--strict",
					"--expr",
					expression,
				])
				.output()
				.unwrap()
		};
		let expected = run(&reference, reference_root.path());
		let actual = run(candidate, candidate_root.path());
		assert!(
			expected.status.success(),
			"reference {expression}: {expected:?}"
		);
		assert_eq!(
			(actual.status.code(), actual.stdout, actual.stderr),
			(expected.status.code(), expected.stdout, expected.stderr),
			"{expression}"
		);
	}
}

/// Compute-only tests use the dummy store. Explicit local-store tests use
/// disposable roots, and an explicitly selected daemon points nowhere.
fn nix_instantiate() -> Command {
	let mut command = Command::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	command
		.env("NIX_DAEMON_SOCKET_PATH", "/nonexistent/nix-rs-test.sock")
		.env("NIX_REMOTE", "dummy://");
	command
}

#[test]
fn instantiation_registers_drv_bytes_in_a_diverted_store() {
	let root = tempfile::tempdir().unwrap();
	let expr = r#"derivation { name = "a"; system = "x"; builder = "/bin/sh"; }"#;
	let output = nix_instantiate()
		.args([
			"--store",
			&format!("local?root={}", root.path().display()),
			"-E",
			expr,
		])
		.output()
		.unwrap();
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert_eq!(
		output.stdout,
		b"/nix/store/l9ldjc53ry43x24hvq73jx7dx0489mhj-a.drv\n"
	);
	let bytes = fs::read(
		root.path()
			.join("nix/store/l9ldjc53ry43x24hvq73jx7dx0489mhj-a.drv"),
	)
	.unwrap();
	assert!(bytes.starts_with(b"Derive("));
	assert!(root.path().join("nix/var/nix/db/db.sqlite").is_file());
	let invalid = nix_instantiate()
		.args([
			"--store",
			&format!("local?root={}", root.path().display()),
			"-E",
			r#"{ type = "derivation"; drvPath = "/nix/store/00000000000000000000000000000000-missing.drv"; }"#,
		])
		.output()
		.unwrap();
	assert!(!invalid.status.success());
	assert!(invalid.stdout.is_empty());
	// A root must be a directory outside /nix, and a daemon must be
	// reachable. Neither touches a store.
	let output = nix_instantiate()
		.args(["--store", "local?root=/", "-E", expr])
		.output()
		.unwrap();
	assert!(!output.status.success());
	assert_eq!(
		String::from_utf8_lossy(&output.stderr),
		"error: store root '/' must be a directory outside /nix\n"
	);
	let output = nix_instantiate()
		.args(["--store", "daemon", "-E", expr])
		.output()
		.unwrap();
	assert!(!output.status.success());
	assert!(String::from_utf8_lossy(&output.stderr).contains(
		"cannot connect to socket at '/nonexistent/nix-rs-test.sock': No such file or directory"
	));
}

#[test]
fn evaluation_registers_paths_only_with_read_write_mode() {
	let root = tempfile::tempdir().unwrap();
	let uri = format!("local?root={}", root.path().display());
	let mut command = nix_instantiate();
	command.args([
		"--store",
		&uri,
		"--eval",
		"-E",
		r#"builtins.toFile "x" "y""#,
	]);
	let readonly = command.output().unwrap();
	assert!(readonly.status.success());
	assert!(root.path().join("nix/var/nix/db/db.sqlite").exists());
	assert!(
		!root
			.path()
			.join("nix/store/lfngsssysp6h1v4ccqg23c52s9sjl779-x")
			.exists()
	);
	let writable = command.arg("--read-write-mode").output().unwrap();
	assert!(
		writable.status.success(),
		"{}",
		String::from_utf8_lossy(&writable.stderr)
	);
	assert_eq!(readonly.stdout, writable.stdout);
	assert_eq!(
		fs::read(
			root.path()
				.join("nix/store/lfngsssysp6h1v4ccqg23c52s9sjl779-x")
		)
		.unwrap(),
		b"y"
	);
}

#[test]
fn readonly_store_selection_matches_reference_initialization_without_registering_values() {
	let root = tempfile::tempdir().unwrap();
	let reference_root = tempfile::tempdir().unwrap();
	let logical = "/nix/store/00000000000000000000000000000000-fixture";
	let physical_store = root.path().join("nix/store");
	fs::create_dir_all(&physical_store).unwrap();
	let fixture = physical_store.join("00000000000000000000000000000000-fixture");
	fs::write(&fixture, "diverted fixture").unwrap();
	let reference_fixture = reference_root
		.path()
		.join("nix/store/00000000000000000000000000000000-fixture");
	fs::create_dir_all(reference_fixture.parent().unwrap()).unwrap();
	fs::write(&reference_fixture, "diverted fixture").unwrap();
	let before = fs::metadata(&fixture).unwrap();
	let expr = format!(
		r#"builtins.seq (builtins.toFile "not-written" "text") (builtins.seq ((derivation {{ name = "not-written"; system = "x"; builder = "/bin/sh"; }}).drvPath) (builtins.readFile "{logical}"))"#
	);
	let uri = format!("local?root={}", root.path().display());
	for (expr, expected, status) in [
		("1".to_owned(), "1\n", 0),
		(expr, "\"diverted fixture\"\n", 0),
		(format!(r#"builtins.pathExists "{logical}""#), "true\n", 0),
		(
			"builtins.attrNames (builtins.readDir \"/nix/store\")".to_owned(),
			"",
			1,
		),
	] {
		let output = nix_instantiate()
			.args(["--store", &uri, "--eval", "--strict", "-E", &expr])
			.output()
			.unwrap();
		assert_eq!(output.status.code(), Some(status), "{output:?}");
		let reference = Command::new(reference_instantiate())
			.env_clear()
			.env("HOME", reference_root.path())
			.env("NIX_CONF_DIR", reference_root.path())
			.env("NIX_USER_CONF_FILES", "")
			.env("NIX_CONFIG", "build-users-group =\nsubstituters =\n")
			.args([
				"--store",
				&format!("local?root={}", reference_root.path().display()),
				"--eval",
				"--strict",
				"-E",
				&expr,
			])
			.output()
			.unwrap();
		assert_eq!(reference.status.code(), Some(status), "{reference:?}");
		assert_eq!(&output.stdout, &reference.stdout);
		if status == 0 {
			assert_eq!(&output.stderr, &reference.stderr);
		} else {
			assert!(
				String::from_utf8_lossy(&output.stderr)
					.contains("path '/' is not a valid store path")
			);
			assert!(
				String::from_utf8_lossy(&reference.stderr)
					.contains("path '/' is not a valid store path")
			);
		}
		assert_eq!(output.stdout, expected.as_bytes());
		assert!(root.path().join("nix/var/nix/db/db.sqlite").exists());
		assert!(
			reference_root
				.path()
				.join("nix/var/nix/db/db.sqlite")
				.exists()
		);
	}
	assert!(root.path().join("nix/var/nix/db/db.sqlite").exists());
	assert!(
		reference_root
			.path()
			.join("nix/var/nix/db/db.sqlite")
			.exists()
	);
	assert_eq!(fs::read_dir(&physical_store).unwrap().count(), 2);
	assert_eq!(fs::read(&fixture).unwrap(), b"diverted fixture");
	let after = fs::metadata(&fixture).unwrap();
	assert_eq!(before.permissions().mode(), after.permissions().mode());
	assert_eq!(before.modified().unwrap(), after.modified().unwrap());
}

#[test]
fn instantiation_forces_each_selection_once() {
	let root = tempfile::tempdir().unwrap();
	let uri = format!("local?root={}", root.path().display());
	let output = nix_instantiate()
		.args([
			"--store", &uri, "-E",
			r#"builtins.trace "selection-forced" (derivation { name = "a"; system = "x"; builder = "/bin/sh"; })"#,
		]).output().unwrap();
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	assert_eq!(
		output.stdout,
		b"/nix/store/l9ldjc53ry43x24hvq73jx7dx0489mhj-a.drv\n"
	);
	assert_eq!(
		String::from_utf8_lossy(&output.stderr)
			.matches("selection-forced")
			.count(),
		1
	);
}

/// Expression, reference stdout, reference exit status. The expressions run
/// in a directory holding the files `fixture_dir` creates. An empty stdout
/// with status 1 is an error in both engines: a `sha256` mismatch, a
/// dependency on a `.drv` that is not in the store, and a fixed-output
/// derivation with two outputs.
const CASES: &[(&str, &str, i32)] = &[
	(
		r#"(derivation { name = "a"; system = "x"; builder = "/bin/sh"; }).drvPath"#,
		r#""/nix/store/l9ldjc53ry43x24hvq73jx7dx0489mhj-a.drv""#,
		0,
	),
	(
		r#"(derivation { name = "a"; system = "x"; builder = "/bin/sh"; outputs = [ "out" "dev" ]; }).dev.outPath"#,
		r#""/nix/store/gqlnmlw49vf82if469f34bk8i241y1b9-a-dev""#,
		0,
	),
	(
		r#"builtins.placeholder "out""#,
		r#""/1rz4g4znpzjwh1xymhjpm42vipw92pr73vdgl6xs1hycac8kf2n9""#,
		0,
	),
	(
		r#"builtins.toFile "x" "y""#,
		r#""/nix/store/lfngsssysp6h1v4ccqg23c52s9sjl779-x""#,
		0,
	),
	(
		r#"builtins.toFile "t" "${builtins.toFile "x" "y"}""#,
		r#""/nix/store/c19nw30gabx0jlzpkbml41lvg9nf8wq7-t""#,
		0,
	),
	(
		r#""${./f.txt}""#,
		r#""/nix/store/qiwmmmgnk4xmy6mma2man57iwykzv7rg-f.txt""#,
		0,
	),
	(
		r#""${./link}""#,
		r#""/nix/store/kjy134i63lpm1gwhvsmzvrx6cwsq5yd1-link""#,
		0,
	),
	(
		r#""${./run.sh}""#,
		r#""/nix/store/78sw4kgnlwlwp82n9crq3226z2pb1g91-run.sh""#,
		0,
	),
	(
		r#"builtins.path { path = ./link; }"#,
		r#""/nix/store/pvl8chzg2l5l8a26fimfkwqjsjwwwnbs-link""#,
		0,
	),
	(
		r#"builtins.path { path = ./f.txt; recursive = false; }"#,
		r#""/nix/store/7m9jlj34ns6r66ablfz6dxbnsazck0sa-f.txt""#,
		0,
	),
	(
		r#"builtins.path { path = ./.; name = "src"; filter = p: t: t != "symlink"; }"#,
		r#""/nix/store/rsa2yydlw4bmh00g5nf9jqnr7j5xgabk-src""#,
		0,
	),
	(
		r#"builtins.path { path = ./sub; name = "src"; sha256 = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="; }"#,
		r#""#,
		1,
	),
	(r#"builtins.storeDir"#, r#""/nix/store""#, 0),
	(
		r#"builtins.getContext (builtins.toFile "x" "y")"#,
		r#"{ "/nix/store/lfngsssysp6h1v4ccqg23c52s9sjl779-x" = { path = true; }; }"#,
		0,
	),
	(
		r#"let a = derivation { name = "a"; system = "x"; builder = "/bin/sh"; }; in (derivation { name = "b"; system = "x"; builder = "/bin/sh"; dep = a; }).drvPath"#,
		r#""/nix/store/fhrz09f778y6mrpg5a0mvw5m76hl683c-b.drv""#,
		0,
	),
	(
		r#"let a = derivation { name = "a"; system = "x"; builder = "/bin/sh"; }; in (derivation { name = "b"; system = "x"; builder = "/bin/sh"; dep = a.drvPath; }).drvPath"#,
		r#""#,
		1,
	),
	(
		r#"(derivation { name = "fo"; system = "x"; builder = "/bin/sh"; outputHash = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="; outputHashMode = "recursive"; }).outPath"#,
		r#""/nix/store/9i0irz866pivqbr2zpg5k0kljwir4fh9-fo""#,
		0,
	),
	(
		r#"let f = derivation { name = "fo"; system = "x"; builder = "/bin/sh"; outputHash = "0000000000000000000000000000000000000000000000000000"; outputHashAlgo = "sha256"; }; in (derivation { name = "b"; system = "x"; builder = "/bin/sh"; src = f; }).drvPath"#,
		r#""/nix/store/6ldgd34b0rrh5mqhps8cgi7s3h4f9c8z-b.drv""#,
		0,
	),
	(
		r#"(derivation { name = "fo"; system = "x"; builder = "/bin/sh"; outputs = [ "out" "dev" ]; outputHash = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="; }).drvPath"#,
		r#""#,
		1,
	),
	(
		r#"(derivation { name = "sa"; system = "x"; builder = "/bin/sh"; __structuredAttrs = true; a = [ 1 2 { b = "c"; } ]; }).drvPath"#,
		r#""/nix/store/7qsr5vn0qc27grq59w8pdbkww9ghqx6p-sa.drv""#,
		0,
	),
	(
		r#"(derivation { name = "n"; system = "x"; builder = "/bin/sh"; args = [ "-c" ./f.txt ]; x = null; y = true; z = [ 1 "a" ]; }).drvPath"#,
		r#""/nix/store/p5g9wzmm9nqrvbirgixxpy599lwf5426-n.drv""#,
		0,
	),
	(
		r#"(derivation { name = "n"; system = "x"; builder = "/bin/sh"; __ignoreNulls = true; x = null; }).drvPath"#,
		r#""/nix/store/83bkihfwajs19k9cs749z6j8bvw2699z-n.drv""#,
		0,
	),
	(
		r#"(derivation { name = "o"; system = "x"; builder = "/bin/sh"; out = "ignored"; }).drvPath"#,
		r#""/nix/store/b19hbcdwjgykbd1djlsm4355f8ih5crj-o.drv""#,
		0,
	),
	(
		r#"(import <nix/fetchurl.nix> { url = "http://example.org/a"; sha256 = "0000000000000000000000000000000000000000000000000000"; }).drvPath"#,
		r#""/nix/store/vkcksp0mqaypwmmwi4wa66lpkwwkmqhc-a.drv""#,
		0,
	),
	(
		r#"builtins.getContext (derivation { name = "a"; system = "x"; builder = "/bin/sh"; }).outPath"#,
		r#"{ "/nix/store/l9ldjc53ry43x24hvq73jx7dx0489mhj-a.drv" = { outputs = [ "out" ]; }; }"#,
		0,
	),
	// A `throw` in any attribute stays catchable.
	(
		r#"builtins.tryEval (derivation { name = "a"; system = "x"; builder = "/bin/sh"; __ignoreNulls = throw "s"; }).drvPath"#,
		"{ success = false; value = false; }",
		0,
	),
	(
		r#"builtins.tryEval (derivation { name = "a"; system = "x"; builder = "/bin/sh"; __structuredAttrs = throw "s"; }).drvPath"#,
		"{ success = false; value = false; }",
		0,
	),
	(
		r#"builtins.tryEval (derivation { name = "a"; system = "x"; builder = "/bin/sh"; args = throw "s"; }).drvPath"#,
		"{ success = false; value = false; }",
		0,
	),
	(
		r#"builtins.tryEval (derivation { name = "a"; system = "x"; builder = "/bin/sh"; outputs = [ (throw "s") ]; }).drvPath"#,
		"{ success = false; value = false; }",
		0,
	),
	(
		r#"builtins.tryEval (derivation { name = "a"; system = "x"; builder = "/bin/sh"; __structuredAttrs = true; foo = { x = throw "s"; }; }).drvPath"#,
		"{ success = false; value = false; }",
		0,
	),
];

fn fixture_dir(dir: &Path) {
	fs::write(dir.join("f.txt"), "hi\n").unwrap();
	symlink("f.txt", dir.join("link")).unwrap();
	fs::write(dir.join("run.sh"), "#!/bin/sh\n").unwrap();
	fs::set_permissions(dir.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
	fs::create_dir(dir.join("sub")).unwrap();
	fs::write(dir.join("sub/a"), "x").unwrap();
}

fn instantiate(cwd: &Path, args: &[&str]) -> (String, i32) {
	let output = nix_instantiate()
		.args(args)
		.current_dir(cwd)
		.env("NIX_PATH", "")
		.output()
		.unwrap();
	(
		String::from_utf8(output.stdout).unwrap(),
		output.status.code().unwrap(),
	)
}

#[test]
fn matches_reference_store_paths() {
	let dir = tempfile::tempdir().unwrap();
	fixture_dir(dir.path());
	let mut mismatches = Vec::new();
	for (expression, expected, status) in CASES {
		let (stdout, code) = instantiate(dir.path(), &["--eval", "--strict", "--expr", expression]);
		if stdout.trim_end() != *expected || code != *status {
			mismatches.push(format!(
				"{expression}\n  expected [{status}] {expected}\n  actual   [{code}] {}",
				stdout.trim_end()
			));
		}
	}
	assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn selects_attributes_and_passes_arguments() {
	let dir = tempfile::tempdir().unwrap();
	fs::write(dir.path().join("f.nix"), "{ a ? 1 }: { b = a; }").unwrap();
	// Recorded from the reference: the file alone is not called, a
	// selection calls it, and `--arg`/`--argstr` also call the result.
	let cases: &[(&[&str], &str)] = &[
		(&["--eval", "f.nix"], "<LAMBDA>"),
		(&["--eval", "f.nix", "-A", "b"], "1"),
		(&["--eval", "f.nix", "--arg", "a", "5", "-A", "b"], "5"),
		(
			&["--eval", "--strict", "f.nix", "--argstr", "a", "hi"],
			"{ b = \"hi\"; }",
		),
		(&["--eval", "-E", "{ ... }: { b = 2; }", "-A", "b"], "2"),
		(
			&["--eval", "-E", "{ x = [ 1 { y = 2; } ]; }", "-A", "x.1.y"],
			"2",
		),
		(
			&[
				"--eval",
				"--strict",
				"-E",
				"{ a = 1; }",
				"-A",
				"a",
				"-A",
				"a",
			],
			"1\n1",
		),
	];
	for (args, expected) in cases {
		let (stdout, code) = instantiate(dir.path(), args);
		assert_eq!((stdout.trim_end(), code), (*expected, 0), "{args:?}");
	}
	let (_, code) = instantiate(dir.path(), &["--eval", "-E", "x: { b = 2; }", "-A", "b"]);
	assert_eq!(code, 1);
}

#[test]
fn rejects_what_needs_a_writable_store() {
	let dir = tempfile::tempdir().unwrap();
	let (_, code) = instantiate(dir.path(), &["-E", "1"]);
	assert_eq!(code, 1);
	let (_, code) = instantiate(dir.path(), &["--eval", "--no-such-flag"]);
	assert_eq!(code, 1);
}

/// `--xml --no-location`, the last of `--json` and `--xml` winning, and `-I`
/// entries searched before `NIX_PATH`, as recorded from the reference.
#[test]
fn prints_xml_and_searches_include_paths() {
	let dir = tempfile::tempdir().unwrap();
	let (stdout, code) = instantiate(
		dir.path(),
		&[
			"--eval",
			"--xml",
			"--no-location",
			"--strict",
			"-E",
			"{ a = [ 1 \"x\" true null ]; b = 1.5; }",
		],
	);
	assert_eq!(code, 0);
	assert_eq!(
		stdout,
		"<?xml version='1.0' encoding='utf-8'?>
<expr>
  <attrs>
    <attr name=\"a\">
      <list>
        <int value=\"1\" />
        <string value=\"x\" />
        <bool value=\"true\" />
        <null />
      </list>
    </attr>
    <attr name=\"b\">
      <float value=\"1.5\" />
    </attr>
  </attrs>
</expr>
"
	);
	let (stdout, code) = instantiate(dir.path(), &["--eval", "--xml", "--json", "-E", "1"]);
	assert_eq!((stdout.as_str(), code), ("1\n", 0));
	let (stdout, code) = instantiate(dir.path(), &["--eval", "--json", "-E", "{ a = [ 1 ]; }"]);
	assert_eq!((stdout.as_str(), code), ("{\"a\":[1]}\n", 0));
	// Source locations in XML output are not implemented.
	let (_, code) = instantiate(dir.path(), &["--eval", "--xml", "-E", "1"]);
	assert_eq!(code, 1);

	for (sub, text) in [
		("d1/x.nix", "\"one\""),
		("d2/x.nix", "\"two\""),
		("d2/y.nix", "\"y2\""),
	] {
		let path = dir.path().join(sub);
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(path, text).unwrap();
	}
	let output = nix_instantiate()
		.args(["--eval", "--strict", "--show-trace", "-I", "d1", "-E"])
		.arg("[ (import <x.nix>) (import <y.nix>) ]")
		.current_dir(dir.path())
		.env("NIX_PATH", "d2")
		.output()
		.unwrap();
	assert_eq!(
		(
			String::from_utf8_lossy(&output.stdout).as_ref(),
			output.status.code()
		),
		("[ \"one\" \"y2\" ]\n", Some(0))
	);
}

/// `--find-file` and `<...>` lookups as recorded from the reference: `-I`
/// entries before `NIX_PATH`, prefixes matched at `/`, symlinks resolved,
/// a missing entry warned about once, a rebound `__nixPath`, and the error.
#[test]
fn finds_files_in_the_lookup_path() {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path().canonicalize().unwrap();
	fs::create_dir_all(root.join("a/sub")).unwrap();
	fs::write(root.join("a/sub/x.nix"), "1").unwrap();
	fs::create_dir_all(root.join("b")).unwrap();
	fs::write(root.join("b/y.nix"), "2").unwrap();
	symlink(root.join("b"), root.join("link")).unwrap();
	let run = |nix_path: &str, args: &[&str]| {
		let output = nix_instantiate()
			.args(args)
			.current_dir(&root)
			.env("HOME", &root)
			.env("NIX_STATE_DIR", root.join("var/nix"))
			.env("NIX_PATH", nix_path)
			.output()
			.unwrap();
		(
			String::from_utf8(output.stdout).unwrap(),
			String::from_utf8(output.stderr).unwrap(),
			output.status.code().unwrap(),
		)
	};
	let r = root.display();
	let (stdout, _, code) = run(
		"p=link:/nonexistent",
		&["-I", "a", "--find-file", "sub/x.nix", "p/y.nix", "p"],
	);
	assert_eq!(
		(stdout, code),
		(format!("{r}/a/sub/x.nix\n{r}/b/y.nix\n{r}/b\n"), 0)
	);
	let (stdout, stderr, code) = run("p=a:/nonexistent", &["--find-file", "p-x"]);
	assert_eq!(
		(stdout.as_str(), stderr.as_str(), code),
		(
			"",
			"warning: Nix search path entry '/nonexistent' does not exist, ignoring\n\
			error: file 'p-x' was not found in the Nix search path (add it using $NIX_PATH or -I)\n",
			1
		)
	);
	let (stdout, _, code) = run(
		"a",
		&[
			"--eval",
			"--strict",
			"-E",
			"[ (import <sub/x.nix>) (let __nixPath = [ { path = ./b; } ]; in import <y.nix>) __nixPath ]",
		],
	);
	assert_eq!(
		(stdout, code),
		(
			"[ 1 2 [ { path = \"a\"; prefix = \"\"; } ] ]\n".to_owned(),
			0
		)
	);
}

/// Derivation and output paths of real Nixpkgs packages, recorded from the
/// reference for the root flake's locked Nixpkgs (`714a5f8c`). A matching
/// `.drv` path means the whole derivation graph below it, byte for byte.
/// Set `NIXRS_NIXPKGS` to that source; `just nixpkgs-paths` does.
#[test]
#[ignore = "needs NIXRS_NIXPKGS; run with `just nixpkgs-paths`"]
fn matches_reference_nixpkgs_paths() {
	let nixpkgs = std::env::var("NIXRS_NIXPKGS").expect("NIXRS_NIXPKGS");
	let cases = [
		(
			"hello.drvPath",
			"/nix/store/gdxx5ilpli16l40mg2z9vs4b0w6wa1gf-hello-2.12.3.drv",
		),
		(
			"hello.outPath",
			"/nix/store/xl1h9i29pgq2q5cszjhm5wpfxfbbqwyi-hello-2.12.3",
		),
		(
			"stdenv.drvPath",
			"/nix/store/r5ssr70bc8l2hmjn56mnpbpgr9dizn40-stdenv-linux.drv",
		),
		(
			"ripgrep.drvPath",
			"/nix/store/1hiqcb1xg6k56wnv6b5d9djgwg5dl9lh-ripgrep-15.2.0.drv",
		),
		(
			"python3.drvPath",
			"/nix/store/a477qxskr7lac8a9ya5wdxd4263ldp4z-python3-3.14.7.drv",
		),
		(
			"git.outPath",
			"/nix/store/msr1v91ybfw6j12rs5mfl8ghb2rqsnsr-git-2.55.0",
		),
	];
	let dir = tempfile::tempdir().unwrap();
	for (attr, expected) in cases {
		let args = [
			"--eval",
			"--strict",
			&nixpkgs,
			"-A",
			attr,
			"--argstr",
			"system",
			"x86_64-linux",
		];
		let (stdout, code) = instantiate(dir.path(), &args);
		assert_eq!(
			(stdout.trim_end(), code),
			(format!("\"{expected}\"").as_str(), 0),
			"{attr}"
		);
	}
}

/// The qualification VM's NixOS configuration from
/// `tests/qualification/flake.nix`, without its `nix.package`, which comes
/// from a flake input. It uses Home Manager as a NixOS module and is
/// evaluated through `nixos/lib/eval-config.nix`, as `nixosSystem` does.
const QUALIFICATION_SYSTEM: &str = r#"{ nixpkgs, homeManager }:
let
  system = import "${nixpkgs}/nixos/lib/eval-config.nix" {
    system = "x86_64-linux";
    modules = [
      "${nixpkgs}/nixos/modules/virtualisation/qemu-vm.nix"
      (import "${homeManager}/nixos")
      ({ ... }: {
        system.stateVersion = "26.05";
        networking.hostName = "nixrs-reference";
        boot.kernelParams = [ "console=ttyS0" ];
        boot.loader.grub.efiSupport = true;
        boot.loader.grub.efiInstallAsRemovable = true;
        virtualisation.memorySize = 2048;
        virtualisation.diskSize = 8192;
        virtualisation.useBootLoader = true;
        virtualisation.useEFIBoot = true;
        nix.settings.experimental-features = [ "nix-command" "flakes" ];
        users.users.qualification.isNormalUser = true;
        home-manager.useGlobalPkgs = true;
        home-manager.users.qualification = { ... }: {
          home.stateVersion = "26.05";
        };
      })
    ];
  };
  standalone = import "${homeManager}/modules" {
    pkgs = import nixpkgs { system = "x86_64-linux"; };
    configuration = { pkgs, ... }: {
      home.username = "qualification";
      home.homeDirectory = "/home/qualification";
      home.stateVersion = "26.05";
      home.packages = [ pkgs.hello ];
      programs.git.enable = true;
    };
  };
in
[
  system.config.system.build.toplevel.drvPath
  system.config.system.build.vm.drvPath
  system.config.home-manager.users.qualification.home.activationPackage.drvPath
  standalone.activationPackage.drvPath
]
"#;

/// Derivation paths of the qualification NixOS system, its VM, its Home
/// Manager generation and a standalone Home Manager generation, recorded
/// from the reference for the qualification flake's locked Nixpkgs
/// (`e94cb152`) and Home Manager (`7b4c5ec4`). Set
/// `NIXRS_QUALIFICATION_NIXPKGS` and `NIXRS_QUALIFICATION_HOME_MANAGER` to
/// those sources; `just nixos-paths` does.
#[test]
#[ignore = "needs the qualification inputs; run with `just nixos-paths`"]
fn matches_reference_nixos_paths() {
	let nixpkgs =
		std::env::var("NIXRS_QUALIFICATION_NIXPKGS").expect("NIXRS_QUALIFICATION_NIXPKGS");
	let home_manager = std::env::var("NIXRS_QUALIFICATION_HOME_MANAGER")
		.expect("NIXRS_QUALIFICATION_HOME_MANAGER");
	let dir = tempfile::tempdir().unwrap();
	fs::write(dir.path().join("system.nix"), QUALIFICATION_SYSTEM).unwrap();
	let args = [
		"--eval",
		"--strict",
		"system.nix",
		"--argstr",
		"nixpkgs",
		&nixpkgs,
		"--argstr",
		"homeManager",
		&home_manager,
	];
	let (stdout, code) = instantiate(dir.path(), &args);
	assert_eq!(
		(stdout.as_str(), code),
		(
			concat!(
				"[ \"/nix/store/f7rbcys5ki5q0bvnalm75sq15cfn7n3b-nixos-system-nixrs-reference-26.11pre-git.drv\" ",
				"\"/nix/store/d111q3a19x7bfpw1hrry7dcyx60awhly-nixos-vm.drv\" ",
				"\"/nix/store/ks2l014wl7n9pm5h58y94wj03mbbmjin-home-manager-generation.drv\" ",
				"\"/nix/store/09p2niv7bmc293ji0wsdqdihgia3jbjc-home-manager-generation.drv\" ]\n"
			),
			0
		)
	);
}

/// A library caller's directory, not the process's, resolves relative paths
/// in `--expr`.
#[test]
fn resolves_paths_against_the_given_directory() {
	let dir = tempfile::tempdir().unwrap();
	fixture_dir(dir.path());
	let options = nix_rs_commands::instantiate::parse_args(
		["--eval", "--expr", "builtins.readFile ./f.txt"].map(String::from),
		Default::default(),
	)
	.unwrap();
	let mut stdout = Vec::new();
	nix_rs_commands::instantiate::run(&options, dir.path(), &mut stdout);
	assert_eq!(String::from_utf8(stdout).unwrap(), "\"hi\\n\"\n");
}

/// Configuration files, `NIX_CONFIG` and settings flags, with the warnings
/// and errors recorded from the reference.
#[test]
fn reads_configuration_like_cppnix() {
	let dir = tempfile::tempdir().unwrap();
	let conf = dir.path().join("etc");
	fs::create_dir(&conf).unwrap();
	let run = |nix_conf: &str, args: &[&str]| {
		fs::write(conf.join("nix.conf"), nix_conf).unwrap();
		let output = nix_instantiate()
			.args(args)
			.current_dir(dir.path())
			.env_clear()
			.env("NIX_CONF_DIR", &conf)
			.env("NIX_USER_CONF_FILES", "")
			.output()
			.unwrap();
		(
			String::from_utf8(output.stdout).unwrap(),
			String::from_utf8(output.stderr).unwrap(),
			output.status.code().unwrap(),
		)
	};
	let has_fetch_tree = ["--eval", "-E", "builtins ? fetchTree"];
	assert_eq!(
		run("", &has_fetch_tree),
		("false\n".into(), String::new(), 0)
	);
	assert_eq!(
		run("extra-experimental-features = flakes\n", &has_fetch_tree),
		("true\n".into(), String::new(), 0)
	);
	let mut args = vec![
		"--extra-experimental-features",
		"fetch-tree",
		"--option",
		"baz",
		"1",
	];
	args.extend(has_fetch_tree);
	assert_eq!(
		run("foo-unknown = 1\nflake-registry = /x\n", &args),
		(
			"true\n".into(),
			"warning: Ignoring setting 'flake-registry' because experimental feature 'flakes' is not enabled\nwarning: unknown setting 'baz'\nwarning: unknown setting 'foo-unknown'\n".into(),
			0
		)
	);
	// Three reductions hide warnings; one verbose flag restores them.
	args.extend(["--quiet", "--quiet", "--quiet"]);
	assert_eq!(
		run("foo-unknown = 1\n", &args),
		("true\n".into(), String::new(), 0)
	);
	args.push("-v");
	assert_eq!(
		run("foo-unknown = 1\n", &args),
		(
			"true\n".into(),
			"warning: unknown setting 'baz'\nwarning: unknown setting 'foo-unknown'\n".into(),
			0
		)
	);
	let (stdout, stderr, code) = run("max-call-depth=3\n", &has_fetch_tree);
	assert_eq!((stdout.as_str(), code), ("", 1));
	assert!(
		stderr.starts_with(&format!(
			"error: syntax error in configuration line 'max-call-depth=3' in \"{}/nix.conf\"\nTry '",
			conf.display()
		)),
		"{stderr}"
	);
}
