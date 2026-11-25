//! `--override-flake` of `nix eval`, `nix build` and `nix-instantiate`
//! compared with CppNix 2.35.2, for installables and expressions.
//!
//! The reference comes from `NOXI_REFERENCE_BIN`, else
//! `target/nix-reference/bin`; without it the test prints a notice and
//! returns.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn reference_dir() -> Option<PathBuf> {
	let dir = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		});
	if dir.join("nix").exists() {
		Some(dir)
	} else {
		eprintln!("skipping: no reference nix in {}", dir.display());
		None
	}
}

struct Fixture {
	dir: noxi_test_support::TempDir,
}

impl Fixture {
	fn new() -> Self {
		let dir = noxi_test_support::tempdir().unwrap();
		for (name, text) in [
			(
				"a",
				r#"{ outputs = _: { x = "a"; y.z = 1; d = derivation { name = "d"; system = "x86_64-linux"; builder = "/b"; }; }; }"#,
			),
			(
				"b",
				r#"{ inputs.foo.url = "foo"; outputs = { foo, ... }: { x = "b-${foo.x}"; }; }"#,
			),
			("sub", "{ }"),
		] {
			fs::create_dir(dir.path().join(name)).unwrap();
			fs::write(dir.path().join(name).join("flake.nix"), text).unwrap();
		}
		fs::create_dir(dir.path().join("sub/dir")).unwrap();
		fs::write(
			dir.path().join("sub/dir/flake.nix"),
			r#"{ outputs = _: { x = "sub"; }; }"#,
		)
		.unwrap();
		fs::write(
			dir.path().join("f.nix"),
			r#"{ x = (builtins.getFlake "foo").x; }"#,
		)
		.unwrap();
		Self { dir }
	}

	/// The exit status, output and errors of `program`, whose arguments may
	/// name the fixture's directory as `@`.
	fn run(&self, bin: &Path, program: &str, args: &[&str]) -> (Option<i32>, String, String) {
		let root = self.dir.path().join("root");
		noxi_test_support::remove_tree(&root).unwrap();
		let here = self.dir.path().to_str().unwrap();
		let output = Command::new(bin.join(program))
			.env_clear()
			.env("HOME", self.dir.path().join("home"))
			.env("NIX_CONF_DIR", self.dir.path().join("none"))
			.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin")
			.env(
				"NIX_CONFIG",
				"experimental-features = nix-command flakes\nsubstituters =\nflake-registry =\n",
			)
			.current_dir(self.dir.path())
			.args(["--store", &format!("local?root={}", root.display())])
			.args(args.iter().map(|arg| arg.replace('@', here)))
			.output()
			.unwrap();
		let errors = String::from_utf8_lossy(&output.stderr)
			.replace(bin.join(program).to_str().unwrap(), "PROGRAM")
			.lines()
			.filter(|line| !line.starts_with("warning: you don't have Internet access"))
			.map(|line| format!("{line}\n"))
			.collect();
		(
			output.status.code(),
			String::from_utf8_lossy(&output.stdout).into_owned(),
			errors,
		)
	}

	fn same(&self, program: &str, args: &[&str]) -> Option<(Option<i32>, String, String)> {
		let candidate = Path::new(env!("CARGO_BIN_EXE_nix")).parent().unwrap();
		let expected = self.run(&reference_dir()?, program, args);
		let actual = self.run(candidate, program, args);
		assert_eq!(actual, expected, "{program} {args:?}");
		Some(expected)
	}
}

#[test]
fn overrides_apply_to_installables() {
	let f = Fixture::new();
	let a = ["--override-flake", "foo", "path:@/a"];
	for (args, output) in [
		(&["foo#x"][..], "\"a\"\n"),
		(&["--raw", "foo#x"], "a"),
		(&["--json", "foo#y"], "{\"z\":1}\n"),
		(&["foo#x", "--apply", "x: x + \"!\""], "\"a!\"\n"),
		(&["flake:foo#x"], "\"a\"\n"),
		(&["path:@/a#x"], "\"a\"\n"),
		(&["path:@/b#x", "--no-write-lock-file"], "\"b-a\"\n"),
		// The first override of a reference is used.
		(
			&["foo#x", "--override-flake", "foo", "path:@/absent"],
			"\"a\"\n",
		),
		(&["bar#x", "--override-flake", "bar", "foo"], "\"a\"\n"),
		(
			&["bar#x", "--override-flake", "bar", "path:@/sub?dir=dir"],
			"\"sub\"\n",
		),
		(
			&["other/main#x", "--override-flake", "other/main", "path:@/a"],
			"\"a\"\n",
		),
	] {
		let args = [&["eval"], &a[..], args].concat();
		if let Some((status, stdout, stderr)) = f.same("nix", &args) {
			assert_eq!(
				(status, &stdout[..]),
				(Some(0), output),
				"{args:?}: {stderr}"
			);
		}
	}
	for to in ["path:a", "./a", "@/a", "flake:foo"] {
		f.same("nix", &["eval", "foo#x", "--override-flake", "foo", to]);
	}
	for args in [
		&["foo#x"][..],
		&["foo#nothing"],
		&["foo/main#x"],
		&["other#x"],
		&["foo#x", "--override-flake"],
		&["foo#x", "--override-flake", "other"],
	] {
		f.same("nix", &[&["eval"], &a[..], args].concat());
	}
	f.same("nix", &["eval", "foo#x"]);
	f.same("nix", &[&["build", "--dry-run", "foo#d"][..], &a].concat());
}

#[test]
fn overrides_apply_to_expressions() {
	let f = Fixture::new();
	let a = ["--override-flake", "foo", "path:@/a"];
	for (args, output) in [
		(
			&["--impure", "--expr", "(builtins.getFlake \"foo\").x"][..],
			"\"a\"\n",
		),
		(
			&["--impure", "--expr", "(builtins.getFlake \"flake:foo\").x"],
			"\"a\"\n",
		),
		(
			&[
				"--impure",
				"--raw",
				"--expr",
				"(builtins.getFlake \"foo\").x",
			],
			"a",
		),
		(
			&[
				"--impure",
				"--expr",
				"builtins.readDir (builtins.fetchTree { type = \"indirect\"; id = \"foo\"; })",
			],
			"{ \"flake.nix\" = \"regular\"; }\n",
		),
		(
			&[
				"--impure",
				"-I",
				"foo=flake:foo",
				"--expr",
				"((import (<foo> + \"/flake.nix\")).outputs null).x",
			],
			"\"a\"\n",
		),
		(&["--impure", "--file", "@/f.nix", "x"], "\"a\"\n"),
		(&["--expr", "1 + 1"], "2\n"),
	] {
		let args = [&["eval"], &a[..], args].concat();
		if let Some((status, stdout, stderr)) = f.same("nix", &args) {
			assert_eq!(
				(status, &stdout[..]),
				(Some(0), output),
				"{args:?}: {stderr}"
			);
		}
	}
	f.same(
		"nix",
		&[
			&[
				"build",
				"--dry-run",
				"--impure",
				"--expr",
				"(builtins.getFlake \"foo\").d",
			][..],
			&a,
		]
		.concat(),
	);
	f.same(
		"nix-instantiate",
		&[&["--eval", "-E", "(builtins.getFlake \"foo\").x"][..], &a].concat(),
	);
}
