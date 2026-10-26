//! Hash encodings captured from CppNix 2.35.2's `nix hash convert` command.
//! Each row hashes `abc`. The reference command used `NIX_CONFIG='experimental-features = nix-command blake3-hashes'` and `--hash-algo ALGO --to FORMAT HEX_DIGEST`.

use nix_compat::nixhash::{HashAlgo, NixHash};

struct Case {
	algo: HashAlgo,
	hex: &'static str,
	nix32: &'static str,
	base64: &'static str,
}

const CASES: &[Case] = &[
	Case {
		algo: HashAlgo::Md5,
		hex: "900150983cd24fb0d6963f7d28e17f72",
		nix32: "3jgzhjhz9zjvbb0kyj7jc500ch",
		base64: "kAFQmDzST7DWlj99KOF/cg==",
	},
	Case {
		algo: HashAlgo::Sha1,
		hex: "a9993e364706816aba3e25717850c26c9cd0d89d",
		nix32: "kpcd173cq987hw957sx6m0868wv3x6d9",
		base64: "qZk+NkcGgWq6PiVxeFDCbJzQ2J0=",
	},
	Case {
		algo: HashAlgo::Sha256,
		hex: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
		nix32: "1b8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s",
		base64: "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=",
	},
	Case {
		algo: HashAlgo::Sha512,
		hex: "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
		nix32: "2gs8k559z4rlahfx0y688s49m2vvszylcikrfinm30ly9rak69236nkam5ydvly1ai7xac99vxfc4ii84hawjbk876blyk1jfhkbbyx",
		base64: "3a81oZNherrMQXNJriBBMRLm+k6JqX6iCp7u5ktV05ohkpkqJ0/BqDa6PCOj/uu9RU1EI2Q86A4qmslPpUyknw==",
	},
	Case {
		algo: HashAlgo::Blake3,
		hex: "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85",
		nix32: "11cxppanr71mzl1xnyax8rccaj5milx2fx9vnvzk6la672nb6dv4",
		base64: "ZDezrDhGUTP/tjt1JzqNtUjFWEZdedsD/TWcbNW9nYU=",
	},
];

#[test]
fn every_reference_algorithm_round_trips_each_encoding() {
	for case in CASES {
		let hash = case.algo.digest_bytes("abc");
		assert_eq!(
			hash.as_nix_lowerhex_string_fmt().to_string(),
			format!("{}:{}", case.algo, case.hex)
		);
		assert_eq!(
			hash.to_nix_nixbase32(),
			format!("{}:{}", case.algo, case.nix32)
		);
		assert_eq!(
			hash.to_sri_string(),
			format!("{}-{}", case.algo, case.base64)
		);

		for digest in [case.hex, case.nix32, case.base64] {
			assert_eq!(NixHash::from_str(digest, Some(case.algo)).unwrap(), hash);
			assert_eq!(
				NixHash::from_str(&format!("{}:{digest}", case.algo), None).unwrap(),
				hash
			);
		}
		assert_eq!(NixHash::from_sri(&hash.to_sri_string()).unwrap(), hash);
	}
}

#[test]
fn reference_accepts_uppercase_hex_and_short_or_extended_sri() {
	let case = &CASES[2];
	let expected = case.algo.digest_bytes("abc");
	assert_eq!(
		NixHash::from_str(&case.hex.to_uppercase(), Some(case.algo)).unwrap(),
		expected
	);
	let short_sri = format!("sha256-{}", case.base64.trim_end_matches('='));
	assert_eq!(NixHash::from_sri(&short_sri).unwrap(), expected);
	let extended_sri = format!("sha256-{} sha1-AAAA", case.base64);
	assert_eq!(NixHash::from_sri(&extended_sri).unwrap(), expected);
}

#[test]
fn reference_rejects_malformed_hashes() {
	let case = &CASES[2];
	for digest in [
		"00".to_string(),
		format!("g{}", &case.hex[1..]),
		format!("{}!", &case.base64[..case.base64.len() - 1]),
		format!("{}u", &case.nix32[..case.nix32.len() - 1]),
	] {
		assert!(
			NixHash::from_str(&digest, Some(case.algo)).is_err(),
			"{digest}"
		);
	}
	assert!(NixHash::from_str(case.hex, None).is_err());
	assert!(
		NixHash::from_str(
			"sha1:a9993e364706816aba3e25717850c26c9cd0d89d",
			Some(case.algo)
		)
		.is_err()
	);
}

/// The `nix` and `nix-hash` commands run against the pinned reference's,
/// with stdout, stderr and the exit status compared.
///
/// The reference executables come from `NIXRS_REFERENCE_BIN` or
/// `target/nix-reference/bin` (`just reference`). The candidate is the
/// `nix` executable of nix-rs-commands, from `NIXRS_CANDIDATE_BIN` or next
/// in the profile directory above this test. `nextest --workspace` builds it first;
/// with `-p nixrs-compat-tests` alone, build nix-rs-commands beforehand.
/// Both run with an empty environment and configuration, and each program
/// name is passed as `argv[0]`, which selects `nix-hash`.
mod commands {
	use std::os::unix::process::CommandExt;
	use std::path::{Path, PathBuf};
	use std::process::Command;

	const HEX: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
	const SRI: &str = "sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=";
	const BLAKE3: &str = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";

	fn reference_bin() -> PathBuf {
		if let Some(path) = std::env::var_os("NIXRS_REFERENCE_BIN") {
			return PathBuf::from(path);
		}
		let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin");
		assert!(
			path.join("nix").exists(),
			"nix-reference/bin is missing; run `just reference` or set NIXRS_REFERENCE_BIN"
		);
		path
	}

	fn candidate() -> PathBuf {
		if let Some(path) = std::env::var_os("NIXRS_CANDIDATE_BIN") {
			return PathBuf::from(path).join("nix");
		}
		// The test runs from below target/<profile>, where the executables are.
		let exe = std::env::current_exe().unwrap();
		let profile = exe
			.ancestors()
			.skip(1)
			.find(|dir| dir.join("nix").is_file());
		let path = profile.unwrap_or(exe.parent().unwrap()).join("nix");
		assert!(
			path.exists(),
			"{} is missing; build nix-rs-commands first or set NIXRS_CANDIDATE_BIN",
			path.display()
		);
		path
	}

	struct Fixture {
		dir: tempfile::TempDir,
	}

	impl Fixture {
		fn new() -> Self {
			use std::os::unix::fs::PermissionsExt;
			let dir = tempfile::tempdir().unwrap();
			for sub in ["conf", "home", "work/d/sub"] {
				std::fs::create_dir_all(dir.path().join(sub)).unwrap();
			}
			let work = dir.path().join("work");
			std::fs::write(work.join("d/f"), "hi\n").unwrap();
			std::fs::write(work.join("d/sub/g"), "x\n").unwrap();
			std::fs::set_permissions(work.join("d/sub/g"), std::fs::Permissions::from_mode(0o755))
				.unwrap();
			std::os::unix::fs::symlink("f", work.join("d/link")).unwrap();
			Fixture { dir }
		}

		/// Run `program` under each engine and compare the results.
		fn check(&self, features: &str, program: &str, args: &[&str]) {
			let run = |executable: PathBuf| {
				let output = Command::new(&executable)
					.arg0(program)
					.args(args)
					.current_dir(self.dir.path().join("work"))
					.env_clear()
					.env("HOME", self.dir.path().join("home"))
					.env("NIX_CONF_DIR", self.dir.path().join("conf"))
					.env("NIX_CONFIG", format!("experimental-features = {features}"))
					.env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin")
					.output()
					.unwrap();
				(
					output.status.code(),
					String::from_utf8_lossy(&output.stdout).into_owned(),
					String::from_utf8_lossy(&output.stderr).into_owned(),
				)
			};
			let expected = run(reference_bin().join(program));
			let actual = run(candidate());
			assert_eq!(
				actual, expected,
				"{program} {args:?} with features '{features}'"
			);
		}
	}

	#[test]
	fn hash_convert_matches_reference() {
		let fixture = Fixture::new();
		let prefixed = format!("sha256:{HEX}");
		let bad_hex = format!("g{}", &HEX[1..]);
		let blake3 = format!("blake3:{BLAKE3}");
		let short_sri = &SRI[..SRI.len() - 1];
		let extended_sri = format!("{SRI}extra");
		let cases: &[(&str, &[&str])] = &[
			("", &["hash", "convert", &prefixed]),
			("", &["hash", "convert", "--bogus"]),
			("nix-command", &["hash", "convert", &prefixed]),
			(
				"nix-command",
				&[
					"hash",
					"convert",
					"--hash-algo",
					"sha256",
					"--to",
					"nix32",
					HEX,
				],
			),
			(
				"nix-command",
				&[
					"hash",
					"convert",
					"--to",
					"base16",
					SRI,
					short_sri,
					&extended_sri,
				],
			),
			(
				"nix-command",
				&[
					"hash",
					"convert",
					"--to",
					"base64",
					&HEX.to_uppercase(),
					"--hash-algo",
					"sha256",
				],
			),
			(
				"nix-command",
				&["hash", "convert", "--to", "base32", &prefixed],
			),
			(
				"nix-command",
				&[
					"hash",
					"convert",
					"--from",
					"nix32",
					"--hash-algo",
					"sha256",
					HEX,
				],
			),
			(
				"nix-command",
				&[
					"hash", "convert", "--from", "sri", "--to", "base16", SRI, &prefixed,
				],
			),
			// Malformed input.
			(
				"nix-command",
				&["hash", "convert", "--hash-algo", "sha256", &bad_hex],
			),
			(
				"nix-command",
				&["hash", "convert", "--hash-algo", "sha256", "00"],
			),
			("nix-command", &["hash", "convert", "00"]),
			("nix-command", &["hash", "convert", "sha256-ungWv48Bz"]),
			(
				"nix-command",
				&[
					"hash",
					"convert",
					"sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa!",
				],
			),
			(
				"nix-command",
				&[
					"hash",
					"convert",
					"--hash-algo",
					"sha256",
					"zb8m03r63zqhnjf7l5wnldhh7c134ap5vpj0850ymkq1iyzicy5s",
				],
			),
			(
				"nix-command",
				&["hash", "convert", "--hash-algo", "md5", &prefixed],
			),
			("nix-command", &["hash", "convert", "foo:abc"]),
			("nix-command", &["hash", "convert", "--", "-abc"]),
			("nix-command", &["hash", "convert", "-abc"]),
			(
				"nix-command",
				&["hash", "convert", "--to", "foo", &prefixed],
			),
			(
				"nix-command",
				&["hash", "convert", "--hash-algo", "foo", "x"],
			),
			("nix-command", &["hash", "convert", "--hash-algo"]),
			// BLAKE3 is gated where its name is read.
			("nix-command", &["hash", "convert", &blake3]),
			(
				"nix-command",
				&["hash", "convert", "--hash-algo", "blake3", BLAKE3],
			),
			(
				"nix-command",
				&[
					"hash",
					"convert",
					"--hash-algo",
					"blake3",
					"--extra-experimental-features",
					"blake3-hashes",
					BLAKE3,
				],
			),
			(
				"nix-command",
				&[
					"--extra-experimental-features",
					"blake3-hashes",
					"hash",
					"convert",
					"--hash-algo",
					"blake3",
					BLAKE3,
				],
			),
			(
				"nix-command blake3-hashes",
				&["hash", "convert", "--to", "nix32", &blake3],
			),
			// The deprecated conversions.
			("nix-command", &["hash", "to-sri", "--type", "sha256", HEX]),
			("nix-command", &["to-base16", SRI]),
		];
		for (features, args) in cases {
			fixture.check(features, "nix", args);
		}
	}

	#[test]
	fn hash_path_and_file_match_reference() {
		let fixture = Fixture::new();
		let cases: &[&[&str]] = &[
			&["hash", "path", "d", "d/f", "d/link"],
			&["hash", "path", "--mode", "flat", "d/f"],
			&["hash", "path", "--mode", "text", "d/f"],
			&["hash", "path", "--mode", "git", "d"],
			&["hash", "path", "--type", "sha1", "--base32", "d"],
			&["hash", "path", "--algo", "sha512", "--base64", "d"],
			&["hash", "path", "--format", "base16", "--sri", "d"],
			&["hash", "path", "--format", "foo", "d"],
			&["hash", "path", "--truncate", "d"],
			&["hash", "path", "missing"],
			&["hash", "file", "d/f"],
			&["hash", "file", "--type", "md5", "--base16", "d/f"],
			&["hash", "file", "d"],
			&["hash", "file", "missing"],
			&["hash-path", "d"],
		];
		for args in cases {
			fixture.check("nix-command", "nix", args);
		}
	}

	#[test]
	fn command_selection_matches_reference() {
		let fixture = Fixture::new();
		let cases: &[(&str, &[&str])] = &[
			("", &[]),
			("nix-command", &[]),
			("nix-command", &["--version"]),
			("nix-command", &["hash"]),
			("nix-command", &["hash", "bogus"]),
			("nix-command", &["hash", "conver"]),
			("nix-command", &["hsh"]),
			("nix-command", &["--option", "foo", "bar", "hash"]),
			("", &["--option", "foo", "bar", "--offline", "hash"]),
		];
		for (features, args) in cases {
			fixture.check(features, "nix", args);
		}
	}

	#[test]
	fn legacy_nix_hash_matches_reference() {
		let fixture = Fixture::new();
		let prefixed = format!("sha256:{HEX}");
		let cases: &[&[&str]] = &[
			&["d"],
			&["--flat", "d/f"],
			&["--flat", "d/link"],
			&["--type", "sha256", "--base32", "d"],
			&["--type", "sha256", "--sri", "d"],
			&["--type", "sha256", "--truncate", "d"],
			&["--type", "sha512", "--truncate", "--base64", "d"],
			&["--to-base32", "--type", "sha256", HEX],
			&["--to-sri", &prefixed],
			&["--to-base16", HEX],
			&["--to-base64", "--type", "sha256", "00"],
			&["--type", "blake3", "d"],
			&[
				"--type",
				"blake3",
				"--extra-experimental-features",
				"blake3-hashes",
				"d",
			],
			&[
				"--extra-experimental-features",
				"blake3-hashes",
				"--type",
				"blake3",
				"d",
			],
			&[
				"--option",
				"experimental-features",
				"blake3-hashes",
				"--type",
				"blake3",
				"d",
			],
			&["--option", "foo", "bar", "d"],
			&["--bogus"],
			&["-"],
			&["--type"],
			&["--type", "foo", "d"],
			&["--type", "sha256", "--", "-x"],
			&["--store"],
			&["--option", "x"],
			&["missing"],
			&["--flat", "missing"],
			&["--flat", "d"],
			&[],
			&["--version"],
		];
		for args in cases {
			fixture.check("", "nix-hash", args);
		}
	}
}
