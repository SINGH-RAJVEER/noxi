//! `noxi snapshot` through its command-line entry point. One test only:
//! `NOXI_SNAPSHOT_DIR` is process-wide.

use std::fs;
use std::process::ExitCode;

use noxi_system::snapshot::{
	Action, DIR_ENV, Observed, Outcome, Request, SnapshotStore, SourceKind, run,
};

fn invoke(args: &[&str]) -> (ExitCode, String, String) {
	let (mut out, mut err) = (Vec::new(), Vec::new());
	let code = run(
		args.iter().map(|arg| (*arg).to_owned()).collect(),
		&mut out,
		&mut err,
	);
	(
		code,
		String::from_utf8(out).unwrap(),
		String::from_utf8(err).unwrap(),
	)
}

const LOCK_1: &str = r#"{"nodes":{"nixpkgs":{"locked":{"lastModified":1,"narHash":"sha256-a","rev":"aaaa","type":"github","owner":"o","repo":"nixpkgs"}},"root":{"inputs":{"nixpkgs":"nixpkgs"}}},"root":"root","version":7}"#;
const LOCK_2: &str = r#"{"nodes":{"nixpkgs":{"locked":{"lastModified":2,"narHash":"sha256-b","rev":"bbbb","type":"github","owner":"o","repo":"nixpkgs"}},"root":{"inputs":{"nixpkgs":"nixpkgs"}}},"root":"root","version":7}"#;

#[test]
fn list_show_diff_and_usage() {
	let tmp = tempfile::tempdir().unwrap();
	let dir = tmp.path().join("snapshots");
	// SAFETY: this binary runs a single test, so no other thread reads the
	// environment concurrently.
	unsafe { std::env::set_var(DIR_ENV, &dir) };
	assert_eq!(SnapshotStore::default_location().unwrap().dir(), dir);

	let (code, out, _) = invoke(&["list"]);
	assert_eq!(code, ExitCode::SUCCESS);
	assert_eq!(out, "no snapshots recorded\n");

	let journal = SnapshotStore::open(dir.clone()).unwrap();
	let lock = tmp.path().join("flake.lock");
	for (action, contents) in [(Action::Switch, LOCK_1), (Action::Boot, LOCK_2)] {
		fs::write(&lock, contents).unwrap();
		let mut request = Request::new(action, SourceKind::Flake, tmp.path().display().to_string());
		request.lock = Some(lock.clone());
		request.profile = tmp.path().join("profiles/system");
		request.current_system = tmp.path().join("current-system");
		let closure = Some("/nix/store/00000000000000000000000000000000-nixos-system-host".into());
		journal
			.begin(request)
			.unwrap()
			.finish(Observed { closure }, Outcome::success(None))
			.unwrap();
	}

	let (code, out, _) = invoke(&["list"]);
	assert_eq!(code, ExitCode::SUCCESS);
	let lines: Vec<&str> = out.lines().collect();
	assert_eq!(lines.len(), 3, "{out}");
	assert!(lines[0].starts_with("ID"));
	assert!(lines[1].starts_with("0001") && lines[1].contains("switch"));
	assert!(lines[2].starts_with("0002") && lines[2].contains("boot"));
	assert!(lines[2].contains("success") && lines[2].contains("nixos-system-host"));

	let (code, out, _) = invoke(&["list", "--json"]);
	assert_eq!(code, ExitCode::SUCCESS);
	let records: serde_json::Value = serde_json::from_str(&out).unwrap();
	assert_eq!(records.as_array().unwrap().len(), 2);

	let (code, out, _) = invoke(&["show", "2"]);
	assert_eq!(code, ExitCode::SUCCESS);
	assert!(out.starts_with("snapshot 0002\n"), "{out}");
	assert!(out.contains("lock changes since snapshot 0001:"), "{out}");
	assert!(out.contains("changed nixpkgs:") && out.contains("rev: aaaa -> bbbb"));

	let (code, out, _) = invoke(&["show", "0001", "--json"]);
	assert_eq!(code, ExitCode::SUCCESS);
	let record: serde_json::Value = serde_json::from_str(&out).unwrap();
	assert_eq!(record["id"], "0001");

	let (code, out, _) = invoke(&["diff", "1", "2", "--json"]);
	assert_eq!(code, ExitCode::SUCCESS);
	let diff: serde_json::Value = serde_json::from_str(&out).unwrap();
	assert_eq!(diff["changed"][0]["name"], "nixpkgs");

	let (code, _, err) = invoke(&["show", "9"]);
	assert_eq!(code, ExitCode::FAILURE);
	assert!(err.starts_with("error: "), "{err}");
	assert!(err.contains("snapshot 0009 does not exist") && err.contains("noxi snapshot list"));

	for args in [
		&[][..],
		&["show"][..],
		&["list", "extra"][..],
		&["diff", "1"][..],
		&["bogus"][..],
		&["list", "--restore-lock"][..],
		&["rollback", "1", "--profile"][..],
	] {
		let (code, _, err) = invoke(args);
		assert_eq!(code, ExitCode::from(2), "{args:?}");
		assert!(err.contains("usage: noxi snapshot"), "{err}");
	}
}
