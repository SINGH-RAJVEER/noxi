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
	let mut settings = parsed.settings.clone();
	if parsed.has("no-registries") {
		settings
			.set("use-registries", "false")
			.map_err(|e| e.to_string())?;
	}
	parsed.unsupported(&["eval-store", "write-to", "debugger", "inputs-from"])?;
	let mut options = Options {
		modern_pure: pure,
		eval,
		read_write_mode: !parsed.has("read-only"),
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
		// The expression is its own `«string»` source, as CppNix parses it.
		options.fragments.push(expression.to_owned());
		format!("@@noxi-frag-{}@@", options.fragments.len() - 1)
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
		options.flake = Some(super::flakes::request(parsed, "outputs")?);
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
		for attr in instantiate::parse_attr_path(&selection).map_err(Failure::Usage)? {
			expression = format!("({expression}).{}", nix_string_literal(&attr));
		}
		if let Some(apply) = parsed.value("apply") {
			options.fragments.push(apply.to_owned());
			let apply = format!("@@noxi-frag-{}@@", options.fragments.len() - 1);
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
