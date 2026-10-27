//! This module implements generator logic for the VM. Generators are functions
//! used during evaluation which can suspend their execution during their
//! control flow, and request that the VM do something.
//!
//! This is used to keep the VM's stack size constant even when evaluating
//! deeply nested recursive data structures.
//!
//! We implement generators using the [`genawaiter`] crate.

use core::pin::Pin;
use genawaiter::rc::Co;
pub use genawaiter::rc::Gen;
use std::ffi::OsString;
use std::fmt::Display;
use std::future::Future;

use crate::FileType;
use crate::NixSearchPathEntry;
use crate::NixString;
use crate::value::{PointerEquality, PrintOptions, ValuePrinter};
use crate::warnings::{EvalWarning, WarningKind};

use super::*;

// -- Implementation of generic generator logic.

/// States that a generator can be in while being driven by the VM.
pub(crate) enum GeneratorState {
	/// Normal execution of the generator.
	Running,

	/// Generator is awaiting the result of a forced value.
	AwaitingValue,
}

/// Messages that can be sent from generators *to* the VM. In most
/// cases, the VM will suspend the generator when receiving a message
/// and enter some other frame to process the request.
///
/// Responses are returned to generators via the [`VMResponse`] type.
pub enum VMRequest {
	/// Look up a builtin in this evaluation, preserving its compiler globals.
	Builtin(&'static str),
	/// Request that the VM forces this value. This message is first sent to the
	/// VM with the unforced value, then returned to the generator with the
	/// forced result.
	ForceValue(Value),

	/// Request that the VM deep-forces the value.
	DeepForceValue(Value),

	/// Request the value at the given index from the VM's with-stack, in forced
	/// state.
	///
	/// The value is returned in the `ForceValue` message.
	WithValue(usize),

	/// Request the value at the given index from the *captured* with-stack, in
	/// forced state.
	CapturedWithValue(usize),

	/// Request that the two values be compared for Nix equality. The result is
	/// returned in the `ForceValue` message.
	NixEquality(Box<(Value, Value)>, PointerEquality),

	/// Push the given value to the VM's stack. This is used to prepare the
	/// stack for requesting a function call from the VM.
	///
	/// The VM does not respond to this request, so the next message received is
	/// `Empty`.
	StackPush(Value),

	/// Pop a value from the stack and return it to the generator.
	StackPop,

	/// Request that the VM coerces this value to a string.
	/// Coerce a value to a string, with the CppNix trace frame of a failure.
	StringCoerce(Value, CoercionKind, Option<&'static str>),

	/// Request that the VM calls the given value, with arguments already
	/// prepared on the stack. Value must already be forced.
	Call(Value),

	/// Request a bytecode frame entering the given lambda immediately. This can be
	/// used to force thunks.
	EnterLambda {
		lambda: Rc<Lambda>,
		upvalues: FrameUpvalues,
		span: Span,
	},

	/// Emit a runtime warning (already containing a span) through the VM.
	EmitWarning(EvalWarning),

	/// Emit a runtime warning through the VM. The span of the current generator
	/// is used for the final warning.
	EmitWarningKind(WarningKind),

	/// Request a lookup in the VM's import cache, which tracks the
	/// thunks yielded by previously imported files.
	ImportCacheLookup(PathBuf),

	/// Provide the VM with an imported value for a given path, which
	/// it can populate its input cache with.
	ImportCachePut(PathBuf, Value),

	/// Request that the VM imports the given path through its I/O interface.
	PathImport(PathBuf),

	/// Request that the VM opens the specified file and provides a reader.
	OpenFile(PathBuf),

	/// Read a contextual Nix string from a file.
	ReadFile(PathBuf),

	/// Resolve an expression path through the evaluation filesystem.
	ResolveImportPath {
		path: PathBuf,
		memoize: bool,
	},

	/// Realize a path string through the evaluator's store integration.
	RealisePath(NixString),

	/// Read the raw attributes of a registered derivation, if applicable.
	ImportDerivation(PathBuf),

	/// Request that the VM checks whether the given path exists.
	PathExists(PathBuf),

	/// Request that the VM reads the given path.
	ReadDir(PathBuf),

	/// Request a reasonable span from the VM.
	Span,

	/// Request evaluation of `builtins.tryEval` from the VM.
	TryForce(Value),

	/// Request the VM for the file type of the given path.
	ReadFileType(PathBuf),

	// Request that the VM reads the given environment variable.
	GetEnv(OsString),

	/// Request column, line and file name for the given span.
	/// Used by builtins.unsafeGetAttrPos
	GetSpanPos(Span),

	/// Request that the VM looks `path` up in a lookup path of `entries`.
	FindFile(Vec<NixSearchPathEntry>, String),

	/// Request that the VM prints the value as CppNix's `ValuePrinter`
	/// does, which needs the source positions of functions.
	Print(Value, PrintOptions),
}

/// Human-readable representation of a generator message, used by observers.
impl Display for VMRequest {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			VMRequest::ForceValue(v) => write!(f, "force_value({})", v.type_of()),
			VMRequest::DeepForceValue(v) => {
				write!(f, "deep_force_value({})", v.type_of())
			}
			VMRequest::WithValue(_) => write!(f, "with_value"),
			VMRequest::CapturedWithValue(_) => write!(f, "captured_with_value"),
			VMRequest::NixEquality(values, ptr_eq) => {
				write!(
					f,
					"nix_eq({}, {}, PointerEquality::{:?})",
					values.0.type_of(),
					values.1.type_of(),
					ptr_eq
				)
			}
			VMRequest::StackPush(v) => write!(f, "stack_push({})", v.type_of()),
			VMRequest::StackPop => write!(f, "stack_pop"),
			VMRequest::StringCoerce(
				v,
				CoercionKind {
					strong,
					import_paths,
				},
				_,
			) => write!(
				f,
				"{}_{}importing_string_coerce({})",
				if *strong { "strong" } else { "weak" },
				if *import_paths { "" } else { "non_" },
				v.type_of()
			),
			VMRequest::Builtin(name) => write!(f, "builtin({name})"),
			VMRequest::Call(v) => write!(f, "call({v})"),
			VMRequest::EnterLambda { lambda, .. } => {
				write!(f, "enter_lambda({:p})", *lambda)
			}
			VMRequest::EmitWarning(_) => write!(f, "emit_warning"),
			VMRequest::EmitWarningKind(_) => write!(f, "emit_warning_kind"),
			VMRequest::ImportCacheLookup(p) => {
				write!(f, "import_cache_lookup({})", p.to_string_lossy())
			}
			VMRequest::ImportCachePut(p, _) => {
				write!(f, "import_cache_put({})", p.to_string_lossy())
			}
			VMRequest::PathImport(p) => write!(f, "path_import({})", p.to_string_lossy()),
			VMRequest::OpenFile(p) => {
				write!(f, "open_file({})", p.to_string_lossy())
			}
			VMRequest::ResolveImportPath { path: p, .. } => {
				write!(f, "resolve_import_path({})", p.to_string_lossy())
			}
			VMRequest::RealisePath(p) => write!(f, "realise_path({p})"),
			VMRequest::ImportDerivation(p) => {
				write!(f, "import_derivation({})", p.to_string_lossy())
			}
			VMRequest::ReadFile(p) => write!(f, "read_file({})", p.to_string_lossy()),
			VMRequest::PathExists(p) => write!(f, "path_exists({})", p.to_string_lossy()),
			VMRequest::ReadDir(p) => write!(f, "read_dir({})", p.to_string_lossy()),
			VMRequest::Span => write!(f, "span"),
			VMRequest::TryForce(v) => write!(f, "try_force({})", v.type_of()),
			VMRequest::ReadFileType(p) => write!(f, "read_file_type({})", p.to_string_lossy()),
			VMRequest::GetEnv(p) => write!(f, "get_env({})", p.to_string_lossy()),
			VMRequest::GetSpanPos(span) => write!(f, "get_span_pos({:?})", span),
			VMRequest::FindFile(_, path) => write!(f, "find_file({path})"),
			VMRequest::Print(v, _) => write!(f, "print({})", v.type_of()),
		}
	}
}

/// Responses returned to generators *from* the VM.
pub enum VMResponse {
	/// Empty message. Passed to the generator as the first message,
	/// or when return values were optional.
	Empty,

	/// Value produced by the VM and returned to the generator.
	Value(Value),

	/// Path produced by the VM in response to some IO operation.
	Path(PathBuf),

	/// VM response with the contents of a directory.
	Directory(Vec<(bytes::Bytes, FileType)>),

	/// VM response with a span to use at the current point.
	Span(Span),

	/// Reader produced by the VM in response to some IO operation.
	Reader(Box<dyn std::io::Read>),

	FileType(FileType),

	/// environment variable
	Env(OsString),

	GetSpanPos(Value),
}

impl Display for VMResponse {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			VMResponse::Empty => write!(f, "empty"),
			VMResponse::Value(v) => write!(f, "value({v})"),
			VMResponse::Path(p) => write!(f, "path({})", p.to_string_lossy()),
			VMResponse::Directory(d) => write!(f, "dir(len = {})", d.len()),
			VMResponse::Span(_) => write!(f, "span"),
			VMResponse::Reader(_) => write!(f, "reader"),
			VMResponse::FileType(t) => write!(f, "file_type({t})"),
			VMResponse::Env(t) => write!(f, "env({})", t.to_string_lossy()),
			VMResponse::GetSpanPos(pos) => write!(f, "get_span_pos({:?})", pos),
		}
	}
}

pub(crate) type Generator =
	Gen<VMRequest, VMResponse, Pin<Box<dyn Future<Output = Result<Value, ErrorKind>>>>>;

/// Helper function to provide type annotations which are otherwise difficult to
/// infer.
pub fn pin_generator(
	f: impl Future<Output = Result<Value, ErrorKind>> + 'static,
) -> Pin<Box<dyn Future<Output = Result<Value, ErrorKind>>>> {
	Box::pin(f)
}

impl<IO> VM<'_, IO>
where
	IO: AsRef<dyn EvalIO> + 'static,
{
	/// Helper function to re-enqueue the current generator while it
	/// is awaiting a value.
	fn reenqueue_generator(&mut self, name: &'static str, span: Span, generator: Generator) {
		self.frames.push(Frame::Generator {
			name,
			generator,
			span,
			state: GeneratorState::AwaitingValue,
			depth: self.call_depth,
			// The generator was running, so the running trace is its own; once it
			// is back on the frame stack, `traces` no longer reads it.
			trace: std::mem::take(&mut self.current.1),
		});
	}

	/// Helper function to enqueue a new generator.
	pub(super) fn enqueue_generator<F, G>(&mut self, name: &'static str, span: Span, r#gen: G)
	where
		F: Future<Output = Result<Value, ErrorKind>> + 'static,
		G: FnOnce(GenCo) -> F,
	{
		self.frames.push(Frame::Generator {
			name,
			span,
			state: GeneratorState::Running,
			generator: Gen::new(|co| pin_generator(r#gen(co))),
			depth: self.call_depth,
			trace: FrameTrace::None,
		});
	}

	/// Run a generator frame until it yields to the outer control loop, or runs
	/// to completion.
	///
	/// The return value indicates whether the generator has completed (true),
	/// or was suspended (false).
	pub(crate) fn run_generator(
		&mut self,
		name: &'static str,
		span: Span,
		frame_id: usize,
		state: GeneratorState,
		mut generator: Generator,
		initial_message: Option<VMResponse>,
	) -> EvalResult<bool> {
		// Determine what to send to the generator based on its state.
		let mut message = match (initial_message, state) {
			(Some(msg), _) => msg,
			(_, GeneratorState::Running) => VMResponse::Empty,

			// If control returned here, and the generator is
			// awaiting a value, send it the top of the stack.
			(_, GeneratorState::AwaitingValue) => VMResponse::Value(self.stack_pop()),
		};

		loop {
			match generator.resume_with(message) {
				// If the generator yields, it contains an instruction
				// for what the VM should do.
				genawaiter::GeneratorState::Yielded(request) => {
					self.observer.observe_generator_request(name, &request);

					match request {
						VMRequest::StackPush(value) => {
							self.stack.push(value);
							message = VMResponse::Empty;
						}

						VMRequest::StackPop => {
							message = VMResponse::Value(self.stack_pop());
						}

						// Generator has requested a force, which means that
						// this function prepares the frame stack and yields
						// back to the outer VM loop.
						VMRequest::ForceValue(value) => {
							self.reenqueue_generator(name, span, generator);
							self.enqueue_force_value(span, value);
							return Ok(false);
						}

						// Generator has requested a deep-force.
						VMRequest::DeepForceValue(value) => {
							self.reenqueue_generator(name, span, generator);
							let path = Rc::new(std::cell::RefCell::new(vec![]));
							self.enqueue_generator("deep_force", span, {
								let path = path.clone();
								|co| value.deep_force(co, span, path)
							});
							if let Some(Frame::Generator { trace, .. }) = self.frames.last_mut() {
								*trace = FrameTrace::Dynamic(path);
							}
							return Ok(false);
						}

						// Generator has requested a value from the with-stack.
						// Logic is similar to `ForceValue`, except with the
						// value being taken from that stack.
						VMRequest::WithValue(idx) => {
							self.reenqueue_generator(name, span, generator);

							let value = self.stack[self.with_stack[idx]].clone();
							self.enqueue_force_value(span, value);

							return Ok(false);
						}

						// Generator has requested a value from the *captured*
						// with-stack. Logic is same as above, except for the
						// value being from that stack.
						VMRequest::CapturedWithValue(idx) => {
							self.reenqueue_generator(name, span, generator);

							let bytecode_frame = self.last_bytecode_frame().expect(
								"nix-rs bug: generator requested captured with-value, but there is no bytecode frame",
							);

							let value = bytecode_frame
								.upvalues
								.get_from_with_stack(idx)
								.expect("nix-rs bug: upvalue not found on stack");
							self.enqueue_force_value(span, value);

							return Ok(false);
						}

						VMRequest::NixEquality(values, ptr_eq) => {
							let values = *values;
							self.reenqueue_generator(name, span, generator);
							self.enqueue_generator("nix_eq", span, |co| {
								values.0.nix_eq_owned_genco(values.1, co, ptr_eq, span)
							});
							return Ok(false);
						}

						VMRequest::StringCoerce(val, kind, context) => {
							self.reenqueue_generator(name, span, generator);
							self.enqueue_generator("coerce_to_string", span, |co| {
								val.coerce_to_string(co, kind, span)
							});
							if let Some(context) = context
								&& let Some(Frame::Generator { trace, .. }) = self.frames.last_mut()
							{
								*trace = FrameTrace::Context(context);
							}
							return Ok(false);
						}

						VMRequest::Call(callable) => {
							self.reenqueue_generator(name, span, generator);
							self.call_value(span, None, callable)?;
							return Ok(false);
						}

						VMRequest::EnterLambda {
							lambda,
							upvalues,
							span,
						} => {
							self.reenqueue_generator(name, span, generator);

							self.frames.push(Frame::BytecodeFrame {
								span,
								bytecode_frame: BytecodeFrame {
									trace: FrameTrace::None,
									lambda,
									upvalues,
									ip: CodeIdx(0),
									stack_offset: self.stack.len(),
									depth: self.call_depth,
								},
							});

							return Ok(false);
						}

						VMRequest::EmitWarning(warning) => {
							self.push_warning(warning);
							message = VMResponse::Empty;
						}

						VMRequest::EmitWarningKind(kind) => {
							self.emit_warning(kind);
							message = VMResponse::Empty;
						}

						VMRequest::Builtin(name) => {
							let attrs = self.globals["builtins"]
								.to_attrs()
								.expect("builtins is an attribute set");
							message = VMResponse::Value(
								attrs
									.select_required(name.as_bytes())
									.with_span(span, self)?
									.clone(),
							);
						}

						VMRequest::ImportCacheLookup(path) => {
							if let Some(cached) = self.import_cache.get(&path) {
								message = VMResponse::Value(cached.clone());
							} else {
								message = VMResponse::Empty;
							}
						}

						VMRequest::ImportCachePut(path, value) => {
							self.import_cache.insert(path, value);
							message = VMResponse::Empty;
						}

						VMRequest::PathImport(path) => {
							let imported = if let Some(p) = self.path_import_cache.get(&path) {
								p.to_owned()
							} else {
								let imported = self
									.io_handle
									.as_ref()
									.import_path(&path)
									.map_err(|e| ErrorKind::IO {
										path: Some(path.to_owned()),
										error: e.into(),
									})
									.with_span(span, self)?;

								self.path_import_cache.insert(path, imported.clone());
								imported
							};

							message = VMResponse::Path(imported);
						}

						VMRequest::OpenFile(path) => {
							let reader = self
								.io_handle
								.as_ref()
								.open(&path)
								.map_err(|e| ErrorKind::IO {
									path: Some(path),
									error: e.into(),
								})
								.with_span(span, self)?;

							message = VMResponse::Reader(reader)
						}

						VMRequest::ResolveImportPath { path, memoize } => {
							let cached = memoize
								.then(|| self.import_resolution_cache.get(&path))
								.flatten();
							let resolved = if let Some(cached) = cached {
								cached.clone()
							} else {
								let resolved = self
									.io_handle
									.as_ref()
									.resolve_import_path(&path)
									.map_err(|e| ErrorKind::IO {
										path: Some(path.clone()),
										error: e.into(),
									})
									.with_span(span, self)?;
								if memoize {
									self.import_resolution_cache.insert(path, resolved.clone());
								}
								resolved
							};
							message = VMResponse::Path(resolved);
						}

						VMRequest::RealisePath(path) => {
							let realised = self
								.io_handle
								.as_ref()
								.realise_path(&path)
								.map_err(|e| ErrorKind::IO {
									path: path.to_path().ok().map(PathBuf::from),
									error: e.into(),
								})
								.with_span(span, self)?;
							message = VMResponse::Path(realised);
						}

						VMRequest::ImportDerivation(path) => {
							let attrs = self
								.io_handle
								.as_ref()
								.import_derivation(&path)
								.map_err(|e| ErrorKind::IO {
									path: Some(path),
									error: e.into(),
								})
								.with_span(span, self)?;
							message = VMResponse::Value(attrs.unwrap_or(Value::Null));
						}

						VMRequest::ReadFile(path) => {
							let string = self
								.io_handle
								.as_ref()
								.read_file(&path)
								.map_err(|e| ErrorKind::IO {
									path: Some(path),
									error: e.into(),
								})
								.with_span(span, self)?;
							message = VMResponse::Value(Value::String(string));
						}

						VMRequest::PathExists(path) => {
							let exists = self
								.io_handle
								.as_ref()
								.path_exists(&path)
								.map_err(|e| ErrorKind::IO {
									path: Some(path),
									error: e.into(),
								})
								.map(Value::Bool)
								.with_span(span, self)?;

							message = VMResponse::Value(exists);
						}

						VMRequest::ReadDir(path) => {
							let dir = self
								.io_handle
								.as_ref()
								.read_dir(&path)
								.map_err(|e| ErrorKind::IO {
									path: Some(path),
									error: e.into(),
								})
								.with_span(span, self)?;
							message = VMResponse::Directory(dir);
						}

						VMRequest::FindFile(entries, path) => {
							let found = self
								.nix_search_path
								.with_entries(entries)
								.resolve(&self.io_handle, &path);
							message = VMResponse::Value(found.into());
						}

						VMRequest::Span => {
							message = VMResponse::Span(self.reasonable_span);
						}

						VMRequest::TryForce(value) => {
							self.try_eval_frames.push(frame_id);
							self.reenqueue_generator(name, span, generator);

							debug_assert!(
								self.frames.len() == frame_id + 1,
								"generator should be reenqueued with the same frame ID"
							);

							self.enqueue_generator("force", span, |co| {
								value.force_owned_genco(co, span)
							});
							return Ok(false);
						}

						VMRequest::ReadFileType(path) => {
							let file_type = self
								.io_handle
								.as_ref()
								.file_type(&path)
								.map_err(|e| ErrorKind::IO {
									path: Some(path),
									error: e.into(),
								})
								.with_span(span, self)?;

							message = VMResponse::FileType(file_type);
						}
						VMRequest::GetEnv(key) => {
							let env = self.io_handle.as_ref().get_env(&key).unwrap_or_default();

							message = VMResponse::Env(env);
						}

						VMRequest::GetSpanPos(span) => {
							let file = self.source.get_file(span);
							let pos = file.find_line_col(span.low());
							let abs_path = std::fs::canonicalize(file.name())
								// canonicalize might return an error if
								// span points to an attrset generated by builtins.derivation
								.unwrap_or(PathBuf::from(file.name()))
								.to_string_lossy()
								.to_string();

							let v = NixAttrs::from_iter([
								("line", Value::Integer((pos.line + 1) as i64)),
								("column", Value::Integer((pos.column + 1) as i64)),
								("file", Value::String(NixString::from(abs_path))),
							]);
							message = VMResponse::GetSpanPos(Value::Attrs(v));
						}

						VMRequest::Print(value, options) => {
							let printed = ValuePrinter::print(&self.source, options, &value);
							message = VMResponse::Value(NixString::from(printed).into());
						}
					}
				}

				// Generator has completed, and its result value should
				// be left on the stack.
				genawaiter::GeneratorState::Complete(result) => {
					let value = result.with_span(span, self)?;
					let value = self.trace_catchable(value, None, None);
					self.stack.push(value);
					return Ok(true);
				}
			}
		}
	}
}

pub type GenCo = Co<VMRequest, VMResponse>;

// -- Implementation of concrete generator use-cases.

/// Request that the VM place the given value on its stack.
pub async fn request_stack_push(co: &GenCo, val: Value) {
	match co.yield_(VMRequest::StackPush(val)).await {
		VMResponse::Empty => {}
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

/// Request that the VM pop a value from the stack and return it to the
/// generator.
pub async fn request_stack_pop(co: &GenCo) -> Value {
	match co.yield_(VMRequest::StackPop).await {
		VMResponse::Value(value) => value,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

/// Force any value and return the evaluated result from the VM.
pub async fn request_force(co: &GenCo, val: Value) -> Value {
	if let Value::Thunk(thunk) = val {
		let thunk = match thunk.force_if_evaluated() {
			Ok(value) => return value,
			Err(thunk) => thunk,
		};
		match co.yield_(VMRequest::ForceValue(Value::Thunk(thunk))).await {
			VMResponse::Value(value) => value,
			msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
		}
	} else {
		val
	}
}

/// Return a builtin from the active evaluation. Store-aware builtins use this
/// to import Nix helpers with the caller's globals and import cache.
pub async fn request_builtin(co: &GenCo, name: &'static str) -> Value {
	match co.yield_(VMRequest::Builtin(name)).await {
		VMResponse::Value(value) => value,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

/// Force a value
pub(crate) async fn request_try_force(co: &GenCo, val: Value) -> Value {
	if let Value::Thunk(thunk) = val {
		let thunk = match thunk.force_if_evaluated() {
			Ok(value) => return value,
			Err(thunk) => thunk,
		};
		match co.yield_(VMRequest::TryForce(Value::Thunk(thunk))).await {
			VMResponse::Value(value) => value,
			msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
		}
	} else {
		val
	}
}

/// Call the given value as a callable. The argument(s) must already be prepared
/// on the stack.
pub async fn request_call(co: &GenCo, val: Value) -> Value {
	let val = request_force(co, val).await;
	match co.yield_(VMRequest::Call(val)).await {
		VMResponse::Value(value) => value,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

/// Helper function to call the given value with the provided list of arguments.
/// This uses the StackPush and Call messages under the hood.
pub async fn request_call_with<I>(co: &GenCo, mut callable: Value, args: I) -> Value
where
	I: IntoIterator<Item = Value>,
	I::IntoIter: DoubleEndedIterator,
{
	let mut num_args = 0_usize;
	for arg in args.into_iter().rev() {
		num_args += 1;
		request_stack_push(co, arg).await;
	}

	debug_assert!(num_args > 0, "call_with called with an empty list of args");

	while num_args > 0 {
		callable = request_call(co, callable).await;
		num_args -= 1;
	}

	callable
}

pub async fn request_string_coerce(
	co: &GenCo,
	val: Value,
	kind: CoercionKind,
) -> Result<NixString, CatchableErrorKind> {
	request_string_coerce_in(co, val, kind, None).await
}

/// [`request_string_coerce`] with CppNix's trace frame for a failure, such as
/// `while evaluating the first argument passed to builtins.toString`.
pub async fn request_string_coerce_in(
	co: &GenCo,
	val: Value,
	kind: CoercionKind,
	context: Option<&'static str>,
) -> Result<NixString, CatchableErrorKind> {
	match val {
		Value::String(s) => Ok(s),
		_ => match co.yield_(VMRequest::StringCoerce(val, kind, context)).await {
			VMResponse::Value(Value::Catchable(c)) => Err(*c),
			VMResponse::Value(value) => Ok(value
				.to_contextful_str()
				.expect("coerce_to_string always returns a string")),
			msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
		},
	}
}

/// Deep-force any value and return the evaluated result from the VM.
pub async fn request_deep_force(co: &GenCo, val: Value) -> Value {
	match co.yield_(VMRequest::DeepForceValue(val)).await {
		VMResponse::Value(value) => value,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

/// Ask the VM to compare two values for equality.
pub(crate) async fn check_equality(
	co: &GenCo,
	a: Value,
	b: Value,
	ptr_eq: PointerEquality,
) -> Result<Result<bool, CatchableErrorKind>, ErrorKind> {
	match co
		.yield_(VMRequest::NixEquality(Box::new((a, b)), ptr_eq))
		.await
	{
		VMResponse::Value(Value::Bool(b)) => Ok(Ok(b)),
		VMResponse::Value(Value::Catchable(cek)) => Ok(Err(*cek)),
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

/// Emit a fully constructed runtime warning.
pub(crate) async fn emit_warning(co: &GenCo, warning: EvalWarning) {
	match co.yield_(VMRequest::EmitWarning(warning)).await {
		VMResponse::Empty => {}
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

/// Emit a runtime warning with the span of the current generator.
pub async fn emit_warning_kind(co: &GenCo, kind: WarningKind) {
	match co.yield_(VMRequest::EmitWarningKind(kind)).await {
		VMResponse::Empty => {}
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

/// Request that the VM enter the given lambda.
pub(crate) async fn request_enter_lambda(
	co: &GenCo,
	lambda: Rc<Lambda>,
	upvalues: Upvalues,
	span: Span,
) -> Value {
	let msg = VMRequest::EnterLambda {
		lambda,
		upvalues: upvalues.into(),
		span,
	};

	match co.yield_(msg).await {
		VMResponse::Value(value) => value,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

/// Request a lookup in the VM's import cache.
pub(crate) async fn request_import_cache_lookup(co: &GenCo, path: PathBuf) -> Option<Value> {
	match co.yield_(VMRequest::ImportCacheLookup(path)).await {
		VMResponse::Value(value) => Some(value),
		VMResponse::Empty => None,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

/// Request that the VM populate its input cache for the given path.
pub(crate) async fn request_import_cache_put(co: &GenCo, path: PathBuf, value: Value) {
	match co.yield_(VMRequest::ImportCachePut(path, value)).await {
		VMResponse::Empty => {}
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

/// Request that the VM import the given path.
pub(crate) async fn request_path_import(co: &GenCo, path: PathBuf) -> PathBuf {
	match co.yield_(VMRequest::PathImport(path)).await {
		VMResponse::Path(path) => path,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

/// Resolve an import through the evaluator's filesystem rather than the host.
pub(crate) async fn request_resolve_import_path(
	co: &GenCo,
	path: PathBuf,
	memoize: bool,
) -> PathBuf {
	match co
		.yield_(VMRequest::ResolveImportPath { path, memoize })
		.await
	{
		VMResponse::Path(path) => path,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

/// Realize a path's string context before reading its file.
pub async fn request_realise_path(co: &GenCo, path: NixString) -> PathBuf {
	match co.yield_(VMRequest::RealisePath(path)).await {
		VMResponse::Path(path) => path,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

pub(crate) async fn request_import_derivation(co: &GenCo, path: PathBuf) -> Option<Value> {
	match co.yield_(VMRequest::ImportDerivation(path)).await {
		VMResponse::Value(Value::Null) => None,
		VMResponse::Value(attrs) => Some(attrs),
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

pub(crate) async fn request_read_file(co: &GenCo, path: PathBuf) -> Value {
	match co.yield_(VMRequest::ReadFile(path)).await {
		VMResponse::Value(string) => string,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

/// Request that the VM open a [std::io::Read] for the specified file.
pub async fn request_open_file(co: &GenCo, path: PathBuf) -> Box<dyn std::io::Read> {
	match co.yield_(VMRequest::OpenFile(path)).await {
		VMResponse::Reader(value) => value,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

#[cfg_attr(not(feature = "impure"), allow(unused))]
pub(crate) async fn request_find_file(
	co: &GenCo,
	entries: Vec<NixSearchPathEntry>,
	path: String,
) -> Value {
	match co.yield_(VMRequest::FindFile(entries, path)).await {
		VMResponse::Value(value) => value,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

#[cfg_attr(not(feature = "impure"), allow(unused))]
pub(crate) async fn request_path_exists(co: &GenCo, path: PathBuf) -> Value {
	match co.yield_(VMRequest::PathExists(path)).await {
		VMResponse::Value(value) => value,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

#[cfg_attr(not(feature = "impure"), allow(unused))]
pub(crate) async fn request_read_dir(co: &GenCo, path: PathBuf) -> Vec<(bytes::Bytes, FileType)> {
	match co.yield_(VMRequest::ReadDir(path)).await {
		VMResponse::Directory(dir) => dir,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

pub(crate) async fn request_span(co: &GenCo) -> Span {
	match co.yield_(VMRequest::Span).await {
		VMResponse::Span(span) => span,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

/// Print a value as CppNix's `ValuePrinter` does.
pub async fn request_print(co: &GenCo, value: Value, options: PrintOptions) -> NixString {
	match co.yield_(VMRequest::Print(value, options)).await {
		VMResponse::Value(Value::String(s)) => s,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

pub(crate) async fn request_span_pos(co: &GenCo, span: Span) -> Value {
	match co.yield_(VMRequest::GetSpanPos(span)).await {
		VMResponse::GetSpanPos(span) => span,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

#[cfg_attr(not(feature = "impure"), allow(unused))]
pub(crate) async fn request_read_file_type(co: &GenCo, path: PathBuf) -> FileType {
	match co.yield_(VMRequest::ReadFileType(path)).await {
		VMResponse::FileType(file_type) => file_type,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

#[cfg_attr(not(feature = "impure"), allow(unused))]
pub(crate) async fn request_get_env(co: &GenCo, key: OsString) -> OsString {
	match co.yield_(VMRequest::GetEnv(key)).await {
		VMResponse::Env(env) => env,
		msg => panic!("nix-rs bug: VM responded with incorrect generator message: {msg}"),
	}
}

/// Call the given value as if it was an attribute set containing a functor. The
/// arguments must already be prepared on the stack when a generator frame from
/// this function is invoked.
///
pub(crate) async fn call_functor(co: GenCo, value: Value) -> Result<Value, ErrorKind> {
	let attrs = value.to_attrs()?;

	match attrs.select("__functor") {
		None => Err(ErrorKind::NotCallable(
			"set without `__functor_` attribute",
			None,
		)),
		Some(functor) => {
			// The functor receives the set itself as its first argument and
			// needs to be called with it.
			let functor = request_force(&co, functor.clone()).await;
			let primed = request_call_with(&co, functor, [value]).await;
			Ok(request_call(&co, primed).await)
		}
	}
}
