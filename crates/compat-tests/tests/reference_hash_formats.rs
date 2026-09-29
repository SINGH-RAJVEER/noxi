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
