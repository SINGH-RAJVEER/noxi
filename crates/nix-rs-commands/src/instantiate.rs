//! `nix-instantiate`, following CppNix 2.35.2's read-only evaluation default.
//!
//! Supported: `--eval`, `--strict`, `--json`, `--expr`/`-E`, `--attr`/`-A`,
//! `--arg`, `--argstr`, file arguments including `<lookup>` paths and `-`
//! for standard input. Single derivation selections can be instantiated
//! with an explicit `--store local?root=ROOT`. No builds or permanent GC roots are made.

use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::ExitCode;

use nix_rs_eval::builtin_macros::builtins;
use nix_rs_eval::{EvalMode, Evaluation, Value};
use rustc_hash::FxHashMap;
use smol_str::SmolStr;

/// A value passed with `--arg` or `--argstr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoArg {
	Expr(String),
	Str(String),
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Options {
	pub eval: bool,
	/// Explicit diverted local store, `local?root=ROOT`.
	pub store: Option<String>,
	/// Permit writes during `--eval`; normal instantiation writes by default.
	pub read_write_mode: bool,
	pub strict: bool,
	pub json: bool,
	/// `--expr`: positional arguments are expressions rather than files.
	pub from_args: bool,
	pub inputs: Vec<String>,
	pub attr_paths: Vec<String>,
	pub auto_args: Vec<(String, AutoArg)>,
}

/// Parse the command line after the program name. The error is the message
/// CppNix prints after `error: `.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Options, String> {
	let mut options = Options::default();
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
			"--json" => options.json = true,
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
				return Err(format!("unrecognised flag '{flag}'"));
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
fn driver_expression(options: &Options, cwd: &Path) -> io::Result<String> {
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
				selection = format!(
					"(let __nixrs_selected = {selection}; in if __nixrs_selected.type or null == \"derivation\" then builtins.storePath __nixrs_selected.drvPath else throw \"expected a derivation for instantiation\")"
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
	if !options.eval && options.json {
		eprintln!("error: --json requires --eval");
		return ExitCode::FAILURE;
	}
	let writable = !options.eval || options.read_write_mode;
	if writable && options.store.is_none() {
		eprintln!(
			"error: writable evaluation requires --store local?root=ROOT; pass --eval for read-only evaluation"
		);
		return ExitCode::FAILURE;
	}

	let driver = match driver_expression(options, cwd) {
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
	let store = match root {
		Some(root) => {
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
		None => nix_rs_glue::ReadOnlyStore::new(),
	};
	let mode = if !options.eval || options.strict || options.json {
		EvalMode::Strict
	} else {
		EvalMode::Lazy
	};
	let mut builder = nix_rs_glue::configure(Evaluation::builder_pure(), store)
		.env(Some(&env))
		.mode(mode);
	let source = builder.source_map().clone();
	let result = builder.build().evaluate_in_dir(&driver, cwd.to_path_buf());

	for warning in &result.warnings {
		warning.fancy_format_stderr(&source);
	}
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
			match value {
				Value::String(s) if options.json => stdout
					.write_all(s.as_bytes())
					.and_then(|()| stdout.write_all(b"\n")),
				value => writeln!(stdout, "{value}"),
			}
		};
		if written.is_err() {
			return ExitCode::FAILURE;
		}
	}
	ExitCode::SUCCESS
}
