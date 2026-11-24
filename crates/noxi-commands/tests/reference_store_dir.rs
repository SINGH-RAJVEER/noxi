//! `NIX_STORE_DIR`, `NIX_STATE_DIR`, `NIX_LOG_DIR` and `NIX_CONF_DIR`
//! compared with CppNix 2.35.2, as the upstream functional tests set them.
//!
//! Store paths hash the store directory, so both engines use the same
//! directory one after the other, wiped in between. The reference comes from
//! `NOXI_REFERENCE_BIN`, else `target/nix-reference/bin`; without it the test
//! prints a notice and returns.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

fn candidate_dir() -> PathBuf {
	Path::new(env!("CARGO_BIN_EXE_nix"))
		.parent()
		.unwrap()
		.to_owned()
}

/// Run `steps` with one engine's commands in a fresh diverted environment
/// below `root`, returning each step's output with `root` replaced by
/// `ROOT`, then the store directory's entries.
fn session(bin: &Path, root: &Path, steps: &[Vec<String>]) -> Vec<String> {
	noxi_test_support::remove_tree(root).unwrap();
	fs::create_dir_all(root.join("etc")).unwrap();
	fs::write(
		root.join("etc/nix.conf"),
		"build-users-group =\nsubstituters =\nsandbox = false\nexperimental-features = nix-command\n",
	)
	.unwrap();
	let mut results = Vec::new();
	for step in steps {
		let output: Output = Command::new(bin.join(&step[0]))
			.args(&step[1..])
			.current_dir(root)
			.env_clear()
			.env("HOME", root.join("home"))
			.env("NIX_STORE_DIR", root.join("store"))
			.env("NIX_STATE_DIR", root.join("var/nix"))
			.env("NIX_LOG_DIR", root.join("var/log/nix"))
			.env("NIX_CONF_DIR", root.join("etc"))
			.env("NIX_REMOTE", "")
			.output()
			.unwrap();
		let show = |bytes: &[u8]| {
			String::from_utf8_lossy(bytes)
				.replace(&root.display().to_string(), "ROOT")
				.lines()
				// The stale `--no-out-link` root names a random temporary directory.
				.filter(|line| !line.starts_with("removing stale link from"))
				.collect::<Vec<_>>()
				.join("\n")
		};
		results.push(format!(
			"{step:?}\nstatus {:?}\nstdout {}\nstderr {}",
			output.status.code(),
			show(&output.stdout),
			show(&output.stderr)
		));
	}
	let mut entries: Vec<String> = fs::read_dir(root.join("store"))
		.map(|dir| {
			dir.flatten()
				.map(|e| e.file_name().to_string_lossy().into_owned())
				.filter(|name| !name.ends_with(".lock"))
				.collect()
		})
		.unwrap_or_default();
	entries.sort();
	results.push(format!("store entries {entries:?}"));
	results
}

#[test]
fn store_state_log_and_conf_directories_follow_the_environment() {
	let Some(reference) = reference_dir() else {
		return;
	};
	let busybox = fs::canonicalize(
		std::env::var_os("NOXI_TEST_BUSYBOX")
			.map(PathBuf::from)
			.unwrap_or_else(|| {
				Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-busybox")
			}),
	)
	.unwrap();
	let fixture = noxi_test_support::tempdir().unwrap();
	let root = fixture.path().join("diverted");
	let expr = format!(
		r#"derivation {{
			name = "env-dir";
			system = "x86_64-linux";
			builder = "{}/bin/sh";
			args = [ "-c" "echo $NIX_STORE > $out; echo built >&2" ];
			dep = builtins.toFile "dep" "dep";
		}}"#,
		busybox.display()
	);
	let file = fixture.path().join("drv.nix");
	fs::write(&file, &expr).unwrap();
	let payload = fixture.path().join("payload");
	fs::write(&payload, "payload\n").unwrap();
	let s = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
	let file = file.to_str().unwrap();
	let payload = payload.to_str().unwrap();
	let steps = vec![
		s(&[
			"nix-instantiate",
			"--eval",
			"--expr",
			r#"builtins.toFile "x" "y""#,
		]),
		s(&["nix-instantiate", "--eval", "--expr", "builtins.storeDir"]),
		s(&["nix", "eval", "--raw", "--expr", "builtins.storeDir"]),
		s(&[
			"nix",
			"eval",
			"--raw",
			"--expr",
			r#"builtins.toFile "z" "w""#,
		]),
		s(&["nix-instantiate", file]),
		s(&["nix-build", "--no-out-link", file]),
		s(&["nix", "store", "add-file", payload]),
		s(&["nix", "path-info", "--json", "--json-format", "2", "--all"]),
		s(&["nix-store", "--gc", "--print-dead"]),
	];
	let expected = session(&reference, &root, &steps);
	let actual = session(&candidate_dir(), &root, &steps);
	noxi_test_support::remove_tree(&root).unwrap();
	for (actual, expected) in actual.iter().zip(&expected) {
		let strip = |s: &str| {
			let mut out = String::new();
			let mut rest = s;
			while let Some(at) = rest.find("\"registrationTime\":") {
				out.push_str(&rest[..at]);
				rest = rest[at..]
					.trim_start_matches("\"registrationTime\":")
					.trim_start_matches(|c: char| c.is_ascii_digit());
			}
			out.push_str(rest);
			out
		};
		assert_eq!(strip(actual), strip(expected));
	}
	assert_eq!(actual.len(), expected.len());
}

/// Without `--store`, `auto` opens the local store when the state directory
/// is writable, as for root on NixOS, so the store accessor commands read
/// paths from disk just as with an explicit `local` store.
#[test]
fn auto_store_reads_paths_like_an_explicit_local_store() {
	let Some(reference) = reference_dir() else {
		return;
	};
	let fixture = noxi_test_support::tempdir().unwrap();
	let root = fixture.path().join("diverted");
	let payload = fixture.path().join("payload");
	fs::write(&payload, "payload\n").unwrap();
	let run = |bin: &Path, args: &[&str]| -> Output {
		Command::new(bin.join("nix"))
			.args(args)
			.env_clear()
			.env("HOME", root.join("home"))
			.env("NIX_STORE_DIR", root.join("store"))
			.env("NIX_STATE_DIR", root.join("var/nix"))
			.env("NIX_LOG_DIR", root.join("var/log/nix"))
			.env("NIX_CONF_DIR", fixture.path())
			.env("NIX_REMOTE", "")
			.output()
			.unwrap()
	};
	fs::write(
		fixture.path().join("nix.conf"),
		"build-users-group =\nsubstituters =\nexperimental-features = nix-command\n",
	)
	.unwrap();
	let session = |bin: &Path| {
		noxi_test_support::remove_tree(&root).unwrap();
		let added = run(bin, &["store", "add-file", payload.to_str().unwrap()]);
		assert!(added.status.success());
		let path = String::from_utf8(added.stdout).unwrap().trim().to_owned();
		[
			vec!["store", "verify", "--no-trust", "-r", path.as_str()],
			vec!["store", "cat", path.as_str()],
		]
		.iter()
		.map(|args| {
			let output = run(bin, args);
			format!(
				"{args:?}\nstatus {:?}\nstdout {}\nstderr {}",
				output.status.code(),
				String::from_utf8_lossy(&output.stdout),
				String::from_utf8_lossy(&output.stderr)
			)
			.replace(&path, "PATH")
		})
		.collect::<Vec<_>>()
	};
	let expected = session(&reference);
	let actual = session(&candidate_dir());
	noxi_test_support::remove_tree(&root).unwrap();
	assert_eq!(actual, expected);
}
