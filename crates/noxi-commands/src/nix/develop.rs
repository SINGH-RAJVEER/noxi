//! `nix develop` and `nix print-dev-env` for expression and store-path
//! installables, following CppNix 2.35.2's `src/nix/develop.cc`.
//!
//! The selected derivation is rewritten as the reference rewrites it: its
//! builder, which must be `bash`, runs `get-env.sh` instead of its
//! arguments, its outputs are deferred and its name gets `-env`. Building
//! that derivation dumps the environment `$stdenv/setup` leaves as JSON,
//! which becomes the rc script a shell or a command runs with. Flake
//! installables use the shared flake adapter. Profiles retain the environment
//! output; redirects replace selected output paths in the shell script. The interactive shell is resolved
//! through `nixpkgs#bashInteractive` when flakes are enabled.

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;
use std::rc::Rc;

use nix_compat::cppnix_json::derivation::{Derivation, Output};
use nix_compat::cppnix_json::derivation_hash::rewrite_strings;
use nix_compat::store_path::StorePath;
use noxi_store::{DerivedPath, Store};
use serde_json::{Map, Value, json};

use super::{Parsed, expression, inspection};
use crate::cli::Failure;

/// The reference's script, kept byte for byte (with its spaces) so the
/// text path it is added as matches. The reference's header generator puts
/// a newline before it.
const GET_ENV_SH: &str = concat!("\n", include_str!("get-env.sh"));

/// `Common::ignoreVars`.
const IGNORE_VARS: &[&str] = &[
	"BASHOPTS",
	"HOME",
	"NIX_BUILD_TOP",
	"NIX_ENFORCE_PURITY",
	"NIX_LOG_FD",
	"NIX_REMOTE",
	"PPID",
	"SHELLOPTS",
	"SSL_CERT_FILE",
	"TEMP",
	"TEMPDIR",
	"TERM",
	"TMP",
	"TMPDIR",
	"TZ",
	"UID",
];

/// A shell variable of `BuildEnvironment`.
#[derive(Debug, PartialEq)]
enum Var {
	String { exported: bool, value: String },
	Array(Vec<String>),
	Associative(BTreeMap<String, String>),
}

/// `BuildEnvironment`.
#[derive(Debug)]
struct BuildEnvironment {
	vars: BTreeMap<String, Var>,
	functions: BTreeMap<String, String>,
	/// The contents of `.attrs.json` and `.attrs.sh`.
	structured: Option<(String, String)>,
}

fn quote(s: &str) -> String {
	format!("'{}'", s.replace('\'', "'\\''"))
}

fn bad_json() -> Failure {
	Failure::Error("get-env.sh produced an invalid environment".into())
}

fn json_strings(value: &Value) -> Result<Vec<String>, Failure> {
	value
		.as_array()
		.ok_or_else(bad_json)?
		.iter()
		.map(|v| v.as_str().map(str::to_owned).ok_or_else(bad_json))
		.collect()
}

fn json_map(value: &Value) -> Result<BTreeMap<String, String>, Failure> {
	value
		.as_object()
		.ok_or_else(bad_json)?
		.iter()
		.map(|(k, v)| Ok((k.clone(), v.as_str().ok_or_else(bad_json)?.to_owned())))
		.collect()
}

impl BuildEnvironment {
	/// `BuildEnvironment::fromJSON`; unknown variable types are skipped.
	fn from_json(text: &[u8]) -> Result<Self, Failure> {
		let json: Value =
			serde_json::from_slice(text).map_err(|e| Failure::Error(e.to_string()))?;
		let mut vars = BTreeMap::new();
		for (name, info) in json["variables"].as_object().ok_or_else(bad_json)? {
			let value = &info["value"];
			let var = match info["type"].as_str().ok_or_else(bad_json)? {
				kind @ ("var" | "exported") => Var::String {
					exported: kind == "exported",
					value: value.as_str().ok_or_else(bad_json)?.to_owned(),
				},
				"array" => Var::Array(json_strings(value)?),
				"associative" => Var::Associative(json_map(value)?),
				_ => continue,
			};
			vars.insert(name.clone(), var);
		}
		let functions = json_map(&json["bashFunctions"])?;
		let structured = match json.get("structuredAttrs") {
			Some(attrs) => Some((
				attrs[".attrs.json"]
					.as_str()
					.ok_or_else(bad_json)?
					.to_owned(),
				attrs[".attrs.sh"].as_str().ok_or_else(bad_json)?.to_owned(),
			)),
			None => None,
		};
		Ok(Self {
			vars,
			functions,
			structured,
		})
	}

	/// `BuildEnvironment::toJSON`.
	fn to_json(&self) -> Value {
		let variables: Map<String, Value> = self
			.vars
			.iter()
			.map(|(name, var)| {
				let info = match var {
					Var::String { exported, value } => json!({
						"type": if *exported { "exported" } else { "var" },
						"value": value,
					}),
					Var::Array(values) => json!({"type": "array", "value": values}),
					Var::Associative(values) => json!({"type": "associative", "value": values}),
				};
				(name.clone(), info)
			})
			.collect();
		let mut res = json!({"variables": variables, "bashFunctions": self.functions});
		if let Some((attrs_json, attrs_sh)) = &self.structured {
			res["structuredAttrs"] = json!({".attrs.sh": attrs_sh, ".attrs.json": attrs_json});
		}
		res
	}

	/// `BuildEnvironment::toBash`.
	fn to_bash(&self, out: &mut String) {
		for (name, var) in &self.vars {
			if IGNORE_VARS.contains(&name.as_str()) {
				continue;
			}
			match var {
				Var::String { exported, value } => {
					out.push_str(&format!("{name}={}\n", quote(value)));
					if *exported {
						out.push_str(&format!("export {name}\n"));
					}
				}
				Var::Array(values) => {
					out.push_str(&format!("declare -a {name}=("));
					for value in values {
						out.push_str(&quote(value));
						out.push(' ');
					}
					out.push_str(")\n");
				}
				Var::Associative(values) => {
					out.push_str(&format!("declare -A {name}=("));
					for (key, value) in values {
						out.push_str(&format!("[{}]={} ", quote(key), quote(value)));
					}
					out.push_str(")\n");
				}
			}
		}
		for (name, def) in &self.functions {
			out.push_str(&format!("{name} ()\n{{\n{def}}}\n"));
		}
	}

	fn string(&self, name: &str) -> Result<String, Failure> {
		match self.vars.get(name) {
			Some(Var::String { value, .. }) => Ok(value.clone()),
			_ => Err(Failure::Error("bash variable is not a string".into())),
		}
	}

	/// `BuildEnvironment::getStrings`.
	fn strings(var: &Var) -> Vec<String> {
		match var {
			Var::String { value, .. } => value
				.split([' ', '\t', '\n', '\r'])
				.filter(|s| !s.is_empty())
				.map(str::to_owned)
				.collect(),
			Var::Array(values) => values.clone(),
			Var::Associative(values) => values.keys().cloned().collect(),
		}
	}
}

/// `Common::makeRcScript`.
fn make_rc_script(
	parsed: &Parsed,
	env: &BuildEnvironment,
	tmp_dir: &std::path::Path,
	outputs_dir: &std::path::Path,
) -> Result<String, Failure> {
	const SAVED_VARS: [&str; 2] = ["PATH", "XDG_DATA_DIRS"];
	let mut out = String::from("unset shellHook\n");
	for var in SAVED_VARS {
		out.push_str(&format!("{var}=${{{var}:-}}\n"));
		out.push_str(&format!("nix_saved_{var}=\"${var}\"\n"));
	}
	env.to_bash(&mut out);
	for var in SAVED_VARS {
		out.push_str(&format!(
			"{var}=\"${var}${{nix_saved_{var}:+:$nix_saved_{var}}}\"\n"
		));
	}
	out.push_str("export NIX_BUILD_TOP=\"$(mktemp -d -t nix-shell.XXXXXX)\"\n");
	for var in ["TMP", "TMPDIR", "TEMP", "TEMPDIR"] {
		out.push_str(&format!("export {var}=\"$NIX_BUILD_TOP\"\n"));
	}
	out.push_str("eval \"${shellHook:-}\"\n");

	let outputs = env
		.vars
		.get("outputs")
		.ok_or_else(|| Failure::Error("derivation does not have an 'outputs' attribute".into()))?;
	let mut rewrites = BTreeMap::new();
	let output_dir = |name: &str| outputs_dir.join(name).to_string_lossy().into_owned();
	if env.structured.is_some() {
		let Var::Associative(outputs) = outputs else {
			return Err(Failure::Error(
				"bash variable is not an associative array".into(),
			));
		};
		for (name, from) in outputs {
			rewrites.insert(from.clone(), output_dir(name));
		}
	} else {
		for name in BuildEnvironment::strings(outputs) {
			rewrites.insert(env.string(&name)?, output_dir(&name));
		}
	}
	if let Some(redirects) = parsed.flags.get("redirect") {
		for pair in redirects {
			if (parsed.has("expr") || parsed.has("file")) && pair[0].starts_with('/') {
				return Err(Failure::Error(format!(
					"attribute '{}' in selection path '{}' not found",
					pair[0], pair[0]
				)));
			}
			let request = Parsed {
				command: "build",
				settings: parsed.settings.clone(),
				flags: parsed
					.flags
					.iter()
					.filter(|(key, _)| key.as_str() != "redirect")
					.map(|(k, v)| (k.clone(), v.clone()))
					.collect(),
				sequence: Vec::new(),
				operands: vec![pair[0].clone()],
			};
			let target = match expression::targets(&request)? {
				Ok(v) => v,
				Err(_) => {
					return Err(Failure::Error(
						"cannot evaluate redirect installable".into(),
					));
				}
			};
			let dir = crate::instantiate::absolute_path(
				&std::env::current_dir()?,
				std::path::Path::new(&pair[1]),
			);
			for path in &target.paths {
				for path in expression::outputs(target.store.as_ref(), path)?.into_values() {
					let from = path.to_absolute_path();
					if !out.contains(&from) {
						eprintln!(
							"warning: '{}' (path '{}') is not used by this build environment",
							pair[0], from
						);
					} else {
						if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
							eprintln!("redirecting '{}' to '\"{}\"'", from, dir.display());
						}
						rewrites
							.entry(from)
							.or_insert_with(|| dir.to_string_lossy().into_owned());
					}
				}
			}
		}
	}
	if let Some((attrs_json, attrs_sh)) = &env.structured {
		for (ext, var, contents) in [
			("sh", "NIX_ATTRS_SH_FILE", attrs_sh),
			("json", "NIX_ATTRS_JSON_FILE", attrs_json),
		] {
			let target = tmp_dir.join(format!(".attrs.{ext}"));
			std::fs::write(&target, contents)?;
			rewrites.insert(env.string(var)?, target.to_string_lossy().into_owned());
		}
	}
	Ok(rewrite_strings(&out, &rewrites))
}

/// `makeTempPath(defaultTempDir(), prefix)`.
fn temp_path(prefix: &str, counter: u32) -> std::path::PathBuf {
	let root = std::env::var_os("TMPDIR")
		.filter(|dir| !dir.is_empty())
		.map_or_else(
			|| std::path::PathBuf::from("/tmp"),
			std::path::PathBuf::from,
		);
	let root = std::fs::canonicalize(&root).unwrap_or(root);
	root.join(format!("{prefix}-{}-{counter}", std::process::id()))
}

fn seed() -> u32 {
	use std::hash::{BuildHasher, Hasher};
	let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
	hasher.write_u32(std::process::id());
	hasher.finish() as u32
}

/// `createTempDir("", prefix)`.
fn create_temp_dir(prefix: &str) -> Result<std::path::PathBuf, Failure> {
	let mut counter = seed();
	loop {
		let dir = temp_path(prefix, counter);
		match std::fs::create_dir(&dir) {
			Ok(()) => return Ok(dir),
			Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
				counter = counter.wrapping_add(1)
			}
			Err(error) => return Err(error.into()),
		}
	}
}

/// `createTempFile(prefix)`: `mkstemp` in the temporary directory.
fn create_temp_file(prefix: &str) -> Result<std::path::PathBuf, Failure> {
	use std::os::unix::ffi::OsStringExt;
	let root = std::env::var_os("TMPDIR")
		.filter(|dir| !dir.is_empty())
		.unwrap_or_else(|| "/tmp".into());
	let template = std::path::Path::new(&root).join(format!("{prefix}.XXXXXX"));
	let mut template = template.into_os_string().into_vec();
	template.push(0);
	// SAFETY: the template is NUL-terminated and mkstemp only rewrites its X's.
	let fd = unsafe { libc::mkstemp(template.as_mut_ptr().cast()) };
	template.pop();
	let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(template));
	if fd < 0 {
		return Err(Failure::Error(format!(
			"creating temporary file '{}': {}",
			path.display(),
			super::hash::strerror(&std::io::Error::last_os_error())
		)));
	}
	// SAFETY: fd was just opened.
	unsafe { libc::close(fd) };
	Ok(path)
}

/// `getDerivationEnvironment`: build the rewritten derivation and return
/// the output holding the environment.
fn derivation_environment(store: &dyn Store, drv_path: &StorePath) -> Result<StorePath, Failure> {
	let mut drv: Derivation = inspection::json_derivation(store, drv_path)?;
	if drv.builder.rsplit('/').next() != Some("bash") {
		return Err(Failure::Error(
			"'nix develop' only works on derivations that use 'bash' as their builder".into(),
		));
	}
	let get_env = store.add_text_to_store("get-env.sh", GET_ENV_SH.as_bytes(), &BTreeSet::new())?;
	drv.args = vec![get_env.to_absolute_path()];
	match drv.structured_attrs.as_mut() {
		Some(attrs) => {
			if let Some(attrs) = attrs.as_object_mut() {
				attrs.remove("outputChecks");
			}
		}
		None => {
			for name in [
				"allowedReferences",
				"allowedRequisites",
				"disallowedReferences",
				"disallowedRequisites",
			] {
				drv.env.remove(name);
			}
		}
	}
	drv.env.remove("name");
	drv.name.push_str("-env");
	drv.env.insert("name".into(), drv.name.clone());
	drv.input_sources.insert(get_env);
	for (name, output) in drv.outputs.iter_mut() {
		if matches!(output, Output::InputAddressed(_) | Output::Fixed(_)) {
			*output = Output::Deferred;
			drv.env.insert(name.to_string(), String::new());
		}
	}
	let read = |path: &StorePath| inspection::json_derivation(store, path).ok();
	let mut warnings = Vec::new();
	drv.fill_in_output_paths(&read, &mut warnings)
		.map_err(|e| Failure::Error(e.to_string()))?;
	for warning in warnings {
		if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn) {
			eprintln!("warning: {warning}");
		}
	}
	let aterm = drv
		.unparse(false, None)
		.map_err(|e| Failure::Error(e.to_string()))?;
	let references: BTreeSet<StorePath> = drv
		.input_sources
		.iter()
		.chain(drv.input_derivations.keys())
		.cloned()
		.collect();
	let env_drv =
		store.add_text_to_store(&format!("{}.drv", drv.name), aterm.as_bytes(), &references)?;
	store.build_paths(&[DerivedPath::Built {
		drv: env_drv.clone(),
		outputs: BTreeSet::new(),
	}])?;
	for path in store.query_derivation_outputs(&env_drv)?.into_values() {
		if std::fs::metadata(store.real_path(&path)).is_ok_and(|m| m.len() > 0) {
			return Ok(path);
		}
	}
	Err(Failure::Error(
		"get-env.sh failed to produce an environment".into(),
	))
}

/// `Common::getBuildEnvironment`: the environment and its store path.
fn build_environment(
	parsed: &Parsed,
) -> Result<Result<(Rc<dyn Store>, BuildEnvironment, StorePath), ExitCode>, Failure> {
	let target = match expression::targets(parsed)? {
		Ok(target) => target,
		Err(code) => return Ok(Err(code)),
	};
	let store = target.store;
	let env_path = match target.paths.as_slice() {
		[DerivedPath::Opaque(path)] if path.name().ends_with("-env") => path.clone(),
		paths => {
			let mut drvs = BTreeSet::new();
			for path in paths {
				drvs.insert(match path {
					DerivedPath::Built { drv, .. } => drv.clone(),
					DerivedPath::Opaque(path) if path.name().ends_with(".drv") => path.clone(),
					DerivedPath::Opaque(path) => store
						.query_path_info(path)?
						.and_then(|info| info.deriver)
						.ok_or_else(|| {
							Failure::Error(format!(
								"path '{}' does not have a known deriver",
								path.to_absolute_path()
							))
						})?,
				});
			}
			let [drv] = Vec::from_iter(drvs).try_into().map_err(|drvs: Vec<_>| {
				Failure::Error(format!(
					"the installable needs to evaluate to a single derivation, but it evaluated to {} derivations",
					drvs.len()
				))
			})?;
			derivation_environment(store.as_ref(), &drv)?
		}
	};
	if let Some(profile) = parsed.value("profile") {
		let profile = crate::instantiate::absolute_path(
			&std::env::current_dir()?,
			std::path::Path::new(profile),
		);
		let _lock = noxi_store::profiles::lock_profile(&profile)?;
		let generation =
			noxi_store::profiles::create_generation(store.as_ref(), &profile, &env_path)?;
		noxi_store::profiles::switch_link(&profile, &generation)?;
	}
	let contents = std::fs::read(store.real_path(&env_path))?;
	Ok(Ok((
		store,
		BuildEnvironment::from_json(&contents)?,
		env_path,
	)))
}

/// `nix print-dev-env`.
pub(super) fn print_dev_env(parsed: Parsed) -> Result<ExitCode, Failure> {
	let (_, env, _) = match build_environment(&parsed)? {
		Ok(found) => found,
		Err(code) => return Ok(code),
	};
	let text = if parsed.has("json") {
		env.to_json().to_string()
	} else {
		let tmp_dir = create_temp_dir("nix-dev-env")?;
		let script = make_rc_script(
			&parsed,
			&env,
			&tmp_dir,
			&std::env::current_dir()?.join("outputs"),
		);
		let _ = std::fs::remove_dir_all(&tmp_dir);
		script?
	};
	use std::io::Write;
	// `writeToStdout` ends the text with a newline.
	std::io::stdout()
		.lock()
		.write_all(format!("{text}\n").as_bytes())?;
	Ok(ExitCode::SUCCESS)
}

/// `nix develop`.
pub(super) fn develop(parsed: Parsed) -> Result<ExitCode, Failure> {
	let mut phase = None;
	let mut command = Vec::new();
	for (name, values) in &parsed.sequence {
		match name.as_str() {
			"phase" => phase = Some(values[0].clone()),
			"unpack" | "configure" | "build" | "check" | "install" => phase = Some(name.clone()),
			"installcheck" => phase = Some("installCheck".into()),
			"command" => command = values.clone(),
			_ => {}
		}
	}
	let (store, env, gc_root) = match build_environment(&parsed)? {
		Ok(found) => found,
		Err(code) => return Ok(code),
	};
	let rc_file = create_temp_file("nix-shell")?;
	let tmp_dir = create_temp_dir("nix-develop")?;
	let mut script = make_rc_script(
		&parsed,
		&env,
		&tmp_dir,
		&std::env::current_dir()?.join("outputs"),
	)?;
	if nix_compat::log::verbosity() >= nix_compat::log::VerbosityLevel::Debug as u64 {
		script.push_str("set -x\n");
	}
	script.push_str(&format!("command rm -f '{}'\n", rc_file.display()));
	if let Some(phase) = &phase {
		if !command.is_empty() {
			return Err(Failure::Usage(
				"you cannot use both '--command' and '--phase'".into(),
			));
		}
		script.push_str("foundMakefile=1\n");
		script.push_str(&format!(
			"if declare -f runPhase >/dev/null; then\n  runPhase {phase}Phase\nelse\n  runHook {phase}Phase\nfi\n"
		));
	} else if !command.is_empty() {
		let args: Vec<String> = command.iter().map(|arg| quote(arg)).collect();
		script.push_str(&format!("exec {}\n", args.join(" ")));
	} else {
		script = format!(
			"[ -n \"$PS1\" ] && [ -e ~/.bashrc ] && source ~/.bashrc;\nshopt -u expand_aliases\n{script}\nshopt -s expand_aliases\n"
		);
		for (setting, line) in [
			("bash-prompt", "[ -n \"$PS1\" ] && PS1={};\n"),
			("bash-prompt-prefix", "[ -n \"$PS1\" ] && PS1={}\"$PS1\";\n"),
			("bash-prompt-suffix", "[ -n \"$PS1\" ] && PS1+={};\n"),
		] {
			if let Some(value) = parsed.settings.get(setting).filter(|v| !v.is_empty()) {
				script.push_str(&line.replace("{}", &quote(value)));
			}
		}
	}
	let shell = if parsed.settings.experimental_features().contains("flakes") {
		let mut shell_flags = BTreeMap::from([("no-write-lock-file".into(), vec![vec![]])]);
		if let Some(uri) = parsed.value("store") {
			shell_flags.insert("store".into(), vec![vec![uri.to_owned()]]);
		}
		let shell_request = Parsed {
			command: "build",
			settings: parsed.settings.clone(),
			flags: shell_flags,
			sequence: Vec::new(),
			operands: vec!["nixpkgs#bashInteractive".into()],
		};
		match expression::targets(&shell_request) {
			Ok(Ok(targets)) => {
				targets.store.build_paths(&targets.paths)?;
				let mut paths = Vec::new();
				for target in &targets.paths {
					paths
						.extend(expression::outputs(targets.store.as_ref(), target)?.into_values());
				}
				paths
					.first()
					.map(|p| format!("{}/bin/bash", p.to_absolute_path()))
					.unwrap_or_else(|| "bash".into())
			}
			_ => "bash".into(),
		}
	} else {
		eprintln!(
			"error (ignored): {}",
			nix_compat::nixcpp::hash::missing_feature("flakes")
		);
		"bash".into()
	};
	script.push_str(&format!("SHELL=\"{shell}\"\n"));
	std::fs::write(&rc_file, script)?;
	let rc = rc_file.to_string_lossy().into_owned();
	let mut child = super::execution::environment(&parsed, &shell)?;
	child.env("NIX_GCROOT", gc_root.to_absolute_path());
	child.env("SHELL", shell);
	if phase.is_some() || !command.is_empty() {
		child.arg(&rc);
	} else {
		child.args(["--rcfile", &rc]);
	}
	drop(store);
	super::execution::exec_store(&parsed, child)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn writes_variables_and_functions_like_to_bash() {
		let env = BuildEnvironment::from_json(
			br#"{"bashFunctions":{"f":"\n    echo hi\n"},"variables":{"a":{"type":"exported","value":"x'y"},"b":{"type":"array","value":["1","2"]},"c":{"type":"associative","value":{"k":"v"}},"HOME":{"type":"var","value":"/h"}}}"#,
		)
		.unwrap();
		let mut out = String::new();
		env.to_bash(&mut out);
		assert_eq!(
			out,
			"a='x'\\''y'\nexport a\ndeclare -a b=('1' '2' )\ndeclare -A c=(['k']='v' )\nf ()\n{\n\n    echo hi\n}\n"
		);
	}
}
