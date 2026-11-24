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
//! `--read-write-mode` also allows IFD builds through that owner, and
//! `--dry-run` makes instantiation read-only too.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use nix_compat::nixcpp::settings::Settings;
use nix_compat::store_path::StorePath;
use noxi_eval::builtin_macros::builtins;
use noxi_eval::{Ambiguous, EvalMode, Evaluation, SourceFragment, Value};
use noxi_store::{DerivedPath, Store};
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
	pub flake: Option<crate::nix::flakes::Request>,
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
	/// `nix-instantiate --dry-run`: CppNix's `readOnlyMode`, so derivation
	/// paths are computed without writing them, even with `--read-write-mode`.
	pub dry_run: bool,
	pub strict: bool,
	pub json: bool,
	/// `--xml`; the last of `--raw`, `--json` and `--xml` wins, as in CppNix.
	pub xml: bool,
	/// `--raw`: print the result coerced to a string, without a newline.
	pub raw: bool,
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
	/// Select outputs as the modern commands' installables do, instead of
	/// each derivation's `outputName`.
	pub installables: bool,
	/// `nix-shell`: functions with an `inNixShell` argument get it as `true`.
	pub in_nix_shell: bool,
	/// Source fragments that inputs reference as `@@noxi-frag-N@@`. When set,
	/// the inputs themselves are hidden driver code and only these fragments
	/// are shown as `«string»` sources, as `nix repl` shows each line.
	pub fragments: Vec<String>,
	/// The inputs are generated driver code even without fragments, as a
	/// modern command's `--file` installable is.
	pub generated: bool,
}

impl Options {
	/// Whether the inputs are driver code rather than `«string»` sources.
	fn generated_inputs(&self) -> bool {
		self.generated || !self.fragments.is_empty()
	}
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
			"--raw" => {
				options.raw = true;
				options.json = false;
				options.xml = false;
			}
			"--json" => {
				options.json = true;
				options.xml = false;
				options.raw = false;
			}
			"--xml" => {
				options.xml = true;
				options.json = false;
				options.raw = false;
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
				// `MixEvalArgs`: the last value given for a name wins.
				options.auto_args.retain(|(existing, _)| *existing != name);
				options.auto_args.push((name, auto_arg));
			}
			"--arg-from-file" | "--arg-from-stdin" => {
				let arity = if arg == "--arg-from-file" { 2 } else { 1 };
				let missing = || {
					format!(
						"flag '{arg}' requires {arity} argument{}",
						if arity > 1 { "s" } else { "" }
					)
				};
				let name = args.next().ok_or_else(missing)?;
				let contents = if arg == "--arg-from-file" {
					let path = args.next().ok_or_else(missing)?;
					let path = absolute_path(
						&std::env::current_dir().map_err(|error| error.to_string())?,
						Path::new(&path),
					);
					std::fs::read(&path).map_err(|error| {
						format!(
							"opening file \"{}\": {}",
							path.display(),
							crate::nix::hash::strerror(&error)
						)
					})?
				} else {
					let mut contents = Vec::new();
					io::stdin()
						.read_to_end(&mut contents)
						.map_err(|error| error.to_string())?;
					contents
				};
				options.auto_args.retain(|(existing, _)| *existing != name);
				options.auto_args.push((
					name,
					AutoArg::Str(String::from_utf8_lossy(&contents).into_owned()),
				));
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

/// `flake:` and URL entries are fetched through the selected store and the
/// flake registries, as an evaluation resolves them.
fn lookup_io(options: &Options) -> Box<dyn noxi_eval::EvalIO> {
	let cwd = std::env::current_dir().unwrap_or_default();
	match evaluator_store(options, &cwd, true) {
		Ok((store, _)) => Box::new(noxi_glue::StoreIO::new(store)),
		Err(_) => Box::new(noxi_eval::StdIO),
	}
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
	let io = lookup_io(options);
	if options.settings.get_bool("restrict-eval", false)
		&& let Err(error) = search_path.resolve_entries(io.as_ref())
	{
		eprintln!("{}", error.report());
		return ExitCode::FAILURE;
	}
	let mut stdout = io::stdout().lock();
	for input in &options.inputs {
		match search_path.resolve(&io, input) {
			Ok(path) => {
				let _ = writeln!(stdout, "{}", path.display());
			}
			Err(noxi_eval::LookupError::NotFound(_)) if input.starts_with("nix/") => {
				eprintln!("error: '<{input}>' has no physical path");
				return ExitCode::FAILURE;
			}
			Err(error) => {
				eprintln!("{}", error.report());
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
	// `-` reads standard input, even with `--expr`.
	if input == "-" && !options.generated_inputs() {
		let mut expr = String::new();
		io::stdin().read_to_string(&mut expr)?;
		lints
			.preflight(&expr, "«stdin»", true)
			.map_err(|error| io::Error::other(error.to_string()))?;
		sources.push(("«stdin»".into(), expr));
		return Ok(format!("@@noxi-input-{}@@", sources.len() - 1));
	}
	if options.from_args {
		if options.generated_inputs() {
			let mut code = String::new();
			let mut rest = input;
			while let Some(start) = rest.find("@@noxi-frag-") {
				code.push_str(&rest[..start]);
				rest = &rest[start + "@@noxi-frag-".len()..];
				// `raw-N` splices bindings without the surrounding parentheses.
				let raw = rest.starts_with("raw-");
				if raw {
					rest = &rest[4..];
				}
				let end = rest
					.find("@@")
					.ok_or_else(|| io::Error::other("fragment marker"))?;
				let index: usize = rest[..end]
					.parse()
					.map_err(|_| io::Error::other("fragment marker"))?;
				let fragment = options
					.fragments
					.get(index)
					.ok_or_else(|| io::Error::other("fragment marker"))?;
				lints
					.preflight(fragment, "«string»", true)
					.map_err(|error| io::Error::other(error.to_string()))?;
				let name = if raw { "«string»\0raw" } else { "«string»" };
				sources.push((name.into(), fragment.clone()));
				code.push_str(&format!("@@noxi-input-{}@@", sources.len() - 1));
				rest = &rest[end + 2..];
			}
			code.push_str(rest);
			return Ok(format!("({code}\n)"));
		}
		lints
			.preflight(input, "«string»", true)
			.map_err(|error| io::Error::other(error.to_string()))?;
		sources.push(("«string»".into(), input.to_owned()));
		return Ok(format!("@@noxi-input-{}@@", sources.len() - 1));
	}
	if is_lookup_path(input) {
		// Resolve before `import`, whose frame a failed lookup lacks.
		return Ok(format!("(builtins.seq {input} (import {input}))"));
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
	// `DrvInfo::queryOutputs` for the outputs to install, or for a flake
	// installable only those the derivation names.
	let _ = writeln!(
		driver,
		"__noxi_outputs = d: {};",
		if !options.installables {
			"[ ]"
		} else if options.flake.is_some() {
			"if d.outputSpecified or false then [ d.outputName ] else d.meta.outputsToInstall or [ ]"
		} else {
			"if d.outputSpecified or false then [ d.outputName ] else d.meta.outputsToInstall or (d.outputs or [ \"out\" ])"
		}
	);
	// `trySinglePathToDerivedPaths`.
	let _ = writeln!(
		driver,
		"__noxi_path = v: {};",
		if options.installables {
			"if builtins.isPath v then [ [ \"${v}\" \"\" null ] ]
	else if builtins.isString v then __noxi_stringPath v
	else null"
		} else {
			"null"
		}
	);
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
__noxi_pair = d: let p = __noxi_drvPath d; in builtins.seq p [ p (d.outputName or \"\") (__noxi_outputs d) ];
__noxi_stringPath = s:
	let
		context = builtins.getContext s;
		names = builtins.attrNames context;
		drv = builtins.head names;
		entry = context.${drv};
		path = builtins.unsafeDiscardStringContext s;
		count = builtins.foldl' (n: name: n + (if context.${name} ? path then 1 else 0) + (if context.${name} ? allOutputs then 1 else 0) + builtins.length (context.${name}.outputs or [ ])) 0 names;
	in
	if count != 1 then throw \"string '${path}' has ${toString count} entries in its context. It should only have exactly one entry\"
	else if entry ? allOutputs then throw \"string '${path}' has a context which refers to a complete source and binary closure. This is not supported at this time\"
	else if entry ? path then
		if drv == path then [ [ path \"\" null ] ]
		else throw \"path string '${path}' has context with the different path '${drv}'\"
	else [ [ drv (builtins.head entry.outputs) entry.outputs ] ];
__noxi_installable = v:
	let path = __noxi_path (__noxi_autoCall v); in
	if path != null then path else __noxi_getDerivations v;
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
				selection = format!(
					"(__noxi_select_step {} {} (__noxi_autoCall {selection}))",
					nix_string(attr_path),
					nix_string(&part)
				);
			}
			if !options.auto_args.is_empty() {
				selection = format!("(__noxi_autoCall {selection})");
			}
			if !options.eval {
				selection = format!("(__noxi_forceEach (__noxi_installable {selection}))");
			} else if options.json {
				selection = format!("(builtins.toJSON {selection})");
			} else if options.raw {
				selection = format!("(__noxi_raw_output \"nix-instantiate\" {selection})");
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
		let (name, raw) = match name.strip_suffix("\0raw") {
			Some(name) => (name.to_owned(), true),
			None => (name.clone(), false),
		};
		if !raw {
			code.push('(');
		}
		let start = code.len();
		code.push_str(input);
		fragments.push(SourceFragment {
			range: start..code.len(),
			name,
			code: input.clone(),
		});
		code.push_str(if raw { "\n" } else { "\n)" });
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

	/// One step of CppNix's `findAlongAttrPath`: select `name` from `value`,
	/// which the caller has auto-called, reporting failures against the
	/// whole selection `path`.
	#[builtin("select_step")]
	async fn builtin_select_step(
		co: GenCo,
		path: Value,
		name: Value,
		value: Value,
	) -> Result<Value, ErrorKind> {
		let _ = &co;
		let path = String::from_utf8_lossy(path.to_contextful_str()?.as_bytes()).into_owned();
		let name = String::from_utf8_lossy(name.to_contextful_str()?.as_bytes()).into_owned();
		let shown = noxi_eval::diagnostics::show_type(&value);
		// `string2Int<unsigned int>`: decimal digits that fit.
		let index = (!name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()))
			.then(|| name.parse::<u32>().ok())
			.flatten();
		match index {
			None => {
				let Value::Attrs(attrs) = value else {
					return Err(ErrorKind::EvalError(format!(
						"the expression selected by the selection path '{path}' should be a set but is {shown}"
					)));
				};
				if name.is_empty() {
					return Err(ErrorKind::EvalError(format!(
						"empty attribute name in selection path '{path}'"
					)));
				}
				if let Some(found) = attrs.select(name.as_str()) {
					return Ok(found.clone());
				}
				let names: Vec<String> = attrs
					.keys()
					.map(|key| String::from_utf8_lossy(key.as_bytes()).into_owned())
					.collect();
				let suggestions = crate::suggestions::Suggestions::best_matches(
					names.iter().map(String::as_str),
					&name,
				);
				Err(ErrorKind::EvalError(suggestions.append_to(format!(
					"attribute '{name}' in selection path '{path}' not found"
				))))
			}
			Some(index) => {
				let Value::List(list) = value else {
					return Err(ErrorKind::EvalError(format!(
						"the expression selected by the selection path '{path}' should be a list but is {shown}"
					)));
				};
				list.get(index as usize).cloned().ok_or_else(|| {
					ErrorKind::EvalError(format!(
						"list index {index} in selection path '{path}' is out of range"
					))
				})
			}
		}
	}

	/// `coerceToString` of a command's raw output, with the command's
	/// `while generating the ... output` frame: `"eval"` for `nix eval`,
	/// anything else for `nix-instantiate`.
	#[builtin("raw_output")]
	async fn builtin_raw_output(
		co: GenCo,
		command: Value,
		value: Value,
	) -> Result<Value, ErrorKind> {
		use noxi_eval::CoercionKind;
		let context = if command.to_contextful_str()?.as_bytes() == b"eval" {
			"while generating the eval command output"
		} else {
			"while generating the nix-instantiate output"
		};
		match noxi_eval::generators::request_string_coerce_in(
			&co,
			value,
			CoercionKind {
				strong: false,
				import_paths: true,
			},
			Some(context),
		)
		.await
		{
			Ok(string) => Ok(Value::String(string)),
			Err(caught) => Ok(Value::Catchable(Box::new(caught))),
		}
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
	// A syntax error in an expression given on the command line is reported
	// for that expression alone, as CppNix parses it, not for the generated
	// code around it.
	// Fragments spliced as bindings (`raw`) are not expressions on their own.
	let raw: Vec<String> = options
		.inputs
		.iter()
		.flat_map(|input| input.split("@@noxi-frag-raw-").skip(1))
		.filter_map(|rest| rest.split_once("@@").map(|(index, _)| index.to_owned()))
		.collect();
	let sources: Vec<&String> = if options.generated_inputs() {
		options
			.fragments
			.iter()
			.enumerate()
			.filter(|(index, _)| !raw.contains(&index.to_string()))
			.map(|(_, code)| code)
			.collect()
	} else if options.from_args {
		options
			.inputs
			.iter()
			.filter(|input| *input != "-")
			.collect()
	} else {
		Vec::new()
	};
	for code in sources {
		if rnix::Root::parse(code).errors().is_empty() {
			continue;
		}
		let result = noxi_eval::Evaluation::builder_pure()
			.build()
			.compile_only(code, None);
		if let Some(error) = result.errors.first() {
			options.error_codes.borrow_mut().push(error.code());
			print_evaluation_error(error, &options.settings);
			return Err(ExitCode::FAILURE);
		}
	}
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

	let writable = !options.dry_run && (!options.eval || options.read_write_mode);
	let (evaluator_store, store) = evaluator_store(options, cwd, writable).map_err(|e| fail(&e))?;
	let mut env: FxHashMap<SmolStr, Value> = driver_builtins::builtins()
		.into_iter()
		.map(|(name, value)| (SmolStr::new(format!("__noxi_{name}")), value))
		.collect();
	if let Some(request) = &options.flake {
		env.extend(
			crate::nix::flakes::builtins(request.clone(), evaluator_store.clone())
				.into_iter()
				.map(|(name, value)| (SmolStr::new(format!("__noxi_{name}")), value)),
		);
	}
	let mode = if !options.eval || options.strict || options.json || options.xml {
		EvalMode::Strict
	} else {
		EvalMode::Lazy
	};
	crate::nix::repl::set_max_print_depth(
		options.settings.get_int("max-call-depth", 10000).max(0) as usize
	);
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
	let builder = noxi_glue::configure(builder, evaluator_store.clone())
		.env(Some(&env))
		.mode(mode);
	// `EvalSettings::getCurrentSystem`: `eval-system`, else `system`.
	let current_system = options
		.settings
		.get("eval-system")
		.filter(|system| !system.is_empty())
		.or_else(|| options.settings.get("system"))
		.map(str::to_owned);
	let builder = match current_system {
		Some(system) => builder.add_builtins([("currentSystem", Value::from(system))]),
		None => builder,
	};
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
			evaluator_store.clone(),
		)
	} else {
		let evaluation = builder.build();
		if options.settings.get_bool("restrict-eval", false)
			&& let Err(error) = evaluation.resolve_search_path()
		{
			eprintln!("{}", error.report());
			return Err(ExitCode::FAILURE);
		}
		evaluation
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
			// The flake adapter is command plumbing, not a Nix source call.
			let command_error = options
				.flake
				.as_ref()
				.map(|_| crate::nix::flakes::command_error(error));
			print_evaluation_error(command_error.as_ref().unwrap_or(error), &options.settings);
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

/// What each input of an evaluation with [`Options::installables`] selects,
/// `toDerivedPaths`: an input's derivations in path order with the outputs to
/// install, or one store path. `specs` holds the outputs requested after `^`.
pub fn installables(
	results: &[Value],
	specs: &[Option<BTreeSet<String>>],
) -> Result<Vec<DerivedPath>, String> {
	let mut paths = Vec::new();
	for (index, result) in results.iter().enumerate() {
		let spec = specs.get(index).and_then(Option::as_ref);
		let mut by_drv: BTreeMap<StorePath, BTreeSet<String>> = BTreeMap::new();
		let entries = result.to_list().map_err(|e| e.to_string())?;
		for entry in entries.iter() {
			let entry = forced(entry).to_list().map_err(|e| e.to_string())?;
			let text = |index: usize| {
				entry
					.get(index)
					.ok_or("missing installable field".to_owned())
					.and_then(|value| text_of(value))
			};
			let path = text(0)?;
			let path = StorePath::from_absolute_path(path.as_bytes())
				.map_err(|_| format!("invalid store path '{path}'"))?;
			let names = entry.get(2).map(forced);
			let Some(Value::List(names)) = names else {
				paths.push(DerivedPath::Opaque(path));
				continue;
			};
			let outputs = by_drv.entry(path).or_default();
			match spec {
				// An empty set already means all of them.
				Some(spec) => outputs.extend(spec.iter().cloned()),
				None => {
					for name in names.iter() {
						outputs.insert(text_of(name)?);
					}
					if outputs.is_empty() {
						outputs.insert("out".to_owned());
					}
				}
			}
		}
		paths.extend(
			by_drv
				.into_iter()
				.map(|(drv, outputs)| DerivedPath::Built { drv, outputs }),
		);
	}
	Ok(paths)
}

fn text_of(value: &Value) -> Result<String, String> {
	let text = forced(value)
		.to_contextful_str()
		.map_err(|e| e.to_string())?;
	String::from_utf8(text.as_bytes().to_vec()).map_err(|e| e.to_string())
}

/// `ExtendedOutputsSpec::parse`: an installable without its `^` suffix, and
/// the outputs the suffix names; the empty set stands for `^*`.
pub fn extended_outputs_spec(
	installable: &str,
) -> Result<(&str, Option<BTreeSet<String>>), String> {
	let Some((prefix, spec)) = installable.split_once('^') else {
		return Ok((installable, None));
	};
	if spec == "*" {
		return Ok((prefix, Some(BTreeSet::new())));
	}
	let valid = |name: &str| {
		!name.is_empty()
			&& name
				.bytes()
				.all(|b| b.is_ascii_alphanumeric() || b"+-._?=".contains(&b))
	};
	if !spec.split(',').all(valid) {
		return Err(format!(
			"invalid extended outputs specifier '{installable}'"
		));
	}
	Ok((prefix, Some(spec.split(',').map(str::to_owned).collect())))
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
		let (code, location) = if input == "-" {
			let mut code = String::new();
			(io::stdin().read_to_string(&mut code).map(|_| code), None)
		} else if options.from_args {
			(Ok(input.clone()), None)
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
				let io = lookup_io(options);
				match lookup.resolve(&io, &input[1..input.len() - 1]) {
					Ok(path) => path,
					Err(error) => {
						eprintln!("{}", error.report());
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
				if options.from_args && input != "-" {
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
					PathBuf::from(if options.from_args && input != "-" {
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
			// CppNix prints raw output without a newline.
			Value::String(s) if options.raw => stdout.write_all(s.as_bytes()),
			value => stdout
				.write_all(&Ambiguous(value).to_bytes())
				.and_then(|()| stdout.write_all(b"\n")),
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
		{
			use std::io::Write;
			let mut stderr = std::io::stderr().lock();
			let formatted = error.cppnix_format_bytes(settings.get_bool("show-trace", false));
			let _ = stderr.write_all(&physical_positions(formatted));
			let _ = stderr.write_all(b"\n");
		}
		if matches!(
			&error.innermost().kind,
			noxi_eval::ErrorKind::UnknownHashType(_)
		) {
			let program = std::env::args()
				.next()
				.unwrap_or_else(|| "nix-instantiate".into());
			eprintln!("Try '{program} --help' for more information.");
		}
	}
}

/// The diverted store's real directory, when the evaluation's store has one.
static REAL_STORE_DIR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// CppNix shows a source position in a diverted store at its physical path,
/// the path its store accessor reads. The evaluator names files by their
/// logical path, which relative path literals depend on, so the positions
/// are rewritten here.
fn physical_positions(formatted: Vec<u8>) -> Vec<u8> {
	let Some(real) = REAL_STORE_DIR.lock().ok().and_then(|dir| dir.clone()) else {
		return formatted;
	};
	let logical = format!("{}/", nix_compat::store_path::store_dir());
	let mut out = Vec::with_capacity(formatted.len());
	for line in formatted.split_inclusive(|b| *b == b'\n') {
		let indent = line.iter().take_while(|b| **b == b' ').count();
		let rest = &line[indent..];
		match rest.strip_prefix(format!("at {logical}").as_bytes()) {
			Some(tail) => {
				out.extend_from_slice(&line[..indent]);
				out.extend_from_slice(format!("at {real}/").as_bytes());
				out.extend_from_slice(tail);
			}
			None => out.extend_from_slice(line),
		}
	}
	out
}

/// Run `nix-instantiate` with the arguments after the program name.
pub fn main(settings: Settings, args: Vec<String>) -> Result<ExitCode, crate::cli::Failure> {
	// Like CppNix, `--version` prints as soon as it is read.
	let mut dry_run = false;
	let mut options = parse_args_with(args, settings, |flag, _| match flag {
		"--version" => {
			println!("nix-instantiate (Nix) 2.35.2");
			std::process::exit(0)
		}
		"--help" => crate::cli::show_man_page_or_exit("nix-instantiate"),
		"--dry-run" => {
			dry_run = true;
			Some(Ok(()))
		}
		_ => None,
	})
	.map_err(crate::cli::Failure::Usage)?;
	options.dry_run = dry_run;
	crate::cli::print_setting_warnings(&mut options.settings);
	if options.find_file {
		return Ok(find_files(&options));
	}
	let cwd = std::env::current_dir()?;
	Ok(run(&options, &cwd, &mut io::stdout().lock()))
}

/// The evaluator store and the store it registers paths in, with fetching
/// configured from the settings, as an evaluation and `--find-file` use them.
fn evaluator_store(
	options: &Options,
	cwd: &Path,
	writable: bool,
) -> Result<(Rc<noxi_glue::EvaluatorStore>, Option<Rc<dyn Store>>), String> {
	let uri = store::store_uri(options.store.as_deref(), &options.settings);
	let opened = store::open(&uri, &options.settings)?;
	if let Ok(mut dir) = REAL_STORE_DIR.lock() {
		*dir = opened
			.as_ref()
			.and_then(|opened| opened.real_store_dir.as_ref())
			.filter(|dir| dir.as_path() != Path::new(nix_compat::store_path::store_dir()))
			.map(|dir| dir.display().to_string());
	}
	let (evaluator_store, store) = match opened {
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
	// Pure and restricted evaluation see only the paths CppNix's `allowPath`
	// admits. Modern pure evaluation has no search path, so it starts with
	// none and admits what the evaluation fetches, copies, writes or builds.
	if options.modern_pure {
		evaluator_store.restrict_access(true, Vec::new());
	} else if options.settings.get_bool("pure-eval", false)
		|| options.settings.get_bool("restrict-eval", false)
	{
		let roots = search_path(options)
			.into_iter()
			.filter_map(|entry| {
				let entry = noxi_eval::NixSearchPathEntry::parse(&entry);
				if entry.path.contains("://") || entry.path.starts_with("flake:") {
					None
				} else {
					let path = PathBuf::from(entry.path);
					Some(if path.is_absolute() {
						path
					} else {
						cwd.join(path)
					})
				}
			})
			.collect();
		evaluator_store.restrict_access(options.settings.get_bool("pure-eval", false), roots);
	}
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
		restrict_eval: options.settings.get_bool("restrict-eval", false),
		allowed_uris: options
			.settings
			.get_list("allowed-uris", &[])
			.into_iter()
			.map(str::to_owned)
			.collect(),
		override_flakes: options.override_flakes.clone(),
		cwd: cwd.to_owned(),
		..Default::default()
	};
	fetch.settings.flakes = options.settings.experimental_features().contains("flakes");
	fetch.settings.allow_dirty = options.settings.get_bool("allow-dirty", true);
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
	Ok((evaluator_store, store))
}
