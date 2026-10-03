//! CppNix 2.35.2's remote builder specification cases (`machines.cc`),
//! including its two copied machines files.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use nix_compat::nixcpp::machines::{Error, Machine};
use nix_compat::nixcpp::store_reference::{StoreReference, Variant};
use nix_compat::nixcpp::url::Query;

fn fixture(name: &str) -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../tests/nix/src/libstore-tests/data/machines")
		.join(name)
}

fn set(items: &[&str]) -> BTreeSet<String> {
	items.iter().map(|item| item.to_string()).collect()
}

fn parse_with(defaults: &[&str], s: &str) -> Result<Vec<Machine>, Error> {
	Machine::parse_config(&set(defaults), s, Path::new("/"), &mut Vec::new())
}

fn parse(s: &str) -> Vec<Machine> {
	parse_with(&[], s).expect("machines")
}

fn reference(uri: &str) -> StoreReference {
	StoreReference::parse(uri, &Query::new(), Path::new("/"), &mut Vec::new()).unwrap()
}

fn authorities(machines: &[Machine]) -> BTreeSet<String> {
	machines
		.iter()
		.map(|machine| match &machine.store_uri.variant {
			Variant::Specified { authority, .. } => authority.clone(),
			other => panic!("unexpected {other:?}"),
		})
		.collect()
}

#[test]
fn empty_builders() {
	assert!(parse("").is_empty());
}

#[test]
fn uri_only_and_defaults() {
	for line in [
		"nix@scratchy.labs.cs.uu.nl",
		"nix@scratchy.labs.cs.uu.nl - - - - - - -",
	] {
		let machines = parse_with(&["TEST_ARCH-TEST_OS"], line).unwrap();
		assert_eq!(machines.len(), 1);
		let machine = &machines[0];
		assert_eq!(
			machine.store_uri,
			reference("ssh://nix@scratchy.labs.cs.uu.nl")
		);
		assert_eq!(machine.system_types, set(&["TEST_ARCH-TEST_OS"]));
		assert_eq!(machine.ssh_key, None);
		assert_eq!(machine.max_jobs, 1);
		assert_eq!(machine.speed_factor, 1.0);
		assert!(machine.supported_features.is_empty());
		assert!(machine.mandatory_features.is_empty());
		assert!(machine.ssh_public_host_key.is_empty());
	}
}

#[test]
fn uri_with_port() {
	let machines = parse_with(&["TEST_ARCH-TEST_OS"], "nix@scratchy.labs.cs.uu.nl:2222").unwrap();
	assert_eq!(
		machines[0].store_uri,
		reference("ssh://nix@scratchy.labs.cs.uu.nl:2222")
	);
}

#[test]
fn separators_comments_and_whitespace() {
	let two = set(&["nix@scratchy.labs.cs.uu.nl", "nix@itchy.labs.cs.uu.nl"]);
	let three = set(&[
		"nix@scratchy.labs.cs.uu.nl",
		"nix@itchy.labs.cs.uu.nl",
		"nix@scabby.labs.cs.uu.nl",
	]);
	assert_eq!(
		authorities(&parse(
			"nix@scratchy.labs.cs.uu.nl\nnix@itchy.labs.cs.uu.nl"
		)),
		two
	);
	assert_eq!(
		authorities(&parse(
			"nix@scratchy.labs.cs.uu.nl ; nix@itchy.labs.cs.uu.nl"
		)),
		two
	);
	let commented = parse(
		"# This is a comment ; this is still that comment\n\
		 nix@scratchy.labs.cs.uu.nl ; nix@itchy.labs.cs.uu.nl\n\
		 # This is also a comment ; this also is still that comment\n\
		 nix@scabby.labs.cs.uu.nl\n",
	);
	assert_eq!(commented.len(), 3);
	assert_eq!(authorities(&commented), three);
	let funny = parse(
		"        # comment ; comment\n   nix@scratchy.labs.cs.uu.nl ; nix@itchy.labs.cs.uu.nl   \n\n    \n\n ;;; \n\n ; ; \nnix@scabby.labs.cs.uu.nl\n\n",
	);
	assert_eq!(funny.len(), 3);
	assert_eq!(authorities(&funny), three);
}

#[test]
fn complete_single_builder() {
	for line in [
		"nix@scratchy.labs.cs.uu.nl     i686-linux      /home/nix/.ssh/id_scratchy_auto        8 3 kvm benchmark SSH+HOST+PUBLIC+KEY+BASE64+ENCODED==",
		"nix@scratchy.labs.cs.uu.nl\ti686-linux\t/home/nix/.ssh/id_scratchy_auto\t8\t3\tkvm\tbenchmark\tSSH+HOST+PUBLIC+KEY+BASE64+ENCODED==",
	] {
		let machines = parse(line);
		assert_eq!(machines.len(), 1);
		let machine = &machines[0];
		assert_eq!(authorities(&machines), set(&["nix@scratchy.labs.cs.uu.nl"]));
		assert_eq!(machine.system_types, set(&["i686-linux"]));
		assert_eq!(
			machine.ssh_key.as_deref(),
			Some(Path::new("/home/nix/.ssh/id_scratchy_auto"))
		);
		assert_eq!(machine.max_jobs, 8);
		assert_eq!(machine.speed_factor, 3.0);
		assert_eq!(machine.supported_features, set(&["kvm"]));
		assert_eq!(machine.mandatory_features, set(&["benchmark"]));
		assert_eq!(
			machine.ssh_public_host_key,
			"SSH+HOST+PUBLIC+KEY+BASE64+ENCODED=="
		);
	}
}

#[test]
fn multiple_options() {
	let machines = parse(
		"nix@scratchy.labs.cs.uu.nl Arch1,Arch2 - - - SupportedFeature1,SupportedFeature2 MandatoryFeature1,MandatoryFeature2",
	);
	let machine = &machines[0];
	assert_eq!(machine.system_types, set(&["Arch1", "Arch2"]));
	assert_eq!(
		machine.supported_features,
		set(&["SupportedFeature1", "SupportedFeature2"])
	);
	assert_eq!(
		machine.mandatory_features,
		set(&["MandatoryFeature1", "MandatoryFeature2"])
	);
}

#[test]
fn incorrect_formats() {
	for (line, usage) in [
		("nix@scratchy.labs.cs.uu.nl - - eight", false),
		("nix@scratchy.labs.cs.uu.nl - - -1", false),
		("nix@scratchy.labs.cs.uu.nl - - 8 three", false),
		("nix@scratchy.labs.cs.uu.nl - - 8 -3", true),
		("nix@scratchy.labs.cs.uu.nl - - 8 3 - - BAD_BASE64", false),
	] {
		match parse_with(&[], line) {
			Err(Error::Usage(_)) if usage => {}
			Err(Error::Format(_)) if !usage => {}
			other => panic!("{line}: {other:?}"),
		}
	}
	assert_eq!(
		parse_with(&[], "nix@scratchy.labs.cs.uu.nl - - eight")
			.unwrap_err()
			.to_string(),
		"bad machine specification: failed to convert column #3 in a row: 'nix@scratchy.labs.cs.uu.nl - - eight' to 'unsigned int'"
	);
}

#[test]
fn file_references() {
	let valid = parse(&format!("@{}", fixture("valid").display()));
	assert_eq!(valid.len(), 3);
	assert_eq!(
		authorities(&valid),
		set(&[
			"nix@scratchy.labs.cs.uu.nl",
			"nix@itchy.labs.cs.uu.nl",
			"nix@poochie.labs.cs.uu.nl",
		])
	);
	let poochie = &valid[2];
	assert_eq!(poochie.max_jobs, 1);
	assert_eq!(poochie.speed_factor, 2.0);
	assert_eq!(poochie.mandatory_features, set(&["benchmark"]));

	let empty = tempfile::NamedTempFile::new().unwrap();
	assert!(parse(&format!("@{}", empty.path().display())).is_empty());
	assert!(parse("@/not/a/file").is_empty());

	match parse_with(&[], &format!("@{}", fixture("bad_format").display())) {
		Err(Error::Format(_)) => {}
		other => panic!("{other:?}"),
	}
}

#[test]
fn complete_store_reference_adds_cppnix_parameters() {
	let machines = parse(
		"nix@host x86_64-linux /key 2 1 kvm,big benchmark SSH+HOST==\nssh-ng://other - - - - kvm",
	);
	assert_eq!(
		machines[0].complete_store_reference().render(true),
		"ssh://nix@host?base64-ssh-public-host-key=SSH%2BHOST%3D%3D&log-fd=4&max-connections=1&ssh-key=/key&system-features=big%20kvm%20benchmark"
	);
	assert_eq!(
		machines[1].complete_store_reference().render(true),
		"ssh-ng://other?system-features=kvm"
	);
}
