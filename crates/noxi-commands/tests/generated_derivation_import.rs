//! Stored derivations retain output declarations when imported in a later evaluation.
use std::{
	fs,
	os::unix::process::CommandExt,
	path::{Path, PathBuf},
	process::{Command, Output},
};

const FEATURES: &str = "ca-derivations impure-derivations dynamic-derivations git-hashing";
const KINDS: &[(&str, &str)] = &[
	("ordinary", ""),
	("ordinary-multiple", "outputs = [ \"out\" \"dev\" ];"),
	("floating", "__contentAddressed = true;"),
	(
		"floating-multiple",
		"__contentAddressed = true; outputs = [ \"out\" \"dev\" ];",
	),
	(
		"floating-text",
		"__contentAddressed = true; outputHashMode = \"text\";",
	),
	(
		"floating-git",
		"__contentAddressed = true; outputHashMode = \"git\"; outputHashAlgo = \"sha1\";",
	),
	("impure", "__impure = true;"),
	(
		"impure-multiple",
		"__impure = true; outputs = [ \"out\" \"dev\" ];",
	),
	("impure-text", "__impure = true; outputHashMode = \"text\";"),
	(
		"impure-git",
		"__impure = true; outputHashMode = \"git\"; outputHashAlgo = \"sha1\";",
	),
	(
		"fixed-text",
		"outputHashMode = \"text\"; outputHash = \"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\";",
	),
	(
		"fixed-git",
		"outputHashMode = \"git\"; outputHashAlgo = \"sha1\"; outputHash = \"da39a3ee5e6b4b0d3255bfef95601890afd80709\";",
	),
];

fn reference() -> PathBuf {
	std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate")
}

fn evaluate(
	binary: &Path,
	home: &Path,
	store: &str,
	expression: &str,
	features: &str,
	trace: bool,
) -> Output {
	let mut command = Command::new(binary);
	command.arg0("nix-instantiate");
	command
		.env_clear()
		.env("HOME", home)
		.env("NIX_CONF_DIR", home)
		.env("NIX_USER_CONF_FILES", "")
		.env("NIX_PATH", "")
		.env(
			"NIX_CONFIG",
			format!("experimental-features = {features}\nsubstituters =\n"),
		)
		.args([
			"--store",
			store,
			"--eval",
			"--strict",
			"--json",
			"--read-write-mode",
			"--expr",
			expression,
		]);
	if trace {
		command.arg("--show-trace");
	}
	command.output().unwrap()
}

fn seed(reference: &Path, home: &Path, store: &str, name: &str, attrs: &str) -> String {
	let expression = format!(
		"(derivation {{ name = \"external-{name}\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; {attrs} }}).drvPath"
	);
	let output = evaluate(reference, home, store, &expression, FEATURES, false);
	assert!(output.status.success(), "seed {name}: {output:?}");
	serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn imported_output_values_contexts_and_downstream_derivations_match_reference() {
	let reference = reference();
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	let home = tempfile::tempdir().unwrap();
	let root = tempfile::tempdir().unwrap();
	let store = format!("local?root={}", root.path().display());
	for (name, attrs) in KINDS {
		let drv = seed(&reference, home.path(), &store, name, attrs);
		let imported = format!("import {}", serde_json::json!(drv));
		let parent_kind = if name.starts_with("impure") {
			"__impure = true;"
		} else {
			""
		};
		for expression in [
			format!(
				"let d = {imported}; in {{ name = d.name; names = builtins.attrNames d; output = d.outPath; context = builtins.getContext d.outPath; drvContext = builtins.getContext d.drvPath; all = builtins.map (x: [ x.outputName x.outPath ]) d.all; }}"
			),
			format!(
				"let d = {imported}; p = derivation {{ name = \"dependent\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; input = d; {parent_kind} }}; in {{ drvPath = p.drvPath; output = p.outPath; }}"
			),
		] {
			let expected = evaluate(
				&reference,
				home.path(),
				&store,
				&expression,
				FEATURES,
				false,
			);
			let actual = evaluate(candidate, home.path(), &store, &expression, FEATURES, false);
			assert!(expected.status.success(), "{name}: {expected:?}");
			assert_eq!(
				(actual.status.code(), &actual.stdout, &actual.stderr),
				(expected.status.code(), &expected.stdout, &expected.stderr),
				"{name}: {expression}\ncandidate: {}",
				String::from_utf8_lossy(&actual.stderr)
			);
			if let Some(path) = serde_json::from_slice::<serde_json::Value>(&actual.stdout)
				.unwrap()
				.get("drvPath")
			{
				let path = path.as_str().unwrap();
				assert!(
					fs::read(root.path().join(path.trim_start_matches('/')))
						.unwrap()
						.starts_with(b"Derive([")
				);
			}
		}
	}
}

#[test]
fn stored_derivation_feature_gates_match_reference() {
	let reference = reference();
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	let home = tempfile::tempdir().unwrap();
	let root = tempfile::tempdir().unwrap();
	let store = format!("local?root={}", root.path().display());
	for (name, attrs) in KINDS {
		let drv = seed(&reference, home.path(), &store, name, attrs);
		let expression = format!("({}).outPath", format!("import {}", serde_json::json!(drv)));
		for features in [
			"",
			"ca-derivations",
			"ca-derivations impure-derivations",
			FEATURES,
		] {
			for trace in [false, true] {
				let expected = evaluate(
					&reference,
					home.path(),
					&store,
					&expression,
					features,
					trace,
				);
				let actual = evaluate(candidate, home.path(), &store, &expression, features, trace);
				assert!(
					matches!(expected.status.code(), Some(0 | 1)),
					"{expected:?}"
				);
				assert_eq!(
					(actual.status.code(), &actual.stdout, &actual.stderr),
					(expected.status.code(), &expected.stdout, &expected.stderr),
					"{name}: features={features}, trace={trace}\ncandidate: {}\nreference: {}",
					String::from_utf8_lossy(&actual.stderr),
					String::from_utf8_lossy(&expected.stderr)
				);
			}
		}
	}
}

#[test]
fn stored_fixed_outputs_recompute_paths_from_hashes() {
	let reference = reference();
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	let home = tempfile::tempdir().unwrap();
	let root = tempfile::tempdir().unwrap();
	let store = format!("local?root={}", root.path().display());
	for mode in ["flat", "recursive", "text", "git"] {
		let name = format!("fixed-{mode}");
		let attrs = format!(
			"outputHashMode = \"{mode}\"; outputHash = \"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\";"
		);
		let path = seed(&reference, home.path(), &store, &name, &attrs);
		let declared = evaluate(
			&reference,
			home.path(),
			&store,
			&format!("(import {}).outPath", serde_json::json!(path)),
			FEATURES,
			false,
		);
		assert!(declared.status.success(), "{declared:?}");
		let declared: String = serde_json::from_slice(&declared.stdout).unwrap();
		let body = fs::read_to_string(root.path().join(path.trim_start_matches('/'))).unwrap();
		let body = body.replacen(
			&declared,
			"/nix/store/00000000000000000000000000000000-stale",
			1,
		);
		let expression = format!(
			"builtins.toFile \"external-{name}.drv\" {}",
			serde_json::json!(body)
		);
		let registered = evaluate(
			&reference,
			home.path(),
			&store,
			&expression,
			FEATURES,
			false,
		);
		assert!(
			registered.status.success(),
			"reference accepts stale output tuple: {registered:?}"
		);
		let path: String = serde_json::from_slice(&registered.stdout).unwrap();
		let expression = format!(
			"let d = import {}; p = derivation {{ name = \"dependent\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; input = d; }}; in {{ output = d.outPath; parent = p.drvPath; }}",
			serde_json::json!(path)
		);
		let expected = evaluate(
			&reference,
			home.path(),
			&store,
			&expression,
			FEATURES,
			false,
		);
		let actual = evaluate(candidate, home.path(), &store, &expression, FEATURES, false);
		assert!(expected.status.success(), "{expected:?}");
		assert_eq!(
			(actual.status.code(), &actual.stdout, &actual.stderr),
			(expected.status.code(), &expected.stdout, &expected.stderr),
			"{mode}: {}",
			String::from_utf8_lossy(&actual.stderr)
		);
	}
}
