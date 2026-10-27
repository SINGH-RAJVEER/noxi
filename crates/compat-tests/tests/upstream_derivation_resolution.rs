//! CppNix 2.35.2's `derivation/invariants` (`derivation/invariants.cc`) and
//! `derivation/try-resolve` (`derivations.cc`) characterization cases.
//!
//! Beyond the C++ tests, the expected paths and errors were checked with the
//! 2.35.2 `nix derivation add`, which fills in and then checks each JSON
//! derivation: the dependency, the filled-in derivations and every failure
//! below are what the reference reports.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bstr::BString;
use nix_compat::cppnix_json::derivation::{Derivation, Features, Output};
use nix_compat::cppnix_json::derivation_hash::{Error, SingleDerivedPath};
use nix_compat::derivation::OutputName;
use nix_compat::nixhash::HashAlgo;
use nix_compat::store_path::StorePath;
use noxi_build::options::DerivationOptions;
use serde_json::Value;

const ROOT: &str = "../../tests/nix/src/libstore-tests/data/derivation";

fn fixture(name: &str) -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join(ROOT).join(name)
}

fn read(name: &str) -> (String, Value) {
	let text = std::fs::read_to_string(fixture(name)).expect(name);
	let value = serde_json::from_str(&text).expect(name);
	(text, value)
}

fn written(value: &Value) -> String {
	serde_json::to_string_pretty(value).unwrap() + "\n"
}

fn ca() -> Features {
	Features {
		ca_derivations: true,
		..Features::default()
	}
}

fn derivation(name: &str) -> Derivation {
	let (text, value) = read(name);
	let drv = Derivation::from_json(&value, ca()).expect(name);
	assert_eq!(written(&drv.to_json()), text, "{name}");
	drv
}

fn path(s: &str) -> StorePath {
	StorePath::from_bytes(s.as_bytes()).unwrap()
}

/// The CA floating derivation the invariants tests write to their dummy
/// store, so that dependent derivations stay deferred.
fn dependency() -> Derivation {
	Derivation {
		name: "dependency".into(),
		outputs: BTreeMap::from([(
			OutputName::out(),
			Output::Floating {
				method: "nar".into(),
				hash_algo: HashAlgo::Sha256,
			},
		)]),
		system: "x86_64-linux".into(),
		builder: "/bin/sh".into(),
		args: vec![],
		env: BTreeMap::from([("out".into(), String::new())]),
		input_sources: BTreeSet::new(),
		input_derivations: BTreeMap::new(),
		structured_attrs: None,
	}
}

fn store() -> BTreeMap<StorePath, Derivation> {
	let dependency = dependency();
	let path = dependency.path().unwrap();
	assert_eq!(
		path.to_string(),
		"lg4c4b8r9hlczwprl6kgnzfd9mc1xmkk-dependency.drv"
	);
	BTreeMap::from([(path, dependency)])
}

fn fill_in(drv: &mut Derivation) -> Result<(), Error> {
	let store = store();
	drv.fill_in_output_paths(&|p| store.get(p).cloned(), &mut Vec::new())
}

fn check(drv: &Derivation) -> Result<(), Error> {
	let store = store();
	drv.check_invariants(&|p| store.get(p).cloned(), &mut Vec::new())
}

fn invariants(stem: &str) -> Derivation {
	derivation(&format!("invariants/{stem}.json"))
}

#[test]
fn deferred_outputs_are_filled_in() {
	let mut drv = invariants("filled-in-deferred-empty-env-var-pre");
	fill_in(&mut drv).unwrap();
	assert_eq!(drv, invariants("filled-in-deferred-empty-env-var-post"));
	check(&drv).unwrap();
	assert_eq!(
		drv.path().unwrap().to_string(),
		"axn89rnycwhh30lw6ab2j4k0rblpzr8d-filled-in-deferred-empty-env-var.drv"
	);

	let mut drv = invariants("filled-in-deferred-no-env-var-pre");
	fill_in(&mut drv).unwrap();
	assert_eq!(drv, invariants("filled-in-deferred-no-env-var-post"));
}

/// The output path is computed before the missing `out` variable is added,
/// so the filled-in derivation fails the reference's own check, and
/// `nix derivation add` rejects both the `pre` and `post` files.
#[test]
fn filling_in_a_missing_variable_breaks_the_invariants_as_in_cppnix() {
	let expected = Error(
		"derivation has incorrect output '/nix/store/wpk9qrgg77fyswhailap0gicgw98izx9-filled-in-deferred-no-env-var', should be '/nix/store/fb8grlzkbsapafb0h0qvy0zhvydwbbxa-filled-in-deferred-no-env-var'"
			.into(),
	);
	assert_eq!(
		check(&invariants("filled-in-deferred-no-env-var-post")),
		Err(expected)
	);
}

#[test]
fn correct_and_deferred_derivations_are_unchanged() {
	for (stem, drv_path) in [
		(
			"filled-in-idempotent",
			"c96961vnpgyv9027qp4ndcfivs0gh53i-filled-in-already.drv",
		),
		(
			"depends-on-drv-pre",
			"vhi8vq1wq3nmhcwqlbp5rxnngp3v4mmk-depends-on-drv.drv",
		),
	] {
		let before = invariants(stem);
		let mut drv = before.clone();
		fill_in(&mut drv).unwrap();
		assert_eq!(drv, before, "{stem}");
		check(&drv).unwrap();
		assert_eq!(drv.path().unwrap().to_string(), drv_path, "{stem}");
	}
}

#[test]
fn wrong_paths_are_errors() {
	for (stem, message) in [
		(
			"bad-path",
			"derivation has incorrect output '/nix/store/c015dhfh5l0lp6wxyvdn7bmwhbbr6hr9-wrong-name', should be '/nix/store/ijjrrpw9gxqjr96w79z1jdwsndy5jxcr-bad-path'",
		),
		// The pinned C++ test for this file is disabled; the reference
		// rejects it.
		(
			"bad-env-var",
			"derivation has incorrect environment variable 'out', should be '/nix/store/yprj3nx8072v0ph3ffccvm5p414y60c0-bad-env-var' but is actually '/nix/store/c015dhfh5l0lp6wxyvdn7bmwhbbr6hr9-wrong-name'",
		),
		(
			"bad-depends-on-drv-pre",
			"derivation has incorrect output '/nix/store/c015dhfh5l0lp6wxyvdn7bmwhbbr6hr9-wrong-name', should be deferred",
		),
	] {
		let mut drv = invariants(stem);
		assert_eq!(fill_in(&mut drv), Err(Error(message.into())), "{stem}");
	}
}

/// `TryResolveTest::BuildTrace`: built outputs and their store paths.
type BuildTrace = BTreeMap<(SingleDerivedPath, OutputName), StorePath>;

fn derived_path_from_json(value: &Value) -> SingleDerivedPath {
	match value {
		Value::String(s) => SingleDerivedPath::Opaque(path(s)),
		Value::Object(object) => SingleDerivedPath::Built {
			drv_path: Box::new(derived_path_from_json(&object["drvPath"])),
			output: object["output"].as_str().unwrap().parse().unwrap(),
		},
		other => panic!("{other}"),
	}
}

fn derived_path_to_json(path: &SingleDerivedPath) -> Value {
	match path {
		SingleDerivedPath::Opaque(path) => Value::String(path.to_string()),
		SingleDerivedPath::Built { drv_path, output } => serde_json::json!({
			"drvPath": derived_path_to_json(drv_path),
			"output": output.to_string(),
		}),
	}
}

fn build_trace(name: &str) -> BuildTrace {
	let (text, value) = read(&format!("try-resolve/{name}.json"));
	let trace: BuildTrace = value
		.as_array()
		.unwrap()
		.iter()
		.map(|entry| {
			let Some(SingleDerivedPath::Built { drv_path, output }) =
				Some(derived_path_from_json(&entry[0]))
			else {
				unreachable!()
			};
			((*drv_path, output), path(entry[1].as_str().unwrap()))
		})
		.collect();
	let rewritten: Vec<Value> = trace
		.iter()
		.map(|((drv_path, output), out)| {
			serde_json::json!([
				{"drvPath": derived_path_to_json(drv_path), "output": output.to_string()},
				out.to_string(),
			])
		})
		.collect();
	assert_eq!(written(&Value::Array(rewritten)), text, "{name}");
	trace
}

fn resolve(drv: &Derivation, trace: &BuildTrace) -> Option<Derivation> {
	let query = |drv_path: &SingleDerivedPath, output: &OutputName| {
		trace.get(&(drv_path.clone(), output.clone())).cloned()
	};
	drv.try_resolve(&query, true, &|_| None, &mut Vec::new())
		.unwrap()
}

fn try_resolve(name: &str) -> Derivation {
	derivation(&format!("try-resolve/{name}.json"))
}

fn assert_resolved(drv: &Derivation, name: &str) {
	let (text, _) = read(&format!("try-resolve/{name}.json"));
	assert_eq!(written(&drv.to_basic_json()), text, "{name}");
}

#[test]
fn derivations_resolve_as_in_cppnix() {
	for (before, trace, after) in [
		(
			"no-inputs-before",
			"no-inputs-buildTrace",
			"no-inputs-after",
		),
		(
			"with-inputs-before",
			"with-inputs-buildTrace",
			"with-inputs-after",
		),
	] {
		let resolved = resolve(&try_resolve(before), &build_trace(trace)).expect(before);
		assert_resolved(&resolved, after);
	}
}

#[test]
fn unknown_inputs_do_not_resolve() {
	let drv = try_resolve("resolution-failure-before");
	let trace = build_trace("resolution-failure-buildTrace");
	assert!(trace.is_empty());
	assert_eq!(resolve(&drv, &trace), None);
}

fn options_json(drv: &Derivation, with_inputs: bool) -> String {
	let env = drv
		.env
		.iter()
		.filter(|(k, _)| k.as_str() != "__json")
		.map(|(k, v)| (BString::from(k.as_str()), BString::from(v.as_str())))
		.collect();
	let input_drvs: BTreeMap<StorePath, BTreeSet<OutputName>> = if with_inputs {
		drv.input_derivations
			.iter()
			.map(|(path, node)| (path.clone(), node.outputs.clone()))
			.collect()
	} else {
		BTreeMap::new()
	};
	let options = DerivationOptions::new(
		&env,
		drv.structured_attrs.as_ref().and_then(Value::as_object),
		&input_drvs,
		true,
		&mut Vec::new(),
	)
	.unwrap();
	written(&options.to_json())
}

/// A placeholder followed by a subpath in `exportReferencesGraph` names the
/// placeholder's output, before and after resolution.
#[test]
fn export_references_graph_placeholders_with_subpaths() {
	for [before, before_options, trace, resolved, resolved_options] in [
		[
			"export-ref-subpath-before",
			"export-ref-subpath-before-options",
			"export-ref-subpath-buildTrace",
			"export-ref-subpath-resolved",
			"export-ref-subpath-resolved-options",
		],
		[
			"export-ref-subpath-sa-before",
			"export-ref-subpath-sa-before-options",
			"export-ref-subpath-sa-buildTrace",
			"export-ref-subpath-sa-resolved",
			"export-ref-subpath-sa-resolved-options",
		],
	] {
		let drv = try_resolve(before);
		let (text, _) = read(&format!("try-resolve/{before_options}.json"));
		assert_eq!(options_json(&drv, true), text, "{before_options}");

		let drv = resolve(&drv, &build_trace(trace)).expect(before);
		assert_resolved(&drv, resolved);

		let (text, _) = read(&format!("try-resolve/{resolved_options}.json"));
		assert_eq!(options_json(&drv, false), text, "{resolved_options}");
	}
}
