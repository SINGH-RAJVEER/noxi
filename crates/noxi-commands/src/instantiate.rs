//! `nix-instantiate`, following CppNix 2.35.2's read-only evaluation default.
//!
//! Supported: `--parse`, `--impure`, `--eval`, `--strict`, `--json`, `--xml` with `--no-location`,
//! `--expr`/`-E`, `--attr`/`-A`, `--arg`, `--argstr`, `-I`/`--include`,
//! `--find-file`, file arguments including `<lookup>` paths and `-` for
//! standard input.
//! Settings come from `nix.conf`, `NIX_CONFIG` and settings flags such as
//! `--option`, `--extra-experimental-features` and `--show-trace`.
//! Evaluation errors are printed in CppNix's format, with function-call
//! frames under `show-trace`; `NOXI_DIAGNOSTICS=codespan` selects noxi's
//! annotated source renderer instead. Single derivation selections can be instantiated
//! with an explicit `--store local?root=ROOT`. The selected owner opens
//! before evaluation. Read-only evaluation does not register computed paths;
//! `--read-write-mode` also allows IFD builds through that owner.

use std::fmt::Write as _;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use nix_compat::nixcpp::settings::Settings;
use nix_compat::store_path::StorePath;
use noxi_eval::builtin_macros::builtins;
use noxi_eval::{Ambiguous, EvalMode, Evaluation, SourceFragment, Value};
use noxi_store::Store;
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
	/// Apply the modern command's pure builtins and explicit filesystem limit.
	pub modern_pure: bool,
	pub eval: bool,
	pub parse: bool,
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
	/// The evaluator's code of each evaluation error printed, such as `E006`,
	/// for callers that classify failures.
	pub error_codes: Rc<std::cell::RefCell<Vec<&'static str>>>,
	/// Flake registry rewrites supplied on the command line.
	pub override_flakes: Vec<(String, String)>,
	/// `nix-shell`: functions with an `inNixShell` argument get it as `true`.
	pub in_nix_shell: bool,
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
			.set("nix-path", &noxi_eval::parse_nix_path(&nix_path).join(" "))
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
			"--override-flake" => {
				let from = value(&mut args, &arg)?;
				let to = value(&mut args, &arg)?;
				options.override_flakes.push((from, to));
			}
			"--store" => options.store = Some(value(&mut args, &arg)?),
			"--add-root" => options.gc_root = Some(value(&mut args, &arg)?),
			"--indirect" => {}
			"--no-gc-warning" => options.gc_warning = false,
			"--read-write-mode" => options.read_write_mode = true,
			"--eval" | "--eval-only" => options.eval = true,
			"--parse" => options.parse = true,
			"--impure" => {
				options
					.settings
					.set("pure-eval", "false")
					.map_err(|error| error.to_string())?;
			}
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
	let root_channels = noxi_store::LocalStoreConfig::system()
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
	let search_path = noxi_eval::NixSearchPath::new(
		search_path(options)
			.iter()
			.map(|element| noxi_eval::NixSearchPathEntry::parse(element))
			.collect(),
		None,
	)
	.with_pure_eval(options.settings.get_bool("pure-eval", false));
	let io = Box::new(noxi_eval::StdIO) as Box<dyn noxi_eval::EvalIO>;
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

pub(crate) fn is_lookup_path(s: &str) -> bool {
	s.len() > 2
		&& s.starts_with('<')
		&& s.ends_with('>')
		&& s[1..s.len() - 1]
			.chars()
			.all(|c| c.is_ascii_alphanumeric() || "._-+/".contains(c))
}

/// The Nix expression one input evaluates to before attribute selection.
fn root_expression(
	options: &Options,
	input: &str,
	cwd: &Path,
	lints: &crate::instantiate_lints::Lints,
	sources: &mut Vec<(String, String)>,
) -> io::Result<String> {
	if options.from_args {
		lints
			.preflight(input, "«string»", true)
			.map_err(|error| io::Error::other(error.to_string()))?;
		sources.push(("«string»".into(), input.to_owned()));
		return Ok(format!("@@noxi-input-{}@@", sources.len() - 1));
	}
	if input == "-" {
		let mut expr = String::new();
		io::stdin().read_to_string(&mut expr)?;
		lints
			.preflight(&expr, "«stdin»", true)
			.map_err(|error| io::Error::other(error.to_string()))?;
		sources.push(("«stdin»".into(), expr));
		return Ok(format!("@@noxi-input-{}@@", sources.len() - 1));
	}
	if is_lookup_path(input) {
		return Ok(format!("(import {input})"));
	}
	let path = cwd.join(input);
	let source_path = if path.is_dir() {
		path.join("default.nix")
	} else {
		path.clone()
	};
	if let Ok(code) = std::fs::read_to_string(&source_path) {
		lints
			.preflight(&code, &source_path.to_string_lossy(), false)
			.map_err(|error| io::Error::other(error.to_string()))?;
	}
	Ok(format!("(import {})", nix_string(&path.to_string_lossy())))
}

/// The expression evaluated for all inputs and attribute paths: a list with
/// one result per selection, in command-line order.
fn driver_expression(
	options: &Options,
	cwd: &Path,
	lints: &crate::instantiate_lints::Lints,
) -> io::Result<(String, Vec<SourceFragment>)> {
	let mut sources = vec![];
	let mut driver = String::from("let\n__noxi_autoArgs = {\n");
	for (name, arg) in &options.auto_args {
		let value = match arg {
			AutoArg::Expr(expr) => {
				lints
					.preflight(expr, "«string»", true)
					.map_err(|error| io::Error::other(error.to_string()))?;
				{
					sources.push(("«string»".into(), expr.clone()));
					format!("@@noxi-input-{}@@", sources.len() - 1)
				}
			}
			AutoArg::Str(s) => nix_string(s),
		};
		let _ = writeln!(driver, "{} = {value};", nix_string(name));
	}
	// CppNix's `autoCallFunction`: functors are called with themselves, and
	// functions with a pattern get the automatic arguments they accept; for
	// `nix-shell`, those taking `inNixShell` also get it as `true`.
	driver.push_str("};\n");
	let _ = writeln!(driver, "__noxi_inNixShell = {};", options.in_nix_shell);
	driver.push_str(
		"__noxi_shellArgs = __noxi_autoArgs // { inNixShell = true; };
__noxi_autoCall = f:
	if builtins.isAttrs f && f ? __functor then __noxi_autoCall (f.__functor f)
	else let ellipsis = __noxi_formals f; in
	if ellipsis == null then f
	else let args = if __noxi_inNixShell && builtins.functionArgs f ? inNixShell then __noxi_shellArgs else __noxi_autoArgs; in
	if ellipsis then f args
	else f (builtins.intersectAttrs (builtins.functionArgs f) args);
__noxi_forceEach = results: builtins.foldl' (acc: v: builtins.seq v acc) results results;
__noxi_isDerivation = v: builtins.isAttrs v && v ? type && v.type == \"derivation\";
__noxi_pair = d: let p = __noxi_drvPath d; in builtins.seq p [ p (d.outputName or \"\") ];
__noxi_getDerivations = v:
	let v' = __noxi_autoCall v; in
	if __noxi_isDerivation v' then [ (__noxi_pair v') ]
	else if builtins.isAttrs v' then builtins.concatMap (name:
		let x = v'.${name}; in
		if builtins.match \"[A-Za-z_][A-Za-z0-9_+-]*\" name == null then [ ]
		else if __noxi_isDerivation x then [ (__noxi_pair x) ]
		else if builtins.isAttrs x && x ? recurseForDerivations && x.recurseForDerivations then __noxi_getDerivations x
		else [ ]) (builtins.attrNames v')
	else if builtins.isList v' then builtins.concatMap (x:
		if __noxi_isDerivation x then [ (__noxi_pair x) ] else __noxi_getDerivations x) v'
	else throw \"expression does not evaluate to a derivation (or a set or list of those)\";
__noxi_select = v: name:
	let v' = __noxi_autoCall v; in
	if builtins.isList v' && builtins.match \"[0-9]+\" name != null
	then builtins.elemAt v' (builtins.fromJSON name)
	else v'.${name};
in __noxi_forceEach [\n",
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
		let root = root_expression(options, input, cwd, lints, &mut sources)?;
		for attr_path in &attr_paths {
			let parts = parse_attr_path(attr_path)
				.map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
			let mut selection = root.clone();
			for part in parts {
				selection = format!("(__noxi_select {selection} {})", nix_string(&part));
			}
			if !options.auto_args.is_empty() {
				selection = format!("(__noxi_autoCall {selection})");
			}
			if !options.eval {
				selection = format!("(__noxi_forceEach (__noxi_getDerivations {selection}))");
			} else if options.json {
				selection = format!("(builtins.toJSON {selection})");
			}
			driver.push_str(&selection);
			driver.push('\n');
		}
	}
	driver.push(']');
	let mut code = String::new();
	let mut fragments = vec![];
	let mut remaining = driver.as_str();
	while let Some(start) = remaining.find("@@noxi-input-") {
		code.push_str(&remaining[..start]);
		remaining = &remaining[start + "@@noxi-input-".len()..];
		let end = remaining.find("@@").expect("generated input marker");
		let index: usize = remaining[..end].parse().expect("generated input index");
		let (name, input) = &sources[index];
		code.push('(');
		let start = code.len();
		code.push_str(input);
		fragments.push(SourceFragment {
			range: start..code.len(),
			name: name.clone(),
			code: input.clone(),
		});
		code.push_str("\n)");
		remaining = &remaining[end + 2..];
	}
	code.push_str(remaining);
	Ok((code, fragments))
}

#[builtins]
mod driver_builtins {
	use noxi_eval::generators::{Gen, GenCo};
	use noxi_eval::{ErrorKind, Value};

	/// Validate the drvPath while its source position is still available.
	#[builtin("drvPath")]
	async fn builtin_drv_path(co: GenCo, derivation: Value) -> Result<Value, ErrorKind> {
		use noxi_eval::{AddContext, generators};
		let attrs = derivation.to_attrs()?;
		let span = attrs.get_attr_pos("drvPath");
		let path = generators::request_force(&co, attrs.select_required("drvPath")?.clone()).await;
		if path.is_catchable() {
			return Ok(path);
		}
		let string = path.to_contextful_str()?;
		let parsed = nix_compat::store_path::StorePath::from_absolute_path(string.as_bytes())
			.map_err(|error| ErrorKind::EvalError(error.to_string()))?;
		if !parsed.name().ends_with(".drv") {
			return Err(ErrorKind::EvalError(format!(
				"store path '{}' is not a valid derivation path",
				parsed
			)))
			.trace(
				span,
				"while evaluating the 'drvPath' attribute of a derivation",
			);
		}
		Ok(path)
	}

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

/// Keep the owner's build status through the evaluator's I/O wrappers.
fn evaluation_build_exit_status(error: &(dyn std::error::Error + 'static)) -> Option<u8> {
	let mut cause = Some(error);
	while let Some(error) = cause {
		if let Some(noxi_store::Error::Build { exit_status, .. }) =
			error.downcast_ref::<noxi_store::Error>()
		{
			return Some(*exit_status);
		}
		cause = if let Some(error) = error.downcast_ref::<io::Error>() {
			error.get_ref().map(|error| error as &dyn std::error::Error)
		} else {
			error.source()
		};
	}
	None
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
	let lints = crate::instantiate_lints::Lints::new(&options.settings);
	let (driver, fragments) = driver_expression(options, cwd, &lints).map_err(|error| {
		if lints.failed.get() {
			ExitCode::FAILURE
		} else {
			fail(&error)
		}
	})?;

	let env: FxHashMap<SmolStr, Value> = driver_builtins::builtins()
		.into_iter()
		.map(|(name, value)| (SmolStr::new(format!("__noxi_{name}")), value))
		.collect();
	let writable = !options.eval || options.read_write_mode;
	let uri = store::store_uri(options.store.as_deref(), &options.settings);
	let (evaluator_store, store) =
		match store::open(&uri, &options.settings).map_err(|e| fail(&e))? {
			Some(opened) if writable => (
				noxi_glue::EvaluatorStore::with_store(opened.store.clone(), opened.real_store_dir),
				Some(opened.store),
			),
			Some(opened) => (
				noxi_glue::EvaluatorStore::with_read_store(opened.store, opened.real_store_dir),
				None,
			),
			None => (noxi_glue::ReadOnlyStore::new(), None),
		};
	evaluator_store.allow_import_from_derivation(
		options
			.settings
			.get_bool("allow-import-from-derivation", true),
	);
	evaluator_store.trace_import_from_derivation(
		options
			.settings
			.get_bool("trace-import-from-derivation", false),
	);
	let mut fetch = noxi_glue::fetch::FetchConfig {
		pure_eval: options.settings.get_bool("pure-eval", false),
		override_flakes: options.override_flakes.clone(),
		cwd: cwd.to_owned(),
		..Default::default()
	};
	fetch.settings.allow_dirty = options.settings.get_bool("allow-dirty", true);
	fetch.settings.warn_dirty = options.settings.get_bool("warn-dirty", true);
	fetch.settings.allow_dirty_locks = options.settings.get_bool("allow-dirty-locks", false);
	fetch.settings.tarball_ttl = options.settings.get_int("tarball-ttl", 3600).max(0) as u64;
	fetch.settings.use_registries = options.settings.get_bool("use-registries", true);
	fetch.settings.trust_tarballs_from_git_forges = options
		.settings
		.get_bool("trust-tarballs-from-git-forges", true);
	fetch.settings.flake_registry = options
		.settings
		.get("flake-registry")
		.unwrap_or("https://channels.nixos.org/flake-registry.json")
		.to_owned();
	fetch.settings.verified_fetches = options
		.settings
		.experimental_features()
		.contains("verified-fetches");
	fetch.settings.access_tokens = options
		.settings
		.get("access-tokens")
		.unwrap_or_default()
		.split_whitespace()
		.filter_map(|entry| {
			entry
				.split_once('=')
				.map(|(host, token)| (host.to_owned(), token.to_owned()))
		})
		.collect();
	evaluator_store.configure_fetching(fetch);
	let mode = if !options.eval || options.strict || options.json || options.xml {
		EvalMode::Strict
	} else {
		EvalMode::Lazy
	};
	let builder = Evaluation::builder_pure()
		.with_source_map(lints.source_map())
		.experimental_features(options.settings.experimental_features())
		.trace_verbose(options.settings.get_bool("trace-verbose", false))
		.message_output(|bytes| {
			// CppNix's simple logger preserves color only on suitable terminals.
			let keep_color = io::stderr().is_terminal()
				&& std::env::var("TERM").is_ok_and(|term| term != "dumb")
				&& std::env::var_os("NO_COLOR").is_none()
				&& std::env::var_os("NOCOLOR").is_none();
			let bytes = nix_compat::log::filter_ansi_escapes(bytes, !keep_color);
			let _ = io::stderr().write_all(&bytes);
		})
		.warning_output(nix_compat::log::log_enabled(
			nix_compat::log::VerbosityLevel::Warn,
		))
		.abort_on_warn(
			options.settings.get_bool(
				"abort-on-warn",
				std::env::var("NIX_ABORT_ON_WARN")
					.is_ok_and(|value| matches!(value.as_str(), "1" | "yes" | "true")),
			),
		)
		.max_call_depth(options.settings.get_int("max-call-depth", 10000).max(0) as usize)
		.search_path(search_path(options))
		.pure_eval(options.settings.get_bool("pure-eval", false));
	let builder = noxi_glue::configure(builder, evaluator_store)
		.env(Some(&env))
		.mode(mode);
	// The evaluator's own lints (unused bindings, legacy `let`, ...) have no
	// CppNix equivalent, so this compatibility command does not print them.
	let result = if options.modern_pure {
		crate::nix::pure::evaluate(
			builder,
			&env,
			mode,
			options.settings.get_int("max-call-depth", 10000).max(0) as usize,
			&driver,
			fragments,
			cwd,
		)
	} else {
		builder
			.build()
			.hide_root_source()
			.with_source_fragments(fragments)
			.evaluate_in_dir(&driver, cwd.to_path_buf())
	};

	if !result.errors.is_empty() {
		if lints.failed.get() {
			return Err(ExitCode::FAILURE);
		}
		for error in &result.errors {
			options.error_codes.borrow_mut().push(error.code());
			print_evaluation_error(error, &options.settings);
		}
		let status = result
			.errors
			.iter()
			.find_map(|error| evaluation_build_exit_status(error));
		return Err(status.map(ExitCode::from).unwrap_or(ExitCode::FAILURE));
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

/// Parse each input without evaluating it or opening a store.
pub fn parse(options: &Options, cwd: &Path, stdout: &mut dyn Write) -> ExitCode {
	let lints = crate::instantiate_lints::Lints::new(&options.settings);
	let default = ["./default.nix".to_owned()];
	let inputs = if options.inputs.is_empty() && !options.from_args {
		&default[..]
	} else {
		&options.inputs
	};
	for input in inputs {
		let (code, location) = if options.from_args {
			(Ok(input.clone()), None)
		} else if input == "-" {
			let mut code = String::new();
			(io::stdin().read_to_string(&mut code).map(|_| code), None)
		} else {
			let mut path = if is_lookup_path(input) {
				let lookup = noxi_eval::NixSearchPath::new(
					search_path(options)
						.iter()
						.map(|entry| noxi_eval::NixSearchPathEntry::parse(entry))
						.collect(),
					None,
				)
				.with_pure_eval(options.settings.get_bool("pure-eval", false));
				let io = Box::new(noxi_eval::StdIO) as Box<dyn noxi_eval::EvalIO>;
				match lookup.resolve(&io, &input[1..input.len() - 1]) {
					Ok(path) => path,
					Err(error) => {
						eprintln!("error: {error}");
						return ExitCode::FAILURE;
					}
				}
			} else {
				cwd.join(input)
			};
			if path.is_dir() {
				path.push("default.nix");
			}
			let path = std::fs::canonicalize(&path).unwrap_or(path);
			(std::fs::read_to_string(&path), Some(path))
		};
		let code = match code {
			Ok(code) => code,
			Err(error) => {
				eprintln!("error: {error}");
				return ExitCode::FAILURE;
			}
		};
		let base = location
			.as_ref()
			.and_then(|path| path.parent())
			.unwrap_or(cwd);
		let name = location
			.as_ref()
			.map(|path| path.to_string_lossy().into_owned())
			.unwrap_or_else(|| {
				if options.from_args {
					"«string»".to_owned()
				} else {
					"«stdin»".to_owned()
				}
			});
		if lints.preflight(&code, &name, true).is_err() {
			return ExitCode::FAILURE;
		}
		// The store builtins (`derivation`, ...) are globals, so names that use
		// them are bound as in CppNix.
		let builder = Evaluation::builder_pure()
			.experimental_features(options.settings.experimental_features());
		let result = noxi_glue::configure(builder, noxi_glue::ReadOnlyStore::new())
			.mode(EvalMode::Strict)
			.build()
			.compile_only(
				&code,
				Some(location.clone().unwrap_or_else(|| {
					PathBuf::from(if options.from_args {
						"<string>"
					} else {
						"<stdin>"
					})
				})),
			);
		if !result.errors.is_empty() {
			for error in &result.errors {
				options.error_codes.borrow_mut().push(error.code());
				print_evaluation_error(error, &options.settings);
			}
			return ExitCode::FAILURE;
		}
		if writeln!(
			stdout,
			"{}",
			noxi_eval::show_expr(result.expr.as_ref().unwrap(), base)
		)
		.is_err()
		{
			return ExitCode::FAILURE;
		}
	}
	ExitCode::SUCCESS
}

/// Run `nix-instantiate` with already parsed options.
pub fn run(options: &Options, cwd: &Path, stdout: &mut dyn Write) -> ExitCode {
	if options.parse {
		return parse(options, cwd, stdout);
	}
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
			value if options.xml => noxi_eval::builtins::value_to_xml(&mut *stdout, value)
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

/// Selects the codespan renderer for evaluation errors instead of CppNix's
/// format, when set to `codespan`.
pub const DIAGNOSTICS_VAR: &str = "NOXI_DIAGNOSTICS";

/// Print an evaluation error as CppNix does, with function-call frames if
/// the `show-trace` setting is on. `NOXI_DIAGNOSTICS=codespan` selects
/// noxi's annotated source renderer instead.
pub fn print_evaluation_error(error: &noxi_eval::Error, settings: &Settings) {
	if std::env::var_os(DIAGNOSTICS_VAR).is_some_and(|value| value == "codespan") {
		error.fancy_format_stderr();
	} else {
		eprintln!(
			"{}",
			error.cppnix_format(settings.get_bool("show-trace", false))
		);
	}
}

/// Run `nix-instantiate` with the arguments after the program name.
pub fn main(settings: Settings, args: Vec<String>) -> Result<ExitCode, crate::cli::Failure> {
	// Like CppNix, `--version` prints as soon as it is read.
	let mut options = parse_args_with(args, settings, |flag, _| {
		(flag == "--version").then(|| {
			println!("nix-instantiate (Nix) 2.35.2");
			std::process::exit(0)
		})
	})
	.map_err(crate::cli::Failure::Usage)?;
	crate::cli::print_setting_warnings(&mut options.settings);
	if options.find_file {
		return Ok(find_files(&options));
	}
	let cwd = std::env::current_dir()?;
	Ok(run(&options, &cwd, &mut io::stdout().lock()))
}
