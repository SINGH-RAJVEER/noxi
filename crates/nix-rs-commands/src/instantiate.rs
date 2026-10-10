//! `nix-instantiate`, following CppNix 2.35.2's read-only evaluation default.
//!
//! Supported: `--eval`, `--strict`, `--json`, `--xml` with `--no-location`,
//! `--expr`/`-E`, `--attr`/`-A`, `--arg`, `--argstr`, `-I`/`--include`,
//! `--find-file`, file arguments including `<lookup>` paths and `-` for
//! standard input.
//! Settings come from `nix.conf`, `NIX_CONFIG` and settings flags such as
//! `--option`, `--extra-experimental-features` and `--show-trace`, which is
//! accepted but does not change the output: error traces are not rendered in
//! CppNix's format yet. Single derivation selections can be instantiated
//! with an explicit `--store local?root=ROOT`. No builds or permanent GC roots are made.

use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use nix_compat::nixcpp::settings::Settings;
use nix_compat::store_path::StorePath;
use nix_rs_eval::builtin_macros::builtins;
use nix_rs_eval::{Ambiguous, EvalMode, Evaluation, Value};
use nix_rs_store::Store;
use rustc_hash::FxHashMap;
use smol_str::SmolStr;

use crate::store;

/// A value passed with `--arg` or `--argstr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoArg {
	Expr(String),
	Str(String),
}

#[derive(Debug, Default, Clone)]
pub struct Options {
	pub eval: bool,
	/// `--store`: the store URI, overriding the `store` setting.
	pub store: Option<String>,
	/// `--add-root`: make the derivations GC roots with this name.
	pub gc_root: Option<String>,
	/// Warn about results that are not GC roots (`--no-gc-warning` clears).
	pub gc_warning: bool,
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
	/// `--find-file`: print where each input is in the lookup path.
	pub find_file: bool,
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
	parse_args_with(args, settings, |_, _| None)
}

/// [`parse_args`] for a command with its own flags. Shared legacy flags are
/// handled first, then `extra` receives the remaining flags and arguments.
/// It returns `None` for flags it does not know.
pub fn parse_args_with(
	args: impl IntoIterator<Item = String>,
	settings: Settings,
	mut extra: impl FnMut(&str, &mut dyn Iterator<Item = String>) -> Option<Result<(), String>>,
) -> Result<Options, String> {
	// CppNix's `initGC` sets `nix-path` from `NIX_PATH`, even an empty one,
	// after reading the configuration and before the command line.
	let mut settings = settings;
	if let Ok(nix_path) = std::env::var("NIX_PATH") {
		settings
			.set(
				"nix-path",
				&nix_rs_eval::parse_nix_path(&nix_path).join(" "),
			)
			.map_err(|error| error.to_string())?;
	}
	let mut options = Options {
		settings,
		gc_warning: true,
		..Options::default()
	};
	let mut args = crate::cli::ArgStream::new(args);
	let value = |args: &mut dyn Iterator<Item = String>, flag: &str| {
		args.next()
			.ok_or_else(|| format!("flag '{flag}' requires 1 argument"))
	};
	while let Some(arg) = args.next_arg() {
		if let Some(result) = crate::cli::apply_common_flag(&arg, &mut args, &mut options.settings)
		{
			result.map_err(failure_message)?;
			continue;
		}
		if arg.starts_with('-')
			&& let Some(result) = extra(&arg, &mut args)
		{
			result?;
			continue;
		}
		match arg.as_str() {
			"--store" => options.store = Some(value(&mut args, &arg)?),
			"--add-root" => options.gc_root = Some(value(&mut args, &arg)?),
			"--indirect" => {}
			"--no-gc-warning" => options.gc_warning = false,
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
			"--find-file" => options.find_file = true,
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

/// The lookup path as CppNix's `EvalState` assembles it: the `-I` entries,
/// then the `nix-path` setting, then those of the default channel
/// directories that exist unless `restrict-eval` is set. Pure evaluation
/// has none.
pub fn search_path(options: &Options) -> Vec<String> {
	if options.settings.get_bool("pure-eval", false) {
		return Vec::new();
	}
	let mut elements = options.include.clone();
	elements.extend(
		options
			.settings
			.get_list("nix-path", &[])
			.into_iter()
			.map(str::to_owned),
	);
	if options.settings.get_bool("restrict-eval", false) {
		return elements;
	}
	let env = |name| {
		std::env::var_os(name)
			.filter(|value| !value.is_empty())
			.map(PathBuf::from)
	};
	let home = env("HOME").unwrap_or_default();
	let def_expr = if options.settings.get_bool("use-xdg-base-directories", false) {
		env("NIX_STATE_HOME")
			.unwrap_or_else(|| {
				env("XDG_STATE_HOME")
					.unwrap_or_else(|| home.join(".local/state"))
					.join("nix")
			})
			.join("defexpr")
	} else {
		home.join(".nix-defexpr")
	};
	let root_channels = nix_rs_store::LocalStoreConfig::system()
		.state_dir
		.join("profiles/per-user/root/channels");
	for (path, prefix) in [
		(def_expr.join("channels"), ""),
		(root_channels.join("nixpkgs"), "nixpkgs="),
		(root_channels, ""),
	] {
		if path.exists() {
			elements.push(format!("{prefix}{}", path.display()));
		}
	}
	elements
}

/// `--find-file`: print the physical path of each input in the lookup path.
pub fn find_files(options: &Options) -> ExitCode {
	let search_path = nix_rs_eval::NixSearchPath::new(
		search_path(options)
			.iter()
			.map(|element| nix_rs_eval::NixSearchPathEntry::parse(element))
			.collect(),
		None,
	)
	.with_pure_eval(options.settings.get_bool("pure-eval", false));
	let io = Box::new(nix_rs_eval::StdIO) as Box<dyn nix_rs_eval::EvalIO>;
	let mut stdout = io::stdout().lock();
	for input in &options.inputs {
		match search_path.resolve(&io, input) {
			Ok(path) => {
				let _ = writeln!(stdout, "{}", path.display());
			}
			Err(_) if input.starts_with("nix/") => {
				eprintln!("error: '<{input}>' has no physical path");
				return ExitCode::FAILURE;
			}
			Err(error) => {
				eprintln!("error: {error}");
				return ExitCode::FAILURE;
			}
		}
	}
	ExitCode::SUCCESS
}

/// The message of a failure from the shared flag handling.
fn failure_message(failure: crate::cli::Failure) -> String {
	match failure {
		crate::cli::Failure::Usage(message)
		| crate::cli::Failure::Error(message)
		| crate::cli::Failure::Exit(_, message) => message,
	}
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

/// A Nix string literal holding `s`, for expressions built from text.
pub fn nix_string_literal(s: &str) -> String {
	nix_string(s)
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
__nixrs_isDerivation = v: builtins.isAttrs v && v ? type && v.type == \"derivation\";
__nixrs_pair = d: [ d.drvPath (d.outputName or \"\") ];
__nixrs_getDerivations = v:
	let v' = __nixrs_autoCall v; in
	if __nixrs_isDerivation v' then [ (__nixrs_pair v') ]
	else if builtins.isAttrs v' then builtins.concatMap (name:
		let x = v'.${name}; in
		if builtins.match \"[A-Za-z_][A-Za-z0-9_+-]*\" name == null then [ ]
		else if __nixrs_isDerivation x then [ (__nixrs_pair x) ]
		else if builtins.isAttrs x && x ? recurseForDerivations && x.recurseForDerivations then __nixrs_getDerivations x
		else [ ]) (builtins.attrNames v')
	else if builtins.isList v' then builtins.concatMap (x:
		if __nixrs_isDerivation x then [ (__nixrs_pair x) ] else __nixrs_getDerivations x) v'
	else throw \"expression does not evaluate to a derivation (or a set or list of those)\";
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
				selection = format!("(__nixrs_getDerivations {selection})");
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

/// The store an evaluation registers paths in, and the result of each
/// selection, in command-line order.
pub struct Evaluated {
	pub store: Option<Rc<dyn Store>>,
	pub results: Vec<Value>,
}

/// Evaluate the selections of `options`. Errors are printed; the exit code
/// is returned. Unless `options.eval`, each result is a list of
/// `[ drvPath outputName ]` pairs, as CppNix's `getDerivations` finds them.
pub fn evaluate(options: &Options, cwd: &Path) -> Result<Evaluated, ExitCode> {
	let fail = |message: &dyn std::fmt::Display| {
		eprintln!("error: {message}");
		ExitCode::FAILURE
	};
	let driver = driver_expression(options, cwd).map_err(|e| fail(&e))?;

	let env: FxHashMap<SmolStr, Value> = driver_builtins::builtins()
		.into_iter()
		.map(|(name, value)| (SmolStr::new(format!("__nixrs_{name}")), value))
		.collect();
	let writable = !options.eval || options.read_write_mode;
	let uri = store::store_uri(options.store.as_deref(), &options.settings);
	let (evaluator_store, store) = if writable {
		match store::open(&uri, &options.settings).map_err(|e| fail(&e))? {
			Some(opened) => (
				nix_rs_glue::EvaluatorStore::with_store(
					opened.store.clone(),
					opened.real_store_dir.clone(),
				),
				Some(opened.store),
			),
			None => (nix_rs_glue::ReadOnlyStore::new(), None),
		}
	} else {
		// Read-only evaluation computes paths without opening a store, but
		// reads a diverted store's files where they are.
		let root = uri
			.strip_prefix("local?root=")
			.filter(|root| !root.is_empty() && !root.contains(['&', '?', '%']));
		let store = match root {
			Some(root) => {
				nix_rs_glue::EvaluatorStore::read_only(Path::new(root)).map_err(|e| fail(&e))?
			}
			None => nix_rs_glue::ReadOnlyStore::new(),
		};
		(store, None)
	};
	let mode = if !options.eval || options.strict || options.json || options.xml {
		EvalMode::Strict
	} else {
		EvalMode::Lazy
	};
	let builder = Evaluation::builder_pure()
		.experimental_features(options.settings.experimental_features())
		.max_call_depth(options.settings.get_int("max-call-depth", 10000).max(0) as usize)
		.search_path(search_path(options))
		.pure_eval(options.settings.get_bool("pure-eval", false));
	let builder = nix_rs_glue::configure(builder, evaluator_store)
		.env(Some(&env))
		.mode(mode);
	// The evaluator's own lints (unused bindings, legacy `let`, ...) have no
	// CppNix equivalent, so this compatibility command does not print them.
	let result = builder.build().evaluate_in_dir(&driver, cwd.to_path_buf());

	if !result.errors.is_empty() {
		for error in &result.errors {
			error.fancy_format_stderr();
		}
		return Err(ExitCode::FAILURE);
	}
	let Some(Value::List(results)) = result.value else {
		return Err(fail(&"evaluation produced no value"));
	};
	Ok(Evaluated {
		store,
		results: results.iter().map(forced).collect(),
	})
}

/// Strict evaluation leaves forced thunks in the result lists.
fn forced(value: &Value) -> Value {
	match value {
		Value::Thunk(thunk) if thunk.is_evaluated() => forced(&thunk.value()),
		value => value.clone(),
	}
}

/// The derivations a non-`--eval` evaluation found: path and output name.
pub fn derivations(results: &[Value]) -> Result<Vec<(StorePath, String)>, String> {
	let mut derivations = Vec::new();
	for result in results {
		let Value::List(pairs) = result else {
			return Err("instantiation produced no derivation list".to_owned());
		};
		for pair in pairs.iter() {
			let Value::List(pair) = forced(pair) else {
				return Err("instantiation produced an invalid derivation".to_owned());
			};
			let drv_path = forced(pair.get(0).ok_or("missing derivation path")?)
				.to_contextful_str()
				.map_err(|e| e.to_string())?;
			let output = forced(pair.get(1).ok_or("missing output name")?)
				.to_contextful_str()
				.map_err(|e| e.to_string())?;
			let drv_path = StorePath::from_absolute_path(drv_path.as_bytes()).map_err(|_| {
				format!(
					"invalid derivation path '{}'",
					String::from_utf8_lossy(drv_path.as_bytes())
				)
			})?;
			let output =
				String::from_utf8(output.as_bytes().to_vec()).map_err(|e| e.to_string())?;
			if output.is_empty() {
				return Err(format!(
					"derivation '{}' lacks an 'outputName' attribute",
					drv_path.to_absolute_path()
				));
			}
			derivations.push((drv_path, output));
		}
	}
	Ok(derivations)
}

/// Run `nix-instantiate` with already parsed options.
pub fn run(options: &Options, cwd: &Path, stdout: &mut dyn Write) -> ExitCode {
	if !options.eval && options.json {
		eprintln!("error: --json requires --eval");
		return ExitCode::FAILURE;
	}
	if options.eval && options.xml && !options.no_location {
		eprintln!("error: --xml output with source locations is not supported; pass --no-location");
		return ExitCode::FAILURE;
	}
	let evaluated = match evaluate(options, cwd) {
		Ok(evaluated) => evaluated,
		Err(code) => return code,
	};

	if !options.eval {
		let derivations = match derivations(&evaluated.results) {
			Ok(derivations) => derivations,
			Err(error) => {
				eprintln!("error: {error}");
				return ExitCode::FAILURE;
			}
		};
		let mut root_nr = 0;
		let mut warned = false;
		for (drv_path, output) in derivations {
			let mut printed = drv_path.to_absolute_path();
			match (&options.gc_root, &evaluated.store) {
				(Some(gc_root), Some(store)) => {
					root_nr += 1;
					let mut root = absolute_path(cwd, Path::new(gc_root)).into_os_string();
					if root_nr > 1 {
						root.push(format!("-{root_nr}"));
					}
					match store.add_perm_root(&drv_path, Path::new(&root)) {
						Ok(link) => printed = link.display().to_string(),
						Err(error) => {
							eprintln!("error: {error}");
							return ExitCode::FAILURE;
						}
					}
				}
				(None, _) if !warned && options.gc_warning => {
					warned = true;
					if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn) {
						eprintln!(
							"warning: you did not specify '--add-root'; the result might be removed by the garbage collector"
						);
					}
				}
				_ => {}
			}
			if output != "out" {
				printed.push('!');
				printed.push_str(&output);
			}
			if writeln!(stdout, "{printed}").is_err() {
				return ExitCode::FAILURE;
			}
		}
		return ExitCode::SUCCESS;
	}

	for value in &evaluated.results {
		let written = match value {
			value if options.xml => nix_rs_eval::builtins::value_to_xml(&mut *stdout, value)
				.map(|_| ())
				.map_err(|error| io::Error::other(error.to_string())),
			Value::String(s) if options.json => stdout
				.write_all(s.as_bytes())
				.and_then(|()| stdout.write_all(b"\n")),
			value => writeln!(stdout, "{}", Ambiguous(value)),
		};
		if written.is_err() {
			return ExitCode::FAILURE;
		}
	}
	ExitCode::SUCCESS
}

/// CppNix's `absPath`: `path` relative to `cwd`, normalised lexically.
pub fn absolute_path(cwd: &Path, path: &Path) -> PathBuf {
	let mut out = PathBuf::new();
	for component in cwd.join(path).components() {
		match component {
			std::path::Component::ParentDir => {
				out.pop();
			}
			std::path::Component::CurDir => {}
			c => out.push(c),
		}
	}
	out
}
