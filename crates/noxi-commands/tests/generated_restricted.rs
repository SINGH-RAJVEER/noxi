//! Restricted-mode access rules against the pinned CppNix evaluator: the
//! ancestors of allowed paths, directory listings, import from derivation
//! closures and the paths a string context does not grant.
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn reference() -> PathBuf {
	std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate")
}

fn busybox() -> PathBuf {
	std::env::var_os("NOXI_TEST_BUSYBOX")
		.map(PathBuf::from)
		.unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-busybox"))
}

fn instantiate(binary: &Path, root: &Path, args: &[&str]) -> Output {
	Command::new(binary)
		.arg0("nix-instantiate")
		.env_clear()
		.env("PATH", std::env::var_os("PATH").unwrap())
		.env("HOME", root.join("home"))
		.env("NIX_CONF_DIR", root.join("home"))
		.env("NIX_USER_CONF_FILES", "")
		.env(
			"NIX_CONFIG",
			"substituters =\nsandbox = false\nbuild-users-group =\n",
		)
		.env("NIX_PATH", "")
		.env("NIX_STORE_DIR", root.join("store"))
		.env("NIX_STATE_DIR", root.join("var"))
		.env("NIX_LOG_DIR", root.join("var/log"))
		.current_dir(root)
		.args(args)
		.output()
		.unwrap()
}

/// Run every case with `binary` in a fresh tree at `root`, so that both
/// evaluators see identical paths.
fn run_cases(binary: &Path, root: &Path) -> Vec<(String, Output)> {
	noxi_store::delete_path(root).unwrap();
	std::fs::create_dir_all(root.join("home")).unwrap();
	std::fs::create_dir_all(root.join("tunnel.d")).unwrap();
	std::os::unix::fs::symlink("..", root.join("tunnel.d/tunnel")).unwrap();
	std::fs::write(root.join("bar"), "foo\n").unwrap();
	let r = root.display();
	let sh = busybox().join("bin/sh");
	let bin = busybox().join("bin");
	let derivations = format!(
		r#"let
			mk = name: attrs: cmd: derivation ({{
				inherit name;
				system = builtins.currentSystem;
				builder = "{sh}";
				args = [ "-c" cmd ];
				PATH = "{bin}";
			}} // attrs);
			step1 = mk "step1" {{ }} "mkdir $out; echo foo > $out/bla";
			expr = mk "expr" {{ inherit step1; }} ''
				mkdir $out
				echo "builtins.readFile \"$step1/bla\"" > $out/default.nix
			'';
		in"#,
		sh = sh.display(),
		bin = bin.display()
	);
	// An existing store path that this evaluation neither creates nor fetches.
	let existing = instantiate(
		binary,
		root,
		&[
			"--read-write-mode",
			"--eval",
			"--expr",
			"builtins.toFile \"existing\" \"text\"",
		],
	);
	assert!(existing.status.success(), "{existing:?}");
	let existing = String::from_utf8(existing.stdout).unwrap();
	let existing = existing.trim().trim_matches('"');
	let tunnel = format!("{r}/tunnel.d");
	let cases: Vec<(String, Vec<String>)> = vec![
		(
			format!(
				"let __nixPath = [ {{ prefix = \"foo\"; path = {tunnel}; }} ]; in builtins.readDir <foo/tunnel>"
			),
			vec!["-I".into(), tunnel.clone()],
		),
		(
			format!("builtins.readFileType {r}"),
			vec!["-I".into(), tunnel.clone()],
		),
		(
			format!("builtins.pathExists {r}/bar"),
			vec!["-I".into(), tunnel.clone()],
		),
		(
			format!("builtins.readFile {r}/bar"),
			vec!["-I".into(), tunnel.clone()],
		),
		(
			"builtins.attrNames (builtins.readDir (dirOf (dirOf (builtins.toFile \"x\" \"y\"))))"
				.into(),
			vec![],
		),
		(
			"builtins.readFile (builtins.toFile \"x\" \"y\")".into(),
			vec![],
		),
		(
			format!("builtins.readFile (builtins.storePath {existing})"),
			vec![],
		),
		(
			format!(
				"builtins.readFile (builtins.appendContext \"{existing}\" {{ \"{existing}\" = {{ path = true; }}; }})"
			),
			vec![],
		),
		(format!("{derivations} import expr"), vec![]),
		(
			format!("{derivations} builtins.readFile step1.drvPath"),
			vec![],
		),
		(format!("{derivations} (import step1.drvPath).name"), vec![]),
	];
	let mut outputs = Vec::new();
	for (expression, flags) in cases {
		let mut args = vec![
			// Compare access results independently of build-progress rendering.
			"--quiet",
			"--restrict-eval",
			"--read-write-mode",
			"--eval",
			"--strict",
			"--expr",
			&expression,
		];
		args.extend(flags.iter().map(String::as_str));
		outputs.push((expression.clone(), instantiate(binary, root, &args)));
	}
	outputs
}

#[test]
fn restricted_access_matches_reference() {
	let temp = tempfile::tempdir().unwrap();
	let root = temp.path().join("w");
	let expected = run_cases(&reference(), &root);
	let actual = run_cases(Path::new(env!("CARGO_BIN_EXE_nix-instantiate")), &root);
	for ((expression, expected), (_, actual)) in expected.iter().zip(&actual) {
		assert_eq!(
			(actual.status.code(), &actual.stdout, &actual.stderr),
			(expected.status.code(), &expected.stdout, &expected.stderr),
			"{expression}\ncandidate: {}\nreference: {}",
			String::from_utf8_lossy(&actual.stderr),
			String::from_utf8_lossy(&expected.stderr)
		);
	}
}
