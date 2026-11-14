//! Local Mercurial fetches against the pinned CppNix evaluator.
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

fn evaluate(binary: &Path, home: &Path, store: &str, expression: &str, flags: &[&str]) -> Output {
	Command::new(binary)
		.arg0("nix-instantiate")
		.env_clear()
		.env("PATH", std::env::var_os("PATH").unwrap())
		.env("HOME", home)
		.env("NIX_CONF_DIR", home)
		.env("NIX_USER_CONF_FILES", "")
		.env("NIX_CONFIG", "substituters =\n")
		.env("NIX_PATH", "")
		.env("HGPLAIN", "")
		.env("HGRCPATH", "")
		.current_dir(home)
		.args(["--store", store, "--eval", "--strict", "--expr", expression])
		.args(flags)
		.output()
		.unwrap()
}

fn compare(root: &Path, expression: &str, flags: &[&str], writable: Option<bool>) {
	let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
	let homes = [root.join("candidate"), root.join("reference")];
	let mut outputs = Vec::new();
	for (i, binary) in [
		PathBuf::from(env!("CARGO_BIN_EXE_nix-instantiate")),
		reference(),
	]
	.iter()
	.enumerate()
	{
		std::fs::create_dir_all(&homes[i]).unwrap();
		let store = if writable.is_some() {
			format!("local?root={}", roots[i].path().display())
		} else {
			"dummy://".into()
		};
		let mut flags = flags.to_vec();
		if writable == Some(true) {
			flags.push("--read-write-mode");
		}
		outputs.push(evaluate(binary, &homes[i], &store, expression, &flags));
	}
	assert!(
		matches!(outputs[1].status.code(), Some(0 | 1)),
		"reference crashed: {:?}",
		outputs[1]
	);
	assert_eq!(
		(
			outputs[0].status.code(),
			&outputs[0].stdout,
			&outputs[0].stderr
		),
		(
			outputs[1].status.code(),
			&outputs[1].stdout,
			&outputs[1].stderr
		),
		"{expression} {flags:?}\ncandidate: {}\nreference: {}",
		String::from_utf8_lossy(&outputs[0].stderr),
		String::from_utf8_lossy(&outputs[1].stderr)
	);
	if writable.is_some() {
		assert!(
			outputs[1].status.success(),
			"reference failed: {:?}",
			outputs[1]
		);
	}
}

fn hg(repo: &Path, args: &[&str]) -> String {
	let output = Command::new("hg")
		.arg("--cwd")
		.arg(repo)
		.args(args)
		.env("HGPLAIN", "")
		.env("HGRCPATH", "")
		.output()
		.unwrap();
	assert!(
		output.status.success(),
		"hg {args:?}: {}",
		String::from_utf8_lossy(&output.stderr)
	);
	String::from_utf8(output.stdout).unwrap().trim().into()
}

#[test]
fn mercurial_arguments_and_pure_revision_gate_match_reference() {
	let root = tempfile::tempdir().unwrap();
	for expression in [
		"builtins.fetchMercurial {}",
		"builtins.fetchMercurial { url = \"\"; }",
		"builtins.fetchMercurial { foo = throw \"unused\"; }",
		"builtins.fetchMercurial { rev = 1; url = \"/missing\"; }",
		"builtins.fetchMercurial { name = false; url = \"/missing\"; }",
		"builtins.fetchMercurial { rev = throw \"rev\"; url = \"/missing\"; }",
		"builtins.fetchMercurial { name = throw \"name\"; url = \"/missing\"; }",
		"builtins.fetchMercurial (throw \"argument\")",
		"builtins.fetchMercurial 1",
		"builtins.fetchMercurial { url = 1; }",
		"builtins.fetchMercurial { name = builtins.toFile \"ctx\" \"\"; url = \"/missing\"; }",
		"builtins.fetchMercurial { rev = builtins.toFile \"ctx\" \"\"; url = \"/missing\"; }",
		"builtins.fetchMercurial { name = (derivation { name = \"ctx\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; }).drvPath; url = \"/missing\"; }",
	] {
		for flags in [vec![], vec!["--show-trace"]] {
			compare(root.path(), expression, &flags, None);
		}
	}
	for argument in ["\"/missing\"", "{ url = \"/missing\"; rev = \"default\"; }"] {
		compare(
			root.path(),
			&format!("builtins.fetchMercurial {argument}"),
			&["--pure-eval"],
			None,
		);
	}
	for argument in [
		"\"/outside\"",
		"\"file:///outside\"",
		"\"https://example.invalid/repo\"",
	] {
		compare(
			root.path(),
			&format!("builtins.fetchMercurial {argument}"),
			&["--restrict-eval"],
			None,
		);
	}
	for uri in [
		"https://example.invalid/repo",
		"file:///outside",
		"/outside",
	] {
		compare(
			root.path(),
			&format!("builtins.fetchMercurial \"{uri}\""),
			&[
				"--restrict-eval",
				"--pure-eval",
				"--option",
				"allowed-uris",
				uri,
			],
			None,
		);
	}
	for allowed in ["https://example.invalid", "https:"] {
		compare(
			root.path(),
			"builtins.fetchMercurial \"https://example.invalid/repo\"",
			&[
				"--restrict-eval",
				"--pure-eval",
				"--option",
				"allowed-uris",
				allowed,
			],
			None,
		);
	}
}

#[test]
fn mercurial_clean_dirty_and_pinned_trees_match_reference() {
	let root = tempfile::tempdir().unwrap();
	let repo = root.path().join("repo");
	std::fs::create_dir(&repo).unwrap();
	hg(&repo, &["init"]);
	std::fs::write(repo.join("hello"), "first\n").unwrap();
	std::fs::write(repo.join(".hgignore"), "").unwrap();
	hg(&repo, &["add", "hello", ".hgignore"]);
	hg(
		&repo,
		&["commit", "-m", "first", "-u", "fixture", "-d", "1 0"],
	);
	let first = hg(&repo, &["log", "-r", ".", "--template", "{node}"]);
	std::fs::write(repo.join("hello"), "second\n").unwrap();
	hg(
		&repo,
		&["commit", "-m", "second", "-u", "fixture", "-d", "2 0"],
	);
	let second = hg(&repo, &["log", "-r", ".", "--template", "{node}"]);
	std::fs::write(repo.join("untracked"), "hidden\n").unwrap();
	let describe = |args: &str| {
		format!(
			"let f = builtins.fetchMercurial {args}; in [ (builtins.removeAttrs f [\"outPath\"]) f.outPath (builtins.getContext f.outPath) (builtins.readFile (f + \"/hello\")) (builtins.readDir f.outPath) ]"
		)
	};
	for args in [
		format!("\"file://{}\"", repo.display()),
		repo.display().to_string(),
		format!(
			"{{ url = \"file://{}\"; rev = \"{first}\"; }}",
			repo.display()
		),
		format!(
			"{{ url = {}; rev = \"default\"; name = \"custom\"; }}",
			repo.display()
		),
	] {
		compare(root.path(), &describe(&args), &[], Some(true));
	}
	compare(
		root.path(),
		&describe(&format!(
			"{{ url = \"file://{}\"; rev = \"{second}\"; }}",
			repo.display()
		)),
		&["--pure-eval"],
		Some(true),
	);
	let clean = describe(&format!("\"file://{}\"", repo.display()));
	compare(
		root.path(),
		&format!(
			"let f = builtins.fetchTree {{ type = \"hg\"; url = \"file://{}\"; }}; in [ (builtins.removeAttrs f [\"outPath\"]) f.outPath (builtins.getContext f.outPath) (builtins.readFile (f + \"/hello\")) ]",
			repo.display()
		),
		&["--extra-experimental-features", "fetch-tree"],
		Some(true),
	);
	compare(root.path(), &clean, &[], Some(false));
	compare(
		root.path(),
		&clean,
		&["--restrict-eval", "-I", repo.to_str().unwrap()],
		Some(true),
	);
	compare(
		root.path(),
		&clean,
		&[
			"--restrict-eval",
			"--option",
			"allowed-uris",
			&format!("file://{}", repo.display()),
		],
		Some(true),
	);
	for flags in [vec![], vec!["--show-trace"]] {
		compare(root.path(), &clean, &flags, None);
	}
	std::fs::write(repo.join("hello"), "dirty\n").unwrap();
	std::fs::write(repo.join("untracked"), "hidden\n").unwrap();
	std::fs::create_dir(repo.join("nested")).unwrap();
	std::fs::write(repo.join("nested/tracked"), "tracked\n").unwrap();
	hg(&repo, &["add", "nested/tracked"]);
	compare(
		root.path(),
		&describe(&format!("\"file://{}\"", repo.display())),
		&[],
		Some(true),
	);
	compare(
		root.path(),
		&describe(&format!(
			"{{ url = {}; rev = \"default\"; }}",
			repo.display()
		)),
		&[],
		Some(true),
	);
}

#[test]
fn mercurial_flake_lock_writes_are_tracked_and_dirty() {
	let root = tempfile::tempdir().unwrap();
	let dependency = root.path().join("dependency");
	std::fs::create_dir(&dependency).unwrap();
	hg(&dependency, &["init"]);
	std::fs::write(
		dependency.join("flake.nix"),
		"{ outputs = { self }: { value = 42; }; }\n",
	)
	.unwrap();
	hg(&dependency, &["add", "flake.nix"]);
	hg(
		&dependency,
		&["commit", "-m", "dependency", "-u", "fixture", "-d", "1 0"],
	);
	let flake = format!(
		"{{ inputs.dep.url = \"hg+file://{}\"; outputs = {{ self, dep }}: {{ value = dep.value; }}; }}\n",
		dependency.display()
	);
	for (index, binary) in [
		PathBuf::from(env!("CARGO_BIN_EXE_nix")),
		reference().with_file_name("nix"),
	]
	.iter()
	.enumerate()
	{
		let home = root.path().join(format!("home-{index}"));
		let repo = root.path().join(format!("flake-{index}"));
		let diverted = root.path().join(format!("store-{index}"));
		for path in [&home, &repo, &diverted] {
			std::fs::create_dir(path).unwrap();
		}
		hg(&repo, &["init"]);
		std::fs::write(repo.join("flake.nix"), &flake).unwrap();
		hg(&repo, &["add", "flake.nix"]);
		hg(
			&repo,
			&["commit", "-m", "flake", "-u", "fixture", "-d", "2 0"],
		);
		let target = format!("hg+file://{}", repo.display());
		let run = |args: &[&str]| {
			Command::new(binary)
				.env_clear()
				.env("PATH", std::env::var_os("PATH").unwrap())
				.env("HOME", &home)
				.env("NIX_CONF_DIR", &home)
				.env("NIX_USER_CONF_FILES", "")
				.env(
					"NIX_CONFIG",
					"experimental-features = nix-command flakes\nsubstituters =\n",
				)
				.env("NIX_PATH", "")
				.env("HGPLAIN", "")
				.env("HGRCPATH", "")
				.current_dir(&home)
				.args(["--store", &format!("local?root={}", diverted.display())])
				.args(args)
				.output()
				.unwrap()
		};
		let external = run(&[
			"flake",
			"lock",
			"--output-lock-file",
			repo.join("flake.lock").to_str().unwrap(),
			&target,
		]);
		assert!(
			external.status.success(),
			"{binary:?}: {}",
			String::from_utf8_lossy(&external.stderr)
		);
		assert_eq!(
			hg(&repo, &["status", "--added", "--no-status"]),
			"",
			"{binary:?} tracked an explicit output lock file"
		);
		assert_eq!(
			hg(&repo, &["status", "--unknown", "--no-status"]),
			"flake.lock"
		);
		let lock = run(&["flake", "lock", &target]);
		assert!(
			lock.status.success(),
			"{binary:?}: {}",
			String::from_utf8_lossy(&lock.stderr)
		);
		assert_eq!(
			hg(&repo, &["status", "--added", "--no-status"]),
			"flake.lock",
			"{binary:?}"
		);
		let metadata = run(&["flake", "metadata", "--json", &target]);
		assert!(
			metadata.status.success(),
			"{binary:?}: {}",
			String::from_utf8_lossy(&metadata.stderr)
		);
		let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout).unwrap();
		assert!(metadata.get("revision").is_none(), "{binary:?}: {metadata}");
		let denied = run(&["flake", "metadata", "--json", "--no-allow-dirty", &target]);
		assert!(
			!denied.status.success(),
			"{binary:?} accepted a dirty Hg tree"
		);
		assert!(String::from_utf8_lossy(&denied.stderr).contains("is unclean"));
		hg(
			&repo,
			&["commit", "-m", "lock", "-u", "fixture", "-d", "3 0"],
		);
		let clean = run(&["flake", "metadata", "--json", "--refresh", &target]);
		assert!(
			clean.status.success(),
			"{binary:?}: {}",
			String::from_utf8_lossy(&clean.stderr)
		);
		let clean: serde_json::Value = serde_json::from_slice(&clean.stdout).unwrap();
		assert!(clean.get("revision").is_some(), "{binary:?}: {clean}");
		assert_eq!(clean["revCount"], 1);
	}
}
