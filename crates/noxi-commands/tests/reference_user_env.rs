//! `nix-env`'s user environments checked against the pinned CppNix 2.35.2
//! reference.
//!
//! A profile's links name `/nix/store` paths, which a diverted store does
//! not have there, so the scenario runs in a user and mount namespace
//! (`unshare`) where `/nix/store` is an overlay: the host's store below,
//! and a scratch directory above that takes every write. The host's store
//! is not changed. There the reference adds four packages, and each
//! engine's `nix-env` installs, replaces, queries and uninstalls them, and
//! hits a file conflict, against the local store and through a reference
//! daemon. Output, exit statuses, the generations' store paths, their
//! manifests and their symlink trees must match.
//!
//! The reference executables come from `NOXI_REFERENCE_BIN` or
//! `target/nix-reference/bin` (`just reference` builds them). `unshare`
//! and `bash` must be on `PATH`, and unprivileged user namespaces and
//! overlay mounts must be allowed.

use std::path::{Path, PathBuf};
use std::process::Command;

fn reference_bin() -> PathBuf {
	if let Some(path) = std::env::var_os("NOXI_REFERENCE_BIN") {
		return PathBuf::from(path);
	}
	let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin");
	assert!(
		path.join("nix-store").exists(),
		"nix-reference/bin is missing; run `just reference` or set NOXI_REFERENCE_BIN"
	);
	path.canonicalize().unwrap()
}

/// Runs in the namespace with `$G` the scratch directory, `$R` the
/// reference's executables, `$NIX_ENV` the `nix-env` under test and
/// `$MODE` either `local` or `daemon`.
const SCENARIO: &str = r#"
set -u
mount -t overlay overlay -o lowerdir=/nix/store,upperdir=$G/up,workdir=$G/work /nix/store || exit 90
export HOME=$G/home NIX_STATE_DIR=$G/state NIX_LOG_DIR=$G/log NIX_CONF_DIR=$G/etc NIX_PATH=
export NIX_CONFIG=$'build-users-group =\nsandbox = false\nsubstituters =\n'
export NIX_REMOTE=local
if [ "$MODE" = daemon ]; then
	# The daemon must not hold the test's output open.
	NIX_DAEMON_SOCKET_PATH=$G/daemon.sock $R/nix-daemon >/dev/null 2>&1 &
	daemon=$!
	trap 'kill $daemon' EXIT
	for _ in $(seq 200); do [ -S $G/daemon.sock ] && break; sleep 0.05; done
	export NIX_REMOTE=unix://$G/daemon.sock
fi
mk() {
	mkdir -p $G/src/$1/bin $G/src/$1/share
	echo "$1" > $G/src/$1/bin/$2
	echo doc > $G/src/$1/share/$1.txt
	$R/nix-store --add $G/src/$1
}
a=$(mk hello-1.0 hello); b=$(mk hello-2.0 hello); c=$(mk tool tool); d=$(mk clash hello)
P=$G/state/profiles/test
cd $G
run() {
	echo "--- nix-env $*"
	$NIX_ENV -p $P "$@" 2>&1
	echo "exit $?"
}
run -i $a; run -q; run -i $c; run -q --out-path; run -i $b; run -q; run -i $d
run -e hello; run -q; run -e nothing; run -i $a --dry-run; run -q --out-path --system --drv-path
run --list-generations | sed -E 's/[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9:]{8}/DATE/'
for g in $P-*-link; do
	echo "== $(basename $g) -> $(readlink $g)"
	cat $g/manifest.nix
	echo
	(cd $g && find . | sort)
done
"#;

fn run(dir: &Path, nix_env: &Path, mode: &str) -> String {
	if dir.exists() {
		// The overlay's work directory belongs to the namespace's root.
		let status = Command::new("unshare")
			.args(["--user", "--map-root-user", "rm", "-rf"])
			.arg(dir)
			.status()
			.unwrap();
		assert!(status.success());
	}
	for sub in ["up", "work", "state", "log", "home", "etc", "src"] {
		std::fs::create_dir_all(dir.join(sub)).unwrap();
	}
	let output = Command::new("unshare")
		.args([
			"--user",
			"--map-root-user",
			"--mount",
			"bash",
			"-c",
			SCENARIO,
		])
		.env("G", dir)
		.env("R", reference_bin())
		.env("NIX_ENV", nix_env)
		.env("MODE", mode)
		.output()
		.expect("run unshare");
	assert_ne!(
		output.status.code(),
		Some(90),
		"cannot mount an overlay on /nix/store in a user namespace"
	);
	String::from_utf8_lossy(&output.stdout)
		.replace(&nix_env.display().to_string(), "nix-env")
		.replace(&dir.display().to_string(), "G")
}

#[test]
fn manages_user_environments_like_cppnix() {
	let base = tempfile::tempdir().unwrap();
	let dir = base.path().join("scenario");
	let reference = reference_bin().join("nix-env");
	let noxi = Path::new(env!("CARGO_BIN_EXE_nix-env"));
	for mode in ["local", "daemon"] {
		let expected = run(&dir, &reference, mode);
		assert!(expected.contains("replacing old 'hello-1.0'"), "{expected}");
		let actual = run(&dir, noxi, mode);
		assert_eq!(actual, expected, "in {mode} mode");
	}
	let _ = Command::new("unshare")
		.args(["--user", "--map-root-user", "rm", "-rf"])
		.arg(&dir)
		.status();
}
