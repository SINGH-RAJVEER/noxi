//! Modern pure-mode adapter using the evaluator's public globals and I/O APIs.
//! Flake source reads are limited to trees fetched by the current evaluation.
//! Other filesystem operations still fail explicitly.
use noxi_eval::{
	EvalIO, EvalMode, Evaluation, EvaluationBuilder, EvaluationResult, FileType, GlobalsMap, Value,
};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

struct PureIO(Option<Rc<noxi_glue::EvaluatorStore>>);
impl PureIO {
	fn source(&self, path: &Path) -> io::Result<noxi_glue::StoreIO> {
		let Some(store) = &self.0 else {
			return Err(unsupported());
		};
		let mounted = path
			.strip_prefix("/nix/store")
			.ok()
			.and_then(|p| p.components().next())
			.and_then(|p| {
				nix_compat::store_path::StorePath::from_bytes(p.as_os_str().as_encoded_bytes()).ok()
			})
			.is_some_and(|p| store.is_fetched_source(&p));
		if mounted || path.starts_with("/__corepkgs__") {
			Ok(noxi_glue::StoreIO::new(store.clone()))
		} else {
			Err(unsupported())
		}
	}
}
fn unsupported() -> io::Error {
	io::Error::other(
		"pure filesystem evaluation is not supported by noxi yet; use --impure for expression inputs",
	)
}
impl EvalIO for PureIO {
	fn path_exists(&self, path: &Path) -> io::Result<bool> {
		self.source(path)?.path_exists(path)
	}
	fn open(&self, path: &Path) -> io::Result<Box<dyn io::Read>> {
		self.source(path)?.open(path)
	}
	fn file_type(&self, path: &Path) -> io::Result<FileType> {
		self.source(path)?.file_type(path)
	}
	fn read_dir(&self, path: &Path) -> io::Result<Vec<(bytes::Bytes, FileType)>> {
		self.source(path)?.read_dir(path)
	}
	fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
		self.source(path)?.canonicalize(path)
	}
	fn import_path(&self, path: &Path) -> io::Result<PathBuf> {
		self.source(path)?.import_path(path)
	}
	fn get_env(&self, _: &OsStr) -> Option<OsString> {
		None
	}
	fn store_dir(&self) -> Option<String> {
		Some("/nix/store".into())
	}
}

#[noxi_eval::builtin_macros::builtins]
mod blocked {
	use noxi_eval::generators::{Gen, GenCo};
	use noxi_eval::{ErrorKind, Value};
	#[builtin("storePath")]
	async fn builtin_store_path(_co: GenCo, #[lazy] _value: Value) -> Result<Value, ErrorKind> {
		Err(ErrorKind::EvalError(
			"'builtins.storePath' is not allowed in pure evaluation mode".into(),
		))
	}
	#[builtin("path")]
	async fn builtin_path(_co: GenCo, #[lazy] _value: Value) -> Result<Value, ErrorKind> {
		Err(ErrorKind::EvalError(
			"pure builtins.path is not supported by noxi yet; use --impure".into(),
		))
	}
	#[builtin("filterSource")]
	async fn builtin_filter_source(
		_co: GenCo,
		#[lazy] _filter: Value,
		#[lazy] _path: Value,
	) -> Result<Value, ErrorKind> {
		Err(ErrorKind::EvalError(
			"pure builtins.filterSource is not supported by noxi yet; use --impure".into(),
		))
	}
}

pub(crate) fn evaluate<'co, 'ro, 'env>(
	builder: EvaluationBuilder<'co, 'ro, 'env, Box<dyn EvalIO>>,
	env: &'env rustc_hash::FxHashMap<smol_str::SmolStr, Value>,
	mode: EvalMode,
	max_depth: usize,
	driver: &str,
	fragments: Vec<noxi_eval::SourceFragment>,
	cwd: &Path,
	flake_store: Option<Rc<noxi_glue::EvaluatorStore>>,
) -> EvaluationResult {
	let template = builder.add_builtins(blocked::builtins()).build();
	let original = template.globals();
	let Value::Attrs(builtins) = &original["builtins"] else {
		unreachable!("builtins namespace")
	};
	let declarations = builtins
		.iter()
		.filter(|(name, _)| {
			!matches!(
				name.as_bytes(),
				b"currentSystem" | b"currentTime" | b"builtins" | b"import" | b"scopedImport"
			)
		})
		.map(|(name, value)| {
			// Each CLI process evaluates once. The globals API requires static names.
			let name: &'static str = Box::leak(
				String::from_utf8(name.as_bytes().to_vec())
					.expect("builtin name UTF-8")
					.into_boxed_str(),
			);
			(name, value.clone())
		})
		.collect();
	let globals: Rc<GlobalsMap> =
		noxi_eval::prepare_globals(declarations, Vec::new(), template.source_map(), true);
	Evaluation::builder_pure()
		.with_globals(globals)
		.with_source_map(template.source_map())
		.io_handle(Box::new(PureIO(flake_store)) as Box<dyn EvalIO>)
		.env(Some(env))
		.mode(mode)
		.max_call_depth(max_depth)
		.pure_eval(true)
		.build()
		.hide_root_source()
		.with_source_fragments(fragments)
		.evaluate_in_dir(driver, cwd.to_path_buf())
}
