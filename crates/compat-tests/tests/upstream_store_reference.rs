//! CppNix 2.35.2's store-reference characterization cases
//! (`store-reference.cc`).
//!
//! Round-trip cases must render to the file's exact bytes; read-only cases
//! only parse. `local_shorthand_path_windows` is read only on Windows, and on
//! Linux the reference rejects it, as checked here.

use std::path::{Path, PathBuf};

use nix_compat::nixcpp::store_reference::{Error, StoreReference, Variant};
use nix_compat::nixcpp::url::Query;

fn fixture(stem: &str) -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../tests/nix/src/libstore-tests/data/store-reference")
		.join(format!("{stem}.txt"))
}

fn read(stem: &str) -> String {
	std::fs::read_to_string(fixture(stem)).expect("fixture")
}

fn parse(text: &str) -> Result<StoreReference, Error> {
	StoreReference::parse(text, &Query::new(), Path::new("/"), &mut Vec::new())
}

fn params(pairs: &[(&str, &str)]) -> Query {
	pairs
		.iter()
		.map(|(key, value)| (key.to_string(), value.to_string()))
		.collect()
}

fn reference(variant: Variant, pairs: &[(&str, &str)]) -> StoreReference {
	StoreReference {
		variant,
		params: params(pairs),
	}
}

fn specified(scheme: &str, authority: &str) -> Variant {
	Variant::Specified {
		scheme: scheme.into(),
		authority: authority.into(),
	}
}

const ZONED: &str = "userinfo@[fea5:23e1:3916:fc24:cb52:2837:2ecb:ea8e%25eth0]";

fn cases() -> Vec<(&'static str, StoreReference, bool)> {
	let local_1 = reference(specified("local", ""), &[("root", "/foo/bar/baz")]);
	let local_2 = reference(specified("local", "/foo/bar/baz"), &[("trusted", "true")]);
	let local_3 = reference(specified("local", ""), &[("root", "/foo bar/baz")]);
	let unix = reference(
		specified("unix", ""),
		&[("max-connections", "7"), ("trusted", "true")],
	);
	let ab = [("a", "b"), ("c", "d")];
	vec![
		("auto", reference(Variant::Auto, &[]), true),
		(
			"auto_param",
			reference(Variant::Auto, &[("root", "/foo/bar/baz")]),
			true,
		),
		("local_1", local_1.clone(), true),
		("local_2", local_2.clone(), true),
		("local_3", local_3.clone(), true),
		("local_3_no_percent", local_3, false),
		("local_shorthand_1", local_1, false),
		("local_shorthand_path_unix", local_2, false),
		("local_shorthand_3", reference(Variant::Local, &[]), true),
		("unix", unix.clone(), true),
		("unix_shorthand", unix, false),
		("ssh", reference(specified("ssh", "localhost"), &[]), true),
		("daemon_shorthand", reference(Variant::Daemon, &[]), true),
		(
			"ssh_unbracketed_ipv6_1",
			reference(specified("ssh", "[::1]"), &[]),
			false,
		),
		(
			"ssh_unbracketed_ipv6_2",
			reference(
				specified("ssh", "userinfo@[fea5:23e1:3916:fc24:cb52:2837:2ecb:ea8e]"),
				&[],
			),
			false,
		),
		(
			"ssh_unbracketed_ipv6_3",
			reference(
				specified("ssh", "userinfo@[fea5:23e1:3916:fc24:cb52:2837:2ecb:ea8e]"),
				&ab,
			),
			false,
		),
		(
			"ssh_unbracketed_ipv6_4",
			reference(specified("ssh", ZONED), &ab),
			false,
		),
		(
			"ssh_unbracketed_ipv6_5",
			reference(specified("ssh", ZONED), &ab),
			false,
		),
		(
			"ssh_unbracketed_ipv6_6",
			reference(
				specified(
					"ssh",
					"userinfo@[fea5:23e1:3916:fc24:cb52:2837:2ecb:ea8e%2525]",
				),
				&ab,
			),
			false,
		),
		(
			"ssh_unbracketed_ipv6_7",
			reference(specified("ssh", ZONED), &ab),
			false,
		),
		(
			"ssh_unbracketed_ipv6_8",
			reference(
				specified("ssh", "[fea5:23e1:3916:fc24:cb52:2837:2ecb:ea8e%25eth0]"),
				&ab,
			),
			false,
		),
		(
			"ssh_unbracketed_ipv6_9",
			reference(
				specified("ssh", "[fea5:23e1:3916:fc24:cb52:2837:2ecb:ea8e%25eth0]"),
				&[],
			),
			false,
		),
	]
}

#[test]
fn store_references_match_cppnix() {
	for (stem, expected, round_trip) in cases() {
		let text = read(stem);
		assert_eq!(parse(&text).as_ref(), Ok(&expected), "{stem}");
		if round_trip {
			assert_eq!(expected.render(true), text, "{stem}");
		}
	}
}

#[test]
fn every_fixture_is_listed() {
	let mut listed: Vec<_> = cases().into_iter().map(|(stem, ..)| stem).collect();
	listed.push("local_shorthand_path_windows");
	listed.sort();
	let mut present: Vec<String> = std::fs::read_dir(fixture("x").parent().unwrap())
		.unwrap()
		.map(|entry| {
			let name = entry.unwrap().file_name().into_string().unwrap();
			name.strip_suffix(".txt").unwrap().to_owned()
		})
		.collect();
	present.sort();
	assert_eq!(listed, present);
}

/// The reference reads `C:\foo\bar\baz` as a store path only on Windows; on
/// Linux it is neither a URL nor a path.
#[test]
fn windows_path_is_rejected_on_linux() {
	let text = read("local_shorthand_path_windows");
	assert_eq!(parse(&text), Err(Error::Unparsable(text.clone())));
	assert_eq!(
		parse(&text).unwrap_err().to_string(),
		format!("Cannot parse Nix store '{text}'")
	);
}
