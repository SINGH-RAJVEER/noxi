//! Floating CA construction must preserve real ATerm declarations and placeholders.
use std::{
	fs,
	path::{Path, PathBuf},
	process::{Command, Output},
};

fn evaluate(binary: &Path, home: &Path, store: &str, expression: &str, writable: bool) -> Output {
	evaluate_with_features(binary, home, store, expression, writable, "ca-derivations")
}

fn evaluate_with_features(
	binary: &Path,
	home: &Path,
	store: &str,
	expression: &str,
	writable: bool,
	features: &str,
) -> Output {
	let mut command = Command::new(binary);
	command
		.env_clear()
		.env("HOME", home)
		.env("NIX_CONF_DIR", home)
		.env("NIX_USER_CONF_FILES", "")
		.env("NIX_PATH", "")
		.env(
			"NIX_CONFIG",
			format!("experimental-features = {features}\nsubstituters =\n"),
		)
		.args([
			"--store", store, "--eval", "--strict", "--json", "--expr", expression,
		]);
	if writable {
		command.arg("--read-write-mode");
	}
	command.output().unwrap()
}

#[test]
fn impure_placeholder_feature_checks_follow_attribute_validation() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate");
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	let home = noxi_test_support::tempdir().unwrap();
	for (attrs, features) in [
		("__impure = true;", "impure-derivations"),
		(
			"__impure = true; __structuredAttrs = true;",
			"impure-derivations",
		),
		(
			"__impure = true; zzz = throw \"later attribute\";",
			"impure-derivations",
		),
		("__impure = false;", "impure-derivations"),
		("outputHashMode = \"text\";", ""),
		("outputHashMode = \"git\";", ""),
		(
			"__structuredAttrs = true; outputHashMode = \"text\"; zzz = throw \"later\";",
			"",
		),
		(
			"__structuredAttrs = true; outputHashMode = \"git\"; zzz = throw \"later\";",
			"",
		),
		(
			"__impure = true; __contentAddressed = true;",
			"impure-derivations ca-derivations",
		),
		(
			"__impure = true; outputHash = \"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\";",
			"impure-derivations",
		),
	] {
		let expression = format!(
			"(derivation {{ name = \"audit\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; {attrs} }}).drvPath"
		);
		let expected = evaluate_with_features(
			&reference,
			home.path(),
			"dummy://",
			&expression,
			false,
			features,
		);
		let actual = evaluate_with_features(
			candidate,
			home.path(),
			"dummy://",
			&expression,
			false,
			features,
		);
		assert_eq!(
			(actual.status.code(), actual.stdout, actual.stderr),
			(expected.status.code(), expected.stdout, expected.stderr),
			"{expression}, features={features}"
		);
	}
}

#[test]
fn floating_ca_paths_placeholders_and_registered_bytes_match_reference() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate");
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	let home = noxi_test_support::tempdir().unwrap();
	for (kind, attrs, features) in [
		("__contentAddressed = true;", "", "ca-derivations"),
		(
			"__contentAddressed = true;",
			"__structuredAttrs = true;",
			"ca-derivations",
		),
		(
			"__contentAddressed = true;",
			"outputs = [ \"out\" \"dev\" ];",
			"ca-derivations",
		),
		(
			"__contentAddressed = true;",
			"outputHashMode = \"flat\"; outputHashAlgo = \"sha1\";",
			"ca-derivations",
		),
		(
			"__contentAddressed = true;",
			"args = [ \"one\" \"two\" ]; value = \"quoted\\\" text\";",
			"ca-derivations",
		),
		(
			"__contentAddressed = true;",
			"outputHashMode = \"text\";",
			"ca-derivations dynamic-derivations",
		),
		(
			"__contentAddressed = true;",
			"outputHashMode = \"git\"; outputHashAlgo = \"sha1\";",
			"ca-derivations git-hashing",
		),
		("__impure = true;", "", "ca-derivations impure-derivations"),
		(
			"__impure = true;",
			"outputs = [ \"out\" \"dev\" ]; __structuredAttrs = true;",
			"ca-derivations impure-derivations",
		),
		(
			"__impure = true;",
			"outputHashMode = \"flat\"; outputHashAlgo = \"sha1\";",
			"ca-derivations impure-derivations",
		),
		(
			"__impure = true;",
			"outputHashMode = \"text\";",
			"ca-derivations impure-derivations dynamic-derivations",
		),
		(
			"__impure = true;",
			"outputHashMode = \"git\"; outputHashAlgo = \"sha1\";",
			"ca-derivations impure-derivations git-hashing",
		),
		(
			"",
			"outputHashMode = \"text\"; outputHashAlgo = \"sha256\"; outputHash = \"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\";",
			"dynamic-derivations",
		),
		(
			"",
			"outputHashMode = \"git\"; outputHashAlgo = \"sha1\"; outputHash = \"da39a3ee5e6b4b0d3255bfef95601890afd80709\";",
			"git-hashing",
		),
		(
			"__impure = true;",
			"outputHashMode = \"text\"; outputHashAlgo = \"sha256\"; outputHash = \"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\";",
			"impure-derivations dynamic-derivations",
		),
		(
			"__contentAddressed = true;",
			"outputHashMode = \"git\"; outputHashAlgo = \"sha256\"; outputHash = \"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\";",
			"ca-derivations git-hashing",
		),
	] {
		let expression = format!(
			"let d = derivation {{ name = \"audit\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; {kind} {attrs} }}; in {{ drvPath = d.drvPath; output = d.outPath; context = builtins.getContext d.outPath; }}"
		);
		for writable in [false, true] {
			let roots = [
				noxi_test_support::tempdir().unwrap(),
				noxi_test_support::tempdir().unwrap(),
			];
			let stores = roots
				.iter()
				.map(|root| format!("local?root={}", root.path().display()))
				.collect::<Vec<_>>();
			let actual = evaluate_with_features(
				candidate,
				home.path(),
				if writable { &stores[0] } else { "dummy://" },
				&expression,
				writable,
				features,
			);
			let expected = evaluate_with_features(
				&reference,
				home.path(),
				if writable { &stores[1] } else { "dummy://" },
				&expression,
				writable,
				features,
			);
			assert!(expected.status.success(), "{expression}: {expected:?}");
			assert_eq!(
				(actual.status.code(), &actual.stdout, actual.stderr),
				(expected.status.code(), &expected.stdout, expected.stderr),
				"{expression} writable={writable}"
			);
			if writable {
				let value: serde_json::Value = serde_json::from_slice(&actual.stdout).unwrap();
				let name = Path::new(value["drvPath"].as_str().unwrap())
					.file_name()
					.unwrap();
				let bytes = roots
					.iter()
					.map(|root| fs::read(root.path().join("nix/store").join(name)).unwrap())
					.collect::<Vec<_>>();
				assert_eq!(bytes[0], bytes[1], "registered ATerm {attrs}");
				let parsed =
					noxi_store::build_derivation::BuildDerivation::from_aterm_bytes_with_features(
						&bytes[0],
						noxi_store::build_derivation::OutputFeatures {
							ca_derivations: true,
							impure_derivations: true,
							dynamic_derivations: true,
							git_hashing: true,
						},
					)
					.unwrap();
				assert!(!parsed.floating.is_empty() || !parsed.experimental.is_empty());
				assert_eq!(parsed.to_aterm_bytes(), bytes[0]);
				if !parsed.experimental.is_empty() {
					assert!(
						noxi_store::build_derivation::BuildDerivation::from_aterm_bytes(
							&bytes[0], true
						)
						.is_err(),
						"the legacy parser must not enable additional output features"
					);
				}
				let mut config = noxi_store::LocalStoreConfig::diverted(roots[0].path()).unwrap();
				config.ca_derivations = true;
				config.impure_derivations = true;
				config.dynamic_derivations = true;
				config.git_hashing = true;
				let store = noxi_store::LocalStore::open(config).unwrap();
				let path = nix_compat::store_path::StorePath::from_absolute_path(
					value["drvPath"].as_str().unwrap().as_bytes(),
				)
				.unwrap();
				assert_eq!(
					store.read_build_derivation(&path).unwrap().to_aterm_bytes(),
					bytes[0]
				);
				let registered_outputs = store.query_derivation_outputs(&path).unwrap();
				for (name, output) in &parsed.experimental {
					if let Some((expected, _)) = &output.fixed {
						assert_eq!(registered_outputs.get(name), Some(expected));
					}
				}
			}
		}
	}
}

#[test]
fn dependencies_of_floating_ca_outputs_keep_deferred_paths() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate");
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	let home = noxi_test_support::tempdir().unwrap();
	for content_addressed in [false, true] {
		let expression = format!(
			"let a = derivation {{ name = \"input\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; __contentAddressed = true; }}; b = derivation {{ name = \"dependent\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; input = a; __contentAddressed = {content_addressed}; }}; in {{ drvPath = b.drvPath; output = b.outPath; }}"
		);
		let actual = evaluate(candidate, home.path(), "dummy://", &expression, false);
		let expected = evaluate(&reference, home.path(), "dummy://", &expression, false);
		assert!(expected.status.success(), "{expected:?}");
		assert_eq!(
			(actual.status.code(), actual.stdout, actual.stderr),
			(expected.status.code(), expected.stdout, expected.stderr),
			"{expression}"
		);
	}
}

#[test]
fn fixed_text_and_git_inputs_preserve_parent_hashes() {
	let reference = std::env::var_os("NOXI_REFERENCE_BIN")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin")
		})
		.join("nix-instantiate");
	let candidate = Path::new(env!("CARGO_BIN_EXE_nix-instantiate"));
	let home = noxi_test_support::tempdir().unwrap();
	for mode in ["text", "git"] {
		let expression = format!(
			"let a = derivation {{ name = \"source\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; outputHashMode = \"{mode}\"; outputHash = \"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\"; }}; b = derivation {{ name = \"dependent\"; system = \"x86_64-linux\"; builder = \"/bin/sh\"; input = a; }}; in {{ drvPath = b.drvPath; output = b.outPath; }}"
		);
		let expected = evaluate_with_features(
			&reference,
			home.path(),
			"dummy://",
			&expression,
			false,
			"dynamic-derivations git-hashing",
		);
		let actual = evaluate_with_features(
			candidate,
			home.path(),
			"dummy://",
			&expression,
			false,
			"dynamic-derivations git-hashing",
		);
		assert!(expected.status.success(), "{expected:?}");
		assert_eq!(
			(actual.status.code(), actual.stdout, actual.stderr),
			(expected.status.code(), expected.stdout, expected.stderr),
			"{expression}"
		);
	}
}
