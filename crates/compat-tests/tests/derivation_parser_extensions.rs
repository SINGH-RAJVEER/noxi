//! CppNix accepts unchecked output names when reading ATerm, even though
//! construction of a new derivation must validate those names.

use nix_compat::derivation::{UnverifiedDerivation, lookup_fn};
use nix_compat::store_path::StorePath;

const OUTPUT_PATH: &str = "/nix/store/4q0pg5zpfmznxscq3avycvf9xdvx50n3-bar";
const INPUT_PATH: &str = "/nix/store/4q0pg5zpfmznxscq3avycvf9xdvx50n3-bar.drv";

fn replace_marker(bytes: &mut Vec<u8>, marker: &[u8], replacement: &[u8]) {
	let offset = bytes
		.windows(marker.len())
		.position(|window| window == marker)
		.expect("marker exists");
	bytes.splice(offset..offset + marker.len(), replacement.iter().copied());
}

#[test]
fn round_trips_non_utf8_platform_builder_argument_and_environment_key() {
	let mut aterm = format!(
		"Derive([(\"out\",{OUTPUT_PATH:?},\"\",\"\")],[],[],\"PLATFORM\",\"COMMAND\",[\"ARGUMENT\"],[(\"ENVKEY\",\"value\")])"
	)
	.into_bytes();
	for marker in [b"PLATFORM".as_slice(), b"COMMAND", b"ARGUMENT", b"ENVKEY"] {
		replace_marker(&mut aterm, marker, b"\xff");
	}

	let parsed = UnverifiedDerivation::from_aterm_bytes(&aterm)
		.expect("CppNix accepts non-UTF-8 ATerm fields");
	assert_eq!(parsed.system().as_ref() as &[u8], b"\xff");
	assert_eq!(parsed.command().as_ref() as &[u8], b"\xff");
	assert_eq!(parsed.arguments()[0].as_slice(), b"\xff");
	assert_eq!(
		parsed
			.environment()
			.get(b"\xff".as_slice())
			.unwrap()
			.as_slice(),
		b"value"
	);
	assert_eq!(parsed.to_aterm_bytes(), aterm);
	assert!(
		serde_json::to_value(&parsed).is_err(),
		"JSON keys require UTF-8"
	);
}

#[test]
fn utf8_fields_keep_the_existing_json_representation() {
	let aterm = format!(
		"Derive([(\"out\",{OUTPUT_PATH:?},\"\",\"\")],[],[],\"system\",\"builder\",[\"arg\"],[(\"key\",\"value\")])"
	);
	let parsed = UnverifiedDerivation::from_aterm_bytes(aterm.as_bytes()).unwrap();
	let json = serde_json::to_value(&parsed).unwrap();
	assert_eq!(json["system"], "system");
	assert_eq!(json["builder"], "builder");
	assert_eq!(json["args"], serde_json::json!(["arg"]));
	assert_eq!(
		json["env"]["key"],
		serde_json::json!([118, 97, 108, 117, 101])
	);
}

#[test]
fn parses_and_round_trips_unchecked_output_names() {
	for name in ["", "drv", "invalid/name"] {
		let aterm = format!("Derive([({name:?},{OUTPUT_PATH:?},\"\",\"\")],[],[],\"\",\"\",[],[])");
		let parsed = UnverifiedDerivation::from_aterm_bytes(aterm.as_bytes())
			.expect("CppNix accepts the output name in ATerm");
		assert_eq!(
			parsed.outputs().names().next().unwrap().as_str(),
			Some(name)
		);
		assert_eq!(parsed.to_aterm_bytes(), aterm.as_bytes());
		let (builder, outputs) = parsed.into_parts();
		builder
			.calculate_outputs("test", lookup_fn(|_| None))
			.expect_err("invalid names cannot be used to calculate output paths");
		builder
			.build_unverified(outputs)
			.expect_err("constructing a new derivation validates output names");
	}
}

#[test]
fn parses_and_round_trips_unchecked_input_output_names() {
	for name in ["", "drv", "invalid/name"] {
		let aterm = format!(
			"Derive([(\"out\",{OUTPUT_PATH:?},\"\",\"\")],[({INPUT_PATH:?},[{name:?}])],[],\"system\",\"builder\",[],[])"
		);
		let parsed = UnverifiedDerivation::from_aterm_bytes(aterm.as_bytes())
			.expect("CppNix accepts the input output name in ATerm");
		assert_eq!(parsed.to_aterm_bytes(), aterm.as_bytes());
		let (builder, outputs) = parsed.into_parts();
		builder
			.build_unverified(outputs)
			.expect_err("constructing a new derivation validates input output names");
	}
}

#[test]
fn round_trips_non_utf8_output_names() {
	let mut aterm = format!(
		"Derive([(\"OUTPUT\",{OUTPUT_PATH:?},\"\",\"\")],[({INPUT_PATH:?},[\"INPUT\"])],[],\"system\",\"builder\",[],[])"
	)
	.into_bytes();
	replace_marker(&mut aterm, b"OUTPUT", b"\xff");
	replace_marker(&mut aterm, b"INPUT", b"\xfe");

	let parsed = UnverifiedDerivation::from_aterm_bytes(&aterm).unwrap();
	assert_eq!(parsed.outputs().names().next().unwrap().as_bytes(), b"\xff");
	let input_path = StorePath::from_absolute_path(INPUT_PATH.as_bytes()).unwrap();
	assert_eq!(
		parsed.input_derivations()[&input_path]
			.iter()
			.next()
			.unwrap()
			.as_bytes(),
		b"\xfe"
	);
	assert_eq!(parsed.to_aterm_bytes(), aterm);
	assert!(serde_json::to_value(&parsed).is_err());
}

#[test]
fn round_trips_unchecked_fixed_output_name() {
	let aterm = format!(
		"Derive([(\"invalid/name\",{OUTPUT_PATH:?},\"sha256\",\"{}\")],[],[],\"\",\"\",[],[])",
		"00".repeat(32)
	);
	let parsed = UnverifiedDerivation::from_aterm_bytes(aterm.as_bytes()).unwrap();
	assert_eq!(
		parsed.outputs().names().next().unwrap().as_bytes(),
		b"invalid/name"
	);
	assert_eq!(parsed.to_aterm_bytes(), aterm.as_bytes());
	let mut raw = aterm.into_bytes();
	replace_marker(&mut raw, b"invalid/name", b"\xff");
	let parsed_raw = UnverifiedDerivation::from_aterm_bytes(&raw).unwrap();
	assert_eq!(
		parsed_raw.outputs().names().next().unwrap().as_bytes(),
		b"\xff"
	);
	assert_eq!(parsed_raw.to_aterm_bytes(), raw);
}

#[test]
fn round_trips_escaped_output_name() {
	let aterm =
		format!("Derive([(\"bad\\\"name\",{OUTPUT_PATH:?},\"\",\"\")],[],[],\"\",\"\",[],[])");
	let parsed = UnverifiedDerivation::from_aterm_bytes(aterm.as_bytes()).unwrap();
	assert_eq!(
		parsed.outputs().names().next().unwrap().as_bytes(),
		b"bad\"name"
	);
	assert_eq!(parsed.to_aterm_bytes(), aterm.as_bytes());
}
