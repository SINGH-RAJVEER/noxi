//! `nix-env` user environments, following CppNix 2.35.2's `user-env.cc`
//! and the parts of `get-drvs.cc` and `names.cc` that read them.
//!
//! An environment's `manifest.nix` lists its elements as derivation-like
//! attribute sets. The evaluator reads it, and a new manifest is printed
//! as CppNix's `printAmbiguous` prints it, added to the store, and built
//! into a symlink tree by `builtin:buildenv`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nix_compat::store_path::StorePath;
use noxi_eval::{AmbiguousUnshared, NixAttrs, NixList, Value};
use noxi_store::{DerivedPath, Store, profiles};

use crate::cli::Failure;
use crate::instantiate::{self, Options};

/// CppNix's `buildenv.nix`, the user environment builder expression.
const BUILDENV_NIX: &str = r#"{ derivations, manifest }:

derivation {
  name = "user-environment";
  system = "builtin";
  builder = "builtin:buildenv";

  inherit manifest;

  # !!! grmbl, need structured data for passing this in a clean way.
  derivations = map (
    d:
    [
      (d.meta.active or "true")
      (d.meta.priority or 5)
      (builtins.length d.outputs)
    ]
    ++ map (output: builtins.getAttr output d) d.outputs
  ) derivations;

  # Building user environments remotely just causes huge amounts of
  # network traffic, so don't do that.
  preferLocalBuild = true;

  # Also don't bother substituting.
  allowSubstitutes = false;
}
"#;

/// The installed elements as `queryInstalled` and `PackageInfo` read
/// them: `meta` keeps the attributes `checkMeta` accepts.
const READ_MANIFEST: &str = r#"manifest:
let
  checkMeta = v:
    if builtins.isList v then builtins.all checkMeta v
    else if builtins.isAttrs v then !(v ? outPath) && builtins.all (n: checkMeta v.${n}) (builtins.attrNames v)
    else builtins.isInt v || builtins.isBool v || builtins.isString v || builtins.isFloat v;
  isDerivation = v: builtins.isAttrs v && v ? type && v.type == "derivation";
  metaOf = e: if e ? meta && builtins.isAttrs e.meta
    then builtins.listToAttrs (builtins.concatMap
      (n: if checkMeta e.meta.${n} then [ { name = n; value = e.meta.${n}; } ] else [ ])
      (builtins.attrNames e.meta))
    else { };
  outputsOf = e: if e ? outputs
    then builtins.listToAttrs (builtins.concatMap
      (o: if e ? ${o} && e.${o} ? outPath then [ { name = o; value = toString e.${o}.outPath; } ] else [ ])
      e.outputs)
    else { out = toString e.outPath; };
  toInstall = e: let all = outputsOf e; meta = metaOf e; in
    if e.outputSpecified or false then { ${e.outputName or "out"} = all.${e.outputName or "out"}; }
    else if meta ? outputsToInstall then builtins.listToAttrs (map (o: { name = o; value = all.${o}; }) meta.outputsToInstall)
    else all;
in map (e: {
  name = e.name;
  system = e.system or "unknown";
  outPath = toString e.outPath;
  drvPath = if e ? drvPath then toString e.drvPath else null;
  outputs = outputsOf e;
  install = toInstall e;
  meta = metaOf e;
}) (builtins.filter isDerivation (import manifest))
"#;

/// One element of a user environment, as `PackageInfo` sees it.
#[derive(Clone, Debug)]
pub struct Element {
	pub name: String,
	/// `querySystem`; empty for a store path given on the command line.
	pub system: String,
	pub out_path: StorePath,
	pub drv_path: Option<StorePath>,
	/// `queryOutputs(true, false)`.
	pub outputs: BTreeMap<String, StorePath>,
	/// `queryOutputs(true, true)`: the outputs installed.
	pub install: BTreeMap<String, StorePath>,
	/// The valid `meta` attributes.
	pub meta: Value,
}

impl Element {
	/// The element for a store path argument, as `queryInstSources` makes
	/// it for `srcStorePaths`.
	pub fn from_store_path(store: &dyn Store, path: StorePath) -> Result<Self, Failure> {
		let name = path.name().to_owned();
		let (out_path, drv_path) = if name.ends_with(".drv") {
			let outputs = store.query_derivation_outputs(&path)?;
			let out = outputs.get("out").cloned().ok_or_else(|| {
				Failure::Error(format!(
					"derivation '{}' does not have an output named 'out'",
					path.to_absolute_path()
				))
			})?;
			(out, Some(path))
		} else {
			(path, None)
		};
		let outputs = BTreeMap::from([("out".to_owned(), out_path.clone())]);
		Ok(Self {
			name,
			system: String::new(),
			out_path,
			drv_path,
			install: outputs.clone(),
			outputs,
			meta: Value::attrs(NixAttrs::empty()),
		})
	}

	fn meta(&self, name: &str) -> Option<Value> {
		self.meta.to_attrs().ok()?.select(name).map(forced)
	}

	/// `setMeta`: replace one meta attribute.
	pub fn set_meta(&mut self, name: &str, value: Value) {
		let mut attrs: Vec<(String, Value)> = self
			.meta
			.to_attrs()
			.map(|attrs| {
				attrs
					.iter_sorted()
					.filter(|(n, _)| n.as_bytes() != name.as_bytes())
					.map(|(n, v)| {
						(
							String::from_utf8_lossy(n.as_bytes()).into_owned(),
							v.clone(),
						)
					})
					.collect()
			})
			.unwrap_or_default();
		attrs.push((name.to_owned(), value));
		self.meta = Value::attrs(NixAttrs::from_iter(attrs));
	}

	/// `queryMetaBool`.
	pub fn meta_bool(&self, name: &str, default: bool) -> bool {
		match self.meta(name) {
			Some(Value::Bool(b)) => b,
			Some(Value::String(s)) if s.as_bytes() == b"true" => true,
			Some(Value::String(s)) if s.as_bytes() == b"false" => false,
			_ => default,
		}
	}

	/// The attribute set `createUserEnv` writes for this element.
	fn manifest_value(&self, keep_derivations: bool) -> Value {
		let string = |s: &str| Value::from(s);
		let mut attrs: Vec<(String, Value)> = vec![
			("type".to_owned(), string("derivation")),
			("name".to_owned(), string(&self.name)),
			(
				"outPath".to_owned(),
				string(&self.out_path.to_absolute_path()),
			),
			("meta".to_owned(), self.meta.clone()),
		];
		if !self.system.is_empty() {
			attrs.push(("system".to_owned(), string(&self.system)));
		}
		if keep_derivations && let Some(drv) = &self.drv_path {
			attrs.push(("drvPath".to_owned(), string(&drv.to_absolute_path())));
		}
		let mut names = Vec::new();
		for (name, path) in &self.install {
			names.push(string(name));
			attrs.push((
				name.clone(),
				Value::attrs(NixAttrs::from_iter([(
					"outPath",
					string(&path.to_absolute_path()),
				)])),
			));
		}
		attrs.push(("outputs".to_owned(), Value::List(NixList::from(names))));
		Value::attrs(NixAttrs::from_iter(attrs))
	}
}

/// CppNix's `DrvName`: a name split before the first `-` that is not
/// followed by a letter.
#[derive(Clone, Debug)]
pub struct DrvName {
	pub name: String,
	pub version: String,
}

impl DrvName {
	pub fn new(s: &str) -> Self {
		let bytes = s.as_bytes();
		for i in 0..bytes.len() {
			if bytes[i] == b'-' && i + 1 < bytes.len() && !bytes[i + 1].is_ascii_alphabetic() {
				return Self {
					name: s[..i].to_owned(),
					version: s[i + 1..].to_owned(),
				};
			}
		}
		Self {
			name: s.to_owned(),
			version: String::new(),
		}
	}

	/// `DrvName::matches`: the name is a whole-string extended regular
	/// expression unless it is `*`.
	pub fn matches(&self, other: &DrvName) -> bool {
		if self.name != "*" {
			match regex::Regex::new(&format!("^(?:{})$", self.name)) {
				Ok(re) if re.is_match(&other.name) => {}
				Ok(_) => return false,
				Err(_) => {
					if self.name != other.name {
						return false;
					}
				}
			}
		}
		self.version.is_empty() || self.version == other.version
	}
}

fn evaluation_options(settings: &nix_compat::nixcpp::settings::Settings, expr: String) -> Options {
	Options {
		eval: true,
		strict: true,
		from_args: true,
		inputs: vec![expr],
		settings: settings.clone(),
		..Options::default()
	}
}

fn string_of(value: &Value) -> Result<String, Failure> {
	let s = value.to_str().map_err(|e| Failure::Error(format!("{e}")))?;
	Ok(String::from_utf8_lossy(s.as_bytes()).into_owned())
}

fn store_path_of(value: &Value) -> Result<StorePath, Failure> {
	let s = string_of(value)?;
	StorePath::from_absolute_path(s.as_bytes())
		.map(|p| p.to_owned())
		.map_err(|_| Failure::Error(format!("path '{s}' is not in the Nix store")))
}

/// The value behind any evaluated thunks.
fn forced(value: &Value) -> Value {
	match value {
		Value::Thunk(thunk) => forced(&thunk.value()),
		value => value.clone(),
	}
}

fn select(attrs: &NixAttrs, name: &str) -> Result<Value, Failure> {
	attrs
		.select(name)
		.map(forced)
		.ok_or_else(|| Failure::Error(format!("attribute '{name}' missing")))
}

fn path_map(value: &Value) -> Result<BTreeMap<String, StorePath>, Failure> {
	let attrs = value
		.to_attrs()
		.map_err(|e| Failure::Error(format!("{e}")))?;
	let mut map = BTreeMap::new();
	for (name, path) in attrs.iter_sorted() {
		map.insert(
			String::from_utf8_lossy(name.as_bytes()).into_owned(),
			store_path_of(path)?,
		);
	}
	Ok(map)
}

/// `queryInstalled`: the elements of the environment `profile` names.
/// `Err(Ok(code))` is an evaluation failure, already reported.
pub fn query_installed(
	settings: &nix_compat::nixcpp::settings::Settings,
	cwd: &Path,
	profile: &Path,
) -> Result<Result<Vec<Element>, ExitCode>, Failure> {
	if std::fs::symlink_metadata(profile.join("manifest.json")).is_ok() {
		return Err(Failure::Error(format!(
			"profile \"{}\" is incompatible with 'nix-env'; please use 'nix profile' instead",
			profile.display()
		)));
	}
	let manifest = profile.join("manifest.nix");
	if std::fs::symlink_metadata(&manifest).is_err() {
		return Ok(Ok(Vec::new()));
	}
	let manifest = std::fs::canonicalize(&manifest)?;
	let expr = format!(
		"({READ_MANIFEST}) {}",
		crate::instantiate::nix_string_literal(&manifest.to_string_lossy())
	);
	let evaluated = match instantiate::evaluate(&evaluation_options(settings, expr), cwd) {
		Ok(evaluated) => evaluated,
		Err(code) => return Ok(Err(code)),
	};
	let list = evaluated
		.results
		.first()
		.and_then(|v| v.to_list().ok())
		.ok_or_else(|| Failure::Error("the manifest is not a list".to_owned()))?;
	let mut elements = Vec::new();
	for value in list.iter() {
		let attrs = value
			.to_attrs()
			.map_err(|e| Failure::Error(format!("{e}")))?;
		let drv_path = match select(&attrs, "drvPath")? {
			Value::Null => None,
			value => Some(store_path_of(&value)?),
		};
		elements.push(Element {
			name: string_of(&select(&attrs, "name")?)?,
			system: string_of(&select(&attrs, "system")?)?,
			out_path: store_path_of(&select(&attrs, "outPath")?)?,
			drv_path,
			outputs: path_map(&select(&attrs, "outputs")?)?,
			install: path_map(&select(&attrs, "install")?)?,
			meta: select(&attrs, "meta")?,
		});
	}
	Ok(Ok(elements))
}

/// `optimisticLockProfile`: the profile's link target, to notice another
/// process switching it.
pub fn lock_token(profile: &Path) -> PathBuf {
	std::fs::read_link(profile).unwrap_or_default()
}

/// `createUserEnv`: build an environment of `elements` and make it the
/// profile's new generation. `Ok(false)` means the profile changed since
/// `token` was taken, and the caller should start again.
pub fn create_user_env(
	store: &dyn Store,
	settings: &nix_compat::nixcpp::settings::Settings,
	store_uri: Option<&str>,
	cwd: &Path,
	elements: &[Element],
	profile: &Path,
	keep_derivations: bool,
	token: &Path,
) -> Result<Result<bool, ExitCode>, Failure> {
	let to_build: Vec<DerivedPath> = elements
		.iter()
		.filter_map(|e| e.drv_path.clone())
		.map(|drv| DerivedPath::Built {
			drv,
			outputs: BTreeSet::new(),
		})
		.collect();
	store.build_paths(&to_build)?;

	let mut references = BTreeSet::new();
	let mut values = Vec::new();
	for element in elements {
		for path in element.install.values() {
			// Needed when installing store paths, such as
			// `nix-env -i /nix/store/abcd...-foo`.
			store.add_temp_root(path)?;
			store.ensure_path(path)?;
			references.insert(path.clone());
		}
		if keep_derivations && let Some(drv) = &element.drv_path {
			references.insert(drv.clone());
		}
		values.push(element.manifest_value(keep_derivations));
	}
	let manifest = Value::List(NixList::from(values));
	let text = AmbiguousUnshared(&manifest).to_string();
	let manifest_path =
		store.add_text_to_store("env-manifest.nix", text.as_bytes(), &references)?;

	let expr = format!(
		"({BUILDENV_NIX}) {{ manifest = builtins.storePath {}; derivations = {text}; }}",
		crate::instantiate::nix_string_literal(&manifest_path.to_absolute_path())
	);
	let options = Options {
		from_args: true,
		inputs: vec![expr],
		settings: settings.clone(),
		store: store_uri.map(str::to_owned),
		gc_warning: false,
		..Options::default()
	};
	let evaluated = match instantiate::evaluate(&options, cwd) {
		Ok(evaluated) => evaluated,
		Err(code) => return Ok(Err(code)),
	};
	let derivations = instantiate::derivations(&evaluated.results)?;
	let [(drv, _)] = &derivations[..] else {
		return Err(Failure::Error(
			"the user environment builder did not give one derivation".to_owned(),
		));
	};
	let out = store
		.query_derivation_outputs(drv)?
		.get("out")
		.cloned()
		.ok_or_else(|| Failure::Error("the user environment has no output 'out'".to_owned()))?;
	store.build_paths(&[DerivedPath::Built {
		drv: drv.clone(),
		outputs: BTreeSet::new(),
	}])?;

	let _lock = profiles::lock_profile(profile)?;
	if lock_token(profile) != token {
		if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
			eprintln!(
				"profile \"{}\" changed while we were busy; restarting",
				profile.display()
			);
		}
		return Ok(Ok(false));
	}
	let generation = profiles::create_generation(store, profile, &out)?;
	profiles::switch_link(profile, &generation)?;
	Ok(Ok(true))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn splits_and_matches_names_like_cppnix() {
		let name = DrvName::new("hello-2.12.1");
		assert_eq!(
			(name.name.as_str(), name.version.as_str()),
			("hello", "2.12.1")
		);
		let name = DrvName::new("home-manager-path");
		assert_eq!(
			(name.name.as_str(), name.version.as_str()),
			("home-manager-path", "")
		);
		assert!(DrvName::new("hello").matches(&DrvName::new("hello-2.12.1")));
		assert!(DrvName::new("hel.*").matches(&DrvName::new("hello-1")));
		assert!(!DrvName::new("hello-1").matches(&DrvName::new("hello-2")));
		assert!(DrvName::new("*").matches(&DrvName::new("anything-3")));
	}
}
