//! Expression installables share the legacy evaluator and store owner.
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use nix_compat::store_path::StorePath;
use noxi_eval::{Ambiguous, Value};
use noxi_store::{DerivedPath, Store};
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
	parsed.unsupported(&[
		"eval-store",
		"write-to",
		"debugger",
		"override-input",
		"inputs-from",
		"profile",
	])?;
	let mut options = Options {
		modern_pure: pure,
		eval,
		read_write_mode: !parsed.has("read-only"),
		strict: true,
		settings: parsed.settings.clone(),
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
	for (name, string) in [("arg", false), ("argstr", true)] {
		if let Some(args) = parsed.flags.get(name) {
			for pair in args {
				options.auto_args.push((
					pair[0].clone(),
					if string {
						AutoArg::Str(pair[1].clone())
					} else {
						AutoArg::Expr(pair[1].clone())
					},
				));
			}
		}
	}
	let root = if let Some(expression) = parsed.value("expr") {
		expression.to_owned()
	} else if let Some(file) = parsed.value("file") {
		if instantiate::is_lookup_path(file) {
			format!("import {file}")
		} else if file == "-" {
			std::io::read_to_string(std::io::stdin())?
		} else {
			let absolute = instantiate::absolute_path(&std::env::current_dir()?, Path::new(file));
			format!("import {}", nix_string_literal(&absolute.to_string_lossy()))
		}
	} else {
		return Err(unsupported(&format!(
			"{} flake installables; use --expr or --file",
			parsed.command
		)));
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
		let mut expression = format!("({root}\n)");
		for attr in instantiate::parse_attr_path(&selection).map_err(Failure::Usage)? {
			expression = format!("({expression}).{}", nix_string_literal(&attr));
		}
		if let Some(apply) = parsed.value("apply") {
			expression = format!("({apply}\n) ({expression})");
		}
		if eval && parsed.has("raw") {
			expression = format!("\"${{{expression}}}\"");
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
	let options = options(&parsed, true)?;
	let evaluated = match instantiate::evaluate(&options, &std::env::current_dir()?) {
		Ok(value) => value,
		Err(code) => return Ok(code),
	};
	let mut stdout = io::stdout().lock();
	for value in evaluated.results {
		if parsed.has("raw") || parsed.has("json") {
			let Value::String(text) = value else {
				return Err(Failure::Error("evaluation produced no string".into()));
			};
			stdout.write_all(text.as_bytes())?;
			if parsed.has("json") {
				writeln!(stdout)?;
			}
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
	if parsed.has("expr") || parsed.has("file") {
		let options = options(parsed, false)?;
		let evaluated = match instantiate::evaluate(&options, &std::env::current_dir()?) {
			Ok(value) => value,
			Err(code) => return Ok(Err(code)),
		};
		let derivations = instantiate::derivations(&evaluated.results)?;
		let store = evaluated
			.store
			.ok_or_else(|| unsupported("building with this store"))?;
		let paths = derivations
			.into_iter()
			.map(|(drv, output)| DerivedPath::Built {
				drv,
				outputs: BTreeSet::from([output]),
			})
			.collect();
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
		let (path, spec) = operand
			.split_once('^')
			.map_or((operand.as_str(), None), |(p, s)| (p, Some(s)));
		let path = crate::paths::follow_links_to_store_path(&std::env::current_dir()?, path)?;
		paths.push(if let Some(spec) = spec {
			DerivedPath::Built {
				drv: path,
				outputs: if spec == "*" {
					BTreeSet::new()
				} else {
					spec.split(',').map(str::to_owned).collect()
				},
			}
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
	Ok(match path {
		DerivedPath::Opaque(path) => BTreeMap::from([(String::new(), path.clone())]),
		DerivedPath::Built { drv, outputs } => {
			let mut all = store.query_derivation_outputs(drv)?;
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
			all
		}
	})
}

pub fn build(parsed: Parsed) -> Result<ExitCode, Failure> {
	parsed.unsupported(&["rebuild", "repair", "profile"])?;
	// The shared build API exposes no BuildResult timing/resource fields.
	// Existing outputs and dry runs have complete JSON; fresh-build --json
	// must fail before building outputs rather than omit those fields.
	let target = match targets(&parsed)? {
		Ok(target) => target,
		Err(code) => return Ok(code),
	};
	let missing = target.store.query_missing(&target.paths)?;
	if parsed.has("json") && !parsed.has("dry-run") && !missing.will_build.is_empty() {
		return Err(unsupported(
			"build --json for new builds (the build API does not return BuildResult metadata)",
		));
	}
	crate::paths::print_missing(target.store.as_ref(), &missing)?;
	if !parsed.has("dry-run") {
		target.store.build_paths(&target.paths)?;
	}
	let mut result = Vec::new();
	for path in &target.paths {
		let outputs = outputs(target.store.as_ref(), path)?;
		result.push(match path {
			DerivedPath::Opaque(path) => json!(path.to_absolute_path()),
			DerivedPath::Built { drv, .. } => json!({"drvPath": drv.to_absolute_path(), "outputs": outputs.into_iter().map(|(name,path)| (name,path.to_absolute_path())).collect::<BTreeMap<_,_>>()}),
		});
	}
	if parsed.has("json") {
		println!("{}", json!(result));
	}
	for (index, path) in target.paths.iter().enumerate() {
		let outputs = outputs(target.store.as_ref(), path)?;
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
				println!("{}", output.to_absolute_path());
			}
		}
	}
	Ok(ExitCode::SUCCESS)
}
