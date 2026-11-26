//! `sandbox-paths` targets compared with CppNix 2.35.2: which ones are bound
//! into the sandbox, which are refused and how, and that none of them changes
//! anything on the host. Every build runs unprivileged, in its own user
//! namespace and a scratch store.
//!
//! The reference comes from `NOXI_REFERENCE_BIN`, else
//! `target/nix-reference/bin`; without it only noxi is checked.
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

fn reference_dir() -> Option<PathBuf> {
	let dir = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		});
	dir.join("nix").exists().then_some(dir)
}

fn busybox() -> PathBuf {
	fs::canonicalize(
		std::env::var_os("NOXI_TEST_BUSYBOX")
			.map(PathBuf::from)
			.unwrap_or_else(|| {
				Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-busybox")
			}),
	)
	.unwrap()
}

/// Every path below `dir` with its mode and contents.
fn tree(dir: &Path, out: &mut Vec<String>) {
	let mut entries: Vec<PathBuf> = fs::read_dir(dir)
		.unwrap()
		.map(|e| e.unwrap().path())
		.collect();
	entries.sort();
	for path in entries {
		let metadata = fs::symlink_metadata(&path).unwrap();
		let contents = if metadata.file_type().is_symlink() {
			fs::read_link(&path).unwrap().display().to_string()
		} else if metadata.is_file() {
			fs::read_to_string(&path).unwrap()
		} else {
			String::new()
		};
		out.push(format!(
			"{} {:o} {contents}",
			path.display(),
			metadata.permissions().mode()
		));
		if metadata.is_dir() {
			tree(&path, out);
		}
	}
}

/// The engine's store, the host files bound and those that must not change.
struct Scene {
	dir: noxi_test_support::TempDir,
	busybox: PathBuf,
}

impl Scene {
	fn new() -> Self {
		let dir = noxi_test_support::tempdir().unwrap();
		let host = dir.path().join("host");
		fs::create_dir_all(host.join("victim/inner")).unwrap();
		fs::create_dir_all(host.join("dir/sub")).unwrap();
		fs::create_dir_all(host.join("shared")).unwrap();
		fs::write(host.join("file"), "bound\n").unwrap();
		fs::write(host.join("victim/existing"), "untouched\n").unwrap();
		symlink(host.join("victim"), host.join("link")).unwrap();
		symlink("../../../victim", host.join("rellink")).unwrap();
		symlink(host.join("file"), host.join("filelink")).unwrap();
		symlink(host.join("absent"), host.join("dangling")).unwrap();
		// What somebody who may write to a bound directory can leave there.
		symlink(host.join("victim"), host.join("shared/planted")).unwrap();
		Self {
			dir,
			busybox: busybox(),
		}
	}

	/// Build a probe of the sandbox with `paths`, in which `@` stands for the
	/// host directory. Returns the exit status, the probe's and the engine's
	/// messages, and whether the host directory stayed as it was.
	fn build(&self, bin: &Path, paths: &[&str]) -> (Option<i32>, String, bool) {
		let host = self.dir.path().join("host");
		let mut before = Vec::new();
		tree(&host, &mut before);
		let root = self.dir.path().join("root");
		noxi_test_support::remove_tree(&root).unwrap();
		let expr = format!(
			r#"let bb = builtins.path {{ path = "{}"; name = "bb"; }}; in derivation {{
				name = "probe";
				system = "x86_64-linux";
				builder = "${{bb}}/bin/busybox";
				args = [ "sh" "-c" ''
					for p in / /* /a/* /a/b/* /dev/null /etc/*; do
						if [ -h $p ]; then echo "$p link"
						elif [ -d $p ]; then echo "$p dir"
						elif [ -f $p ]; then echo "$p file $(${{bb}}/bin/busybox head -n 1 $p)"
						elif [ -c $p ]; then echo "$p device"
						fi
					done >&2
					exit 3
				'' ];
			}}"#,
			self.busybox.display()
		);
		let mut command = Command::new(bin.join("nix-build"));
		command
			.env_clear()
			.env("HOME", self.dir.path().join("home"))
			.env("NIX_CONF_DIR", self.dir.path().join("none"))
			.env(
				"NIX_CONFIG",
				"sandbox = true\nsandbox-paths =\nbuild-users-group =\nsubstituters =\n",
			)
			.args(["--store", &format!("local?root={}", root.display())])
			.args(["--no-out-link", "-E", &expr]);
		for path in paths {
			command.args([
				"--option",
				"extra-sandbox-paths",
				&path.replace('@', host.to_str().unwrap()),
			]);
		}
		let output = command.output().unwrap();
		// The reference announces the build once the sandbox is set up.
		let errors: String = String::from_utf8_lossy(&output.stderr)
			.lines()
			.filter(|line| {
				!line.starts_with("warning: you don't have Internet access")
					&& !line.starts_with("building '")
					&& !line.contains("For full logs")
					&& !line.contains("nix-store -l")
					&& !line.contains("nix log")
			})
			.map(|line| format!("{line}\n"))
			.collect();
		let mut after = Vec::new();
		tree(&host, &mut after);
		(output.status.code(), errors, before == after)
	}

	/// Check `cases` with both engines: the reference's result must be the
	/// expected one, noxi's the reference's.
	fn check(&self, cases: &[(&[&str], i32, &str)]) {
		let candidate = Path::new(env!("CARGO_BIN_EXE_nix")).parent().unwrap();
		let reference = reference_dir();
		if reference.is_none() {
			eprintln!("no reference: checking noxi alone");
		}
		for (paths, status, message) in cases {
			let actual = self.build(candidate, paths);
			if let Some(reference) = &reference {
				let expected = self.build(reference, paths);
				assert_eq!(actual, expected, "noxi, then the reference, for {paths:?}");
			}
			assert_eq!(actual.0, Some(*status), "{paths:?}: {}", actual.1);
			assert!(actual.1.contains(message), "{paths:?}: {}", actual.1);
			assert!(actual.2, "{paths:?} changed the host");
		}
	}
}

/// Targets with a component that is a symlink, whose source is a file, a
/// directory or a symlink. Before they were refused, the mount point was
/// created wherever the symlink led on the host.
#[test]
fn targets_below_symlinks_cannot_reach_the_host() {
	let refused = "in the sandbox path \"";
	Scene::new().check(&[
		(&["/a=@/link", "/a/new=@/file"], 1, refused),
		(&["/a=@/link", "/a/new=@/dir"], 1, refused),
		(&["/a=@/link", "/a/new=@/filelink"], 1, refused),
		(&["/a=@/link", "/a/existing=@/file"], 1, refused),
		(&["/a=@/link", "/a/inner=@/dir"], 1, refused),
		(&["/a=@/link", "/a/inner/deep/er=@/dir"], 1, refused),
		(&["/a=@/link", "/a/=@/dir"], 1, refused),
		(&["/a=@/link", "/a/../b/c=@/file"], 1, refused),
		(&["/a=@/rellink", "/a/new=@/file"], 1, refused),
		(&["/a/b=@/link", "/a/b/new=@/file"], 1, refused),
		(&["/a=@/dangling", "/a/new=@/dir"], 1, refused),
		// A symlink inside a bound directory.
		(&["/a=@/shared", "/a/planted/new=@/file"], 1, refused),
		(&["/a=@/shared", "/a/planted/inner=@/dir"], 1, refused),
		// The target itself is a symlink or a file.
		(&["/a=@/shared", "/a/planted=@/dir"], 1, refused),
		(&["/a=@/shared", "/a/planted=@/file"], 1, "File exists"),
		(&["/a=@/file", "/a/b=@/file"], 1, "Not a directory"),
		// The chroot's own symlinks.
		(&["/dev/fd/new=@/file"], 1, refused),
		(&["/dev/stdout=@/file"], 1, "File exists"),
		(&["/proc=@/link"], 100, "builder failed with exit code 3"),
	]);
}

/// `..`, `.`, empty names and the root.
#[test]
fn targets_that_leave_or_cover_the_chroot_are_refused() {
	let escapes = "escapes the chroot";
	let filename = "has an invalid filename";
	let invalid = "invalid path to bind mount in the chroot: \"\"";
	let mut cases: Vec<(Vec<String>, i32, &str)> = Vec::new();
	for target in [
		"/../escape",
		"/foo/../../escape",
		"//foo///.//../../escape",
		"/./../escape",
		"///.//./../escape//",
		"/a/b/../../../escape",
		"../escape",
		"a/../../escape",
	] {
		for source in ["file", "filelink", "link", "dir"] {
			cases.push((vec![format!("{target}=@/{source}")], 1, escapes));
		}
	}
	for target in ["/foo/..", "/foo/.", "/..", "/.", "..", "foo/.."] {
		for source in ["file", "link", "dir"] {
			cases.push((vec![format!("{target}=@/{source}")], 1, filename));
		}
	}
	for target in ["", "/", "//"] {
		for source in ["file", "link", "dir"] {
			cases.push((vec![format!("{target}=@/{source}")], 1, invalid));
		}
	}
	for target in ["/foo/../", "/a/../b/../"] {
		cases.push((vec![format!("{target}=@/dir")], 1, escapes));
		cases.push((
			vec![format!("{target}=@/file")],
			1,
			"error: creating regular file",
		));
	}
	cases.push((vec!["/a/=@/file".into()], 1, "error: creating regular file"));
	cases.push((vec!["/a/=@/link".into()], 1, "error: creating symlink"));
	let cases: Vec<(Vec<&str>, i32, &str)> = cases
		.iter()
		.map(|(paths, status, message)| {
			(
				paths.iter().map(String::as_str).collect(),
				*status,
				*message,
			)
		})
		.collect();
	let cases: Vec<(&[&str], i32, &str)> = cases
		.iter()
		.map(|(paths, status, message)| (&paths[..], *status, *message))
		.collect();
	Scene::new().check(&cases);
}

/// Paths the sandbox sets up itself.
#[test]
fn targets_over_the_sandboxs_own_paths_match_the_reference() {
	let built = "builder failed with exit code 3";
	Scene::new().check(&[
		(&["/etc/passwd=@/file"], 1, "File exists"),
		(&["/etc/group=@/filelink"], 1, "File exists"),
		(&["/etc/hosts=@/file"], 1, "File exists"),
		(&["/etc=@/link"], 1, "File exists"),
		(&["/tmp=@/link"], 1, "File exists"),
		(&["/nix=@/link"], 1, "File exists"),
		(&["/build=@/link"], 100, built),
		(&["/proc=@/dir"], 100, built),
		(&["/proc=/proc"], 100, built),
		(&["/dev=@/dir"], 1, "mounting /dev/shm"),
		(&["/dev=@/link"], 1, "mounting /dev/shm"),
		(&["/dev/null=@/file"], 100, "/dev/null file bound"),
		(
			&["/real-root=@/dir"],
			1,
			"cannot create real-root directory",
		),
		(
			&["/real-root=@/link"],
			1,
			"cannot create real-root directory",
		),
	]);
}

#[test]
fn accepted_targets_match_the_reference() {
	let built = "builder failed with exit code 3";
	Scene::new().check(&[
		(&["/a=@/file"], 100, "/a file bound"),
		(&["/./a=@/file"], 100, "/a file bound"),
		(&["///./a=@/dir/"], 100, "/a/sub dir"),
		(&["/a/=@/dir"], 100, "/a/sub dir"),
		(&["//a/b/=@/dir/"], 100, "/a/b/sub dir"),
		(&["/x/../a=@/file"], 100, "/a file bound"),
		(&["/x/../a/b=@/dir"], 100, "/a/b/sub dir"),
		(&["/a=@/dir", "/a/sub=@/shared"], 100, "/a/sub dir"),
		// A relative target is below the chroot's root.
		(&["a=@/file"], 100, "/a file bound"),
		(&["a/b=@/dir"], 100, "/a/b/sub dir"),
		(&["./a=@/filelink"], 100, "/a link"),
		(&["/a=@/dangling"], 100, "/a link"),
		// Only a final `?` makes a path optional, and only if it is missing.
		(&["/a=@/absent?"], 100, built),
		(&["/a=@/file/absent?"], 100, built),
		(&["/a=@/file?"], 100, "/a file bound"),
		(&["/a?=@/file"], 100, "/a? file bound"),
		(&["/a=@/absent"], 1, "error: getting attributes of path"),
		(
			&["/a=@/file/absent"],
			1,
			"error: getting attributes of path",
		),
		(&["/a=@/absent??"], 100, built),
		(&["/..=@/file?"], 1, "has an invalid filename"),
		(&["/../a=@/absent?"], 100, built),
		// The first of two sources for a target is used.
		(&["/a=@/filelink", "/a=@/file"], 100, "/a link"),
		// Two spellings of one target are two targets.
		(&["/a=@/file", "/./a=@/file"], 1, "File exists"),
	]);
}
