//! Extra-corpus builtin forcing and context regressions against CppNix 2.35.2.
use std::os::unix::process::CommandExt;
use std::{
	path::{Path, PathBuf},
	process::Command,
};

fn compare(expressions: &[&str]) {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate");
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	let home = noxi_test_support::tempdir().unwrap();
	for expression in expressions {
		for trace in [false, true] {
			let run = |binary: &Path| {
				let mut command = Command::new(binary);
				command.arg0("nix-instantiate");
				command
					.env_clear()
					.env("HOME", home.path())
					.env("NIX_CONF_DIR", home.path())
					.env("NIX_USER_CONF_FILES", "")
					.env("NIX_CONFIG", "")
					.env("NIX_PATH", "")
					.current_dir(home.path())
					.args([
						"--store", "dummy://", "--eval", "--strict", "--expr", expression,
					]);
				if trace {
					command.arg("--show-trace");
				}
				command.output().unwrap()
			};
			let expected = run(&reference);
			let actual = run(candidate);
			assert!(
				matches!(expected.status.code(), Some(0 | 1)),
				"reference crashed: {expected:?}"
			);
			assert_eq!(
				(actual.status.code(), &actual.stdout, &actual.stderr),
				(expected.status.code(), &expected.stdout, &expected.stderr),
				"{expression}, trace={trace}\ncandidate: {}\nreference: {}",
				String::from_utf8_lossy(&actual.stderr),
				String::from_utf8_lossy(&expected.stderr)
			);
		}
	}
}

#[test]
fn ordered_builtin_type_checks_and_forcing_match_reference() {
	compare(&[
		"let x = 1; in x.y",
		"let a = throw \"bad\"; in builtins.seq (builtins.tryEval a) a",
		"builtins.tryEval (builtins.addErrorContext \"outer\" (abort \"trap\"))",
		"builtins.tryEval (builtins.throw (builtins.throw \"inner\"))",
		"builtins.abort (throw \"argument\")",
		"builtins.mapAttrs (n: v: v) 17",
		"builtins.mapAttrs {} { a = 1; }",
		"builtins.deepSeq {} (throw \"later\")",
		"builtins.toString (throw \"argument\")",
		"builtins.toJSON (x: x)",
		"builtins.fromTOML \"text\"",
		"builtins.replaceStrings [\"a\"] [\"b\"] (throw \"argument\")",
		"builtins.replaceStrings [0] [] {}",
		"builtins.replaceStrings [1] [\"new\"] {}",
		"builtins.replaceStrings [\"a\"] [] (throw \"subject\")",
		"builtins.replaceStrings [(throw \"pattern\")] [\"new\"] (throw \"subject\")",
		"builtins.replaceStrings [(throw \"pattern\")] [\"new\"] {}",
		"builtins.replaceStrings [\"a\"] [(throw \"replacement\")] \"a\"",
		"builtins.replaceStrings [\"a\"] [(abort \"replacement\")] \"a\"",
		"builtins.replaceStrings [\"a\"] [(throw \"unused\")] \"b\"",
		"builtins.foldl' (a: x: a + x) null [1 2]",
		"builtins.tryEval (builtins.bitAnd {} (throw \"later\"))",
		"builtins.bitAnd (throw \"first\") 2",
		"builtins.bitOr 2 (throw \"second\")",
		"builtins.bitXor {} (throw \"later\")",
		"builtins.ceil (throw \"number\")",
		"builtins.compareVersions {} (throw \"later\")",
		"builtins.getAttr {} (throw \"later\")",
		"builtins.hasAttr {} (throw \"later\")",
		"builtins.unsafeGetAttrPos {} {}",
		"builtins.unsafeGetAttrPos (throw \"name\") {}",
		"builtins.catAttrs {} (throw \"later\")",
		"builtins.all {} (throw \"later\")",
		"builtins.any {} (throw \"later\")",
		"builtins.concatMap {} (throw \"later\")",
		"builtins.elemAt (throw \"list\") {}",
		"builtins.filter (throw \"unused\") []",
		"builtins.filter (throw \"function\") [1]",
		"builtins.foldl' {} (throw \"initial\") (throw \"list\")",
		"builtins.genList (throw \"function\") {}",
		"builtins.partition {} (throw \"list\")",
		"builtins.sort (throw \"unused\") []",
		"builtins.zipAttrsWith {} (throw \"list\")",
		"builtins.substring {} (throw \"length\") (throw \"string\")",
	]);
}

#[test]
fn string_context_field_errors_keep_reference_positions_and_traces() {
	compare(&[
		"builtins.appendContext \"\" { \"/nix/store/00000000000000000000000000000000-test\" = { path = 1; }; }",
		"builtins.appendContext \"\" { \"/nix/store/00000000000000000000000000000000-test\" = { path = throw \"path\"; }; }",
		"builtins.appendContext \"\" { \"/nix/store/00000000000000000000000000000000-test.drv\" = { allOutputs = 1; }; }",
		"builtins.appendContext \"\" { \"/nix/store/00000000000000000000000000000000-test.drv\" = { outputs = 1; }; }",
		"builtins.appendContext \"\" { \"/nix/store/00000000000000000000000000000000-test.drv\" = { outputs = [1]; }; }",
	]);
}
