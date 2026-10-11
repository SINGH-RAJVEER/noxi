//! `nix-rs-eval` implements the evaluation of the Nix programming language in
//! nix-rs.
//!
//! It is designed to allow users to use Nix as a versatile language for
//! different use-cases.
//!
//! This module exports the high-level functions and types needed for evaluating
//! Nix code and interacting with the language's data structures.
//!
//! Nix has several language features that make use of impurities (such as
//! reading from the NIX_PATH environment variable, or interacting with files).
//! These features are optional and the API of this crate exposes functionality
//! for controlling how they work.
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod builtins;
mod chunk;
mod compiler;
mod errors;
mod io;
mod line_endings;
pub mod observer;
mod opcode;
mod pretty_ast;
mod source;
mod spans;
mod systems;
mod upvalues;
mod value;
mod vm;
mod warnings;

mod nix_search_path;
#[cfg(all(test, feature = "arbitrary"))]
mod properties;
#[cfg(test)]
mod test_utils;
#[cfg(test)]
mod tests;

use observer::OptionalCompilerObserver;
use rustc_hash::FxHashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use crate::observer::{CompilerObserver, RuntimeObserver};
use crate::value::Lambda;
use crate::vm::run_lambda;

// Re-export the public interface used by other crates.
pub use crate::compiler::{CompilationOutput, GlobalsMap, compile, prepare_globals};
pub use crate::errors::{AddContext, CatchableErrorKind, Error, ErrorKind, EvalResult};
pub use crate::io::{DummyIO, EvalIO, FileType};
pub use crate::nix_search_path::{
	NixSearchPath, NixSearchPathEntry, is_pseudo_url, parse_nix_path,
};
pub use crate::pretty_ast::pretty_print_expr;
pub use crate::source::SourceCode;
pub use crate::value::{NixContext, NixContextElement};
pub use crate::vm::{EvalMode, generators};
pub use crate::warnings::{EvalWarning, WarningKind};
pub use builtin_macros;
use smol_str::SmolStr;

pub use crate::value::{
	Ambiguous, AmbiguousUnshared, Builtin, CoercionKind, NixAttrs, NixList, NixString,
	PrintOptions, Value, ValuePrinter,
};

pub(crate) const REPL_LOCATION: &str = "[code]";

#[cfg(feature = "impure")]
pub use crate::io::StdIO;

struct BuilderBuiltins {
	builtins: Vec<(&'static str, Value)>,
	src_builtins: Vec<(&'static str, &'static str)>,
}

enum BuilderGlobals {
	Builtins(BuilderBuiltins),
	Globals(Rc<GlobalsMap>),
}

/// Builder for building an [`Evaluation`].
///
/// Construct an [`EvaluationBuilder`] by calling one of:
///
/// - [`Evaluation::builder`] / [`EvaluationBuilder::new`]
/// - [`Evaluation::builder_impure`] [`EvaluationBuilder::new_impure`]
/// - [`Evaluation::builder_pure`] [`EvaluationBuilder::new_pure`]
///
/// Then configure the fields by calling the various methods on [`EvaluationBuilder`], and finally
/// call [`build`](Self::build) to construct an [`Evaluation`]
pub struct EvaluationBuilder<'co, 'ro, 'env, IO> {
	source_map: Option<SourceCode>,
	globals: BuilderGlobals,
	env: Option<&'env FxHashMap<SmolStr, Value>>,
	io_handle: IO,
	enable_import: bool,
	mode: EvalMode,
	search_path: Option<Vec<String>>,
	corepkgs_dir: Option<PathBuf>,
	pure_eval: bool,
	experimental_features: builtins::ExperimentalFeatures,
	messages: builtins::EvaluationMessages,
	compiler_observer: Option<&'co mut dyn CompilerObserver>,
	runtime_observer: Option<&'ro mut dyn RuntimeObserver>,
}

impl<'co, 'ro, 'env, IO> EvaluationBuilder<'co, 'ro, 'env, IO>
where
	IO: AsRef<dyn EvalIO> + 'static,
{
	/// Build an [`Evaluation`] based on the configuration in this builder.
	///
	/// This:
	///
	/// - Adds a `"storeDir"` builtin containing the store directory of the configured IO handle
	/// - Sets up globals based on the configured builtins
	/// - Copies all other configured fields to the [`Evaluation`]
	pub fn build(self) -> Evaluation<'co, 'ro, 'env, IO> {
		let source_map = self.source_map.unwrap_or_default();
		let nix_search_path = NixSearchPath::new(
			self.search_path
				.iter()
				.flatten()
				.map(|element| NixSearchPathEntry::parse(element))
				.collect(),
			self.corepkgs_dir,
		)
		.with_pure_eval(self.pure_eval);

		let globals = match self.globals {
			BuilderGlobals::Globals(globals) => globals,
			BuilderGlobals::Builtins(BuilderBuiltins {
				mut builtins,
				src_builtins,
			}) => {
				// Insert a storeDir builtin *iff* a store directory is present.
				if let Some(store_dir) = self.io_handle.as_ref().store_dir() {
					builtins.push(("storeDir", store_dir.into()));
				}
				builtins.push(("nixPath", builtins::nix_path_value(&nix_search_path)));

				crate::compiler::prepare_globals(
					builtins,
					src_builtins,
					source_map.clone(),
					self.enable_import,
				)
			}
		};

		Evaluation {
			source_map,
			globals,
			env: self.env,
			io_handle: self.io_handle,
			mode: self.mode,
			nix_search_path,
			compiler_observer: self.compiler_observer,
			runtime_observer: self.runtime_observer,
		}
	}
}

thread_local! {
	/// CppNix's `max-call-depth`, which bounds recursive conversions such as
	/// `builtins.toJSON` on this thread.
	static MAX_CALL_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(10000) };
}

pub(crate) fn max_call_depth() -> usize {
	MAX_CALL_DEPTH.with(std::cell::Cell::get)
}

// NOTE(aspen): The methods here are intentionally incomplete; feel free to add new ones (ideally
// with similar naming conventions to the ones already present) but don't expose fields publicly!
impl<'co, 'ro, 'env, IO> EvaluationBuilder<'co, 'ro, 'env, IO> {
	pub fn new(io_handle: IO) -> Self {
		let experimental_features = builtins::ExperimentalFeatures::default();
		let messages = builtins::EvaluationMessages::default();
		let mut builtins =
			builtins::pure_builtins_with_messages(experimental_features.clone(), messages.clone());
		builtins.extend(builtins::placeholders()); // these are temporary

		Self {
			source_map: None,
			enable_import: false,
			io_handle,
			globals: BuilderGlobals::Builtins(BuilderBuiltins {
				builtins,
				src_builtins: vec![],
			}),
			env: None,
			mode: Default::default(),
			search_path: None,
			corepkgs_dir: None,
			pure_eval: false,
			experimental_features,
			messages,
			compiler_observer: None,
			runtime_observer: None,
		}
	}

	pub fn io_handle<IO2>(self, io_handle: IO2) -> EvaluationBuilder<'co, 'ro, 'env, IO2> {
		EvaluationBuilder {
			io_handle,
			source_map: self.source_map,
			globals: self.globals,
			env: self.env,
			enable_import: self.enable_import,
			mode: self.mode,
			search_path: self.search_path,
			corepkgs_dir: self.corepkgs_dir,
			pure_eval: self.pure_eval,
			experimental_features: self.experimental_features,
			messages: self.messages,
			compiler_observer: self.compiler_observer,
			runtime_observer: self.runtime_observer,
		}
	}

	pub fn with_enable_import(self, enable_import: bool) -> Self {
		Self {
			enable_import,
			..self
		}
	}

	pub fn disable_import(self) -> Self {
		self.with_enable_import(false)
	}

	pub fn enable_import(self) -> Self {
		self.with_enable_import(true)
	}

	fn builtins_mut(&mut self) -> &mut BuilderBuiltins {
		match &mut self.globals {
			BuilderGlobals::Builtins(builtins) => builtins,
			BuilderGlobals::Globals(_) => {
				panic!("Cannot modify builtins on an EvaluationBuilder with globals configured")
			}
		}
	}

	/// Add additional builtins (represented as tuples of name and [`Value`]) to this evaluation
	/// builder.
	///
	/// # Panics
	///
	/// Panics if this evaluation builder has had globals set via [`Self::with_globals`]
	pub fn add_builtins<I>(mut self, builtins: I) -> Self
	where
		I: IntoIterator<Item = (&'static str, Value)>,
	{
		self.builtins_mut().builtins.extend(builtins);
		self
	}

	/// Add additional builtins that are implemented in Nix source code (represented as tuples of
	/// name and nix source) to this evaluation builder.
	///
	/// # Panics
	///
	/// Panics if this evaluation builder has had globals set via [`Self::with_globals`]
	pub fn add_src_builtin(mut self, name: &'static str, src: &'static str) -> Self {
		self.builtins_mut().src_builtins.push((name, src));
		self
	}

	/// Set the globals for this evaluation builder to a previously-constructed globals map.
	/// Intended to allow sharing globals across multiple evaluations (eg for the REPL).
	///
	/// Discards any builtins previously configured via [`Self::add_builtins`] and [`Self::add_src_builtin`].
	/// If either of those methods is called on the evaluation builder after this one, they will
	/// panic.
	pub fn with_globals(self, globals: Rc<GlobalsMap>) -> Self {
		Self {
			globals: BuilderGlobals::Globals(globals),
			..self
		}
	}

	pub fn with_source_map(self, source_map: SourceCode) -> Self {
		debug_assert!(
			self.source_map.is_none(),
			"Cannot set the source_map on an EvaluationBuilder twice"
		);
		Self {
			source_map: Some(source_map),
			..self
		}
	}

	pub fn mode(self, mode: EvalMode) -> Self {
		Self { mode, ..self }
	}

	/// Set the lookup path from a `NIX_PATH` value, split as CppNix does.
	pub fn nix_path(self, nix_path: Option<String>) -> Self {
		Self {
			search_path: nix_path.map(|s| parse_nix_path(&s)),
			..self
		}
	}

	/// Set the lookup path's elements, each `prefix=path` or `path`.
	pub fn search_path(self, elements: Vec<String>) -> Self {
		Self {
			search_path: Some(elements),
			..self
		}
	}

	/// CppNix's `pure-eval`. Only `<...>` lookups honour it so far: the
	/// caller leaves the lookup path empty, and a missing file names pure
	/// evaluation as the reason.
	pub fn pure_eval(self, pure_eval: bool) -> Self {
		Self { pure_eval, ..self }
	}

	/// The directory `<nix/...>` paths resolve in when no lookup path entry
	/// has the file.
	pub fn corepkgs_dir(self, dir: Option<PathBuf>) -> Self {
		Self {
			corepkgs_dir: dir,
			..self
		}
	}

	/// Enable the named CppNix experimental features, replacing those
	/// enabled before. Builtins whose behaviour depends on a feature see the
	/// change.
	pub fn experimental_features<I, S>(self, features: I) -> Self
	where
		I: IntoIterator<Item = S>,
		S: Into<String>,
	{
		self.experimental_features
			.set(features.into_iter().map(Into::into));
		self
	}

	/// Set CppNix's `max-call-depth` (default 10000) for evaluations on this
	/// thread. Deeper recursive conversions, such as `builtins.toJSON` of a
	/// deeply nested value, fail with CppNix's stack overflow error.
	pub fn max_call_depth(self, depth: usize) -> Self {
		MAX_CALL_DEPTH.with(|max| max.set(depth));
		self
	}

	/// Set CppNix's `trace-verbose` (default false) for this evaluation:
	/// whether `builtins.traceVerbose` prints its message like
	/// `builtins.trace`.
	pub fn trace_verbose(self, enabled: bool) -> Self {
		self.messages.trace_verbose.set(enabled);
		self
	}

	/// Set CppNix's `abort-on-warn` (default false) for this evaluation:
	/// whether `builtins.warn` fails after printing its warning.
	pub fn abort_on_warn(self, enabled: bool) -> Self {
		self.messages.abort_on_warn.set(enabled);
		self
	}

	/// Whether `builtins.warn` prints to standard error, default true.
	/// Suppressing output does not disable `abort-on-warn`.
	pub fn warning_output(self, enabled: bool) -> Self {
		self.messages.warning_output.set(enabled);
		self
	}

	/// Send builtin trace and warning messages to this sink, default stderr.
	/// Each call receives a complete message, including its final newline.
	pub fn message_output(self, output: impl Fn(&[u8]) + 'static) -> Self {
		*self.messages.output.borrow_mut() = Rc::new(output);
		self
	}

	/// Whether the named CppNix experimental feature is enabled.
	pub fn experimental_feature_enabled(&self, feature: &str) -> bool {
		self.experimental_features.enabled(feature)
	}

	pub fn env(self, env: Option<&'env FxHashMap<SmolStr, Value>>) -> Self {
		Self { env, ..self }
	}

	pub fn compiler_observer(
		self,
		compiler_observer: Option<&'co mut dyn CompilerObserver>,
	) -> Self {
		Self {
			compiler_observer,
			..self
		}
	}

	pub fn set_compiler_observer(
		&mut self,
		compiler_observer: Option<&'co mut dyn CompilerObserver>,
	) {
		self.compiler_observer = compiler_observer;
	}

	pub fn runtime_observer(self, runtime_observer: Option<&'ro mut dyn RuntimeObserver>) -> Self {
		Self {
			runtime_observer,
			..self
		}
	}

	pub fn set_runtime_observer(&mut self, runtime_observer: Option<&'ro mut dyn RuntimeObserver>) {
		self.runtime_observer = runtime_observer;
	}
}

impl<IO> EvaluationBuilder<'_, '_, '_, IO> {
	pub fn source_map(&mut self) -> &SourceCode {
		self.source_map.get_or_insert_with(SourceCode::default)
	}
}

impl EvaluationBuilder<'_, '_, '_, Box<dyn EvalIO>> {
	/// Initialize an `Evaluation`, without the import statement available, and
	/// all IO operations stubbed out.
	pub fn new_pure() -> Self {
		Self::new(Box::new(DummyIO) as Box<dyn EvalIO>).with_enable_import(false)
	}

	#[cfg(feature = "impure")]
	/// Configure an `Evaluation` to have impure features available
	/// with the given I/O implementation.
	///
	/// If no I/O implementation is supplied, [`StdIO`] is used by
	/// default.
	pub fn enable_impure(mut self, io: Option<Box<dyn EvalIO>>) -> Self {
		self.io_handle = io.unwrap_or_else(|| Box::new(StdIO) as Box<dyn EvalIO>);
		self.enable_import = true;
		self.builtins_mut()
			.builtins
			.extend(builtins::impure_builtins());

		// Make `NIX_PATH` resolutions work by default, unless the
		// user already overrode this with something else.
		if self.search_path.is_none() {
			self.search_path = std::env::var("NIX_PATH").ok().map(|s| parse_nix_path(&s));
		}
		self
	}

	#[cfg(feature = "impure")]
	/// Initialise an `Evaluation`, with all impure features turned on by default.
	pub fn new_impure() -> Self {
		Self::new_pure().enable_impure(None)
	}
}

/// An `Evaluation` represents how a piece of Nix code is evaluated. It can be
/// instantiated and configured directly, or it can be accessed through the
/// various simplified helper methods available below.
///
/// Public fields are intended to be set by the caller. Setting all
/// fields is optional.
pub struct Evaluation<'co, 'ro, 'env, IO> {
	/// Source code map used for error reporting.
	source_map: SourceCode,

	/// Set of all global values available at the top-level scope
	globals: Rc<GlobalsMap>,

	/// Top-level variables to define in the evaluation
	env: Option<&'env FxHashMap<SmolStr, Value>>,

	/// Implementation of file-IO to use during evaluation, e.g. for
	/// impure builtins.
	///
	/// Defaults to [`DummyIO`] if not set explicitly.
	io_handle: IO,

	/// Specification for how to handle top-level values returned by evaluation
	///
	/// See the documentation for [`EvalMode`] for more information.
	mode: EvalMode,

	/// The lookup path for resolving `<...>` paths such as `<nixpkgs>`.
	nix_search_path: NixSearchPath,

	/// (optional) compiler observer for reporting on compilation
	/// details, like the emitted bytecode.
	compiler_observer: Option<&'co mut dyn CompilerObserver>,

	/// (optional) runtime observer, for reporting on execution steps
	/// of Nix code.
	runtime_observer: Option<&'ro mut dyn RuntimeObserver>,
}

/// Result of evaluating a piece of Nix code. If evaluation succeeded, a value
/// will be present (and potentially some warnings!). If evaluation failed,
/// errors will be present.
#[derive(Debug, Default)]
pub struct EvaluationResult {
	/// Nix value that the code evaluated to.
	pub value: Option<Value>,

	/// Errors that occurred during evaluation (if any).
	pub errors: Vec<Error>,

	/// Warnings that occurred during evaluation. Warnings are not critical, but
	/// should be addressed either to modernise code or improve performance.
	pub warnings: Vec<EvalWarning>,

	/// AST node that was parsed from the code (on success only).
	pub expr: Option<rnix::ast::Expr>,
}

impl<'co, 'ro, 'env, IO> Evaluation<'co, 'ro, 'env, IO> {
	/// Make a new [builder][] for configuring an evaluation
	///
	/// [builder]: EvaluationBuilder
	pub fn builder(io_handle: IO) -> EvaluationBuilder<'co, 'ro, 'env, IO> {
		EvaluationBuilder::new(io_handle)
	}

	/// Clone the reference to the map of Nix globals for this evaluation. If [`Value`]s are shared
	/// across subsequent [`Evaluation`]s, it is important that those evaluations all have the same
	/// underlying globals map.
	pub fn globals(&self) -> Rc<GlobalsMap> {
		self.globals.clone()
	}

	/// Clone the reference to the contained source code map. This is used after an evaluation for
	/// pretty error printing. Also, if [`Value`]s are shared across subsequent [`Evaluation`]s, it
	/// is important that those evaluations all have the same underlying source code map.
	pub fn source_map(&self) -> SourceCode {
		self.source_map.clone()
	}
}

impl<'co, 'ro, 'env> Evaluation<'co, 'ro, 'env, Box<dyn EvalIO>> {
	#[cfg(feature = "impure")]
	pub fn builder_impure() -> EvaluationBuilder<'co, 'ro, 'env, Box<dyn EvalIO>> {
		EvaluationBuilder::new_impure()
	}

	pub fn builder_pure() -> EvaluationBuilder<'co, 'ro, 'env, Box<dyn EvalIO>> {
		EvaluationBuilder::new_pure()
	}
}

impl<IO> Evaluation<'_, '_, '_, IO>
where
	IO: AsRef<dyn EvalIO> + 'static,
{
	/// Only compile the provided source code, at an optional location of the
	/// source code (i.e. path to the file it was read from; used for error
	/// reporting, and for resolving relative paths in impure functions)
	/// This does not *run* the code, it only provides analysis (errors and
	/// warnings) of the compiler.
	pub fn compile_only(
		mut self,
		code: impl AsRef<str>,
		location: Option<PathBuf>,
	) -> EvaluationResult {
		let mut result = EvaluationResult::default();
		let source = self.source_map();

		let location_str = location
			.as_ref()
			.map(|p| p.to_string_lossy().to_string())
			.unwrap_or_else(|| REPL_LOCATION.into());

		let code = line_endings::normalize(code.as_ref());
		let file = source.add_file(location_str, code.to_string());

		let compiler_observer = self.compiler_observer.take();

		parse_compile_internal(
			&mut result,
			&code,
			file,
			location,
			source,
			self.globals,
			self.env,
			compiler_observer.into(),
		);

		result
	}

	/// Evaluate the provided source code, at an optional location of the source
	/// code (i.e. path to the file it was read from; used for error reporting,
	/// and for resolving relative paths in impure functions)
	pub fn evaluate(self, code: impl AsRef<str>, location: Option<PathBuf>) -> EvaluationResult {
		self.evaluate_at(code, location.clone(), location)
	}

	/// Evaluate source code that was not read from a file, such as a
	/// `--expr` argument, resolving its relative paths against `dir` rather
	/// than the process's current directory.
	pub fn evaluate_in_dir(self, code: impl AsRef<str>, dir: PathBuf) -> EvaluationResult {
		self.evaluate_at(code, None, Some(dir))
	}

	/// Evaluate `code` named `location` in diagnostics and positions, with
	/// relative paths resolved against `base`.
	fn evaluate_at(
		mut self,
		code: impl AsRef<str>,
		location: Option<PathBuf>,
		base: Option<PathBuf>,
	) -> EvaluationResult {
		let mut result = EvaluationResult::default();
		let source = self.source_map();

		let location_str = location
			.as_ref()
			.map(|p| p.to_string_lossy().to_string())
			.unwrap_or_else(|| REPL_LOCATION.into());

		let code = line_endings::normalize(code.as_ref());
		let file = source.add_file(location_str, code.to_string());

		let compiler_observer = self.compiler_observer.take();

		let lambda = match parse_compile_internal(
			&mut result,
			&code,
			file.clone(),
			base,
			source.clone(),
			self.globals.clone(),
			self.env,
			compiler_observer.into(),
		) {
			None => return result,
			Some(cr) => cr,
		};

		// If bytecode was returned, there were no errors and the
		// code is safe to execute.

		let runtime_observer = self.runtime_observer.take();

		let vm_result = run_lambda(
			self.nix_search_path,
			self.io_handle,
			runtime_observer,
			source.clone(),
			self.globals,
			lambda,
			self.mode,
		);

		match vm_result {
			Ok(mut runtime_result) => {
				result.warnings.append(&mut runtime_result.warnings);
				if let Value::Catchable(inner) = runtime_result.value {
					result.errors.push(Error::new(
						ErrorKind::CatchableError(*inner),
						file.span,
						source,
					));
					return result;
				}

				result.value = Some(runtime_result.value);
			}
			Err(err) => {
				result.errors.push(err);
			}
		}

		result
	}
}

/// Internal helper function for common parsing & compilation logic
/// between the public functions.
#[allow(clippy::too_many_arguments)] // internal API, no point making an indirection type
fn parse_compile_internal(
	result: &mut EvaluationResult,
	code: &str,
	file: Arc<codemap::File>,
	location: Option<PathBuf>,
	source: SourceCode,
	globals: Rc<GlobalsMap>,
	env: Option<&FxHashMap<SmolStr, Value>>,
	compiler_observer: OptionalCompilerObserver<'_>,
) -> Option<Rc<Lambda>> {
	let parsed = rnix::ast::Root::parse(code);
	let parse_errors = parsed.errors();

	if !parse_errors.is_empty() {
		result.errors.push(Error::new(
			ErrorKind::ParseErrors(parse_errors.to_vec()),
			file.span,
			source,
		));
		return None;
	}

	// At this point we know that the code is free of parse errors and
	// we can continue to compile it. The expression is persisted in
	// the result, in case the caller needs it for something.
	result.expr = parsed.tree().expr();

	let compiler_result = match compiler::compile(
		result.expr.as_ref().unwrap(),
		location,
		globals,
		env,
		&source,
		&file,
		compiler_observer,
	) {
		Ok(result) => result,
		Err(err) => {
			result.errors.push(err);
			return None;
		}
	};

	result.warnings = compiler_result.warnings;
	result.errors.extend(compiler_result.errors);

	// Short-circuit if errors exist at this point (do not pass broken
	// bytecode to the runtime).
	if !result.errors.is_empty() {
		return None;
	}

	// Return the lambda (for execution) and the globals map (to
	// ensure the invariant that the globals outlive the runtime).
	Some(compiler_result.lambda)
}
