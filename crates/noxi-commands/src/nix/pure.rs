//! Modern pure-mode adapter using the evaluator's public globals and I/O APIs.
//! Flake source reads are limited to trees fetched by the current evaluation.
//! Context realization also permits generated text and derivation outputs.
//! Other filesystem operations still fail explicitly.
use noxi_eval::{
	EvalIO, EvalMode, Evaluation, EvaluationBuilder, EvaluationResult, FileType, GlobalsMap, Value,
};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

struct PureIO {
	store: Option<Rc<noxi_glue::EvaluatorStore>>,
	realised: std::cell::RefCell<Vec<PathBuf>>,
}
impl PureIO {
	fn source(&self, path: &Path) -> io::Result<noxi_glue::StoreIO> {
		let Some(store) = &self.store else {
			return Err(unsupported());
		};
		let allowed = |path: &Path| {
			in_fetched_source(store, path)
				|| path.starts_with("/__corepkgs__")
				|| self
					.realised
					.borrow()
					.iter()
					.any(|root| path.starts_with(root))
		};
		// Where the path really leads must be allowed too, as CppNix checks
		// its resolved source path: neither `..` nor a symlink may leave the
		// allowed trees.
		if !allowed(path) {
			return Err(unsupported());
		}
		if !path.starts_with("/__corepkgs__") {
			let resolved = store.resolved_logical_path(path);
			if !allowed(&resolved) {
				return Err(io::Error::new(
					io::ErrorKind::PermissionDenied,
					format!(
						"access to absolute path '{}' is forbidden in pure evaluation mode (use '--impure' to override)",
						resolved.display()
					),
				));
			}
		}
		Ok(noxi_glue::StoreIO::new(store.clone()))
	}
}

/// Whether `path` is inside a source tree this evaluation fetched.
fn in_fetched_source(store: &noxi_glue::EvaluatorStore, path: &Path) -> bool {
	path.strip_prefix("/nix/store")
		.ok()
		.and_then(|p| p.components().next())
		.and_then(|p| {
			nix_compat::store_path::StorePath::from_bytes(p.as_os_str().as_encoded_bytes()).ok()
		})
		.is_some_and(|p| store.is_fetched_source(&p))
}

/// The store directory itself. Like the ancestors CppNix's allow list
/// permits for traversal, its metadata is visible in pure mode so that
/// fetched store paths can be reached by name; its listing is not.
fn is_store_dir(path: &Path) -> bool {
	path.components().eq(Path::new("/nix/store").components())
}

fn unsupported() -> io::Error {
	io::Error::other(
		"pure filesystem evaluation is not supported by noxi yet; use --impure for expression inputs",
	)
}
impl EvalIO for PureIO {
	fn resolve_lookup_path(&self, value: &str) -> io::Result<Option<PathBuf>> {
		let store = self.store.as_ref().ok_or_else(unsupported)?;
		noxi_glue::StoreIO::new(store.clone()).resolve_lookup_path(value)
	}

	fn resolve_import_path(&self, path: &Path) -> io::Result<PathBuf> {
		let io = self.source(path)?;
		let resolved = io.resolve_import_path(path)?;
		self.source(&io.canonicalize(&resolved)?)?;
		Ok(resolved)
	}
	fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
		self.source(path)?.read_link(path)
	}

	fn realise_path(&self, string: &noxi_eval::NixString) -> io::Result<PathBuf> {
		let store = self.store.as_ref().ok_or_else(unsupported)?;
		let io = noxi_glue::StoreIO::new(store.clone());
		let path = io.realise_path(string)?;
		if string
			.iter_context()
			.any(|context| context.iter().next().is_some())
		{
			self.realised.borrow_mut().push(path.clone());
		}
		Ok(path)
	}

	fn path_exists(&self, path: &Path) -> io::Result<bool> {
		if is_store_dir(path) {
			return Ok(true);
		}
		// CppNix's `pathExists` answers false for a path pure evaluation
		// may not access, as Nixpkgs' overlay lookup under an empty HOME needs.
		match self.source(path) {
			Ok(io) => io.path_exists(path),
			Err(_) => Ok(false),
		}
	}
	fn open(&self, path: &Path) -> io::Result<Box<dyn io::Read>> {
		self.source(path)?.open(path)
	}
	fn file_type(&self, path: &Path) -> io::Result<FileType> {
		if is_store_dir(path) {
			return Ok(FileType::Directory);
		}
		self.source(path)?.file_type(path)
	}
	fn read_dir(&self, path: &Path) -> io::Result<Vec<(bytes::Bytes, FileType)>> {
		self.source(path)?.read_dir(path)
	}
	fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
		if is_store_dir(path) {
			return Ok(path.to_owned());
		}
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
}

/// The impure `builtins.path` and `builtins.filterSource`, which read the
/// host filesystem directly, admitted only for paths inside a source tree
/// this evaluation fetched, as CppNix's pure-mode allow list admits them.
struct Guard {
	path: Value,
	filter_source: Value,
	store: Option<Rc<noxi_glue::EvaluatorStore>>,
}

impl Guard {
	fn allows(&self, path: &Path) -> bool {
		let Some(store) = &self.store else {
			return false;
		};
		// Check where the path really leads, as CppNix checks its resolved
		// source path: neither `..` nor a symlink may leave the tree.
		in_fetched_source(store, path)
			&& in_fetched_source(store, &store.resolved_logical_path(path))
	}
}

#[noxi_eval::builtin_macros::builtins(state = "Rc<Guard>")]
mod guarded {
	use super::Guard;
	use noxi_eval::generators::{self, Gen, GenCo};
	use noxi_eval::{ErrorKind, Value};
	use std::rc::Rc;

	fn forbidden(path: &std::path::Path) -> ErrorKind {
		ErrorKind::EvalError(format!(
			"access to absolute path '{}' is forbidden in pure evaluation mode (use '--impure' to override)",
			path.display()
		))
	}

	#[builtin("path")]
	async fn builtin_path(state: Rc<Guard>, co: GenCo, args: Value) -> Result<Value, ErrorKind> {
		if let Ok(attrs) = args.to_attrs()
			&& let Some(value) = attrs.select("path")
		{
			let path = noxi_eval::builtins::realise_value_to_path(&co, value.clone()).await?;
			if let Ok(path) = path
				&& !state.allows(&path)
			{
				return Err(forbidden(&path));
			}
		}
		Ok(generators::request_call_with(&co, state.path.clone(), [args]).await)
	}

	#[builtin("filterSource")]
	async fn builtin_filter_source(
		state: Rc<Guard>,
		co: GenCo,
		#[lazy] filter: Value,
		path: Value,
	) -> Result<Value, ErrorKind> {
		let checked = noxi_eval::builtins::realise_value_to_path(&co, path.clone()).await?;
		if let Ok(checked) = checked
			&& !state.allows(&checked)
		{
			return Err(forbidden(&checked));
		}
		Ok(generators::request_call_with(&co, state.filter_source.clone(), [filter, path]).await)
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
				b"currentSystem"
					| b"currentTime"
					| b"builtins"
					| b"import"
					| b"scopedImport"
					| b"path"
					| b"filterSource"
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
		.chain(guarded::builtins(Rc::new(Guard {
			path: builtins.select("path").expect("builtins.path").clone(),
			filter_source: builtins
				.select("filterSource")
				.expect("builtins.filterSource")
				.clone(),
			store: flake_store.clone(),
		})))
		.collect();
	let globals: Rc<GlobalsMap> =
		noxi_eval::prepare_globals(declarations, Vec::new(), template.source_map(), true);
	Evaluation::builder_pure()
		.with_globals(globals)
		// Built-in `<nix/...>` files resolve in pure mode as in CppNix.
		.corepkgs_dir(Some(noxi_glue::store::COREPKGS_DIR.into()))
		.with_source_map(template.source_map())
		.io_handle(Box::new(PureIO {
			store: flake_store,
			realised: Default::default(),
		}) as Box<dyn EvalIO>)
		.env(Some(env))
		.mode(mode)
		.max_call_depth(max_depth)
		.pure_eval(true)
		.build()
		.hide_root_source()
		.with_source_fragments(fragments)
		.evaluate_in_dir(driver, cwd.to_path_buf())
}
