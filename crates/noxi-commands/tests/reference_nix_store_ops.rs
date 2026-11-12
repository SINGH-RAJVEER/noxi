//! `nix-store` add, import/export, validity and verification operations
//! compared with CppNix 2.35.2. Each engine has its own working directory
//! and `local?root=` store, seeded identically by the reference. Without a
//! reference executable the tests print a notice and return.
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn reference_dir() -> Option<PathBuf> {
	let dir = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		});
	if dir.join("nix-store").exists() {
		Some(dir)
	} else {
		eprintln!("skipping: no reference nix-store in {}", dir.display());
		None
	}
}

struct Pair {
	reference: PathBuf,
	dirs: [tempfile::TempDir; 2],
}

impl Pair {
	fn new() -> Option<Self> {
		Some(Self {
			reference: reference_dir()?,
			dirs: [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()],
		})
	}

	fn dir(&self, engine: usize) -> &Path {
		self.dirs[engine].path()
	}

	fn run_in(&self, engine: usize, reference: bool, args: &[&str], input: &[u8]) -> Output {
		let (program, name) = (args[0], args[0]);
		let bin = if reference {
			self.reference.join(program)
		} else {
			Path::new(env!("CARGO_BIN_EXE_nix")).with_file_name(program)
		};
		let dir = self.dir(engine);
		let mut child = Command::new(bin)
			.arg0(name)
			.current_dir(dir)
			.env_clear()
			.env("HOME", dir)
			.env("NIX_CONF_DIR", dir.join("etc"))
			.env("NIX_USER_CONF_FILES", "")
			.env(
				"NIX_CONFIG",
				"experimental-features = nix-command\nbuild-users-group =\nsubstituters =\n",
			)
			.args([
				"--store",
				&format!("local?root={}", dir.join("root").display()),
			])
			.args(&args[1..])
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap();
		let mut stdin = child.stdin.take().unwrap();
		let _ = stdin.write_all(input);
		drop(stdin);
		child.wait_with_output().unwrap()
	}

	fn file(&self, name: &str, contents: &[u8]) {
		for engine in 0..2 {
			let path = self.dir(engine).join(name);
			fs::create_dir_all(path.parent().unwrap()).unwrap();
			fs::write(path, contents).unwrap();
		}
	}

	fn seed(&self, args: &[&str]) -> String {
		let a = self.run_in(0, true, args, b"");
		let b = self.run_in(1, true, args, b"");
		assert!(
			a.status.success(),
			"seed {args:?}: {}",
			String::from_utf8_lossy(&a.stderr)
		);
		assert_eq!(a.stdout, b.stdout);
		String::from_utf8(a.stdout).unwrap().trim().to_owned()
	}

	fn norm(&self, engine: usize, bytes: &[u8]) -> String {
		String::from_utf8_lossy(bytes).replace(&self.dir(engine).display().to_string(), "DIR")
	}

	fn same_input(&self, args: &[&str], input: &[u8]) -> Output {
		let expected = self.run_in(0, true, args, input);
		let actual = self.run_in(1, false, args, input);
		assert_eq!(
			actual.status.code(),
			expected.status.code(),
			"status {args:?}\nreference: {}\nnoxi: {}",
			self.norm(0, &expected.stderr),
			self.norm(1, &actual.stderr)
		);
		assert_eq!(
			self.norm(1, &actual.stdout),
			self.norm(0, &expected.stdout),
			"stdout {args:?}"
		);
		assert_eq!(
			self.norm(1, &actual.stderr),
			self.norm(0, &expected.stderr),
			"stderr {args:?}"
		);
		expected
	}

	fn same(&self, args: &[&str]) -> Output {
		self.same_input(args, b"")
	}

	/// The reference's `--dump-db` of both stores, which covers hashes,
	/// sizes, derivers and references.
	fn same_db(&self) {
		let a = self.run_in(0, true, &["nix-store", "--dump-db"], b"");
		let b = self.run_in(1, true, &["nix-store", "--dump-db"], b"");
		assert!(a.status.success());
		assert_eq!(
			String::from_utf8_lossy(&b.stdout),
			String::from_utf8_lossy(&a.stdout)
		);
	}
}

#[test]
fn adding_printing_and_checking_paths() {
	let Some(pair) = Pair::new() else { return };
	pair.file("tree/a", b"a\n");
	pair.file("tree/sub/b", b"b\n");
	pair.file("flat", b"flat\n");
	for args in [
		vec!["nix-store", "--add", "tree", "flat"],
		vec!["nix-store", "--add", "tree/"],
		vec!["nix-store", "--add", "--bogus", "flat"],
		vec!["nix-store", "--add", "missing"],
		vec!["nix-store", "--add-fixed", "sha256", "flat"],
		vec!["nix-store", "--add-fixed", "--recursive", "sha1", "tree"],
		vec!["nix-store", "--add-fixed"],
		vec!["nix-store", "--add-fixed", "md4", "flat"],
		vec![
			"nix-store",
			"--print-fixed-path",
			"sha256",
			"1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s",
			"x",
		],
		vec![
			"nix-store",
			"--print-fixed-path",
			"--recursive",
			"sha256",
			"sha256-GjLnGBhJRWGlMOHyfAkXBGZhcLs/gdXsl3zchUe2vOU=",
			"y",
		],
		vec!["nix-store", "--print-fixed-path", "sha256", "x"],
		vec![
			"nix-store",
			"--check-validity",
			"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x",
		],
		vec![
			"nix-store",
			"--check-validity",
			"--print-invalid",
			"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x",
		],
	] {
		pair.same(&args);
	}
	pair.same_db();
	let flat = pair.seed(&["nix-store", "--add", "flat"]);
	pair.same(&["nix-store", "--check-validity", &flat]);
	pair.same(&["nix-store", "--verify-path", &flat]);
	pair.same(&["nix-store", "--dump-db", &flat]);
	pair.same(&["nix-store", "--dump-db"]);
	// A modified path is reported and fails verification.
	for engine in 0..2 {
		let real = pair
			.dir(engine)
			.join("root")
			.join(flat.trim_start_matches('/'));
		fs::set_permissions(&real, fs::Permissions::from_mode(0o644)).unwrap();
		fs::write(&real, b"changed\n").unwrap();
	}
	pair.same(&["nix-store", "--verify-path", &flat]);
	pair.same(&["nix-store", "--repair-path", &flat]);
	pair.same(&[
		"nix-store",
		"--repair-path",
		"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x",
	]);
}

#[test]
fn export_import_and_registration() {
	let Some(pair) = Pair::new() else { return };
	let dep = pair.seed(&[
		"nix",
		"eval",
		"--raw",
		"--impure",
		"--expr",
		r#"builtins.toFile "dep" "dep""#,
	]);
	let top = pair.seed(&[
		"nix",
		"eval",
		"--raw",
		"--impure",
		"--expr",
		r#"builtins.toFile "top" "top ${builtins.toFile "dep" "dep"}""#,
	]);
	let exported = pair.same(&["nix-store", "--export", &top, &dep]);
	pair.same(&["nix-store", "--export"]);
	pair.same(&["nix-store", "--export", "--bogus"]);
	let dump = pair.same(&["nix-store", "--dump-db"]);
	pair.same(&[
		"nix-store",
		"--print-env",
		"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv",
	]);
	// Import into fresh stores.
	let fresh = Pair::new().unwrap();
	fresh.same_input(&["nix-store", "--import"], &exported.stdout);
	fresh.same_db();
	fresh.same_input(&["nix-store", "--import"], &exported.stdout);
	fresh.same_input(&["nix-store", "--import"], b"\x02\0\0\0\0\0\0\0");
	fresh.same_input(&["nix-store", "--import", "x"], b"");
	// Load the dump into fresh stores whose files exist but are unregistered.
	let load = Pair::new().unwrap();
	for engine in 0..2 {
		let store = load.dir(engine).join("root/nix/store");
		fs::create_dir_all(&store).unwrap();
		for path in [&dep, &top] {
			let source = pair.dir(0).join("root").join(path.trim_start_matches('/'));
			fs::copy(&source, store.join(Path::new(path).file_name().unwrap())).unwrap();
		}
	}
	load.same(&["nix-store", "--init"]);
	load.same_input(&["nix-store", "--load-db"], &dump.stdout);
	load.same_db();
	// Registration without hashes computes them and canonicalises the files.
	let register = Pair::new().unwrap();
	for engine in 0..2 {
		let store = register.dir(engine).join("root/nix/store");
		fs::create_dir_all(&store).unwrap();
		let source = pair.dir(0).join("root").join(dep.trim_start_matches('/'));
		let target = store.join(Path::new(&dep).file_name().unwrap());
		fs::copy(&source, &target).unwrap();
		fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
	}
	register.same(&["nix-store", "--init"]);
	register.same_input(
		&["nix-store", "--register-validity"],
		format!("{dep}\n\n0\n").as_bytes(),
	);
	register.same_db();
	register.same_input(&["nix-store", "--register-validity", "--bogus"], b"");
	register.same_input(
		&["nix-store", "--register-validity", "--hash-given"],
		b"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x\nnothex\n",
	);
	for engine in 0..2 {
		let mode = fs::symlink_metadata(
			register
				.dir(engine)
				.join("root/nix/store")
				.join(Path::new(&dep).file_name().unwrap()),
		)
		.unwrap()
		.permissions()
		.mode();
		assert_eq!(mode & 0o777, 0o444, "engine {engine}");
	}
}

#[test]
fn restore_logs_environment_and_keys() {
	let Some(pair) = Pair::new() else { return };
	pair.file("tree/a", b"a\n");
	let nar = pair.same(&["nix-store", "--dump", "tree"]);
	pair.same_input(&["nix-store", "--restore", "restored"], &nar.stdout);
	for engine in 0..2 {
		assert_eq!(
			fs::read(pair.dir(engine).join("restored/a")).unwrap(),
			b"a\n"
		);
	}
	pair.same_input(&["nix-store", "--restore", "restored"], &nar.stdout);
	pair.same(&["nix-store", "--restore"]);
	let show = pair.run_in(
		0,
		true,
		&["nix", "derivation", "show", "--expr", r#"derivation { name = "env"; system = "x86_64-linux"; builder = "/bin/sh"; args = [ "-c" "it's" ]; quote = "a'b"; }"#],
		b"",
	);
	let value: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
	let drv = value["derivations"]
		.as_object()
		.unwrap()
		.values()
		.next()
		.unwrap()
		.to_string();
	let added = pair.run_in(0, true, &["nix", "derivation", "add"], drv.as_bytes());
	pair.run_in(1, true, &["nix", "derivation", "add"], drv.as_bytes());
	let drv_path = String::from_utf8(added.stdout).unwrap().trim().to_owned();
	pair.same(&["nix-store", "--print-env", &drv_path]);
	pair.same(&["nix-store", "--read-log", &drv_path]);
	let name = drv_path.trim_start_matches("/nix/store/").to_owned();
	for engine in 0..2 {
		let dir = pair
			.dir(engine)
			.join("root/nix/var/log/nix/drvs")
			.join(&name[..2]);
		fs::create_dir_all(&dir).unwrap();
		fs::write(dir.join(&name[2..]), "log text\n").unwrap();
	}
	pair.same(&["nix-store", "-l", &drv_path]);
	pair.same(&["nix-store", "--generate-binary-cache-key", "k", "sec"]);
	for engine in 0..2 {
		let output = pair.run_in(
			engine,
			engine == 0,
			&[
				"nix-store",
				"--generate-binary-cache-key",
				"k-1",
				"sec",
				"pub",
			],
			b"",
		);
		assert!(output.status.success() && output.stdout.is_empty() && output.stderr.is_empty());
		let sec = fs::metadata(pair.dir(engine).join("sec"))
			.unwrap()
			.permissions()
			.mode();
		assert_eq!(sec & 0o777, 0o600);
	}
	// Each engine's key converts to the public key it wrote.
	for engine in 0..2 {
		let secret = fs::read(pair.dir(engine).join("sec")).unwrap();
		let public = pair.run_in(
			engine,
			true,
			&["nix", "key", "convert-secret-to-public"],
			&secret,
		);
		assert_eq!(
			public.stdout,
			fs::read(pair.dir(engine).join("pub")).unwrap()
		);
	}
}
