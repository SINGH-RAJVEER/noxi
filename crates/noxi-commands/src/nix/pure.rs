//! Modern pure-mode adapter using the evaluator's public globals and I/O APIs.
//! Filesystem access follows the evaluator store's allow list, which admits
//! what CppNix's `allowPath` does: fetched source trees, paths the evaluation
//! copies or writes to the store, and outputs built for import.
use noxi_eval::{
	EvalIO, EvalMode, Evaluation, EvaluationBuilder, EvaluationResult, FileType, GlobalsMap, Value,
};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

struct PureIO {
	store: Rc<noxi_glue::EvaluatorStore>,
}
impl PureIO {
	fn io(&self) -> noxi_glue::StoreIO {
		noxi_glue::StoreIO::new(self.store.clone())
	}

	/// The store I/O for `path` once the allow list admits it. The store
	/// checks where the path really leads too, as CppNix checks its resolved
	/// source path: neither `..` nor a symlink may leave the allowed trees.
	fn source(&self, path: &Path) -> io::Result<noxi_glue::StoreIO> {
		self.store.check_restricted_access(path, true)?;
		Ok(self.io())
	}

	/// The store I/O for a metadata operation on `path`. Like CppNix's lstat,
	/// it inspects a final symlink without granting access to its target.
	fn metadata(&self, path: &Path) -> io::Result<noxi_glue::StoreIO> {
		self.store.check_restricted_access(path, false)?;
		Ok(self.io())
	}
}

/// The store directory itself. Like the ancestors CppNix's allow list
/// permits for traversal, its metadata is visible in pure mode so that
/// fetched store paths can be reached by name; its listing is not.
fn is_store_dir(path: &Path) -> bool {
	path.components()
		.eq(Path::new(nix_compat::store_path::store_dir()).components())
}

impl EvalIO for PureIO {
	fn resolve_lookup_path(&self, value: &str) -> io::Result<Option<PathBuf>> {
		self.io().resolve_lookup_path(value)
	}

	fn resolve_import_path(&self, path: &Path) -> io::Result<PathBuf> {
		let io = self.source(path)?;
		let resolved = io.resolve_import_path(path)?;
		self.source(&io.canonicalize(&resolved)?)?;
		Ok(resolved)
	}
	fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
		self.metadata(path)?.read_link(path)
	}

	fn ensure_context_path(&self, path: &Path) -> io::Result<()> {
		self.io().ensure_context_path(path)
	}

	/// Realising a context admits only the outputs it builds for import,
	/// as CppNix's `realiseContext` does; other context paths stay as
	/// allowed as the evaluation made them.
	fn realise_path(&self, string: &noxi_eval::NixString) -> io::Result<PathBuf> {
		self.io().realise_path(string)
	}

	fn path_exists(&self, path: &Path) -> io::Result<bool> {
		if is_store_dir(path) {
			return Ok(true);
		}
		// CppNix's `pathExists` answers false for a path pure evaluation
		// may not access, as Nixpkgs' overlay lookup under an empty HOME needs.
		match self.metadata(path) {
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
		self.metadata(path)?.file_type(path)
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
		Some(nix_compat::store_path::store_dir().into())
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

/// Evaluate with the pure builtins and `store`'s allow list, which the
/// caller must have started with `restrict_access`. `builtins.path` and
/// `builtins.filterSource` check the same list before they copy anything.
pub(crate) fn evaluate<'co, 'ro, 'env>(
	builder: EvaluationBuilder<'co, 'ro, 'env, Box<dyn EvalIO>>,
	env: &'env rustc_hash::FxHashMap<smol_str::SmolStr, Value>,
	mode: EvalMode,
	max_depth: usize,
	driver: &str,
	fragments: Vec<noxi_eval::SourceFragment>,
	cwd: &Path,
	store: Rc<noxi_glue::EvaluatorStore>,
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
		// Built-in `<nix/...>` files resolve in pure mode as in CppNix.
		.corepkgs_dir(Some(noxi_glue::store::COREPKGS_DIR.into()))
		.with_source_map(template.source_map())
		.io_handle(Box::new(PureIO { store }) as Box<dyn EvalIO>)
		.env(Some(env))
		.mode(mode)
		.max_call_depth(max_depth)
		.pure_eval(true)
		.build()
		.hide_root_source()
		.with_source_fragments(fragments)
		.evaluate_in_dir(driver, cwd.to_path_buf())
}
