//! `nix-instantiate`, following CppNix 2.35.2's read-only evaluation default.
//!
//! Supported: `--eval`, `--strict`, `--json`, `--xml` with `--no-location`,
//! `--expr`/`-E`, `--attr`/`-A`, `--arg`, `--argstr`, `-I`/`--include`, file
//! arguments including `<lookup>` paths and `-` for standard input.
//! Settings come from `nix.conf`, `NIX_CONFIG` and settings flags such as
//! `--option`, `--extra-experimental-features` and `--show-trace`, which is
//! accepted but does not change the output: error traces are not rendered in
//! CppNix's format yet. Single derivation selections can be instantiated
//! with an explicit `--store local?root=ROOT`. No builds or permanent GC roots are made.

use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::ExitCode;

use nix_compat::nixcpp::settings::Settings;
use nix_rs_eval::builtin_macros::builtins;
use nix_rs_eval::{Ambiguous, EvalMode, Evaluation, Value};
use rustc_hash::FxHashMap;
use smol_str::SmolStr;

/// A value passed with `--arg` or `--argstr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoArg {
	Expr(String),
	Str(String),
}

#[derive(Debug, Default, Clone)]
pub struct Options {
	pub eval: bool,
	/// Explicit diverted local store, `local?root=ROOT`.
	pub store: Option<String>,
	/// Permit writes during `--eval`; normal instantiation writes by default.
	pub read_write_mode: bool,
	pub strict: bool,
	pub json: bool,
	/// `--xml`; the last of `--json` and `--xml` wins, as in CppNix.
	pub xml: bool,
	/// `--no-location`: omit source positions from XML output.
	pub no_location: bool,
	/// Settings from the configuration files and the command line.
	pub settings: Settings,
	/// `-I` search-path entries, searched before `NIX_PATH`.
	pub include: Vec<String>,
	/// `--expr`: positional arguments are expressions rather than files.
	pub from_args: bool,
	pub inputs: Vec<String>,
	pub attr_paths: Vec<String>,
	pub auto_args: Vec<(String, AutoArg)>,
}

/// Parse the command line after the program name, applying settings flags
/// such as `--option NAME VALUE` and `--extra-experimental-features` to
/// `settings`. The error is the message CppNix prints after `error: `.
pub fn parse_args(
	args: impl IntoIterator<Item = String>,
	settings: Settings,
) -> Result<Options, String> {
	let mut options = Options {
		settings,
		..Options::default()
	};
	let mut args = args.into_iter();
	let value = |args: &mut dyn Iterator<Item = String>, flag: &str| {
		args.next()
			.ok_or_else(|| format!("flag '{flag}' requires 1 argument"))
	};
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--store" => options.store = Some(value(&mut args, &arg)?),
			"--read-write-mode" => options.read_write_mode = true,
			"--eval" | "--eval-only" => options.eval = true,
			"--strict" => options.strict = true,
			"--json" => {
				options.json = true;
				options.xml = false;
			}
			"--xml" => {
				options.xml = true;
				options.json = false;
			}
			"--no-location" => options.no_location = true,
			"-I" | "--include" => options.include.push(value(&mut args, &arg)?),
			"--expr" | "-E" => options.from_args = true,
			"--attr" | "-A" => options.attr_paths.push(value(&mut args, &arg)?),
			"--arg" | "--argstr" => {
				let name = args
					.next()
					.ok_or_else(|| format!("flag '{arg}' requires 2 arguments"))?;
				let text = args
					.next()
					.ok_or_else(|| format!("flag '{arg}' requires 2 arguments"))?;
				let auto_arg = if arg == "--arg" {
					AutoArg::Expr(text)
				} else {
					AutoArg::Str(text)
				};
				options.auto_args.push((name, auto_arg));
			}
			"--" => options.inputs.extend(args.by_ref()),
			flag if flag.starts_with('-') && flag != "-" => {
				match options.settings.apply_flag(flag, || args.next()) {
					Some(result) => result.map_err(|error| error.to_string())?,
					None => return Err(format!("unrecognised flag '{flag}'")),
				}
			}
			_ => options.inputs.push(arg),
		}
	}
	Ok(options)
}

/// Split an attribute path the way CppNix's `parseAttrPath` does.
pub fn parse_attr_path(path: &str) -> Result<Vec<String>, String> {
	let mut parts = Vec::new();
	let mut current = String::new();
	let mut chars = path.chars();
	while let Some(c) = chars.next() {
		match c {
			'.' => parts.push(std::mem::take(&mut current)),
			'"' => loop {
				match chars.next() {
					Some('"') => break,
					Some(c) => current.push(c),
					None => {
						return Err(format!("missing closing quote in selection path '{path}'"));
					}
				}
			},
			c => current.push(c),
		}
	}
	if !current.is_empty() {
		parts.push(current);
	}
	Ok(parts)
}

/// A Nix string literal holding `s`.
fn nix_string(s: &str) -> String {
	let mut out = String::with_capacity(s.len() + 2);
	out.push('"');
	let mut chars = s.chars().peekable();
	while let Some(c) = chars.next() {
		match c {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			'$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
			c => out.push(c),
		}
	}
	out.push('"');
	out
}

fn is_lookup_path(s: &str) -> bool {
	s.len() > 2
		&& s.starts_with('<')
		&& s.ends_with('>')
		&& s[1..s.len() - 1]
			.chars()
			.all(|c| c.is_ascii_alphanumeric() || "._-+/".contains(c))
}

/// The Nix expression one input evaluates to before attribute selection.
fn root_expression(options: &Options, input: &str, cwd: &Path) -> io::Result<String> {
	if options.from_args {
		return Ok(format!("({input}\n)"));
	}
	if input == "-" {
		let mut expr = String::new();
		io::stdin().read_to_string(&mut expr)?;
		return Ok(format!("({expr}\n)"));
	}
	if is_lookup_path(input) {
		return Ok(format!("(import {input})"));
	}
	let path = cwd.join(input);
	Ok(format!("(import {})", nix_string(&path.to_string_lossy())))
}

/// The expression evaluated for all inputs and attribute paths: a list with
/// one result per selection, in command-line order.
fn driver_expression(options: &Options, cwd: &Path, build_outputs: bool) -> io::Result<String> {
	let mut driver = String::from("let\n__nixrs_autoArgs = {\n");
	for (name, arg) in &options.auto_args {
		let value = match arg {
			AutoArg::Expr(expr) => format!("({expr}\n)"),
			AutoArg::Str(s) => nix_string(s),
		};
		let _ = writeln!(driver, "{} = {value};", nix_string(name));
	}
	// CppNix's `autoCallFunction`: functors are called with themselves, and
	// functions with a pattern get the automatic arguments they accept.
	driver.push_str(
		"};
__nixrs_autoCall = f:
	if builtins.isAttrs f && f ? __functor then __nixrs_autoCall (f.__functor f)
	else let ellipsis = __nixrs_formals f; in
	if ellipsis == null then f
	else if ellipsis then f __nixrs_autoArgs
	else f (builtins.intersectAttrs (builtins.functionArgs f) __nixrs_autoArgs);
__nixrs_forceEach = results: builtins.foldl' (acc: v: builtins.seq v acc) results results;
__nixrs_select = v: name:
	let v' = __nixrs_autoCall v; in
	if builtins.isList v' && builtins.match \"[0-9]+\" name != null
	then builtins.elemAt v' (builtins.fromJSON name)
	else v'.${name};
in __nixrs_forceEach [\n",
	);

	let default_input = [if options.from_args {
		String::new()
	} else {
		"./default.nix".to_owned()
	}];
	let inputs: &[String] = if options.inputs.is_empty() && !options.from_args {
		&default_input
	} else {
		&options.inputs
	};
	let attr_paths = if options.attr_paths.is_empty() {
		vec![String::new()]
	} else {
		options.attr_paths.clone()
	};

	for input in inputs {
		let root = root_expression(options, input, cwd)?;
		for attr_path in &attr_paths {
			let parts = parse_attr_path(attr_path)
				.map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
			let mut selection = root.clone();
			for part in parts {
				selection = format!("(__nixrs_select {selection} {})", nix_string(&part));
			}
			if !options.auto_args.is_empty() {
				selection = format!("(__nixrs_autoCall {selection})");
			}
			if !options.eval {
				let result = if build_outputs {
					"builtins.toJSON { drvPath = builtins.storePath __nixrs_selected.drvPath; outputName = __nixrs_selected.outputName or \"out\"; }"
				} else {
					"builtins.storePath __nixrs_selected.drvPath"
				};
				selection = format!(
					"(let __nixrs_selected = {selection}; in if __nixrs_selected.type or null == \"derivation\" then {result} else throw \"expected a derivation for instantiation\")"
				);
			} else if options.json {
				selection = format!("(builtins.toJSON {selection})");
			}
			driver.push_str(&selection);
			driver.push('\n');
		}
	}
	driver.push(']');
	Ok(driver)
}

#[builtins]
mod driver_builtins {
	use nix_rs_eval::generators::{Gen, GenCo};
	use nix_rs_eval::{ErrorKind, Value};

	/// `null` for anything but a function with a pattern; otherwise whether
	/// the pattern has an ellipsis.
	#[builtin("formals")]
	async fn builtin_formals(co: GenCo, f: Value) -> Result<Value, ErrorKind> {
		let _ = co;
		Ok(match f {
			Value::Closure(closure) => match closure.lambda.formals_ellipsis() {
				Some(ellipsis) => Value::Bool(ellipsis),
				None => Value::Null,
			},
			_ => Value::Null,
		})
	}
}

/// Run `nix-instantiate` with already parsed options.
pub fn run(options: &Options, cwd: &Path, stdout: &mut dyn Write) -> ExitCode {
	run_with_store(options, cwd, stdout, None)
}

/// Run with an evaluator store retained by the caller through subsequent
/// realization. Its temporary roots protect the newly instantiated graph.
pub fn run_with_store(
	options: &Options,
	cwd: &Path,
	stdout: &mut dyn Write,
	supplied_store: Option<std::rc::Rc<nix_rs_glue::EvaluatorStore>>,
) -> ExitCode {
	run_internal(options, cwd, stdout, supplied_store, false)
}

pub(crate) fn run_for_build(
	options: &Options,
	cwd: &Path,
	stdout: &mut dyn Write,
	store: std::rc::Rc<nix_rs_glue::EvaluatorStore>,
) -> ExitCode {
	run_internal(options, cwd, stdout, Some(store), true)
}

fn run_internal(
	options: &Options,
	cwd: &Path,
	stdout: &mut dyn Write,
	supplied_store: Option<std::rc::Rc<nix_rs_glue::EvaluatorStore>>,
	build_outputs: bool,
) -> ExitCode {
	if !options.eval && options.json {
		eprintln!("error: --json requires --eval");
		return ExitCode::FAILURE;
	}
	if options.eval && options.xml && !options.no_location {
		eprintln!("error: --xml output with source locations is not supported; pass --no-location");
		return ExitCode::FAILURE;
	}
	let writable = !options.eval || options.read_write_mode;
	if writable && options.store.is_none() {
		eprintln!(
			"error: writable evaluation requires --store local?root=ROOT; pass --eval for read-only evaluation"
		);
		return ExitCode::FAILURE;
	}

	let driver = match driver_expression(options, cwd, build_outputs) {
		Ok(driver) => driver,
		Err(error) => {
			eprintln!("error: {error}");
			return ExitCode::FAILURE;
		}
	};

	let env: FxHashMap<SmolStr, Value> = driver_builtins::builtins()
		.into_iter()
		.map(|(name, value)| (SmolStr::new(format!("__nixrs_{name}")), value))
		.collect();
	// Validate even read-only selections rather than silently accepting a
	// daemon, system store, or unsupported URI parameters.
	let root = match options.store.as_deref() {
		None => None,
		Some(uri) => match uri.strip_prefix("local?root=") {
			Some(root)
				if !root.is_empty()
					&& !root.contains(['&', '?', '%'])
					&& Path::new(root).is_absolute() =>
			{
				Some(Path::new(root))
			}
			_ => {
				eprintln!("error: only --store local?root=ABSOLUTE_ROOT is supported");
				return ExitCode::FAILURE;
			}
		},
	};
	let store = match (supplied_store, root) {
		(Some(store), _) => store,
		(None, Some(root)) => {
			let store = if writable {
				nix_rs_glue::EvaluatorStore::writable(root)
			} else {
				nix_rs_glue::EvaluatorStore::read_only(root)
			};
			match store {
				Ok(store) => store,
				Err(error) => {
					eprintln!("error: {error}");
					return ExitCode::FAILURE;
				}
			}
		}
		(None, None) => nix_rs_glue::ReadOnlyStore::new(),
	};
	let mode = if !options.eval || options.strict || options.json || options.xml {
		EvalMode::Strict
	} else {
		EvalMode::Lazy
	};
	let mut builder = Evaluation::builder_pure()
		.experimental_features(options.settings.experimental_features())
		.max_call_depth(options.settings.get_int("max-call-depth", 10000).max(0) as usize);
	if !options.include.is_empty() {
		// CppNix searches `-I` entries before `NIX_PATH`.
		let mut nix_path = options.include.join(":");
		if let Some(env) = std::env::var("NIX_PATH").ok().filter(|p| !p.is_empty()) {
			nix_path.push(':');
			nix_path.push_str(&env);
		}
		builder = builder.nix_path(Some(nix_path));
	}
	let builder = nix_rs_glue::configure(builder, store)
		.env(Some(&env))
		.mode(mode);
	// The evaluator's own lints (unused bindings, legacy `let`, ...) have no
	// CppNix equivalent, so this compatibility command does not print them.
	let result = builder.build().evaluate_in_dir(&driver, cwd.to_path_buf());

	if !result.errors.is_empty() {
		for error in &result.errors {
			error.fancy_format_stderr();
		}
		return ExitCode::FAILURE;
	}
	let Some(Value::List(results)) = result.value else {
		eprintln!("error: evaluation produced no value");
		return ExitCode::FAILURE;
	};
	for value in results.iter() {
		let written = if !options.eval {
			let string = match value.to_contextful_str() {
				Ok(string) => string,
				Err(error) => {
					eprintln!("error: {error}");
					return ExitCode::FAILURE;
				}
			};
			stdout
				.write_all(string.as_bytes())
				.and_then(|()| stdout.write_all(b"\n"))
		} else {
			// Strict evaluation leaves forced thunks in the result list.
			let forced;
			let value = match value {
				Value::Thunk(thunk) => {
					forced = thunk.value().clone();
					&forced
				}
				value => value,
			};
			match value {
				value if options.xml => nix_rs_eval::builtins::value_to_xml(&mut *stdout, value)
					.map(|_| ())
					.map_err(|error| io::Error::other(error.to_string())),
				Value::String(s) if options.json => stdout
					.write_all(s.as_bytes())
					.and_then(|()| stdout.write_all(b"\n")),
				value => writeln!(stdout, "{}", Ambiguous(value)),
			}
		};
		if written.is_err() {
			return ExitCode::FAILURE;
		}
	}
	ExitCode::SUCCESS
}
