//! This module implements the Nix language's `import` feature, which
//! is exposed as a builtin in the Nix language.
//!
//! This is not a typical builtin, as it needs access to internal
//! compiler and VM state (such as the [`crate::SourceCode`]
//! instance, or observers).

use super::GlobalsMap;
use bstr::ByteSlice;
use genawaiter::rc::Gen;
use rustc_hash::FxHashMap;
use smol_str::SmolStr;
use std::path::PathBuf;
use std::rc::Weak;

use crate::{
	ErrorKind, SourceCode, Value,
	builtins::realise_value_to_path,
	generators::pin_generator,
	try_cek_to_value,
	value::{Builtin, Thunk},
	vm::generators::{self, GenCo},
};

async fn import_impl(
	co: GenCo,
	globals: Weak<GlobalsMap>,
	source: SourceCode,
	mut args: Vec<Value>,
) -> Result<Value, ErrorKind> {
	let path = try_cek_to_value!(realise_value_to_path(&co, args.pop().unwrap()).await?);

	if let Some(value) = import_derivation(&co, globals.clone(), source.clone(), &path).await? {
		return Ok(value);
	}

	let path = generators::request_resolve_import_path(&co, path, true).await;

	let span = generators::request_span(&co).await;
	if !source.is_hidden(span) {
		generators::request_builtin_context(
			&co,
			vec![crate::Trace::new(
				None,
				format!("while evaluating the file '{}':", path.display()),
			)],
		)
		.await;
	}
	if let Some(cached) = generators::request_import_cache_lookup(&co, path.clone()).await {
		return Ok(generators::request_force(&co, cached).await);
	}
	let res = compile_file(&co, globals, source, path.clone(), None).await?;
	generators::request_import_cache_put(&co, path, res.clone()).await;
	let res = generators::request_force(&co, res).await;
	Ok(res)
}

/// `scopedImport scope path`: the file compiled with the attributes of
/// `scope` as variables around it, shadowing the builtins of the same name.
/// Each call compiles the file again; results are not cached.
async fn scoped_import_impl(
	co: GenCo,
	globals: Weak<GlobalsMap>,
	source: SourceCode,
	mut args: Vec<Value>,
) -> Result<Value, ErrorKind> {
	let path = args.pop().unwrap();
	// The reference realises the path before inspecting the scope. In
	// particular, a failing path must leave scope values unevaluated.
	let path = try_cek_to_value!(realise_value_to_path(&co, path).await?);
	if let Some(value) = import_derivation(&co, globals.clone(), source.clone(), &path).await? {
		return Ok(value);
	}
	let scope = generators::request_force(&co, args.pop().unwrap()).await;
	if scope.is_catchable() {
		return Ok(scope);
	}
	let scope: FxHashMap<SmolStr, Value> = scope
		.to_attrs()?
		.iter()
		.map(|(name, value)| (SmolStr::new(name.to_str_lossy()), value.clone()))
		.collect();

	let path = generators::request_resolve_import_path(&co, path, false).await;
	compile_file(&co, globals, source, path, Some(&scope)).await
}

/// Read, parse and compile the Nix file at `path` with `env` as variables
/// around it, and return a thunk of its value.
async fn compile_file(
	co: &GenCo,
	globals: Weak<GlobalsMap>,
	source: SourceCode,
	path: PathBuf,
	env: Option<&FxHashMap<SmolStr, Value>>,
) -> Result<Value, ErrorKind> {
	let mut reader = generators::request_open_file(co, path.clone()).await;
	// Preserve source bytes while adapting them to rnix's UTF-8 syntax tree.
	let mut bytes = Vec::new();
	reader.read_to_end(&mut bytes)?;
	let (contents, encoding) = SourceCode::encode_bytes(&bytes);
	compile_contents(co, globals, source, path, env, &contents, encoding).await
}

async fn compile_contents(
	co: &GenCo,
	globals: Weak<GlobalsMap>,
	source: SourceCode,
	path: PathBuf,
	env: Option<&FxHashMap<SmolStr, Value>>,
	contents: &str,
	encoding: Option<u32>,
) -> Result<Value, ErrorKind> {
	let contents = crate::line_endings::normalize(contents).into_owned();

	let parsed = rnix::ast::Root::parse(&contents);
	let errors = parsed.errors();
	let file = source.add_file(path.to_string_lossy().to_string(), contents.to_owned());
	source.set_byte_encoding(file.span, encoding);

	if !errors.is_empty() {
		return Err(ErrorKind::ImportParseError {
			path,
			file,
			errors: errors.to_vec(),
		});
	}

	let result = crate::compiler::compile(
		&parsed.tree().expr().unwrap(),
		path.parent().map(PathBuf::from),
		// The VM must ensure that a strong reference to the globals outlives
		// any self-references (which are weak) embedded within the globals. If
		// the expect() below panics, it means that did not happen.
		globals
			.upgrade()
			.expect("globals dropped while still in use"),
		env,
		&source,
		&file,
		Default::default(),
	)
	.map_err(|err| ErrorKind::ImportCompilerError {
		path: path.clone(),
		errors: vec![err],
	})?;

	if !result.errors.is_empty() {
		return Err(ErrorKind::ImportCompilerError {
			path,
			errors: result.errors,
		});
	}

	for warning in result.warnings {
		generators::emit_warning(co, warning).await;
	}

	// Compilation succeeded, we can construct a thunk from whatever it spat
	// out and return that.
	Ok(Value::Thunk(Thunk::new_suspended(
		result.lambda,
		generators::request_span(co).await,
	)))
}

/// Store-specific derivation reading stays behind EvalIO. The recursive
/// output/all graph is constructed by the reference's Nix helper, so it
/// retains the same laziness and pointer equality as normal evaluation.
async fn import_derivation(
	co: &GenCo,
	globals: Weak<GlobalsMap>,
	source: SourceCode,
	path: &std::path::Path,
) -> Result<Option<Value>, ErrorKind> {
	let Some(attrs) = generators::request_import_derivation(co, path.to_owned()).await else {
		return Ok(None);
	};
	let helper_path = PathBuf::from("/__corepkgs__/imported-drv-to-derivation.nix");
	let helper = match generators::request_import_cache_lookup(co, helper_path.clone()).await {
		Some(helper) => helper,
		None => {
			let helper = compile_contents(
				co,
				globals,
				source,
				helper_path.clone(),
				None,
				include_str!("imported-drv-to-derivation.nix"),
				None,
			)
			.await?;
			generators::request_import_cache_put(co, helper_path, helper.clone()).await;
			helper
		}
	};
	let value = generators::request_call_with(co, helper, [attrs]).await;
	Ok(Some(generators::request_force(co, value).await))
}

/// Constructs the `import` builtin. This builtin is special in that
/// it needs to capture the [crate::SourceCode] structure to correctly
/// track source code locations while invoking a compiler.
// TODO: need to be able to pass through a CompilationObserver, too.
// TODO: can the `SourceCode` come from the compiler?
pub(super) fn builtins_import(globals: &Weak<GlobalsMap>, source: SourceCode) -> Builtin {
	// This (very cheap, once-per-compiler-startup) clone exists
	// solely in order to keep the borrow checker happy.  It
	// resolves the tension between the requirements of
	// Rc::new_cyclic() and Builtin::new()
	let globals = globals.clone();

	Builtin::new(
		"import",
		Some("Import the given file and return the Nix value it evaluates to"),
		1,
		move |args| {
			Gen::new(|co| pin_generator(import_impl(co, globals.clone(), source.clone(), args)))
		},
	)
}

/// Constructs the `scopedImport` builtin, which captures the same state as
/// `import`.
pub(super) fn builtins_scoped_import(globals: &Weak<GlobalsMap>, source: SourceCode) -> Builtin {
	let globals = globals.clone();

	Builtin::new(
		"scopedImport",
		Some("Import the given file with the attributes of a set in scope"),
		2,
		move |args| {
			Gen::new(|co| {
				pin_generator(scoped_import_impl(
					co,
					globals.clone(),
					source.clone(),
					args,
				))
			})
		},
	)
}
