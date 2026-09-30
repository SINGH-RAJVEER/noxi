//! `nix-instantiate --eval --strict` against results recorded from the
//! CppNix 2.35.2 reference (`just reference`), run in read-only mode as the
//! reference does by default.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;

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
	let output = Command::new(env!("CARGO_BIN_EXE_nix-instantiate"))
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
