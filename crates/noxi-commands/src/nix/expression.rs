//! Expression installables share the legacy evaluator and store owner.
use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nix_compat::store_path::StorePath;
use noxi_eval::{Ambiguous, Value};
use noxi_store::{BuildMode, DerivedPath, ImpureOutputs, Store, profiles};
use serde_json::json;

use super::{Parsed, unsupported};
use crate::cli::Failure;
use crate::instantiate::{self, AutoArg, Options, nix_string_literal};

pub fn options(parsed: &Parsed, eval: bool) -> Result<Options, Failure> {
	if parsed.has("expr") && parsed.has("file") {
		return Err(Failure::Usage("'--file' and '--expr' are exclusive".into()));
	}
	if parsed.has("file") && parsed.settings.get_bool("pure-eval", false) {
		return Err(Failure::Usage(
			"'--file' is not compatible with '--pure-eval'".into(),
		));
	}
	let pure = !parsed.has("file") && parsed.settings.get_bool("pure-eval", true);
	let mut settings = parsed.settings.clone();
	if parsed.has("no-registries") {
		settings
			.set("use-registries", "false")
			.map_err(|e| e.to_string())?;
	}
	parsed.unsupported(&["eval-store", "write-to", "debugger", "inputs-from"])?;
	let mut options = Options {
		modern_pure: pure,
		generated: true,
		eval,
		read_write_mode: !parsed.has("read-only"),
		repair: parsed.has("repair"),
		strict: true,
		settings,
		store: parsed.value("store").map(str::to_owned),
		..Options::default()
	};
	options
		.settings
		.set("pure-eval", if pure { "true" } else { "false" })
		.map_err(|e| e.to_string())?;
	if let Some(includes) = parsed.flags.get("include") {
		options.include = includes.iter().map(|v| v[0].clone()).collect();
	}
	if let Some(overrides) = parsed.flags.get("override-flake") {
		options.override_flakes = overrides
			.iter()
			.map(|v| (v[0].clone(), v[1].clone()))
			.collect();
	}
	// `MixEvalArgs`: the last value given for a name wins.
	for (flag, values) in &parsed.sequence {
		let value = match flag.as_str() {
			"arg" => AutoArg::Expr(values[1].clone()),
			"argstr" => AutoArg::Str(values[1].clone()),
			"arg-from-file" => {
				let path =
					instantiate::absolute_path(&std::env::current_dir()?, Path::new(&values[1]));
				let contents = std::fs::read(&path).map_err(|error| {
					Failure::Error(format!(
						"opening file \"{}\": {}",
						path.display(),
						super::hash::strerror(&error)
					))
				})?;
				AutoArg::Str(String::from_utf8_lossy(&contents).into_owned())
			}
			"arg-from-stdin" => {
				let mut contents = Vec::new();
				std::io::Read::read_to_end(&mut std::io::stdin(), &mut contents)?;
				AutoArg::Str(String::from_utf8_lossy(&contents).into_owned())
			}
			_ => continue,
		};
		let name = values[0].clone();
		options.auto_args.retain(|(existing, _)| *existing != name);
		options.auto_args.push((name, value));
	}
	let root = if let Some(expression) = parsed.value("expr") {
		// The expression is its own `«string»` source, as CppNix parses it.
		options.fragments.push(expression.to_owned());
		format!("@@noxi-frag-{}@@", options.fragments.len() - 1)
	} else if let Some(file) = parsed.value("file") {
		if instantiate::is_lookup_path(file) {
			format!("builtins.seq {file} (import {file})")
		} else if file == "-" {
			std::io::read_to_string(std::io::stdin())?
		} else {
			let absolute = instantiate::absolute_path(&std::env::current_dir()?, Path::new(file));
			format!("import {}", nix_string_literal(&absolute.to_string_lossy()))
		}
	} else {
		// The overrides are the evaluation's, below.
		options.flake = Some(super::flakes::Request {
			registry_overrides: Vec::new(),
			..super::flakes::request(parsed, "outputs")?
		});
		String::new()
	};
	let selections = if parsed.operands.is_empty() {
		vec![String::new()]
	} else {
		parsed.operands.clone()
	};
	if eval && selections.len() != 1 {
		return Err(Failure::Usage(format!(
			"unexpected argument '{}'",
			selections[1]
		)));
	}
	options.from_args = true;
	for selection in selections {
		let selection = instantiate::extended_outputs_spec(&selection)?.0.to_owned();
		let (root, selection) = if options.flake.is_some() {
			let (reference, attr) = selection.split_once('#').unwrap_or((
				if selection.is_empty() {
					"."
				} else {
					&selection
				},
				"",
			));
			let system = parsed.settings.get("system").unwrap_or("x86_64-linux");
			let candidates: Vec<String> = flake_attr_paths(parsed.command, attr, system)
				.iter()
				.map(|c| nix_string_literal(c))
				.collect();
			let reference = nix_string_literal(reference);
			(
				format!(
					"__noxi_flake_select (__noxi_flake {reference}) [ {} ] (__noxi_flake_label {reference})",
					candidates.join(" ")
				),
				String::new(),
			)
		} else {
			(root.clone(), selection)
		};
		let mut expression = format!("({root}\n)");
		// `findAlongAttrPath`: each step auto-calls the value it selects from.
		for attr in instantiate::parse_attr_path(&selection).map_err(Failure::Usage)? {
			expression = format!(
				"(__noxi_select_step {} {} (__noxi_autoCall {expression}))",
				nix_string_literal(&selection),
				nix_string_literal(&attr)
			);
		}
		if let Some(apply) = parsed.value("apply") {
			options.fragments.push(apply.to_owned());
			let apply = format!("@@noxi-frag-{}@@", options.fragments.len() - 1);
			expression = format!("({apply}\n) ({expression})");
		}
		if eval && parsed.has("raw") {
			expression = format!("(__noxi_raw_output \"eval\" ({expression}\n))");
		}
		options.inputs.push(expression);
	}
	options.json = eval && parsed.has("json");
	Ok(options)
}

pub fn eval(parsed: Parsed) -> Result<ExitCode, Failure> {
	if parsed.has("raw") && parsed.has("json") {
		return Err(Failure::Usage(
			"--raw and --json are mutually exclusive".into(),
		));
	}
	let mut options = options(&parsed, true)?;
	let plain = !parsed.has("raw") && !parsed.has("json");
	if plain {
		// CppNix's `ValuePrinter` forces the value as it prints it, showing
		// derivations by path and thrown errors as `«error: ...»`.
		options.strict = false;
		for input in &mut options.inputs {
			*input = format!("__noxi_eval_print ({input}\n)");
		}
		if options.flake.is_none() {
			options.flake = Some(super::flakes::Request::default());
		}
	}
	let evaluated = match instantiate::evaluate(&options, &std::env::current_dir()?) {
		Ok(value) => value,
		Err(code) => return Ok(code),
	};
	// `eval` stops the progress bar before printing, as `eval.cc` does.
	nix_compat::log::logger::stop();
	let mut stdout = io::stdout().lock();
	for value in evaluated.results {
		if parsed.has("raw") || parsed.has("json") {
			let Value::String(text) = value else {
				return Err(Failure::Error("evaluation produced no string".into()));
			};
			// `MixJSON`: pretty with `--pretty`, or on a terminal unless
			// `--no-pretty`.
			let pretty = parsed.has("json")
				&& (parsed.has("pretty")
					|| !parsed.has("no-pretty") && std::io::IsTerminal::is_terminal(&io::stdout()));
			if pretty {
				stdout
					.write_all(pretty_json(&String::from_utf8_lossy(text.as_bytes())).as_bytes())?;
			} else {
				stdout.write_all(text.as_bytes())?;
			}
			if parsed.has("json") {
				writeln!(stdout)?;
			}
		} else if let Value::String(text) = &value {
			stdout.write_all(text.as_bytes())?;
			writeln!(stdout)?;
		} else {
			writeln!(stdout, "{}", Ambiguous(&value))?;
		}
	}
	Ok(ExitCode::SUCCESS)
}

pub struct Targets {
	pub store: std::rc::Rc<dyn Store>,
	pub paths: Vec<DerivedPath>,
}

pub fn targets(parsed: &Parsed) -> Result<Result<Targets, ExitCode>, Failure> {
	if parsed.has("expr")
		|| parsed.has("file")
		|| parsed.operands.is_empty()
		|| parsed.operands.iter().any(|s| {
			// A fragment selects a flake output even when the source lives
			// in the store. It is not part of a derived store path.
			s.contains('#')
				|| (!s.starts_with(nix_compat::store_path::store_dir_with_slash())
					&& !std::path::Path::new(s).is_symlink())
		}) {
		let specs = parsed
			.operands
			.iter()
			.map(|operand| Ok(instantiate::extended_outputs_spec(operand)?.1))
			.collect::<Result<Vec<_>, String>>()?;
		let mut options = options(parsed, false)?;
		options.installables = true;
		let evaluated = match instantiate::evaluate(&options, &std::env::current_dir()?) {
			Ok(value) => value,
			Err(code) => return Ok(Err(code)),
		};
		let paths =
			instantiate::installables(&evaluated.results, &specs, evaluated.store.as_deref())?;
		let store = evaluated
			.store
			.ok_or_else(|| unsupported("building with this store"))?;
		return Ok(Ok(Targets { store, paths }));
	}
	let uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
	let opened = crate::store::open(&uri, &parsed.settings)?
		.ok_or_else(|| unsupported("installables in this store"))?;
	let mut paths = Vec::new();
	if parsed.operands.is_empty() {
		return Err(unsupported(&format!(
			"{} flake installables; use --expr or --file",
			parsed.command
		)));
	}
	for operand in &parsed.operands {
		let (path, spec) = instantiate::extended_outputs_spec(operand)?;
		let nested: Vec<&str> = path.split('^').collect();
		let dynamic = parsed
			.settings
			.experimental_features()
			.contains("dynamic-derivations");
		if nested.len() > 1
			&& !dynamic
			&& let Err(error) = std::fs::symlink_metadata(path)
		{
			return Err(Failure::Error(format!(
				"getting status of \"{path}\": {}",
				super::hash::strerror(&error)
			)));
		}
		let path = if nested.len() > 1 && dynamic {
			if parsed.has("json") || parsed.has("dry-run") {
				return Err(unsupported(
					"--json and --dry-run with the outputs of derivations that are still to be built",
				));
			}
			instantiate::built_derivation(opened.store.as_ref(), &nested)?
		} else {
			crate::paths::follow_links_to_store_path(&std::env::current_dir()?, path)?
		};
		paths.push(if let Some(outputs) = spec {
			DerivedPath::Built { drv: path, outputs }
		} else {
			DerivedPath::Opaque(path)
		});
	}
	Ok(Ok(Targets {
		store: opened.store,
		paths,
	}))
}

pub fn outputs(
	store: &dyn Store,
	path: &DerivedPath,
) -> Result<BTreeMap<String, StorePath>, Failure> {
	built_outputs(store, path, &ImpureOutputs::new())
}

/// The outputs of an installable that was built with these results.
pub fn built_outputs(
	store: &dyn Store,
	path: &DerivedPath,
	results: &ImpureOutputs,
) -> Result<BTreeMap<String, StorePath>, Failure> {
	Ok(match path {
		DerivedPath::Opaque(path) => BTreeMap::from([(String::new(), path.clone())]),
		DerivedPath::Built { drv, outputs } => {
			// Content-addressed outputs are known by their realisations.
			let mut all = match results.get(drv) {
				Some(outputs) => outputs
					.iter()
					.map(|(name, path)| (name.clone(), Some(path.clone())))
					.collect(),
				None => store.deep_query_partial_derivation_output_map(drv)?,
			};
			if !outputs.is_empty() {
				for name in outputs {
					if !all.contains_key(name) {
						return Err(Failure::Error(format!(
							"derivation '{}' does not have an output named '{name}'",
							drv.to_absolute_path()
						)));
					}
				}
				all.retain(|name, _| outputs.contains(name));
			}
			all.into_iter()
				.map(|(name, path)| {
					path.map(|path| (name.clone(), path)).ok_or_else(|| {
						Failure::Error(format!(
							"output '{name}' of derivation '{}' has not been built",
							drv.to_absolute_path()
						))
					})
				})
				.collect::<Result<_, _>>()?
		}
	})
}

pub fn build(mut parsed: Parsed) -> Result<ExitCode, Failure> {
	if parsed.has("stdin") && !io::stdin().is_terminal() {
		let input = io::read_to_string(io::stdin())?;
		parsed
			.operands
			.extend(input.split_ascii_whitespace().map(str::to_owned));
		if parsed.operands.is_empty() {
			if parsed.has("json") {
				nix_compat::log::logger::write_stdout("[]");
			}
			if parsed.has("profile") && !parsed.has("dry-run") {
				return Err(Failure::Usage("'--profile' requires that the arguments produce a single store path, but there are 0".into()));
			}
			return Ok(ExitCode::SUCCESS);
		}
	}
	// `MixRepair` and `--rebuild`; a repair takes precedence.
	let mode = if parsed.has("repair") {
		BuildMode::Repair
	} else if parsed.has("rebuild") {
		BuildMode::Check
	} else {
		BuildMode::Normal
	};
	let target = match targets(&parsed)? {
		Ok(target) => target,
		Err(code) => return Ok(code),
	};
	let profile = parsed
		.value("profile")
		.map(|path| std::path::absolute(PathBuf::from(path)))
		.transpose()?;
	if profile.is_some() && target.paths.len() != 1 {
		return Err(Failure::Error(
			"--profile requires exactly one installable".into(),
		));
	}
	let missing = target.store.query_missing(&target.paths)?;
	crate::paths::print_missing(target.store.as_ref(), &missing)?;
	let mut results = ImpureOutputs::new();
	if !parsed.has("dry-run") {
		results = target.store.build_paths_with_results(&target.paths, mode)?;
	}
	let mut result = Vec::new();
	for path in &target.paths {
		result.push(match path {
			DerivedPath::Opaque(path) => json!(path.to_absolute_path()),
			DerivedPath::Built { drv, outputs } => {
				let outputs = if parsed.has("dry-run") {
					// Outputs that are not known yet are null.
					let known = target.store.query_partial_derivation_output_map(drv)?;
					known
						.into_iter()
						.filter(|(name, _)| outputs.is_empty() || outputs.contains(name))
						.collect()
				} else {
					built_outputs(target.store.as_ref(), path, &results)?
						.into_iter()
						.map(|(name, path)| (name, Some(path)))
						.collect::<BTreeMap<_, _>>()
				};
				json!({"drvPath": drv.to_absolute_path(), "outputs": outputs.into_iter().map(|(name,path)| (name,path.map(|path| path.to_absolute_path()))).collect::<BTreeMap<_,_>>()})
			}
		});
	}
	if parsed.has("json") {
		nix_compat::log::logger::write_stdout(&json!(result).to_string());
	}
	// `printOutPathsMaybe` stops the progress bar before printing.
	if !parsed.has("dry-run") && parsed.has("print-out-paths") {
		nix_compat::log::logger::stop();
	}
	if let Some(profile) = profile.filter(|_| !parsed.has("dry-run")) {
		let paths = built_outputs(target.store.as_ref(), &target.paths[0], &results)?;
		let mut paths = paths.values();
		let output = paths
			.next()
			.filter(|_| paths.next().is_none())
			.ok_or_else(|| Failure::Error("--profile requires exactly one output path".into()))?;
		if let Some(parent) = profile.parent() {
			std::fs::create_dir_all(parent)?;
		}
		let _lock = profiles::lock_profile(&profile)?;
		let generation = profiles::create_generation(target.store.as_ref(), &profile, output)?;
		profiles::switch_link(&profile, &generation)?;
	}
	if parsed.has("dry-run") {
		return Ok(ExitCode::SUCCESS);
	}
	for (index, path) in target.paths.iter().enumerate() {
		let outputs = built_outputs(target.store.as_ref(), path, &results)?;
		for (name, output) in &outputs {
			if !parsed.has("dry-run") && !parsed.has("no-link") {
				let mut link = parsed.value("out-link").unwrap_or("result").to_owned();
				if index > 0 {
					link.push_str(&format!("-{index}"));
				}
				if !name.is_empty() && name != "out" {
					link.push_str(&format!("-{name}"));
				}
				target
					.store
					.add_perm_root(output, &std::env::current_dir()?.join(link))?;
			}
			if !parsed.has("dry-run") && parsed.has("print-out-paths") {
				nix_compat::log::logger::write_stdout(&output.to_absolute_path());
			}
		}
	}
	Ok(ExitCode::SUCCESS)
}

/// `InstallableFlake::getActualAttrPaths` with the command's
/// `getDefaultFlakeAttrPaths` and `getDefaultFlakeAttrPathPrefixes`.
pub fn flake_attr_paths(command: &str, fragment: &str, system: &str) -> Vec<String> {
	let mut defaults = Vec::new();
	let mut prefixes = Vec::new();
	match command {
		"develop" | "print-dev-env" => {
			defaults.extend([
				format!("devShells.{system}.default"),
				format!("devShell.{system}"),
			]);
			prefixes.push(format!("devShells.{system}."));
		}
		"run" | "bundle" => {
			defaults.extend([
				format!("apps.{system}.default"),
				format!("defaultApp.{system}"),
			]);
			prefixes.push(format!("apps.{system}."));
		}
		_ => {}
	}
	defaults.extend([
		format!("packages.{system}.default"),
		format!("defaultPackage.{system}"),
	]);
	prefixes.extend([
		format!("packages.{system}."),
		format!("legacyPackages.{system}."),
	]);
	if fragment.is_empty() {
		return defaults;
	}
	if let Some(exact) = fragment.strip_prefix('.') {
		return vec![exact.to_owned()];
	}
	prefixes
		.iter()
		.map(|prefix| format!("{prefix}{fragment}"))
		.chain([fragment.to_owned()])
		.collect()
}

/// nlohmann's `dump(2)` of compact JSON text: two-space indentation, `": "`
/// after keys, empty containers kept as `{}` and `[]`, scalars unchanged.
pub fn pretty_json(compact: &str) -> String {
	let mut out = String::new();
	let mut depth = 0usize;
	let mut chars = compact.chars().peekable();
	let newline = |out: &mut String, depth: usize| {
		out.push('\n');
		out.push_str(&"  ".repeat(depth));
	};
	while let Some(c) = chars.next() {
		match c {
			'"' => {
				out.push('"');
				while let Some(c) = chars.next() {
					out.push(c);
					match c {
						'\\' => {
							if let Some(escaped) = chars.next() {
								out.push(escaped);
							}
						}
						'"' => break,
						_ => {}
					}
				}
			}
			'{' | '[' => {
				let close = if c == '{' { '}' } else { ']' };
				out.push(c);
				if chars.peek() == Some(&close) {
					out.push(chars.next().unwrap_or(close));
				} else {
					depth += 1;
					newline(&mut out, depth);
				}
			}
			'}' | ']' => {
				depth = depth.saturating_sub(1);
				newline(&mut out, depth);
				out.push(c);
			}
			',' => {
				out.push(',');
				newline(&mut out, depth);
			}
			':' => out.push_str(": "),
			c => out.push(c),
		}
	}
	out
}
