//! Lock-graph, follows, override, registry and lock-policy behaviour against
//! pinned CppNix 2.35.2, with local inputs only.
//!
//! Each case builds its fixture once and copies it, mtimes and Git index bytes
//! included, to the same path for each engine. A pre-existing lock is created
//! once by the reference and shared. Exit status, stdout, stderr (with each
//! engine's store root replaced) and the bytes of every written lock file must
//! be identical.
use std::{
	fs,
	path::{Path, PathBuf},
	process::Command,
};

const DATE: &str = "2001-01-01T00:00:00Z";
const HASH: &str = "sha256-e7pxuRsyffgNszX3kNFvaoAf8JrBHB2m2ieW7cbI1bs=";

fn reference() -> PathBuf {
	std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix")
}

fn sh(dir: &Path, script: &str) {
	let status = Command::new("sh")
		.arg("-ec")
		.arg(script)
		.current_dir(dir)
		.env("GIT_AUTHOR_DATE", DATE)
		.env("GIT_COMMITTER_DATE", DATE)
		.env("GIT_AUTHOR_NAME", "t")
		.env("GIT_AUTHOR_EMAIL", "t@example.org")
		.env("GIT_COMMITTER_NAME", "t")
		.env("GIT_COMMITTER_EMAIL", "t@example.org")
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.status()
		.unwrap();
	assert!(status.success(), "{script}");
}

/// Remove a tree that may contain read-only store paths.
fn remove(path: &Path) {
	if path.exists() {
		let _ = Command::new("chmod")
			.arg("-R")
			.arg("u+w")
			.arg(path)
			.status();
		fs::remove_dir_all(path).unwrap();
	}
}

struct Run {
	code: Option<i32>,
	stdout: String,
	stderr: String,
	locks: Vec<(String, Vec<u8>)>,
}

fn locks(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
	let mut entries: Vec<_> = fs::read_dir(dir)
		.unwrap()
		.map(|e| e.unwrap().path())
		.collect();
	entries.sort();
	for path in entries {
		if path.file_name().unwrap() == ".git" {
			continue;
		}
		if path.is_dir() {
			locks(root, &path, out);
		} else if path.extension().is_some_and(|e| e == "lock") {
			let name = path.strip_prefix(root).unwrap().display().to_string();
			out.push((name, fs::read(&path).unwrap()));
		}
	}
}

fn run(bin: &Path, work: &Path, conf: &str, args: &[String]) -> Run {
	let root = tempfile::tempdir().unwrap();
	let output = Command::new(bin)
		.env_clear()
		.env("PATH", std::env::var_os("PATH").unwrap())
		.env("HOME", root.path())
		.env("NIX_CONF_DIR", root.path())
		.env("NIX_USER_CONF_FILES", "")
		.env("GIT_AUTHOR_DATE", DATE)
		.env("GIT_COMMITTER_DATE", DATE)
		.env("GIT_AUTHOR_NAME", "t")
		.env("GIT_AUTHOR_EMAIL", "t@example.org")
		.env("GIT_COMMITTER_NAME", "t")
		.env("GIT_COMMITTER_EMAIL", "t@example.org")
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.env("GIT_CONFIG_NOSYSTEM", "1")
		.env(
			"NIX_CONFIG",
			format!(
				"experimental-features = nix-command flakes\nsubstituters =\nflake-registry = {}/global.json\nwarn-dirty = false\nbuild-users-group =\n{conf}",
				work.display()
			),
		)
		.current_dir(work)
		.args(["--store", &format!("local?root={}", root.path().display())])
		.args(args)
		.output()
		.unwrap();
	let mut found = Vec::new();
	locks(work, work, &mut found);
	let normalize = |bytes: &[u8]| {
		String::from_utf8_lossy(bytes).replace(&root.path().display().to_string(), "<root>")
	};
	let result = Run {
		code: output.status.code(),
		stdout: normalize(&output.stdout),
		stderr: normalize(&output.stderr),
		locks: found,
	};
	remove(&root.keep());
	result
}

struct Case {
	name: &'static str,
	/// Shell script creating the fixture; `$W` is the shared work path.
	setup: &'static str,
	/// Lock the root with the reference once, then run this script.
	after_lock: Option<&'static str>,
	conf: &'static str,
	args: &'static [&'static str],
}

const BASE: &str = r#"
mkdir -p a b c root
echo '{ outputs = { self }: { v = 1; }; }' > c/flake.nix
echo "{ inputs.c.url = \"path:$W/c\"; outputs = { self, c }: {}; }" > b/flake.nix
echo "{ inputs.b.url = \"path:$W/b\"; inputs.c.url = \"path:$W/c\"; outputs = { self, ... }: {}; }" > a/flake.nix
echo '{ "flakes": [], "version": 2 }' > global.json
root() { echo "{ $1 outputs = { self, ... }: {}; }" > root/flake.nix; }
repo() {
	mkdir -p $1; git -C $1 init -q -b main
	echo "{ outputs = { self }: { v = ${2:-1}; }; }" > $1/flake.nix
	git -C $1 add .; git -C $1 commit -qm init
}
"#;

fn cases() -> Vec<Case> {
	vec![
		Case {
			name: "nested follows lock",
			setup: r#"root "inputs.a.url = \"path:$W/a\"; inputs.c.url = \"path:$W/c\"; inputs.a.inputs.b.inputs.c.follows = \"c\"; inputs.a.inputs.c.follows = \"a/b/c\";""#,
			after_lock: None,
			conf: "",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "nested follows metadata",
			setup: r#"root "inputs.a.url = \"path:$W/a\"; inputs.c.url = \"path:$W/c\"; inputs.a.inputs.c.follows = \"a/b/c\";""#,
			after_lock: None,
			conf: "",
			args: &["flake", "metadata", "./root"],
		},
		Case {
			name: "missing follows target",
			setup: r#"root "inputs.a.url = \"path:$W/a\"; inputs.a.inputs.c.follows = \"nope\";""#,
			after_lock: None,
			conf: "",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "self follows cycle",
			setup: r#"root "inputs.a.url = \"path:$W/a\"; inputs.a.inputs.c.follows = \"a/c\";""#,
			after_lock: None,
			conf: "",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "mutual follows cycle",
			setup: r#"root "inputs.x.follows = \"y\"; inputs.y.follows = \"x\";""#,
			after_lock: None,
			conf: "",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "declared override of a missing input",
			setup: r#"root "inputs.a.url = \"path:$W/a\"; inputs.a.inputs.nonexist.follows = \"a\";""#,
			after_lock: None,
			conf: "",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "unmatched override-input",
			setup: r#"root "inputs.a.url = \"path:$W/a\";""#,
			after_lock: None,
			conf: "",
			args: &[
				"flake",
				"lock",
				"./root",
				"--override-input",
				"zz",
				"path:$W/a",
			],
		},
		Case {
			name: "nested override-input circular import",
			setup: r#"root "inputs.a.url = \"path:$W/a\";""#,
			after_lock: None,
			conf: "",
			args: &[
				"flake",
				"lock",
				"./root",
				"--override-input",
				"a/b/c",
				"path:$W/a",
			],
		},
		Case {
			name: "update of unknown and follows inputs",
			setup: r#"root "inputs.a.url = \"path:$W/a\"; inputs.a.inputs.c.follows = \"a/b/c\";""#,
			after_lock: Some(""),
			conf: "",
			args: &["flake", "update", "zz", "a/c", "a/b", "--flake", "./root"],
		},
		Case {
			name: "removed follows override re-locks the input",
			setup: r#"root "inputs.a.url = \"path:$W/a\"; inputs.a.inputs.c.follows = \"a/b/c\";""#,
			after_lock: Some(r#"root "inputs.a.url = \"path:$W/a\";""#),
			conf: "",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "declared follows keeps a changed input without fetching it",
			setup: r#"root "inputs.b.url = \"path:$W/b\"; inputs.c.url = \"path:$W/c\"; inputs.b.inputs.c.follows = \"c\";""#,
			after_lock: Some("echo '# changed' >> b/flake.nix"),
			conf: "",
			args: &["flake", "metadata", "./root", "--json"],
		},
		Case {
			name: "changed removed and added inputs",
			setup: r#"root "inputs.a.url = \"path:$W/a\"; inputs.c.url = \"path:$W/c\";""#,
			after_lock: Some(
				r#"root "inputs.a.url = \"path:$W/b\"; inputs.n.url = \"path:$W/c\";""#,
			),
			conf: "",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "missing reference lock file reads as empty",
			setup: r#"root "inputs.a.url = \"path:$W/a\";""#,
			after_lock: Some(""),
			conf: "",
			args: &[
				"flake",
				"update",
				"--flake",
				"./root",
				"--reference-lock-file",
				"$W/missing.lock",
			],
		},
		Case {
			name: "reference lock file needs allow-dirty",
			setup: r#"root "inputs.a.url = \"path:$W/a\";""#,
			after_lock: None,
			conf: "allow-dirty = false",
			args: &[
				"flake",
				"lock",
				"./root",
				"--reference-lock-file",
				"$W/missing.lock",
			],
		},
		Case {
			name: "lock refuses an unlocked input",
			setup: r#"repo g; echo x > g/dirty; git -C g add dirty; root "inputs.g.url = \"git+file://$W/g\";""#,
			after_lock: None,
			conf: "",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "metadata does not write an unlocked input",
			setup: r#"repo g; echo x > g/dirty; git -C g add dirty; root "inputs.g.url = \"git+file://$W/g\";""#,
			after_lock: None,
			conf: "",
			args: &["flake", "metadata", "./root"],
		},
		Case {
			name: "lock file version 5",
			setup: VERSION_5,
			after_lock: None,
			conf: "",
			args: &["flake", "metadata", "./root", "--json"],
		},
		Case {
			name: "lock file version 6",
			setup: VERSION_6,
			after_lock: None,
			conf: "",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "tarball layouts and non-flake inputs",
			setup: TARBALLS,
			after_lock: None,
			conf: "",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "tarball prefetch",
			setup: TARBALLS,
			after_lock: None,
			conf: "",
			args: &["flake", "prefetch", "file://$W/t.tar.gz", "--json"],
		},
		Case {
			name: "path input without flake.nix",
			setup: r#"mkdir p; echo x > p/x; root "inputs.t.url = \"path:$W/p?dir=x\";""#,
			after_lock: None,
			conf: "",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "git input without flake.nix",
			setup: r#"mkdir g; echo x > g/x; git -C g init -q; git -C g add x; git -C g commit -qm i; root "inputs.t.url = \"git+file://$W/g?dir=sub\";""#,
			after_lock: None,
			conf: "",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "tarball input without flake.nix",
			setup: r#"mkdir t; echo x > t/x; tar -C t -czf t.tar.gz x; root "inputs.t.url = \"file://$W/t.tar.gz\";""#,
			after_lock: None,
			conf: "",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "relative input above its parent",
			setup: r#"mkdir -p root/sub; echo '{ inputs.up.url = "path:../a"; outputs = { self, ... }: {}; }' > root/sub/flake.nix; root 'inputs.s.url = "path:./sub";'"#,
			after_lock: None,
			conf: "",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "global registry inputs",
			setup: REGISTRY,
			after_lock: None,
			conf: "",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "registry resolution",
			setup: REGISTRY,
			after_lock: None,
			conf: "",
			args: &[
				"registry", "resolve", "foo", "gr", "gr/other", "gr/main", "pinned", "sub",
			],
		},
		Case {
			name: "registry disabled",
			setup: REGISTRY,
			after_lock: None,
			conf: "use-registries = false",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "missing registry entry",
			setup: r#"root 'inputs.nope.url = "nope";'"#,
			after_lock: None,
			conf: "",
			args: &["flake", "lock", "./root"],
		},
		Case {
			name: "git update all",
			setup: GIT_INPUTS,
			after_lock: Some(GIT_BUMP),
			conf: "",
			args: &["flake", "update", "--flake", "./root"],
		},
		Case {
			name: "git update one",
			setup: GIT_INPUTS,
			after_lock: Some(GIT_BUMP),
			conf: "",
			args: &["flake", "update", "g1", "--flake", "./root"],
		},
		Case {
			name: "git update-input flag and output lock",
			setup: GIT_INPUTS,
			after_lock: Some(GIT_BUMP),
			conf: "",
			args: &[
				"flake",
				"lock",
				"./root",
				"--update-input",
				"g2",
				"--output-lock-file",
				"$W/out.lock",
			],
		},
		Case {
			name: "output lock with commit",
			setup: GIT_INPUTS,
			after_lock: None,
			conf: "",
			args: &[
				"flake",
				"lock",
				"./root",
				"--output-lock-file",
				"$W/out.lock",
				"--commit-lock-file",
			],
		},
	]
}

const VERSION_5: &str = r#"root "inputs.c.url = \"path:$W/c\";"
cat > root/flake.lock <<EOF
{
  "nodes": {
    "c": {
      "locked": { "lastModified": 1000000000, "narHash": "HASH", "path": "$W/c", "type": "path" },
      "original": { "path": "$W/c", "type": "path" }
    },
    "root": { "inputs": { "c": "c" } }
  },
  "root": "root",
  "version": 5
}
EOF
sed -i "s|HASH|$H|" root/flake.lock"#;

const VERSION_6: &str = r#"root "inputs.c.url = \"path:$W/c\";"
cat > root/flake.lock <<EOF
{
  "nodes": {
    "c": {
      "locked": { "lastModified": 1000000000, "narHash": "HASH", "path": "$W/c", "type": "path" },
      "original": { "path": "$W/c", "type": "path" },
      "flake": false
    },
    "root": { "inputs": { "c": "c" } }
  },
  "root": "root",
  "version": 6
}
EOF
sed -i "s|HASH|$H|" root/flake.lock"#;

const TARBALLS: &str = r#"
mkdir -p t m/d e
echo hello > t/x; echo '{ outputs = { self }: {}; }' > t/flake.nix
tar -C t -czf t.tar.gz x flake.nix
echo a > m/d/a; echo '{ outputs = { self }: {}; }' > m/d/flake.nix
tar -C m -czf one.tar.gz d
tar -C e -czf empty.tar.gz --files-from /dev/null
root "inputs.t.url = \"file://$W/t.tar.gz\"; inputs.o.url = \"file://$W/one.tar.gz\"; inputs.e = { url = \"file://$W/empty.tar.gz\"; flake = false; }; inputs.tt = { url = \"tarball+file://$W/t.tar.gz\"; flake = false; };"
"#;

const REGISTRY: &str = r#"
repo g; git -C g branch other
cat > global.json <<EOF
{ "flakes": [
  { "from": { "type": "indirect", "id": "foo" }, "to": { "type": "path", "path": "$W/c" } },
  { "from": { "type": "indirect", "id": "gr" }, "to": { "type": "git", "url": "file://$W/g" } },
  { "from": { "type": "indirect", "id": "pinned" }, "to": { "type": "git", "url": "file://$W/g", "rev": "$(git -C g rev-parse HEAD)" }, "exact": true },
  { "from": { "type": "indirect", "id": "sub" }, "to": { "type": "path", "path": "$W", "dir": "c" } },
  { "from": { "type": "indirect", "id": "gr", "ref": "other" }, "to": { "type": "git", "url": "file://$W/g", "ref": "other" } }
], "version": 2 }
EOF
root 'inputs.foo.url = "foo"; inputs.gr.url = "flake:gr"; inputs.pinned.url = "pinned"; inputs.s.url = "sub"; inputs.o.url = "gr/other";'
"#;

const GIT_INPUTS: &str = r#"repo g1; repo g2; root "inputs.g1.url = \"git+file://$W/g1\"; inputs.g2.url = \"git+file://$W/g2\";""#;

const GIT_BUMP: &str = r#"
echo '{ outputs = { self }: { v = 2; }; }' > g1/flake.nix; git -C g1 commit -qam bump
echo '{ outputs = { self }: { v = 3; }; }' > g2/flake.nix; git -C g2 commit -qam bump
"#;

fn script(work: &Path, body: &str) -> String {
	format!(
		"W='{}'\nH='{HASH}'\n{BASE}\n{body}\nfind . -exec touch -h -d @1000000000 {{}} +\n",
		work.display()
	)
}

/// Copy the template to the work path, keeping mtimes and Git index bytes.
fn place(template: &Path, work: &Path) {
	remove(work);
	let status = Command::new("cp")
		.arg("-a")
		.arg(template)
		.arg(work)
		.status()
		.unwrap();
	assert!(status.success());
}

#[test]
fn lock_graphs_registries_and_policies_match_reference() {
	let reference = reference();
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix"));
	let scratch = tempfile::tempdir().unwrap();
	let template = scratch.path().join("template");
	let work = scratch.path().join("work");
	let mut failures = Vec::new();
	for case in cases() {
		remove(&template);
		fs::create_dir(&template).unwrap();
		sh(&template, &script(&work, case.setup));
		if let Some(after) = case.after_lock {
			place(&template, &work);
			let locked = run(
				&reference,
				&work,
				"",
				&["flake".into(), "lock".into(), "./root".into()],
			);
			assert_eq!(locked.code, Some(0), "{}: {}", case.name, locked.stderr);
			fs::copy(
				work.join("root/flake.lock"),
				template.join("root/flake.lock"),
			)
			.unwrap();
			sh(&template, &script(&work, after));
		}
		let args: Vec<String> = case
			.args
			.iter()
			.map(|a| a.replace("$W", &work.display().to_string()))
			.collect();
		let mut runs = Vec::new();
		for bin in [reference.as_path(), candidate] {
			place(&template, &work);
			runs.push(run(bin, &work, case.conf, &args));
		}
		let (expected, actual) = (&runs[0], &runs[1]);
		for (what, same) in [
			("exit status", expected.code == actual.code),
			("stdout", expected.stdout == actual.stdout),
			("stderr", expected.stderr == actual.stderr),
			("lock files", expected.locks == actual.locks),
		] {
			if !same {
				failures.push(format!(
					"{}: {what} differs\n--- reference ({:?})\n{}{}\n--- candidate ({:?})\n{}{}",
					case.name,
					expected.code,
					expected.stdout,
					expected.stderr,
					actual.code,
					actual.stdout,
					actual.stderr
				));
			}
		}
	}
	remove(&work);
	remove(&template);
	assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
