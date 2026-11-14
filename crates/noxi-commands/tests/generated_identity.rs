//! Extra-corpus identity and numeric printing regressions against pinned CppNix.
use std::os::unix::process::CommandExt;
use std::{
	path::{Path, PathBuf},
	process::{Command, Output},
};

fn evaluate(binary: &Path, home: &Path, expression: &str) -> Output {
	Command::new(binary)
		.arg0("nix-instantiate")
		.env_clear()
		.env("HOME", home)
		.env("NIX_CONF_DIR", home)
		.env("NIX_USER_CONF_FILES", "")
		.env("NIX_CONFIG", "")
		.env("NIX_PATH", "")
		.args([
			"--store", "dummy://", "--eval", "--strict", "--expr", expression,
		])
		.output()
		.unwrap()
}

#[test]
fn generated_identity_and_float_outputs_match_reference() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate");
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	let home = tempfile::tempdir().unwrap();
	for expression in [
		"builtins == builtins",
		"let alias = builtins; in [ (alias == builtins) (builtins == builtins.builtins) (alias == alias) ]",
		"let foo = { bar = x: x; }; in [ foo.bar ] == [ foo.bar ]",
		"let id = x: x; in [ (id id) ] == [ (id id) ]",
		"let f = x: x; in [ (f == f) ([ f ] == [ f ]) (builtins.elem f [ f ]) ]",
		"[builtins.add] == [builtins.add]",
		"let foo = { bar = { value = throw \"forced\"; }; }; in builtins.tryEval ([foo.bar] == [foo.bar])",
		"let a = { value = throw \"forced\"; }; in builtins.tryEval (a == a)",
		"let a = { value = throw \"forced\"; }; in [a] == [a]",
		"let a = rec { self = a; }; in a == a",
		"0.0 / (-1)",
		"1.5 / 9223372036854775807",
		"[ 1.0e-7 1.0e-6 1.0e-5 1.0e-4 1.0e5 1.0e6 1.0e7 ]",
		"(builtins.fromTOML \"value = nan\").value",
		"(builtins.fromTOML \"value = -nan\").value",
	] {
		let expected = evaluate(&reference, home.path(), expression);
		let actual = evaluate(candidate, home.path(), expression);
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
