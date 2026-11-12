//! `nix search`, `flake init/new/clone`, `fmt`/`formatter`, `edit` and `flake:`
//! lookup-path entries against pinned CppNix 2.35.2, offline and in diverted stores.
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// An `env` entry with this name is written to standard input instead.
const STDIN: &str = "\0stdin";
/// An `env` entry with this name names a regular file used as standard input.
const STDIN_FILE: &str = "\0stdin-file";

/// The pinned reference `bin` directory, or `None` (the test is skipped).
fn reference() -> Option<PathBuf> {
	let bin = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		});
	if bin.join("nix").exists() {
		Some(bin)
	} else {
		eprintln!("CppNix comparison skipped: set NOXI_REFERENCE_BIN to the pinned 2.35.2 bin");
		None
	}
}

/// One engine's private directory: `work` is the working directory, `home`
/// the home directory and `root` the diverted store root.
struct Engine {
	dir: tempfile::TempDir,
	program: PathBuf,
	name: &'static str,
}
impl Engine {
	fn work(&self) -> PathBuf {
		self.dir.path().join("work")
	}
	fn home(&self) -> PathBuf {
		self.dir.path().join("home")
	}
	fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
		fs::create_dir_all(self.work()).unwrap();
		fs::create_dir_all(self.home()).unwrap();
		let mut command = Command::new(&self.program);
		command
			.arg0(self.name)
			.current_dir(self.work())
			.env_clear()
			.env("PATH", std::env::var_os("PATH").unwrap())
			.env("HOME", self.home())
			.env("NIX_CONF_DIR", self.home().join("etc"))
			.env("NIX_USER_CONF_FILES", "")
			.env("GIT_CONFIG_GLOBAL", "/dev/null")
			.env("GIT_CONFIG_NOSYSTEM", "1")
			.env(
				"NIX_CONFIG",
				"experimental-features = nix-command flakes\nsubstituters =\nflake-registry =\nbuild-users-group =\nwarn-dirty = false\n",
			)
			.envs(env.iter().copied().filter(|(k, _)| *k != STDIN && *k != STDIN_FILE))
			.args([
				"--store",
				&format!("local?root={}", self.dir.path().join("root").display()),
			])
			.args(args);
		// The `STDIN` pseudo-variable is written to the command's standard input.
		let input = env
			.iter()
			.find(|(k, _)| *k == STDIN)
			.map(|(_, v)| v.as_bytes().to_vec());
		command
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped());
		if let Some((_, file)) = env.iter().find(|(k, _)| *k == STDIN_FILE) {
			command.stdin(fs::File::open(file).unwrap());
		}
		let mut child = command.spawn().unwrap();
		if let Some(mut stdin) = child.stdin.take() {
			stdin.write_all(&input.unwrap_or_default()).unwrap();
		}
		child.wait_with_output().unwrap()
	}
	/// Output with this engine's directory replaced by a common marker.
	fn normalize(&self, bytes: &[u8]) -> String {
		String::from_utf8_lossy(bytes).replace(&self.dir.path().display().to_string(), "@T")
	}
}

struct Pair {
	engines: [Engine; 2],
}
impl Pair {
	fn new(reference: &Path, legacy: Option<&'static str>) -> Self {
		let candidate = PathBuf::from(env!("CARGO_BIN_EXE_nix"));
		let name = legacy.unwrap_or("nix");
		Self {
			engines: [
				Engine {
					dir: tempfile::tempdir().unwrap(),
					program: reference.join(name),
					name,
				},
				Engine {
					dir: tempfile::tempdir().unwrap(),
					program: candidate,
					name,
				},
			],
		}
	}
	/// Run in both engines and require identical status, stdout and stderr.
	fn same(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
		let outputs: Vec<Output> = self.engines.iter().map(|e| e.run(args, env)).collect();
		let [a, b] = [&self.engines[0], &self.engines[1]];
		assert_eq!(
			outputs[0].status.code(),
			outputs[1].status.code(),
			"{args:?}: {}\n---\n{}",
			a.normalize(&outputs[0].stderr),
			b.normalize(&outputs[1].stderr)
		);
		assert_eq!(
			a.normalize(&outputs[0].stdout),
			b.normalize(&outputs[1].stdout),
			"stdout: {args:?}"
		);
		assert_eq!(
			a.normalize(&outputs[0].stderr),
			b.normalize(&outputs[1].stderr),
			"stderr: {args:?}"
		);
		outputs.into_iter().next().unwrap()
	}
	/// Copy a fixture tree into both working directories.
	fn seed(&self, from: &Path) {
		for engine in &self.engines {
			fs::create_dir_all(engine.work()).unwrap();
			copy_tree(from, &engine.work());
		}
	}
	/// Both working directories: file kinds, modes, contents and link targets.
	fn same_tree(&self) {
		let trees: Vec<Vec<String>> = self
			.engines
			.iter()
			.map(|e| {
				let mut entries = Vec::new();
				listing(&e.work(), &e.work(), &mut entries);
				entries
			})
			.collect();
		assert_eq!(trees[0], trees[1]);
	}
}

fn copy_tree(from: &Path, to: &Path) {
	for entry in fs::read_dir(from).unwrap() {
		let entry = entry.unwrap();
		let target = to.join(entry.file_name());
		let kind = entry.file_type().unwrap();
		if kind.is_dir() {
			fs::create_dir_all(&target).unwrap();
			copy_tree(&entry.path(), &target);
		} else if kind.is_symlink() {
			std::os::unix::fs::symlink(fs::read_link(entry.path()).unwrap(), &target).unwrap();
		} else {
			fs::copy(entry.path(), &target).unwrap();
		}
	}
}

fn listing(root: &Path, dir: &Path, out: &mut Vec<String>) {
	let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
	entries.sort_by_key(|e| e.file_name());
	for entry in entries {
		let path = entry.path();
		let name = path.strip_prefix(root).unwrap().display().to_string();
		if entry.file_name() == ".git" {
			continue;
		}
		let meta = fs::symlink_metadata(&path).unwrap();
		if meta.file_type().is_symlink() {
			out.push(format!(
				"{name} -> {}",
				fs::read_link(&path).unwrap().display()
			));
		} else if meta.is_dir() {
			out.push(format!("{name}/ {:o}", meta.permissions().mode() & 0o777));
			listing(root, &path, out);
		} else {
			out.push(format!(
				"{name} {:o} {:?}",
				meta.permissions().mode() & 0o777,
				fs::read(&path).unwrap()
			));
		}
	}
}

const PACKAGES: &str = r#"{
	outputs = { self }: let
		mk = name: attrs: derivation ({ inherit name; system = "x86_64-linux"; builder = "/bin/sh"; } // attrs);
	in {
		packages.x86_64-linux = {
			hello = mk "hello-2.12" { meta.description = "A program that\nproduces a familiar greeting"; meta.position = "${toString ./flake.nix}:4"; };
			world = mk "world" {};
			default = mk "hello-default-1.0" { meta.description = "Default hello"; };
		};
		legacyPackages.x86_64-linux = {
			nested = { recurseForDerivations = true; foo = mk "foo-1.0" { meta.description = "foo tool"; }; };
			hidden = { bar = mk "bar-1.0" {}; };
			broken = throw "broken";
			hellolegacy = mk "hellol-3" {};
		};
	};
}"#;

fn fixture(contents: &str) -> tempfile::TempDir {
	let dir = tempfile::tempdir().unwrap();
	fs::write(dir.path().join("flake.nix"), contents).unwrap();
	dir
}

#[test]
fn search_text_json_exclusion_and_errors_match() {
	let Some(reference) = reference() else { return };
	let flake = fixture(PACKAGES);
	let url = format!("path:{}", flake.path().display());
	let file = flake.path().join("pkgs.nix");
	fs::write(
		&file,
		"let mk = name: attrs: derivation ({ inherit name; system = \"x86_64-linux\"; builder = \"/bin/sh\"; } // attrs); in { hello = mk \"hello-1\" { meta.description = \"hi\"; }; sub = { recurseForDerivations = true; x = mk \"x-1\" {}; }; }",
	)
	.unwrap();
	let file = file.display().to_string();
	let pair = Pair::new(&reference, None);
	for args in [
		vec!["search", &url, "hello"],
		vec!["search", &url, "l"],
		vec!["search", &url, "hello", "foo"],
		vec!["search", &url, "^", "--json"],
		vec!["search", &url, "^", "--json", "--pretty"],
		vec!["search", &url, "-e", "default", "hello"],
		vec!["search", &url, "PROGRAM"],
		vec!["search", &url, "zzz"],
		vec!["search", &url, "--json", "zzz"],
		vec!["search", &url],
	] {
		pair.same(&args, &[]);
	}
	let nested = format!("{url}#nested");
	let exact = format!("{url}#.legacyPackages.x86_64-linux.nested");
	let missing = format!("{url}#nope");
	for args in [
		vec!["search", &nested, "foo"],
		vec!["search", &exact, "foo"],
		vec!["search", &missing, "x"],
		vec!["search", "-f", &file, "", "hello"],
		vec!["search", "-f", &file, "sub", "x"],
	] {
		pair.same(&args, &[]);
	}
}

#[test]
fn flake_init_new_and_conflicts_match() {
	let Some(reference) = reference() else { return };
	let templates = tempfile::tempdir().unwrap();
	let root = templates.path();
	fs::create_dir_all(root.join("a/sub")).unwrap();
	fs::create_dir_all(root.join("b")).unwrap();
	fs::write(root.join("a/file.txt"), "hello\n").unwrap();
	fs::write(root.join("a/run.sh"), "#!/bin/sh\n").unwrap();
	fs::set_permissions(root.join("a/run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
	fs::write(root.join("a/sub/inner"), "x").unwrap();
	std::os::unix::fs::symlink("file.txt", root.join("a/link")).unwrap();
	fs::write(root.join("b/file.txt"), "b\n").unwrap();
	fs::write(
		root.join("flake.nix"),
		r##"{ outputs = { self }: {
	templates.default = { path = ./a; description = "A"; welcomeText = "# Title\n\nSome *text* with `code`.\n\n- item one\n- item two\n"; };
	templates.b = { path = ./b; description = "B"; };
	templates.long = { path = ./b; description = "long"; welcomeText = "This is a fairly long paragraph of text that should certainly wrap around somewhere because it exceeds sixty columns by quite a lot of words indeed.\nSecond line.\n\n## Sub heading\n\n1. first\n2. second\n\n```\ncode block\n```\n\n> quote here\n\nA [link](https://example.com) and **bold**."; };
}; }"##,
	)
	.unwrap();
	let url = format!("path:{}", root.display());
	let with = |fragment: &str| format!("{url}#{fragment}");
	// A fresh directory with the default template, then an exact selection.
	let pair = Pair::new(&reference, None);
	pair.same(&["flake", "init", "-t", &url], &[]);
	pair.same_tree();
	let pair = Pair::new(&reference, None);
	pair.same(&["flake", "init", "-t", &with(".templates.b")], &[]);
	pair.same_tree();
	let pair = Pair::new(&reference, None);
	pair.same(&["flake", "new", "-t", &with("long"), "sub/dir"], &[]);
	pair.same_tree();
	// Missing templates, the unresolvable default registry entry and arity.
	let pair = Pair::new(&reference, None);
	pair.same(&["flake", "init", "-t", &with("nope")], &[]);
	pair.same(&["flake", "init"], &[]);
	pair.same(&["flake", "new"], &[]);
	// Identical files are skipped, others conflict, and new files are
	// added to an existing Git checkout with intent-to-add.
	let existing = tempfile::tempdir().unwrap();
	fs::write(existing.path().join("file.txt"), "hello\n").unwrap();
	fs::write(existing.path().join("run.sh"), "other\n").unwrap();
	std::os::unix::fs::symlink("other", existing.path().join("link")).unwrap();
	let pair = Pair::new(&reference, None);
	pair.seed(existing.path());
	for engine in &pair.engines {
		assert!(
			Command::new("git")
				.args(["init", "-q"])
				.current_dir(engine.work())
				.status()
				.unwrap()
				.success()
		);
	}
	pair.same(&["flake", "init", "-t", &url], &[]);
	pair.same_tree();
	let status: Vec<Vec<u8>> = pair
		.engines
		.iter()
		.map(|e| {
			Command::new("git")
				.args(["status", "--short"])
				.current_dir(e.work())
				.env("GIT_CONFIG_GLOBAL", "/dev/null")
				.output()
				.unwrap()
				.stdout
		})
		.collect();
	assert_eq!(status[0], status[1]);
}

#[test]
fn flake_clone_matches_for_path_git_and_registry_sources() {
	let Some(reference) = reference() else { return };
	let source = fixture("{ outputs = { self }: {}; }");
	fs::write(source.path().join("x.sh"), "#!/bin/sh\n").unwrap();
	fs::set_permissions(
		source.path().join("x.sh"),
		fs::Permissions::from_mode(0o755),
	)
	.unwrap();
	let git = |args: &[&str]| {
		assert!(
			Command::new("git")
				.args(args)
				.current_dir(source.path())
				.env("GIT_CONFIG_GLOBAL", "/dev/null")
				.env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00Z")
				.env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z")
				.status()
				.unwrap()
				.success()
		)
	};
	let plain = tempfile::tempdir().unwrap();
	copy_tree(source.path(), plain.path());
	git(&["init", "-q", "-b", "main"]);
	git(&["add", "."]);
	git(&[
		"-c",
		"user.name=a",
		"-c",
		"user.email=a@b",
		"commit",
		"-qm",
		"init",
	]);
	git(&["branch", "other"]);
	let rev = String::from_utf8(
		Command::new("git")
			.args(["rev-parse", "HEAD"])
			.current_dir(source.path())
			.output()
			.unwrap()
			.stdout,
	)
	.unwrap();
	let path_url = format!("path:{}", plain.path().display());
	let git_url = format!("git+file://{}", source.path().display());
	let branch = format!("{git_url}?ref=other");
	let pinned = format!("{git_url}?rev={}", rev.trim());
	for args in [
		vec!["flake", "clone", &path_url, "--dest", "out"],
		vec!["flake", "clone", &git_url, "--dest", "out"],
		vec!["flake", "clone", &branch, "--dest", "out"],
		vec!["flake", "clone", &pinned, "--dest", "out"],
		vec!["flake", "clone", &path_url],
		vec!["flake", "clone", "nope", "--dest", "out"],
	] {
		let pair = Pair::new(&reference, None);
		pair.same(&args, &[]);
		pair.same_tree();
	}
	// A registry entry from the user registry resolves to the local path.
	let pair = Pair::new(&reference, None);
	for engine in &pair.engines {
		let config = engine.home().join(".config/nix");
		fs::create_dir_all(&config).unwrap();
		fs::write(
			config.join("registry.json"),
			format!(
				r#"{{"version":2,"flakes":[{{"from":{{"type":"indirect","id":"local"}},"to":{{"type":"path","path":"{}"}}}}]}}"#,
				plain.path().display()
			),
		)
		.unwrap();
	}
	pair.same(&["flake", "clone", "local", "--dest", "out"], &[]);
	pair.same(&["flake", "clone", "local", "--dest", "out"], &[]);
	pair.same_tree();
}

#[test]
fn formatter_build_run_and_errors_match() {
	let Some(reference) = reference() else { return };
	// `builtin:unpack-channel` builds the formatter without a host builder.
	let fixture = tempfile::tempdir().unwrap();
	let unpacked = tempfile::tempdir().unwrap();
	fs::create_dir(unpacked.path().join("x")).unwrap();
	let script = unpacked.path().join("x/fmt");
	fs::write(
		&script,
		"#!/bin/sh\necho \"formatting $# args: $* root=$PRJ_ROOT\"\nexit 3\n",
	)
	.unwrap();
	fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
	assert!(
		Command::new("tar")
			.args(["-czf"])
			.arg(fixture.path().join("fmt.tar.gz"))
			.arg("-C")
			.arg(unpacked.path())
			.arg("x")
			.status()
			.unwrap()
			.success()
	);
	fs::write(
		fixture.path().join("flake.nix"),
		r#"{ outputs = { self }: {
	formatter.x86_64-linux = derivation { name = "fmt-1.0"; builder = "builtin:unpack-channel"; system = "builtin"; channelName = "bin"; src = self + "/fmt.tar.gz"; };
}; }"#,
	)
	.unwrap();
	for args in [
		vec!["formatter", "build"],
		vec!["formatter", "build", "--no-link"],
		vec!["formatter", "build", "-o", "out"],
	] {
		let pair = Pair::new(&reference, None);
		pair.seed(fixture.path());
		pair.same(&args, &[]);
		let links: Vec<_> = pair
			.engines
			.iter()
			.map(|e| ["result", "out"].map(|name| fs::read_link(e.work().join(name)).ok()))
			.collect();
		assert_eq!(links[0], links[1], "{args:?}");
	}
	let pair = Pair::new(&reference, None);
	pair.seed(fixture.path());
	let output = pair.same(&["fmt", "a", "b"], &[]);
	eprintln!(
		"nix fmt exit status {:?}: {}",
		output.status.code(),
		String::from_utf8_lossy(&output.stdout).trim()
	);
	pair.same(&["formatter", "run", "c"], &[]);
	// Missing and wrongly typed formatters, and no flake at all.
	let pair = Pair::new(&reference, None);
	pair.seed(fixture.path());
	for engine in &pair.engines {
		fs::write(
			engine.work().join("flake.nix"),
			"{ outputs = { self }: { formatter.x86_64-linux = { type = \"app\"; program = \"x\"; }; }; }",
		)
		.unwrap();
	}
	pair.same(&["fmt"], &[]);
	for engine in &pair.engines {
		fs::write(
			engine.work().join("flake.nix"),
			"{ outputs = { self }: { }; }",
		)
		.unwrap();
	}
	pair.same(&["fmt"], &[]);
	let pair = Pair::new(&reference, None);
	pair.same(&["fmt"], &[]);
}

#[test]
fn edit_positions_editors_and_errors_match() {
	let Some(reference) = reference() else { return };
	let flake = fixture(
		r#"{ outputs = { self }: let mk = name: extra: derivation ({ inherit name; system = "x86_64-linux"; builder = "/bin/sh"; } // extra); in {
	packages.x86_64-linux = {
		default = mk "a" { meta.position = "${toString ./flake.nix}:2"; };
		nopos = mk "b" {};
		badline = mk "c" { meta.position = "${toString ./flake.nix}:x"; };
		other = mk "d" { meta.position = "${toString ./other.nix}:7"; };
	};
}; }"#,
	);
	fs::write(flake.path().join("other.nix"), "other file\n").unwrap();
	let editor = tempfile::tempdir().unwrap();
	fs::write(editor.path().join("vim"), "#!/bin/sh\necho \"$@\"\n").unwrap();
	fs::set_permissions(editor.path().join("vim"), fs::Permissions::from_mode(0o755)).unwrap();
	let vim = editor.path().join("vim").display().to_string();
	let url = format!("path:{}", flake.path().display());
	let pair = Pair::new(&reference, None);
	for fragment in ["", "#other", "#nopos", "#badline", "#nonexistent"] {
		let installable = format!("{url}{fragment}");
		pair.same(&["edit", &installable], &[]);
		pair.same(&["edit", &installable], &[("EDITOR", &vim)]);
	}
}

#[test]
fn flake_lookup_path_entries_resolve_through_the_registry() {
	let Some(reference) = reference() else { return };
	let source = fixture("{ outputs = { self }: {}; }");
	fs::write(source.path().join("default.nix"), "{ answer = 42; }").unwrap();
	let registry = format!(
		r#"{{"version":2,"flakes":[{{"from":{{"type":"indirect","id":"nixpkgs"}},"to":{{"type":"path","path":"{}"}}}}]}}"#,
		source.path().display()
	);
	let seed = |pair: &Pair| {
		for engine in &pair.engines {
			let config = engine.home().join(".config/nix");
			fs::create_dir_all(&config).unwrap();
			fs::write(config.join("registry.json"), &registry).unwrap();
		}
	};
	let pair = Pair::new(&reference, None);
	seed(&pair);
	for args in [
		vec![
			"eval",
			"--impure",
			"-I",
			"nixpkgs=flake:nixpkgs",
			"--expr",
			"(import <nixpkgs>).answer",
		],
		vec![
			"eval",
			"--impure",
			"-I",
			"nixpkgs=flake:nixpkgs",
			"--expr",
			"<nixpkgs>",
		],
	] {
		pair.same(&args, &[]);
	}
	let pair = Pair::new(&reference, Some("nix-instantiate"));
	seed(&pair);
	for args in [
		vec!["--find-file", "nixpkgs"],
		vec!["--find-file", "nixpkgs/default.nix"],
		vec!["--eval", "-E", "(import <nixpkgs>).answer"],
	] {
		pair.same(&args, &[("NIX_PATH", "nixpkgs=flake:nixpkgs")]);
	}
}

#[test]
fn flake_installables_use_reference_default_attribute_paths() {
	let Some(reference) = reference() else { return };
	let flake = fixture(
		r#"{ outputs = { self }: let mk = name: derivation { inherit name; system = "x86_64-linux"; builder = "/bin/sh"; }; in {
	defaultPackage.x86_64-linux = mk "dp";
	packages.x86_64-linux.foo = mk "foo";
	legacyPackages.x86_64-linux.bar = mk "bar";
}; }"#,
	);
	let url = format!("path:{}", flake.path().display());
	let with = |fragment: &str| format!("{url}#{fragment}");
	let pair = Pair::new(&reference, None);
	for args in [
		vec!["eval", &url, "--apply", "d: d.name"],
		vec!["eval", &with("foo.name")],
		vec!["eval", &with("bar.name")],
		vec!["eval", &with(".packages.x86_64-linux.foo.name")],
		vec!["eval", &with(".foo")],
		vec!["eval", &with("nope")],
		vec!["eval", "--no-registries", &with("foo.name")],
		vec!["build", "--dry-run", &with("nope")],
		vec!["edit", &with(".packages.x86_64-linux.foo")],
	] {
		pair.same(&args, &[]);
	}
}

#[test]
fn help_page_lookup_and_errors_match() {
	let Some(reference) = reference() else { return };
	let pair = Pair::new(&reference, None);
	for args in [
		vec!["help", "nonexistent"],
		vec!["help", "a", "b"],
		vec!["help", "flake", "lock-x"],
		vec!["help", "--foo"],
		vec!["help-stores", "x"],
	] {
		pair.same(&args, &[]);
	}
	// Existing pages, including aliases and dash-joined paths, are rendered
	// from CppNix's embedded manual; noxi fails explicitly instead.
	for args in [
		vec!["help"],
		vec!["help", "eval"],
		vec!["help", "flake-lock"],
		vec!["help", "shell"],
		vec!["help-stores"],
	] {
		let [a, b] = [
			pair.engines[0].run(&args, &[]),
			pair.engines[1].run(&args, &[]),
		];
		assert!(a.status.success(), "{args:?}");
		assert!(
			String::from_utf8_lossy(&b.stderr).contains("is not supported by noxi yet"),
			"{args:?}"
		);
	}
}

#[test]
fn repl_sessions_match() {
	let Some(reference) = reference() else { return };
	let file = tempfile::tempdir().unwrap();
	let pkgs = file.path().join("pkgs.nix");
	fs::write(
		&pkgs,
		"{ hello = { name = \"hello-1\"; }; legacyPackages = { }; other = 2; }",
	)
	.unwrap();
	let load = format!(":l {}\nhello.name\n:r\n", pkgs.display());
	let sessions: Vec<(Vec<&str>, String)> = vec![
		(
			vec!["repl", "--expr", "{ a = 1; }"],
			"a\n:t a\n:p { x = [ 1 ]; }\n1 + 2\n:q\n".into(),
		),
		(vec!["repl"], "\"x\"\nundefinedvar\n".into()),
		(
			vec!["repl"],
			"x = 1\ny = x + 1\ny\n{ a = { b = 2; }; c = [ 1 2 ]; d = \"s\"; }\n[ { } ]\n:t builtins.map\n:t (builtins.map (x: x))\n:foo\n:?\n".into(),
		),
		(
			vec!["repl"],
			"f = x: x\nf\nbuiltins.map\nthrow \"no\"\n{ a = throw \"x\"; }\n(\n1 +\n2)\n:ll\n".into(),
		),
		(
			vec!["repl"],
			":p \"a\\nb\"\n:p { a.b.c = [ { x = 1; } ]; }\n{ a = 1; b = throw \"e\"; }\n:te\n:te\n:te bad\n".into(),
		),
		(vec!["repl"], load),
		(vec!["repl", "--file", pkgs.to_str().unwrap()], "other\n".into()),
	];
	for (args, input) in &sessions {
		let pair = Pair::new(&reference, None);
		pair.same(args, &[(STDIN, input)]);
		let echo = Pair::new(&reference, None);
		echo.same(args, &[(STDIN, input), ("_NIX_TEST_REPL_ECHO", "1")]);
	}
}

#[test]
fn daemon_usage_and_stdio_handshake_match() {
	let Some(reference) = reference() else { return };
	// `WORKER_MAGIC_1` and client protocol 1.38, then end of input. A regular
	// file, unlike a pipe, never hangs up, so the reference does not race its
	// interrupt handler against the end-of-file error.
	let input = tempfile::NamedTempFile::new().unwrap();
	fs::write(input.path(), b"cxin\0\0\0\0\x26\x01\0\0\0\0\0\0").unwrap();
	let handshake = input.path().to_str().unwrap();
	let pair = Pair::new(&reference, None);
	let features = "experimental-features = nix-command flakes daemon-trust-override mounted-ssh-store\nsubstituters =\nflake-registry =\nbuild-users-group =\n";
	for args in [
		vec!["daemon", "--stdio", "--socket-path", "x"],
		vec!["daemon", "--stdio"],
		vec!["daemon", "--stdio", "--process-ops"],
		vec!["daemon", "--stdio", "--force-untrusted"],
		vec!["daemon", "--default-trust", "--stdio"],
	] {
		pair.same(&args, &[(STDIN_FILE, handshake), ("NIX_CONFIG", features)]);
	}
	// Without the feature, the trust flags are rejected.
	pair.same(&["daemon", "--force-trusted"], &[]);
	pair.same(&["daemon", "--process-ops"], &[]);
}

#[test]
fn upgrade_nix_dry_run_and_profile_errors_match() {
	let Some(reference) = reference() else { return };
	let fixture = tempfile::tempdir().unwrap();
	let paths = fixture.path().join("paths.nix");
	fs::write(
		&paths,
		"{ x86_64-linux = \"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-nix-2.99.0\"; }",
	)
	.unwrap();
	let url = format!("file://{}", paths.display());
	let mut profiles = Vec::new();
	for (name, manifest) in [
		("json", Some("manifest.json")),
		("nix", Some("manifest.nix")),
		("none", None),
	] {
		let profile = fixture.path().join(name);
		fs::create_dir_all(profile.join("bin")).unwrap();
		fs::write(
			profile.join("bin/nix-env"),
			"#!/bin/sh\necho 'nix-env (Nix) 2.99'\n",
		)
		.unwrap();
		fs::set_permissions(
			profile.join("bin/nix-env"),
			fs::Permissions::from_mode(0o755),
		)
		.unwrap();
		if let Some(manifest) = manifest {
			fs::write(profile.join(manifest), "").unwrap();
		}
		profiles.push(profile);
	}
	let pair = Pair::new(&reference, None);
	let explicit = profiles[1].display().to_string();
	pair.same(
		&[
			"upgrade-nix",
			"--dry-run",
			"--nix-store-paths-url",
			&url,
			"--profile",
			&explicit,
		],
		&[],
	);
	for profile in &profiles {
		let path = format!(
			"{}:{}",
			profile.join("bin").display(),
			std::env::var("PATH").unwrap()
		);
		pair.same(
			&["upgrade-nix", "--dry-run", "--nix-store-paths-url", &url],
			&[("PATH", &path)],
		);
	}
}

#[test]
fn bundle_with_a_local_bundler_matches() {
	let Some(reference) = reference() else { return };
	let bundler = tempfile::tempdir().unwrap();
	let unpacked = tempfile::tempdir().unwrap();
	fs::create_dir(unpacked.path().join("x")).unwrap();
	fs::write(unpacked.path().join("x/tool"), "#!/bin/sh\n").unwrap();
	assert!(
		Command::new("tar")
			.arg("-czf")
			.arg(bundler.path().join("src.tar.gz"))
			.arg("-C")
			.arg(unpacked.path())
			.arg("x")
			.status()
			.unwrap()
			.success()
	);
	fs::write(
		bundler.path().join("flake.nix"),
		r#"{ outputs = { self }: {
	bundlers.x86_64-linux.default = drv: derivation { name = "bundled-${drv.name}"; builder = "builtin:unpack-channel"; system = "builtin"; channelName = "bin"; src = self + "/src.tar.gz"; };
	bundlers.x86_64-linux.bad = drv: { name = "x"; };
	bundlers.x86_64-linux.notdrv = drv: { type = "derivation"; name = "x"; };
}; }"#,
	)
	.unwrap();
	let url = format!("path:{}", bundler.path().display());
	let with = |fragment: &str| format!("{url}#{fragment}");
	for args in [
		vec!["bundle", "--bundler", &url, "--expr", "{ name = \"app\"; }"],
		vec![
			"bundle",
			"--bundler",
			&url,
			"--expr",
			"{ name = \"app\"; }",
			"-o",
			"out",
		],
	] {
		let pair = Pair::new(&reference, None);
		pair.same(&args, &[]);
		pair.same_tree();
	}
	let pair = Pair::new(&reference, None);
	for args in [
		vec![
			"bundle",
			"--bundler",
			&with("bad"),
			"--expr",
			"{ name = \"app\"; }",
		],
		vec![
			"bundle",
			"--bundler",
			&with("notdrv"),
			"--expr",
			"{ name = \"app\"; }",
		],
		vec![
			"bundle",
			"--bundler",
			&with(".bundlers.x86_64-linux.bad"),
			"--expr",
			"{ }",
		],
		vec!["bundle", "--bundler", &with("nope"), "--expr", "{ }"],
	] {
		pair.same(&args, &[]);
	}
}

#[test]
fn eval_printing_and_expression_errors_match() {
	let Some(reference) = reference() else { return };
	let pair = Pair::new(&reference, None);
	for expression in [
		"{ a = throw \"x\"; b = 1; }",
		"let d = derivation { name = \"d\"; system = \"x\"; builder = \"/b\"; }; in { inherit d; }",
		"throw \"top\"",
		"{ a = { b = 1; }; c = [ 1 2 ]; f = x: x; s = \"a\\${b\"; fl = 1.5; n = null; t = true; }",
		"builtins.map",
		"undefinedvar",
		"1 +",
	] {
		pair.same(&["eval", "--impure", "--expr", expression], &[]);
	}
	pair.same(&["eval", "--expr", "{ a = 1; }", "--apply", "x: x.b"], &[]);
	// Unresolvable `flake:` entries are errors, as is using one without
	// the `flakes` feature.
	let pair = Pair::new(&reference, Some("nix-instantiate"));
	pair.same(
		&["--find-file", "nixpkgs"],
		&[("NIX_PATH", "nixpkgs=flake:nope")],
	);
}

/// Add the static test BusyBox to an engine's diverted store by its NAR.
fn seed_busybox(engine: &Engine) -> String {
	let root = engine.dir.path().join("root");
	fs::create_dir_all(&root).unwrap();
	let busybox = fs::canonicalize(
		std::env::var_os("NOXI_TEST_BUSYBOX")
			.map(PathBuf::from)
			.unwrap_or_else(|| {
				Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-busybox")
			}),
	)
	.unwrap();
	let (hash, size) = noxi_store::nar::hash_path(&busybox).unwrap();
	let path =
		nix_compat::store_path::StorePath::from_absolute_path(busybox.to_str().unwrap().as_bytes())
			.unwrap();
	let mut info = noxi_store::ValidPathInfo::new(path, hash);
	info.nar_size = size;
	let store =
		noxi_store::LocalStore::open(noxi_store::LocalStoreConfig::diverted(&root).unwrap())
			.unwrap();
	let mut dump = tempfile::tempfile().unwrap();
	noxi_store::nar::dump_path(&mut dump, &busybox).unwrap();
	use std::io::Seek;
	dump.rewind().unwrap();
	store
		.add_to_store(&info, &mut std::io::BufReader::new(dump), false, false)
		.unwrap();
	busybox.to_string_lossy().into_owned()
}

#[test]
fn bar_format_build_logs_match() {
	let Some(reference) = reference() else { return };
	for args in [
		vec!["build", "-L", "--no-link"],
		vec![
			"build",
			"--print-build-logs",
			"--log-format",
			"bar-with-logs",
			"--no-link",
		],
		vec!["build", "-L", "--log-format", "raw", "--no-link"],
		vec!["build", "--no-link"],
	] {
		let pair = Pair::new(&reference, None);
		let mut busybox = String::new();
		for engine in &pair.engines {
			busybox = seed_busybox(engine);
		}
		let expression = format!(
			r#"let bb = builtins.storePath {busybox}; in derivation {{
				name = "logs-1.0"; system = "x86_64-linux"; builder = "${{bb}}/bin/sh";
				args = [ "-c" "printf 'first\nsecond\roverwritten\n\nhas\ttab\n'; echo stderr-line >&2; printf 'unterminated'; echo done > $out" ];
			}}"#
		);
		let mut full = args.clone();
		full.extend(["--impure", "--expr", &expression]);
		pair.same(&full, &[]);
	}
}
