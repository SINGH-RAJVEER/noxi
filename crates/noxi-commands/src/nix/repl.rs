//! `nix repl`, following CppNix 2.35.2 `libcmd/repl.cc`, `repl-interacter.cc`,
//! `nix/repl.cc` and `libexpr/print.cc`.
//!
//! The session is a stack of `let` layers in front of each new line. Every
//! line the user typed is its own `«string»` source fragment, so diagnostics
//! name the positions CppNix names. Values are printed by a builtin that
//! forces them lazily inside the evaluation, as CppNix's printer does.
//! Interactive line editing is not provided: on a terminal the prompt is
//! printed and lines are read as typed.
use std::future::Future;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::pin::Pin;
use std::process::ExitCode;

use bstr::ByteSlice;
use noxi_eval::{
	CatchableErrorKind, ErrorKind, NixAttrs, PrintOptions, Value,
	generators::{self, GenCo},
};

use super::Parsed;
use crate::cli::Failure;
use crate::instantiate::{self, Options, nix_string_literal};

const CYAN: &str = "\x1b[36;1m";
const MAGENTA: &str = "\x1b[35;1m";
const GREEN: &str = "\x1b[32;1m";
const BLUE: &str = "\x1b[34;1m";
const RED: &str = "\x1b[31;1m";
const NORMAL: &str = "\x1b[0m";

type LocalFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ErrorKind>> + 'a>>;

fn notice(message: &str) {
	if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Notice) {
		eprintln!("{message}");
	}
}

fn print_error(message: &str) {
	if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Error) {
		eprintln!("error: {message}");
	}
}

/// `isVarName`: printed without quotes.
fn is_var_name(name: &str) -> bool {
	let mut chars = name.chars();
	let first_ok = chars
		.next()
		.is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
	first_ok
		&& chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '\''))
		&& !matches!(
			name,
			"if" | "then" | "else" | "assert" | "with" | "let" | "in" | "rec" | "inherit"
		)
}

/// `printLiteralString`.
fn literal(s: &str, colors: bool) -> String {
	let mut out = String::new();
	if colors {
		out.push_str(MAGENTA);
	}
	out.push('"');
	let mut chars = s.chars().peekable();
	while let Some(c) = chars.next() {
		match c {
			'"' | '\\' => {
				out.push('\\');
				out.push(c);
			}
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			'$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
			c => out.push(c),
		}
	}
	out.push('"');
	if colors {
		out.push_str(NORMAL);
	}
	out
}

/// `printAttributeName`.
fn attribute_name(name: &str) -> String {
	if is_var_name(name) {
		name.to_owned()
	} else {
		literal(name, false)
	}
}

fn caught_message(caught: &CatchableErrorKind) -> String {
	match caught.untraced() {
		CatchableErrorKind::Throw(message) => message.to_str_lossy().into_owned(),
		other => other.to_string(),
	}
}

/// Standard output written as it is produced, as CppNix's printer streams
/// to `std::cout`: a value that fails part-way leaves its prefix printed.
struct Out {
	/// Collected output instead of standard output, for `nix eval`.
	buffer: Option<String>,
	/// Without colours, as `nix eval` prints.
	plain: bool,
}
impl Out {
	fn push(&mut self, c: char) {
		self.push_str(c.encode_utf8(&mut [0; 4]));
	}
	fn push_str(&mut self, s: &str) {
		let text = if self.plain {
			String::from_utf8(nix_compat::log::filter_ansi_escapes(s.as_bytes(), true))
				.unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
		} else {
			s.to_owned()
		};
		match &mut self.buffer {
			Some(buffer) => buffer.push_str(&text),
			None => print!("{text}"),
		}
	}
}

struct Printer {
	out: Out,
	indent: String,
	max_depth: usize,
	/// Multi-line sets and lists, as the REPL prints them.
	pretty: bool,
	/// A failing top-level value is an error rather than `«error: ...»`.
	throw_top: bool,
}

impl Printer {
	fn space(&mut self, pretty: bool) {
		if pretty {
			self.out.push('\n');
			let indent = self.indent.clone();
			self.out.push_str(&indent);
		} else {
			self.out.push(' ');
		}
	}

	/// Whether a one-element set or list is printed over several lines.
	async fn nested(co: &GenCo, value: Value) -> Result<Result<bool, Value>, ErrorKind> {
		let value = generators::request_force(co, value).await;
		if value.is_catchable() {
			return Ok(Err(value));
		}
		Ok(Ok(matches!(value, Value::List(_) | Value::Attrs(_))))
	}

	async fn derivation(co: &GenCo, attrs: &NixAttrs) -> Result<Result<bool, Value>, ErrorKind> {
		let Some(kind) = attrs.select("type") else {
			return Ok(Ok(false));
		};
		let kind = generators::request_force(co, kind.clone()).await;
		if kind.is_catchable() {
			return Ok(Err(kind));
		}
		Ok(Ok(
			matches!(&kind, Value::String(s) if s.as_bytes() == b"derivation"),
		))
	}

	fn print<'a>(
		&'a mut self,
		co: &'a GenCo,
		value: Value,
		depth: usize,
	) -> LocalFuture<'a, Result<(), Value>> {
		Box::pin(async move {
			let value = generators::request_force(co, value).await;
			if let Value::Catchable(caught) = &value {
				if depth == 0 && self.throw_top {
					return Ok(Err(value));
				}
				let message = caught_message(caught);
				self.out
					.push_str(&format!("{RED}«error: {message}»{NORMAL}"));
				return Ok(Ok(()));
			}
			match self.print_forced(co, value, depth).await? {
				Err(caught) if depth > 0 || !self.throw_top => {
					let Value::Catchable(inner) = &caught else {
						unreachable!("caught values are catchable")
					};
					let message = caught_message(inner);
					self.out
						.push_str(&format!("{RED}«error: {message}»{NORMAL}"));
					Ok(Ok(()))
				}
				result => Ok(result),
			}
		})
	}

	async fn print_forced(
		&mut self,
		co: &GenCo,
		value: Value,
		depth: usize,
	) -> Result<Result<(), Value>, ErrorKind> {
		match &value {
			Value::Integer(i) => self.out.push_str(&format!("{CYAN}{i}{NORMAL}")),
			Value::Float(_) => {
				let text =
					generators::request_print(co, value.clone(), PrintOptions::UNLIMITED).await;
				self.out
					.push_str(&format!("{CYAN}{}{NORMAL}", text.to_str_lossy()));
			}
			Value::Bool(b) => self.out.push_str(&format!("{CYAN}{b}{NORMAL}")),
			Value::Null => self.out.push_str(&format!("{CYAN}null{NORMAL}")),
			Value::String(s) => self.out.push_str(&literal(&s.to_str_lossy(), true)),
			Value::Path(p) => self
				.out
				.push_str(&format!("{GREEN}{}{NORMAL}", p.display())),
			Value::Attrs(attrs) => {
				match Self::derivation(co, attrs).await? {
					Err(caught) => return Ok(Err(caught)),
					Ok(true) => {
						let mut text = format!("{GREEN}«derivation");
						if let Some(path) = attrs.select("drvPath") {
							let path = generators::request_force(co, path.clone()).await;
							if path.is_catchable() {
								return Ok(Err(path));
							}
							text.push(' ');
							text.push_str(&path.to_contextful_str()?.to_str_lossy());
						}
						text.push_str(&format!("»{NORMAL}"));
						self.out.push_str(&text);
						return Ok(Ok(()));
					}
					Ok(false) => {}
				}
				if depth >= self.max_depth {
					self.out.push_str("{ ... }");
					return Ok(Ok(()));
				}
				self.indent.push_str("  ");
				self.out.push('{');
				let items: Vec<(String, Value)> = attrs
					.iter_sorted()
					.map(|(name, value)| (name.to_str_lossy().into_owned(), value.clone()))
					.collect();
				let pretty = self.pretty
					&& match items.as_slice() {
						[] => false,
						[(_, only)] => match Self::nested(co, only.clone()).await? {
							Ok(nested) => nested,
							Err(caught) => return Ok(Err(caught)),
						},
						_ => true,
					};
				for (name, child) in items {
					self.space(pretty);
					self.out.push_str(&attribute_name(&name));
					self.out.push_str(" = ");
					if let Err(caught) = self.print(co, child, depth + 1).await? {
						return Ok(Err(caught));
					}
					self.out.push(';');
				}
				self.indent.truncate(self.indent.len() - 2);
				self.space(pretty);
				self.out.push('}');
			}
			Value::List(list) => {
				if depth >= self.max_depth {
					self.out.push_str("[ ... ]");
					return Ok(Ok(()));
				}
				self.indent.push_str("  ");
				self.out.push('[');
				let items: Vec<Value> = list.iter().cloned().collect();
				let pretty = self.pretty
					&& match items.as_slice() {
						[] => false,
						[only] => match Self::nested(co, only.clone()).await? {
							Ok(nested) => nested,
							Err(caught) => return Ok(Err(caught)),
						},
						_ => true,
					};
				for child in items {
					self.space(pretty);
					if let Err(caught) = self.print(co, child, depth + 1).await? {
						return Ok(Err(caught));
					}
				}
				self.indent.truncate(self.indent.len() - 2);
				self.space(pretty);
				self.out.push(']');
			}
			Value::Closure(_) | Value::Builtin(_) => {
				let text =
					generators::request_print(co, value.clone(), PrintOptions::UNLIMITED).await;
				self.out
					.push_str(&format!("{BLUE}{}{NORMAL}", text.to_str_lossy()));
			}
			_ => self.out.push_str(&format!("{RED}«unknown»{NORMAL}")),
		}
		Ok(Ok(()))
	}
}

/// `printValue` with the REPL's options: colours, forcing, derivation paths,
/// two-space pretty printing and errors thrown only at the top level.
pub async fn print_builtin(co: GenCo, value: Value, depth: Value) -> Result<Value, ErrorKind> {
	let max_depth = match depth {
		Value::Integer(depth) if depth >= 0 => depth as usize,
		_ => usize::MAX,
	};
	let value = generators::request_force(&co, value).await;
	// `:p` prints a string without quoting or escaping.
	if max_depth == usize::MAX {
		if let Value::String(s) = &value {
			print!("{}", s.to_str_lossy());
			return Ok(Value::Null);
		}
	}
	let mut printer = Printer {
		out: Out {
			buffer: None,
			plain: false,
		},
		indent: String::new(),
		max_depth,
		pretty: true,
		throw_top: true,
	};
	match printer.print(&co, value, 0).await? {
		Err(caught) => Ok(caught),
		Ok(()) => Ok(Value::Null),
	}
}

/// `showType`.
pub async fn type_builtin(co: GenCo, value: Value) -> Result<Value, ErrorKind> {
	let value = generators::request_force(&co, value).await;
	let text = match &value {
		Value::Catchable(_) => return Ok(value),
		Value::Integer(_) => "an integer".to_owned(),
		Value::Bool(_) => "a Boolean".to_owned(),
		Value::String(s) if s.has_context() => "a string with context".to_owned(),
		Value::String(_) => "a string".to_owned(),
		Value::Path(_) => "a path".to_owned(),
		Value::Null => "null".to_owned(),
		Value::Attrs(_) => "a set".to_owned(),
		Value::List(_) => "a list".to_owned(),
		Value::Float(_) => "a float".to_owned(),
		Value::Closure(_) => "a function".to_owned(),
		Value::Builtin(builtin) => {
			let shown =
				generators::request_print(&co, value.clone(), PrintOptions::UNLIMITED).await;
			if shown.to_str_lossy().starts_with("«partially applied") {
				format!(
					"the partially applied built-in function '{}'",
					builtin.name()
				)
			} else {
				format!("the built-in function '{}'", builtin.name())
			}
		}
		_ => "a thunk".to_owned(),
	};
	Ok(Value::from(text))
}

/// The names of a set added to the scope, in lexicographic order.
pub async fn names_builtin(co: GenCo, value: Value) -> Result<Value, ErrorKind> {
	let value = generators::request_force(&co, value).await;
	if value.is_catchable() {
		return Ok(value);
	}
	let attrs = value.to_attrs()?;
	let names: Vec<Value> = attrs
		.iter_sorted()
		.map(|(name, _)| Value::from(name.to_str_lossy().into_owned()))
		.collect();
	Ok(Value::List(names.into()))
}

/// The REPL's scope: hidden `let` layers over the user's fragments.
struct Session {
	options: Options,
	layers: Vec<String>,
	installables: Vec<(String, String)>,
	files: Vec<String>,
	flakes: Vec<String>,
	last_loaded: Option<Vec<String>>,
	/// Fragments of the base options (the installables' own sources).
	base_fragments: usize,
}

enum Outcome {
	Value(Value),
	Failed,
}

impl Session {
	fn fragment(&mut self, text: &str) -> String {
		self.options.fragments.push(text.to_owned());
		format!("@@noxi-frag-{}@@", self.options.fragments.len() - 1)
	}

	fn evaluate(&self, body: &str) -> Result<Outcome, Failure> {
		let mut options = self.options.clone();
		options.inputs = vec![format!("{} ({body})", self.layers.join(" "))];
		match instantiate::evaluate(&options, &std::env::current_dir()?) {
			Ok(evaluated) => Ok(Outcome::Value(
				evaluated.results.into_iter().next().unwrap_or(Value::Null),
			)),
			Err(_) => Ok(Outcome::Failed),
		}
	}

	fn text(value: &Value) -> String {
		match value {
			Value::String(s) => s.to_str_lossy().into_owned(),
			_ => String::new(),
		}
	}

	/// `addAttrsToScope` for the hidden code `code`.
	fn add_attrs(&mut self, code: &str) -> Result<bool, Failure> {
		let Outcome::Value(Value::List(names)) =
			self.evaluate(&format!("__noxi_repl_names ({code})"))?
		else {
			return Ok(false);
		};
		let names: Vec<String> = names.iter().map(Self::text).collect();
		let index = self.layers.len();
		let mut layer = format!("let __noxi_repl_{index} = ({code}); in let");
		let mut bound = false;
		for name in &names {
			if is_var_name(name) {
				layer.push_str(&format!(
					" {name} = __noxi_repl_get __noxi_repl_{index} \"{name}\";"
				));
				bound = true;
			}
		}
		if bound {
			layer.push_str(" in");
		} else {
			layer = format!("let __noxi_repl_{index} = ({code}); in");
		}
		self.layers.push(layer);
		notice(&format!("Added {} variables.", names.len()));
		let shown: Vec<String> = names.iter().take(20).map(|n| attribute_name(n)).collect();
		notice(&shown.join(", "));
		if names.len() > 20 {
			notice(&format!("... and {} more; view with :ll", names.len() - 20));
		}
		self.last_loaded = Some(names);
		Ok(true)
	}

	fn load_file(&mut self, path: &str) -> Result<bool, Failure> {
		let target = if instantiate::is_lookup_path(path) {
			path.to_owned()
		} else {
			nix_string_literal(
				&instantiate::absolute_path(&std::env::current_dir()?, Path::new(path))
					.to_string_lossy(),
			)
		};
		self.add_attrs(&format!("__noxi_autoCall (import {target})"))
	}

	fn load_flake(&mut self, reference: &str) -> Result<bool, Failure> {
		if reference.is_empty() {
			print_error(
				"cannot use ':load-flake' without a path specified. (Use '.' for the current working directory.)",
			);
			return Ok(false);
		}
		self.add_attrs(&format!("__noxi_flake {}", nix_string_literal(reference)))
	}

	/// `loadFiles` and `loadFlakes` after `initEnv`.
	fn reload(&mut self, startup: bool) -> Result<(), Failure> {
		self.layers.clear();
		self.options.fragments.truncate(self.base_fragments);
		for file in std::mem::take(&mut self.files) {
			notice(&format!(
				"Loading {}...",
				super::flake_init::quoted(Path::new(&file))
			));
			let _ = self.load_file(&file)?;
			self.files.push(file);
		}
		for (code, what) in self.installables.clone() {
			notice(&format!("Loading installable '{what}'..."));
			self.add_attrs(&code)?;
		}
		if !startup {
			for flake in std::mem::take(&mut self.flakes) {
				notice(&format!("Loading flake '{flake}'..."));
				let _ = self.load_flake(&flake)?;
				self.flakes.push(flake);
			}
		}
		Ok(())
	}
}

/// Whether rnix reports only a premature end of input.
fn incomplete(code: &str) -> bool {
	let parsed = rnix::Root::parse(code);
	!parsed.errors().is_empty()
		&& parsed.errors().iter().all(|error| {
			matches!(
				error,
				rnix::ParseError::UnexpectedEOF | rnix::ParseError::UnexpectedEOFWanted(_)
			)
		})
}

fn parses(code: &str) -> bool {
	rnix::Root::parse(code).errors().is_empty()
}

const HELP: &str = "The following commands are available:

  <expr>                       Evaluate and print expression
  <x> = <expr>                 Bind expression to variable
  :a, :add <expr>              Add attributes from resulting set to scope
  :b <expr>                    Build a derivation
  :bl <expr>                   Build a derivation, creating GC roots in the
                               working directory
  :e, :edit <expr>             Open package or function in $EDITOR
  :i <expr>                    Build derivation, then install result into
                               current profile
  :l, :load <path>             Load Nix expression and add it to scope
  :lf, :load-flake <ref>       Load Nix flake and add it to scope
  :ll, :last-loaded            Show most recently loaded variables added to scope
  :p, :print <expr>            Evaluate and print expression recursively
                               Strings are printed directly, without escaping.
  :q, :quit                    Exit nix-repl
  :r, :reload                  Reload all files
  :sh <expr>                   Build dependencies of derivation, then start
                               nix-shell
  :t <expr>                    Describe result of evaluation
  :u <expr>                    Build derivation, then start nix-shell
  :doc <expr>                  Show documentation of a builtin function
  :log <expr>                  Show logs for a derivation
  :te, :trace-enable [bool]    Enable, disable or toggle showing traces for
                               errors
  :?, :help                    Brings up this help menu
";

enum Line {
	Prompt,
	Incomplete,
	Quit,
}

fn process(session: &mut Session, input: &str) -> Result<Line, Failure> {
	let line = input.trim();
	if line.is_empty() {
		return Ok(Line::Prompt);
	}
	let (command, arg) = if line.starts_with(':') {
		match line.find([' ', '\n', '\r', '\t']) {
			Some(p) => (&line[..p], line[p..].trim()),
			None => (line, ""),
		}
	} else {
		("", line)
	};
	let evaluating = matches!(command, "" | ":a" | ":add" | ":t" | ":p" | ":print");
	if evaluating && incomplete(arg) {
		return Ok(Line::Incomplete);
	}
	match command {
		":?" | ":help" => print!("{HELP}"),
		":q" | ":quit" => return Ok(Line::Quit),
		":a" | ":add" => {
			let fragment = session.fragment(arg);
			session.add_attrs(&fragment)?;
		}
		":l" | ":load" => {
			if session.load_file(arg)? {
				session.files.retain(|f| f != arg);
				session.files.push(arg.to_owned());
			}
		}
		":lf" | ":load-flake" => {
			if session.load_flake(arg)? {
				session.flakes.retain(|f| f != arg);
				session.flakes.push(arg.to_owned());
			}
		}
		":ll" | ":last-loaded" => match &session.last_loaded {
			Some(names) => {
				for name in names {
					println!("{name}");
				}
			}
			None => print_error("nothing has been loaded yet"),
		},
		":r" | ":reload" => session.reload(false)?,
		":t" => {
			let fragment = session.fragment(arg);
			if let Outcome::Value(value) =
				session.evaluate(&format!("__noxi_repl_type {fragment}"))?
			{
				println!("{}", Session::text(&value));
			}
		}
		":p" | ":print" => {
			let fragment = session.fragment(arg);
			if let Outcome::Value(value) =
				session.evaluate(&format!("__noxi_repl_print {fragment} (-1)"))?
			{
				let _ = value;
				println!();
			}
		}
		":te" | ":trace-enable" => {
			let showing = session.options.settings.get_bool("show-trace", false);
			let enable = match arg {
				"false" => false,
				"true" => true,
				"" => !showing,
				_ => {
					print_error(&format!("unexpected argument '{arg}' to {command}"));
					return Ok(Line::Prompt);
				}
			};
			println!(
				"{}",
				if enable {
					"showing error traces"
				} else {
					"not showing error traces"
				}
			);
			let _ = session
				.options
				.settings
				.set("show-trace", if enable { "true" } else { "false" });
		}
		":b" | ":bl" | ":i" | ":sh" | ":log" | ":u" | ":e" | ":edit" | ":doc" => {
			print_error(&format!("the noxi repl does not support '{command}' yet"));
		}
		"" => {
			// `parseReplBindings`: bindings, or bindings with a final `;`.
			let bindings = if parses(&format!("{{ {line}\n}}")) {
				Some("")
			} else if parses(&format!("{{ {line}\n; }}")) {
				Some(";")
			} else {
				None
			};
			match bindings {
				Some(suffix) => {
					let fragment = session
						.fragment(line)
						.replace("@@noxi-frag-", "@@noxi-frag-raw-");
					let set = format!("{{ {fragment}{suffix} }}");
					let Outcome::Value(Value::List(names)) =
						session.evaluate(&format!("__noxi_repl_names {set}"))?
					else {
						return Ok(Line::Prompt);
					};
					let index = session.layers.len();
					let mut layer = format!("let __noxi_repl_{index} = {set}; in let");
					for name in names.iter().map(Session::text) {
						if is_var_name(&name) {
							layer.push_str(&format!(
								" {name} = __noxi_repl_get __noxi_repl_{index} \"{name}\";"
							));
						}
					}
					layer.push_str(" __noxi_repl_bound_{index} = null; in");
					session
						.layers
						.push(layer.replace("{index}", &index.to_string()));
				}
				None => {
					let fragment = session.fragment(line);
					if let Outcome::Value(value) =
						session.evaluate(&format!("__noxi_repl_print {fragment} 1"))?
					{
						let _ = value;
						println!();
					}
				}
			}
		}
		command => print_error(&format!("unknown command '{command}'")),
	}
	Ok(Line::Prompt)
}

pub(super) fn run(mut parsed: Parsed) -> Result<ExitCode, Failure> {
	let expr = parsed.value("expr").map(str::to_owned);
	let file = parsed.value("file").map(str::to_owned);
	let mut operands = std::mem::take(&mut parsed.operands);
	if operands.is_empty() && (expr.is_some() || file.is_some()) {
		operands.push(".".into());
	}
	parsed.flags.remove("expr");
	parsed.flags.remove("file");
	// The installables are composed below; a placeholder keeps the shared
	// option handling off its flake path, which needs the `flakes` feature.
	parsed
		.flags
		.insert("expr".into(), vec![vec!["null".into()]]);
	// `forceImpureByDefault`.
	if parsed.settings.get("pure-eval").is_none() {
		parsed
			.settings
			.set("pure-eval", "false")
			.map_err(|e| e.to_string())?;
	}
	let mut options = super::expression::options(&parsed, true)?;
	options.strict = false;
	options.json = false;
	options.read_write_mode = true;
	options.inputs.clear();
	options.fragments.clear();
	options.flake = Some(super::flakes::Request {
		operation: "outputs".into(),
		no_write: true,
		..Default::default()
	});
	let mut session = Session {
		options,
		layers: Vec::new(),
		installables: Vec::new(),
		files: Vec::new(),
		flakes: Vec::new(),
		last_loaded: None,
		base_fragments: 0,
	};
	for operand in &operands {
		let (root, what) = if let Some(expression) = &expr {
			let fragment = session.fragment(expression);
			let attr = if operand == "." { "" } else { operand.as_str() };
			let mut code = format!("({fragment})");
			for name in instantiate::parse_attr_path(attr).map_err(Failure::Usage)? {
				code = format!("({code}).{}", nix_string_literal(&name));
			}
			(code, attr.to_owned())
		} else if let Some(file) = &file {
			let attr = if operand == "." { "" } else { operand.as_str() };
			let target = if instantiate::is_lookup_path(file) {
				file.clone()
			} else {
				nix_string_literal(
					&instantiate::absolute_path(&std::env::current_dir()?, Path::new(file))
						.to_string_lossy(),
				)
			};
			let mut code = format!("(import {target})");
			for name in instantiate::parse_attr_path(attr).map_err(Failure::Usage)? {
				code = format!("(__noxi_select {code} {})", nix_string_literal(&name));
			}
			(format!("__noxi_autoCall {code}"), attr.to_owned())
		} else {
			let (reference, fragment) = operand.split_once('#').unwrap_or((operand, ""));
			let reference = nix_string_literal(reference);
			let candidate = nix_string_literal(fragment.strip_prefix('.').unwrap_or(fragment));
			(
				format!(
					"__noxi_flake_select (__noxi_flake {reference}) [ {candidate} ] (__noxi_flake_label {reference})"
				),
				format!("{operand}{}", if fragment.is_empty() { "#" } else { "" }),
			)
		};
		session.installables.push((root, what));
	}
	session.base_fragments = session.options.fragments.len();
	notice("Nix 2.35.2\nType :? for help.");
	session.reload(true)?;
	let interactive = std::io::stdin().is_terminal();
	let echo = std::env::var("_NIX_TEST_REPL_ECHO").is_ok_and(|v| v == "1");
	let mut stdin = std::io::stdin().lock();
	let mut input = String::new();
	loop {
		let prompt = if input.is_empty() {
			"nix-repl> "
		} else {
			"        > "
		};
		if interactive {
			print!("{prompt}");
			std::io::stdout().flush()?;
		}
		let mut line = String::new();
		if stdin.read_line(&mut line)? == 0 {
			println!();
			break;
		}
		let line = line.strip_suffix('\n').unwrap_or(&line);
		if echo {
			println!("{prompt}{line}");
		}
		input.push_str(line);
		input.push('\n');
		match process(&mut session, &input)? {
			Line::Quit => break,
			Line::Incomplete => continue,
			Line::Prompt => {}
		}
		input.clear();
		println!();
	}
	Ok(ExitCode::SUCCESS)
}

/// `nix eval`'s `ValuePrinter`: forcing, derivation paths, no colours, no
/// pretty printing and thrown errors shown as `«error: ...»` at every depth.
pub async fn eval_print_builtin(co: GenCo, value: Value) -> Result<Value, ErrorKind> {
	let mut printer = Printer {
		out: Out {
			buffer: Some(String::new()),
			plain: true,
		},
		indent: String::new(),
		max_depth: usize::MAX,
		pretty: false,
		// The installable is forced before it is printed.
		throw_top: true,
	};
	match printer.print(&co, value, 0).await? {
		Err(caught) => Ok(caught),
		Ok(()) => Ok(Value::from(printer.out.buffer.unwrap_or_default())),
	}
}

/// An attribute's value as the REPL binds it, without the attribute-selection
/// frame that a Nix selection would add to its errors.
pub async fn get_builtin(co: GenCo, set: Value, name: Value) -> Result<Value, ErrorKind> {
	let set = generators::request_force(&co, set).await;
	if set.is_catchable() {
		return Ok(set);
	}
	let name = name.to_contextful_str()?;
	match set.to_attrs()?.select(name.as_bytes()) {
		Some(value) => Ok(generators::request_force(&co, value.clone()).await),
		None => Err(ErrorKind::EvalError(format!(
			"attribute '{}' missing",
			name.to_str_lossy()
		))),
	}
}
