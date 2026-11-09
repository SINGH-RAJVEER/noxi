//! This module implements the abstract/virtual machine that runs noxi
//! bytecode.
//!
//! The operation of the VM is facilitated by the [`Frame`] type,
//! which controls the current execution state of the VM and is
//! processed within the VM's operating loop.
//!
//! A [`VM`] is used by instantiating it with an initial [`Frame`],
//! then triggering its execution and waiting for the VM to return or
//! yield an error.

mod assertions;
pub mod generators;
mod macros;
pub(crate) mod native;

use bstr::{BString, ByteSlice, ByteVec};
use codemap::Span;
use rustc_hash::FxHashMap;
use serde_json::json;
use std::{
	cmp::Ordering,
	ffi::OsStr,
	path::{Path, PathBuf},
	rc::Rc,
};

use crate::{
	NixString, SourceCode, arithmetic_op,
	chunk::Chunk,
	cmp_op,
	compiler::GlobalsMap,
	errors::{AddContext, CatchableErrorKind, CatchableTrace, Error, ErrorKind, EvalResult, Trace},
	io::EvalIO,
	lifted_pop,
	nix_search_path::NixSearchPath,
	observer::{OptionalRuntimeObserver, RuntimeObserver},
	opcode::{CodeIdx, Op, Position, UpvalueIdx},
	upvalues::{FrameUpvalues, UpvalueData, Upvalues},
	value::{
		Builtin, BuiltinResult, Closure, CoercionKind, Lambda, NixAttrs, NixContext, NixList,
		PointerEquality, Thunk, Value, canon_path,
	},
	vm::generators::GenCo,
	warnings::{EvalWarning, WarningKind},
};

use generators::{Generator, GeneratorState, call_functor};

use self::generators::{VMRequest, VMResponse};

/// Internal helper trait for taking a span from a variety of types, to make use
/// of `WithSpan` (defined below) more ergonomic at call sites.
trait GetSpan {
	fn get_span(self) -> Span;

	/// The trace of the running frame, if this is it.
	fn frame_trace(&self) -> Option<FrameTrace> {
		None
	}
}

impl<IO> GetSpan for &VM<'_, IO> {
	fn get_span(self) -> Span {
		self.reasonable_span
	}
}

impl GetSpan for &BytecodeFrame {
	fn get_span(self) -> Span {
		self.current_span()
	}

	fn frame_trace(&self) -> Option<FrameTrace> {
		Some(self.trace.clone())
	}
}

/// A span in the running frame, which supplies its trace.
impl GetSpan for (Span, &BytecodeFrame) {
	fn get_span(self) -> Span {
		self.0
	}

	fn frame_trace(&self) -> Option<FrameTrace> {
		Some(self.1.trace.clone())
	}
}

impl GetSpan for &Span {
	fn get_span(self) -> Span {
		*self
	}
}

impl GetSpan for Span {
	fn get_span(self) -> Span {
		self
	}
}

/// Internal helper trait for ergonomically converting from a `Result<T,
/// ErrorKind>` to a `Result<T, Error>` using the current span of a bytecode frame,
/// and chaining the VM's frame stack around it for printing a cause chain.
trait WithSpan<T, S: GetSpan, IO> {
	fn with_span(self, top_span: S, vm: &VM<IO>) -> Result<T, Error>;
}

/// The most frames of the frame stack an error records.
const MAX_TRACE_FRAMES: usize = 1000;

impl<T, S: GetSpan, IO> WithSpan<T, S, IO> for Result<T, ErrorKind> {
	fn with_span(self, top_span: S, vm: &VM<IO>) -> Result<T, Error> {
		match self {
			Ok(something) => Ok(something),
			Err(kind) => {
				let running = top_span.frame_trace();
				let mut error = Error::new(kind, top_span.get_span(), vm.source.clone());
				let traces = vm.traces(Some(&error.kind), running.as_ref());

				// Wrap the top-level error in chaining errors for each element
				// of the frame stack. Recursion can leave many thousands of
				// frames, and so many nested errors would overflow the stack
				// when dropped or printed, so only the innermost are kept.
				for frame in vm.frames.iter().rev().take(MAX_TRACE_FRAMES) {
					match frame {
						Frame::BytecodeFrame { span, .. } => {
							error = Error::new(
								ErrorKind::BytecodeError(Box::new(error)),
								*span,
								vm.source.clone(),
							);
						}
						Frame::Generator { name, span, .. } => {
							error = Error::new(
								ErrorKind::NativeError {
									err: Box::new(error),
									gen_type: name,
								},
								*span,
								vm.source.clone(),
							);
						}
						Frame::Builtin { name, span, .. } => {
							error = Error::new(
								ErrorKind::NativeError {
									err: Box::new(error),
									gen_type: name,
								},
								*span,
								vm.source.clone(),
							);
						}
						Frame::ErrorContext { .. } | Frame::ContextMessage { .. } => {}
						Frame::Force { span, .. } => {
							error = Error::new(
								ErrorKind::NativeError {
									err: Box::new(error),
									gen_type: "force",
								},
								*span,
								vm.source.clone(),
							);
						}
					}
				}

				error.traces = traces;
				Err(error)
			}
		}
	}
}

struct BytecodeFrame {
	/// The lambda currently being executed.
	lambda: Rc<Lambda>,

	/// The CppNix trace frames this frame contributes to errors.
	trace: FrameTrace,

	/// Optional captured upvalues of this frame (if a thunk or
	/// closure if being evaluated).
	upvalues: FrameUpvalues,

	/// Instruction pointer to the instruction currently being
	/// executed.
	ip: CodeIdx,

	/// Stack offset, i.e. the frames "view" into the VM's full stack.
	stack_offset: usize,

	/// The function calls active when this frame runs, itself included if
	/// it is one; see [`VM::call_depth`].
	depth: usize,
}

impl BytecodeFrame {
	/// Retrieve an upvalue from this frame at the given index.
	fn upvalue(&self, idx: UpvalueIdx) -> &Value {
		&self.upvalues[idx]
	}

	/// Borrow the chunk of this frame's lambda.
	fn chunk(&self) -> &Chunk {
		&self.lambda.chunk
	}

	/// Increment this frame's instruction pointer and return the operation that
	/// the pointer moved past.
	fn inc_ip(&mut self) -> Op {
		debug_assert!(
			self.ip.0 < self.chunk().code.len(),
			"out of bounds code at IP {} in {:p}",
			self.ip.0,
			self.lambda
		);

		let op = self.chunk().code[self.ip.0];
		self.ip += 1;
		op.into()
	}

	/// Read a varint-encoded operand and return it. The frame pointer is
	/// incremented internally.
	#[inline]
	fn read_uvarint(&mut self) -> u64 {
		// Most operands fit in vu128's single-byte representation.
		let first = self.chunk().code[self.ip.0];
		if first < 0x80 {
			self.ip.0 += 1;
			return first.into();
		}
		let (arg, size) = self.chunk().read_uvarint(self.ip.0);
		self.ip += size;
		arg
	}

	/// Read a fixed-size u16 and increment the frame pointer.
	fn read_u16(&mut self) -> u16 {
		let arg = self.chunk().read_u16(self.ip.0);
		self.ip += 2;
		arg
	}

	/// Construct an error result from the given ErrorKind and the source span
	/// of the current instruction.
	pub fn error<T, IO>(&self, vm: &VM<IO>, kind: ErrorKind) -> Result<T, Error> {
		Err(kind).with_span(self, vm)
	}

	/// Record the attribute a selection found, for CppNix's trace frame of
	/// the selection.
	fn record_selection(&mut self, attrs: NixAttrs, key: NixString) {
		if let Some(path) = self.lambda.selection_path() {
			self.trace = FrameTrace::Select {
				path: path.clone(),
				attrs,
				key,
			};
		}
	}

	/// Returns the current span. This is potentially expensive and should only
	/// be used when actually constructing an error or warning.
	pub fn current_span(&self) -> Span {
		self.chunk().get_span(self.ip - 1)
	}
}

/// A frame represents an execution state of the VM. The VM has a stack of
/// frames representing the nesting of execution inside of the VM, and operates
/// on the frame at the top.
///
/// When a frame has been fully executed, it is removed from the VM's frame
/// stack and expected to leave a result [`Value`] on the top of the stack.
enum Frame {
	/// A lazy addErrorContext boundary. Errors resume at this frame, so
	/// nested contexts use the VM stack rather than the Rust call stack.
	ErrorContext {
		context: Value,
		span: Span,
		depth: usize,
		stack_base: usize,
		outer_traces: usize,
	},
	ContextMessage {
		result: Result<Value, Error>,
		span: Span,
		depth: usize,
		outer_traces: usize,
	},
	/// A synchronous builtin awaiting its next strict argument.
	Builtin {
		name: &'static str,
		pending: crate::value::PendingBuiltin,
		span: Span,
		depth: usize,
		trace: FrameTrace,
	},
	/// BytecodeFrame represents the execution of noxi bytecode within a thunk,
	/// function or closure.
	BytecodeFrame {
		/// The bytecode frame itself, separated out into another type to pass it
		/// around easily.
		bytecode_frame: BytecodeFrame,

		/// Span from which the bytecode frame was launched.
		span: Span,
	},

	/// Generator represents a frame that can yield further
	/// instructions to the VM while its execution is being driven.
	///
	/// A generator is essentially an asynchronous function that can
	/// be suspended while waiting for the VM to do something (e.g.
	/// thunk forcing), and resume at the same point.
	Generator {
		/// human-readable description of the generator,
		name: &'static str,

		/// Span from which the generator was launched.
		span: Span,

		state: GeneratorState,

		/// Generator itself, which can be resumed with `.resume()`.
		generator: Generator,

		/// The function calls active when this frame runs.
		depth: usize,

		/// The CppNix trace frames this frame contributes to errors.
		trace: FrameTrace,
	},

	/// The `force` generator of a suspended thunk, whose code the VM entered
	/// directly instead. It runs once that code returns, storing the result
	/// and leaving it on the stack as the generator would.
	Force {
		/// The thunk being forced, a blackhole until it is updated.
		thunk: Thunk,

		/// Where the thunk was suspended, the span its generator would have.
		span: Span,

		/// Where the thunk was forced.
		forced_at: Span,

		/// The function calls active when this frame runs.
		depth: usize,
	},
}

/// What a frame contributes to a CppNix-style error trace.
#[derive(Clone, Debug, Default)]
pub(crate) enum FrameTrace {
	/// Thunks, internal generators and the top level add nothing.
	#[default]
	None,
	Scoped {
		parent: Box<FrameTrace>,
		context: Box<FrameTrace>,
	},

	/// A builtin applied to all its arguments: `while calling the 'NAME'
	/// builtin`, at the call unless another builtin made it.
	Builtin {
		name: &'static str,
		call_site: Option<Span>,
		contexts: Vec<Trace>,
		path: Option<Rc<std::cell::RefCell<Vec<Trace>>>>,
	},

	/// The body of a function: `from call site` for a call in Nix code,
	/// then `while calling 'NAME'`, both only with `--show-trace`.
	Lambda {
		lambda: Rc<Lambda>,
		call_site: Option<Span>,
	},

	/// Frames a generator maintains as it runs, such as the path of the
	/// element a deep force is forcing.
	Dynamic(Rc<std::cell::RefCell<Vec<Trace>>>),

	/// The thunk of an attribute selection that found an attribute: `while
	/// evaluating the attribute 'PATH'` at that attribute's definition.
	Select {
		path: Rc<str>,
		attrs: NixAttrs,
		key: NixString,
	},

	/// A fixed frame without a position, such as the context of a string
	/// coercion a builtin requested.
	Context(&'static str),

	/// A fixed frame, such as the condition of an assertion being compared.
	Frame(Trace),
}

/// The trace of frames that contribute none.
const NO_TRACE: &FrameTrace = &FrameTrace::None;

impl Frame {
	pub fn span(&self) -> Span {
		match self {
			Frame::BytecodeFrame { span, .. }
			| Frame::Generator { span, .. }
			| Frame::Builtin { span, .. }
			| Frame::Force { span, .. }
			| Frame::ErrorContext { span, .. }
			| Frame::ContextMessage { span, .. } => *span,
		}
	}

	fn trace(&self) -> &FrameTrace {
		match self {
			Frame::BytecodeFrame { bytecode_frame, .. } => &bytecode_frame.trace,
			Frame::Generator { trace, .. } | Frame::Builtin { trace, .. } => trace,
			Frame::Force { .. } | Frame::ErrorContext { .. } | Frame::ContextMessage { .. } => {
				NO_TRACE
			}
		}
	}
}

/// Values indexed by the expression path resolved through `EvalIO`.
/// Only final symlinks are resolved: ancestor aliases can have distinct
/// relative path semantics, and must not share an evaluation result.
#[derive(Default)]
struct ImportCache(FxHashMap<PathBuf, Value>);

impl ImportCache {
	fn get(&self, path: impl AsRef<Path>) -> Option<&Value> {
		self.0.get(path.as_ref())
	}

	fn insert(&mut self, path: PathBuf, value: Value) -> Option<Value> {
		self.0.insert(path, value)
	}
}

/// Path import cache, mapping absolute file paths to paths in the store.
#[derive(Default)]
struct PathImportCache(FxHashMap<PathBuf, PathBuf>);
impl PathImportCache {
	fn get(&self, path: impl AsRef<Path>) -> Option<PathBuf> {
		self.0.get(path.as_ref()).cloned()
	}

	fn insert(&mut self, path: PathBuf, imported_path: PathBuf) -> Option<PathBuf> {
		self.0.insert(path, imported_path)
	}
}

struct VM<'o, IO> {
	/// VM's frame stack, representing the execution contexts the VM is working
	/// through. Elements are usually pushed when functions are called, or
	/// thunks are being forced.
	frames: Vec<Frame>,

	/// The VM's top-level value stack. Within this stack, each code-executing
	/// frame holds a "view" of the stack representing the slice of the
	/// top-level stack that is relevant to its operation. This is done to avoid
	/// allocating a new `Vec` for each frame's stack.
	pub(crate) stack: Vec<Value>,

	/// Stack indices (absolute indexes into `stack`) of attribute
	/// sets from which variables should be dynamically resolved
	/// (`with`).
	with_stack: Vec<usize>,

	/// Runtime warnings collected during evaluation.
	warnings: Vec<EvalWarning>,

	/// Import cache, mapping absolute file paths to the value that
	/// they compile to. Note that this reuses thunks, too!
	// TODO: should probably be based on a file hash
	pub import_cache: ImportCache,

	/// Ordinary imports keep their resolved path even if a symlink changes.
	import_resolution_cache: FxHashMap<PathBuf, PathBuf>,

	/// Path import cache, mapping absolute file paths to paths in the store.
	// TODO: should probably be based on a file hash
	path_import_cache: PathImportCache,

	/// Data structure holding all source code evaluated in this VM,
	/// used for pretty error reporting.
	source: SourceCode,

	/// Parsed Nix search path, which is used to resolve `<...>`
	/// references.
	nix_search_path: NixSearchPath,

	/// Implementation of I/O operations used for impure builtins and
	/// features like `import`.
	io_handle: IO,

	/// Runtime observer which can print traces of runtime operations.
	observer: OptionalRuntimeObserver<'o>,

	/// Strong reference to the globals, guaranteeing that they are
	/// kept alive for the duration of evaluation.
	///
	/// This is important because recursive builtins (specifically
	/// `import`) hold a weak reference to the builtins, while the
	/// original strong reference is held by the compiler which does
	/// not exist anymore at runtime.
	#[allow(dead_code)]
	globals: Rc<GlobalsMap>,

	/// A reasonably applicable span that can be used for errors in each
	/// execution situation.
	///
	/// The VM should update this whenever control flow changes take place (i.e.
	/// entering or exiting a frame to yield control somewhere).
	reasonable_span: Span,

	/// This field is responsible for handling `builtins.tryEval`. When that
	/// builtin is encountered, it sends a special message to the VM which
	/// pushes the frame index that requested to be informed of catchable
	/// errors in this field.
	///
	/// The frame stack is then laid out like this:
	///
	/// ```notrust
	/// ┌──┬──────────────────────────┐
	/// │ 0│ `Result`-producing frame │
	/// ├──┼──────────────────────────┤
	/// │-1│ `builtins.tryEval` frame │
	/// ├──┼──────────────────────────┤
	/// │..│ ... other frames ...     │
	/// └──┴──────────────────────────┘
	/// ```
	///
	/// Control is yielded to the outer VM loop, which evaluates the next frame
	/// and returns the result itself to the `builtins.tryEval` frame.
	try_eval_frames: Vec<usize>,

	/// The function calls active in the running frame, CppNix's
	/// `callDepth`. Each frame records it, so that a frame resumed after
	/// others ran restores its own.
	call_depth: usize,

	/// CppNix's `max-call-depth`: a call made with more calls than this
	/// already active fails.
	max_call_depth: usize,

	/// The frame the outer loop is running: its index in the frame stack and
	/// its trace, for errors raised before it is pushed back.
	current: (usize, FrameTrace),
}

impl<'o, IO> VM<'o, IO>
where
	IO: AsRef<dyn EvalIO> + 'static,
{
	pub fn new(
		nix_search_path: NixSearchPath,
		io_handle: IO,
		observer: OptionalRuntimeObserver<'o>,
		source: SourceCode,
		globals: Rc<GlobalsMap>,
		reasonable_span: Span,
	) -> Self {
		Self {
			nix_search_path,
			io_handle,
			observer,
			globals,
			reasonable_span,
			source,
			frames: vec![],
			stack: vec![],
			with_stack: vec![],
			warnings: vec![],
			import_cache: Default::default(),
			import_resolution_cache: Default::default(),
			path_import_cache: Default::default(),
			try_eval_frames: vec![],
			call_depth: 0,
			max_call_depth: crate::max_call_depth(),
			current: (0, FrameTrace::None),
		}
	}

	/// Push a bytecode frame onto the frame stack.
	fn push_bytecode_frame(&mut self, span: Span, bytecode_frame: BytecodeFrame) {
		self.frames.push(Frame::BytecodeFrame {
			span,
			bytecode_frame,
		})
	}

	/// Force a thunk as a `force` generator at `span` would. The code of a
	/// suspended thunk is entered directly, under a [`Frame::Force`] that
	/// stands in for the generator: the same frames, spans and blackhole.
	fn enqueue_force(&mut self, span: Span, thunk: Thunk) {
		match thunk.begin_direct_force(span) {
			Some((lambda, upvalues, suspended_at)) => {
				self.frames.push(Frame::Force {
					thunk,
					span: suspended_at,
					forced_at: span,
					depth: self.call_depth,
				});
				self.push_bytecode_frame(
					suspended_at,
					BytecodeFrame {
						trace: FrameTrace::None,
						lambda,
						upvalues: upvalues.into(),
						ip: CodeIdx(0),
						stack_offset: self.stack.len(),
						depth: self.call_depth,
					},
				);
			}
			None => self.enqueue_generator("force", span, |co| Thunk::force(thunk, co, span)),
		}
	}

	/// Force a value as a `force` generator at `span` would.
	fn enqueue_force_value(&mut self, span: Span, value: Value) {
		match value {
			Value::Thunk(thunk) => self.enqueue_force(span, thunk),
			value => self.enqueue_generator("force", span, |co| value.force_owned_genco(co, span)),
		}
	}

	/// Run the VM's primary (outer) execution loop, continuing execution based
	/// on the current frame at the top of the frame stack.
	fn execute(mut self) -> EvalResult<RuntimeResult> {
		self.execute_frames()?;
		let value = self.stack.pop().expect("runtime result");
		Ok(RuntimeResult {
			value,
			warnings: self.warnings,
		})
	}

	/// Drive evaluation and resume lazy context handlers on failure.
	fn execute_frames(&mut self) -> EvalResult<()> {
		while !self.frames.is_empty() {
			let frame = self.frames.pop().expect("frame above boundary");
			self.reasonable_span = frame.span();
			let frame_id = self.frames.len();
			let step = (|| -> EvalResult<()> {
				match frame {
					Frame::ErrorContext {
						context,
						span,
						depth,
						outer_traces,
						..
					} => {
						self.call_depth = depth;
						let value = self.stack_pop();
						if value.is_catchable() {
							self.enqueue_context_message(
								context,
								Ok(value),
								span,
								depth,
								outer_traces,
							);
						} else {
							self.stack.push(value);
						}
					}
					Frame::ContextMessage {
						result,
						span,
						depth,
						outer_traces,
					} => {
						self.call_depth = depth;
						let context = self.stack_pop();
						if context.is_catchable() {
							self.stack.push(context);
							return Ok(());
						}
						let context = context.to_contextful_str().with_span(span, self)?;
						let mut trace = Trace::new(None, context.to_str_lossy());
						trace.always = true;
						match result {
							Err(mut error) => {
								error
									.traces
									.insert(outer_traces.min(error.traces.len()), trace);
								return Err(error);
							}
							Ok(Value::Catchable(kind)) => {
								let info = match &*kind {
									CatchableErrorKind::Traced { info, .. } if !info.relative => {
										info.clone()
									}
									_ => {
										let mut traces = self.traces(None, None);
										if let Some(info) = kind.trace() {
											traces.extend(info.traces.borrow().iter().cloned());
										}
										Rc::new(CatchableTrace {
											traces: traces.into(),
											memoized_prefix: Default::default(),
											span: kind.trace().and_then(|info| info.span),
											message: kind
												.trace()
												.and_then(|info| info.message.clone()),
											relative: false,
										})
									}
								};
								let index = outer_traces.min(info.traces.borrow().len());
								info.traces.borrow_mut().insert(index, trace);
								self.stack.push(Value::Catchable(Box::new(
									CatchableErrorKind::Traced {
										kind: Box::new(kind.untraced().clone()),
										info,
									},
								)));
							}
							_ => unreachable!("successful context boundary"),
						}
					}
					Frame::Builtin {
						name,
						mut pending,
						span,
						depth,
						trace,
					} => {
						self.current = (frame_id, trace);
						self.call_depth = depth;
						self.observer
							.observe_enter_generator(frame_id, name, &self.stack);
						let value = pending.awaiting_value().then(|| self.stack_pop());
						match pending.advance(value, span) {
							Ok(thunk) => {
								self.frames.push(Frame::Builtin {
									name,
									pending,
									span,
									depth,
									trace: std::mem::take(&mut self.current.1),
								});
								self.enqueue_force(span, thunk);
								self.observer.observe_suspend_generator(
									frame_id,
									name,
									&self.stack,
								);
							}
							Err(BuiltinResult::Synchronous(result)) => {
								let value = result.with_span(span, &self)?;
								let value = self.trace_catchable(value, None, None);
								self.stack.push(value);
								self.observer
									.observe_exit_generator(frame_id, name, &self.stack);
							}
							Err(BuiltinResult::Called(name, generator)) => {
								self.frames.push(Frame::Generator {
									name,
									generator,
									span,
									depth,
									state: GeneratorState::Running,
									trace: std::mem::take(&mut self.current.1),
								});
							}
							Err(_) => {
								unreachable!("pending builtin returned a partial application")
							}
						}
					}
					Frame::BytecodeFrame {
						bytecode_frame,
						span,
					} => {
						// Bytecode errors borrow the running frame's trace. Suspensions
						// put the frame itself back on the stack.
						self.current = (frame_id, FrameTrace::None);
						self.call_depth = bytecode_frame.depth;
						self.observer.observe_enter_bytecode_frame(
							0,
							&bytecode_frame.lambda,
							frame_id,
						);

						match self.execute_bytecode(span, bytecode_frame) {
							Ok(true) => {
								self.observer
									.observe_exit_bytecode_frame(frame_id, &self.stack);
							}
							Ok(false) => {
								self.observer
									.observe_suspend_bytecode_frame(frame_id, &self.stack);
							}
							Err(err) => return Err(err),
						};
					}

					// Handle generator frames, which can request thunk forcing
					// during their execution.
					Frame::Generator {
						name,
						span,
						state,
						generator,
						depth,
						trace,
					} => {
						// A generator records its trace only while suspended, so the
						// running trace takes it and re-enqueuing moves it back.
						self.current = (frame_id, trace);
						self.call_depth = depth;
						self.observer
							.observe_enter_generator(frame_id, name, &self.stack);

						match self.run_generator(name, span, frame_id, state, generator, None) {
							Ok(true) => {
								self.observer
									.observe_exit_generator(frame_id, name, &self.stack)
							}
							Ok(false) => {
								self.observer
									.observe_suspend_generator(frame_id, name, &self.stack)
							}

							Err(err) => return Err(err),
						};
					}

					// The code of a directly forced thunk has returned its value.
					Frame::Force {
						thunk,
						span,
						forced_at,
						depth,
					} => {
						self.current = (frame_id, FrameTrace::None);
						self.call_depth = depth;
						self.observer
							.observe_enter_generator(frame_id, "force", &self.stack);

						let value = self.stack_pop();
						if let Value::Catchable(kind) = &value
							&& let Some(info) = kind.trace()
						{
							if info.memoized_prefix.get().is_none() {
								info.memoized_prefix
									.set(Some(self.traces(None, None).len()));
							}
						}
						match thunk.finish_direct_force(value) {
							Some(value) => {
								let value = self.trace_catchable(value, None, None);
								self.stack.push(value);
								self.observer.observe_exit_generator(
									frame_id,
									"force",
									&self.stack,
								);
							}

							// The value is another thunk, which the generator forces.
							None => {
								self.enqueue_generator("force", span, |co| {
									Thunk::force(thunk, co, forced_at)
								});
								self.observer.observe_suspend_generator(
									frame_id,
									"force",
									&self.stack,
								);
							}
						}
					}
				}
				Ok(())
			})();
			if let Err(error) = step {
				let Some(index) = self
					.frames
					.iter()
					.rposition(|frame| matches!(frame, Frame::ErrorContext { .. }))
				else {
					return Err(error);
				};
				self.frames.truncate(index + 1);
				self.try_eval_frames.retain(|frame| *frame < index);
				let Some(Frame::ErrorContext {
					context,
					span,
					depth,
					stack_base,
					outer_traces,
				}) = self.frames.pop()
				else {
					unreachable!()
				};
				self.stack.truncate(stack_base);
				self.enqueue_context_message(context, Err(error), span, depth, outer_traces);
			}
		}
		Ok(())
	}

	fn enqueue_context_message(
		&mut self,
		context: Value,
		result: Result<Value, Error>,
		span: Span,
		depth: usize,
		outer_traces: usize,
	) {
		self.call_depth = depth;
		self.frames.push(Frame::ContextMessage {
			result,
			span,
			depth,
			outer_traces,
		});
		self.enqueue_generator("error_context_message", span, |co| async move {
			let context = generators::request_force(&co, context).await;
			context
				.coerce_to_string(
					co,
					CoercionKind {
						strong: false,
						import_paths: false,
					},
					span,
				)
				.await
				.trace(
					None,
					"while evaluating the error message passed to builtins.addErrorContext",
				)
		});
	}

	/// Run the VM's inner execution loop, processing noxi bytecode from a
	/// chunk. This function returns if:
	///
	/// 1. The code has run to the end, and has left a value on the top of the
	///    stack. In this case, the frame is not returned to the frame stack.
	///
	/// 2. The code encounters a generator, in which case the frame in its
	///    current state is pushed back on the stack, and the generator is left
	///    on top of it for the outer loop to execute.
	///
	/// 3. An error is encountered.
	///
	/// This function *must* ensure that it leaves the frame stack in the
	/// correct order, especially when re-enqueuing a frame to execute.
	///
	/// The return value indicates whether the bytecode has been executed to
	/// completion, or whether it has been suspended in favour of a generator.
	fn execute_bytecode(&mut self, span: Span, mut frame: BytecodeFrame) -> EvalResult<bool> {
		loop {
			let op = frame.inc_ip();
			self.observer.observe_execute_op(frame.ip, &op, &self.stack);

			match op {
				Op::ThunkSuspended | Op::ThunkClosure => {
					let idx = frame.read_uvarint() as usize;

					let blueprint = match &frame.chunk().constants[idx] {
						Value::Blueprint(lambda) => lambda.clone(),
						_ => panic!("compiler bug: non-blueprint in blueprint slot"),
					};

					let upvalues = self.populate_upvalues(&mut frame)?;
					debug_assert!(
						upvalues.len() == blueprint.upvalue_count,
						"TODO: new upvalue count not correct",
					);

					let thunk = if op == Op::ThunkClosure {
						debug_assert!(
							((upvalues.len() > 0) || (upvalues.with_stack_len() > 0)),
							"OpThunkClosure should not be called for plain lambdas",
						);
						Thunk::new_closure_with_upvalues(blueprint, upvalues)
					} else {
						let span = blueprint
							.suspended_span
							.unwrap_or_else(|| frame.current_span());
						Thunk::new_suspended_with_upvalues(blueprint, span, upvalues)
					};

					// Recursive captures remain deferred until Op::Finalise.
					self.stack.push(Value::Thunk(thunk));
				}

				Op::PushContext => {
					let hint = self
						.stack_pop()
						.to_str()
						.expect("compiler context string")
						.to_str_lossy()
						.into_owned();
					frame.trace = FrameTrace::Scoped {
						parent: Box::new(std::mem::take(&mut frame.trace)),
						context: Box::new(FrameTrace::Frame(Trace::new(
							if hint.starts_with('\0') {
								None
							} else {
								Some(frame.current_span())
							},
							hint.trim_start_matches('\0'),
						))),
					};
				}
				Op::PopContext => {
					if let FrameTrace::Scoped { parent, .. } = std::mem::take(&mut frame.trace) {
						frame.trace = *parent;
					}
				}
				Op::Force => {
					if let Some(Value::Thunk(_)) = self.stack.last() {
						let thunk = match self.stack_pop() {
							Value::Thunk(t) => t,
							_ => unreachable!(),
						};
						let thunk = match thunk.force_if_evaluated() {
							Ok(value) => {
								self.stack.push(value);
								continue;
							}
							Err(thunk) => thunk,
						};

						let gen_span = frame.current_span();

						self.push_bytecode_frame(span, frame);
						self.enqueue_force(gen_span, thunk);

						return Ok(false);
					}
				}

				Op::GetUpvalue => {
					let idx = UpvalueIdx(frame.read_uvarint() as usize);
					let value = frame.upvalue(idx).clone();
					self.stack.push(value);
				}

				// Discard the current frame.
				Op::Return => {
					// TODO(amjoseph): I think this should assert `==` rather
					// than `<=` but it fails with the stricter condition.
					debug_assert!(self.stack.len() - 1 <= frame.stack_offset);
					return Ok(true);
				}

				Op::Constant => {
					let idx = frame.read_uvarint() as usize;

					debug_assert!(
						idx < frame.chunk().constants.len(),
						"out of bounds constant at IP {} in {:p}",
						frame.ip.0,
						frame.lambda
					);

					let c = frame.chunk().constants[idx].clone();
					self.stack.push(c);
				}

				Op::Call => {
					let callable = self.stack_pop();
					if let Value::Builtin(builtin) = callable {
						let builtin_name = builtin.name();
						match self.apply_builtin(builtin, || frame.current_span()) {
							BuiltinResult::Partial(partial) => {
								self.stack.push(Value::Builtin(partial))
							}
							BuiltinResult::Synchronous(Ok(value))
								if !matches!(value, Value::Catchable(_)) =>
							{
								self.stack.push(value)
							}
							result => {
								let call_span = frame.current_span();
								let call_site = (!frame.lambda.synthetic_call).then_some(call_span);
								// Suspensions and errors retain the caller frame, including
								// its original span for the ordinary trace construction.
								self.push_bytecode_frame(span, frame);
								self.finish_builtin(call_span, call_site, builtin_name, result)?;
								return Ok(true);
							}
						}
						continue;
					}
					self.call_value(frame.current_span(), Some((span, frame)), callable)?;

					// exit this loop and let the outer loop enter the new call
					return Ok(true);
				}

				// Remove the given number of elements from the stack,
				// but retain the top value.
				Op::CloseScope => {
					let count = frame.read_uvarint() as usize;
					// Immediately move the top value into the right
					// position.
					let target_idx = self.stack.len() - 1 - count;
					self.stack[target_idx] = self.stack_pop();

					// Then drop the remaining values.
					for _ in 0..(count - 1) {
						self.stack.pop();
					}
				}

				Op::Closure => {
					let idx = frame.read_uvarint() as usize;
					let blueprint = match &frame.chunk().constants[idx] {
						Value::Blueprint(lambda) => lambda.clone(),
						_ => panic!("compiler bug: non-blueprint in blueprint slot"),
					};

					let upvalues = self.populate_upvalues(&mut frame)?;
					let upvalue_count = upvalues.len();
					debug_assert!(
						upvalue_count == blueprint.upvalue_count,
						"TODO: new upvalue count not correct in closure",
					);

					debug_assert!(
						(upvalue_count > 0 || upvalues.with_stack_len() > 0),
						"OpClosure should not be called for plain lambdas"
					);

					self.stack
						.push(Value::Closure(Rc::new(Closure::new_with_upvalues(
							Rc::new(upvalues),
							blueprint,
						))));
				}

				Op::AttrsSelect => lifted_pop! {
					self(key, attrs) => {
						let key = attribute_name(key, || frame.current_span()).with_span(&frame, self)?;
						let attrs = attrs.to_attrs().with_span(&frame, self)?;

						match attrs.select(&key) {
							Some(value) => {
								self.stack.push(value.clone());
								frame.record_selection(attrs, key);
							}

							None => {
								let name = key.to_str_lossy().into_owned();
								let suggestions = crate::diagnostics::best_matches(
									attrs.keys().map(|key| key.to_str().unwrap_or_default()),
									&name,
								);
								// CppNix reports the start of the selection.
								let span = frame.lambda.selection_span().unwrap_or_else(|| frame.current_span());
								return Err(ErrorKind::Positioned(Box::new(ErrorKind::AttributeNotFound {
									name,
									suggestions,
								})))
								.with_span((span, &frame), self);
							}
						}
					}
				},

				Op::JumpIfFalse => {
					let offset = frame.read_u16() as usize;
					debug_assert!(offset != 0);
					if !self
						.stack_peek(0)
						.as_bool()
						.map_err(|e| ErrorKind::Positioned(Box::new(e)))
						.with_span(&frame, self)?
					{
						frame.ip += offset;
					}
				}

				Op::JumpIfCatchable => {
					let offset = frame.read_u16() as usize;
					debug_assert!(offset != 0);
					if self.stack_peek(0).is_catchable() {
						frame.ip += offset;
					}
				}

				Op::JumpIfNoFinaliseRequest => {
					let offset = frame.read_u16() as usize;
					debug_assert!(offset != 0);
					match self.stack_peek(0) {
						Value::FinaliseRequest(finalise) => {
							if !finalise {
								frame.ip += offset;
							}
						}
						val => panic!(
							"noxi bug: OpJumIfNoFinaliseRequest: expected FinaliseRequest, but got {}",
							val.type_of()
						),
					}
				}

				Op::Dup => {
					let value = self.stack_peek(0).clone();
					self.stack.push(value);
				}

				Op::Pop => {
					self.stack.pop();
				}

				Op::AttrsTrySelect => {
					let key = attribute_name(self.stack_pop(), || frame.current_span())
						.with_span(&frame, self)?;
					let value = match self.stack_pop() {
						Value::Attrs(attrs) => match attrs.select(&key) {
							Some(value) => {
								let value = value.clone();
								frame.record_selection(attrs, key);
								value
							}
							None => Value::AttrNotFound,
						},

						_ => Value::AttrNotFound,
					};

					self.stack.push(value);
				}

				Op::GetLocal => {
					let local_idx = frame.read_uvarint() as usize;
					let idx = frame.stack_offset + local_idx;
					self.stack.push(self.stack[idx].clone());
				}

				Op::JumpIfNotFound => {
					let offset = frame.read_u16() as usize;
					debug_assert!(offset != 0);
					if matches!(self.stack_peek(0), Value::AttrNotFound) {
						self.stack_pop();
						frame.ip += offset;
					}
				}

				Op::Jump => {
					let offset = frame.read_u16() as usize;
					debug_assert!(offset != 0);
					frame.ip += offset;
				}

				Op::Equal => lifted_pop! {
					self(b, a) => {
						if let Some(equal) = native::equal(&a, &b, PointerEquality::ForbidAll) {
							self.stack.push(Value::Bool(equal));
							continue;
						}
						let gen_span = frame.current_span();
						self.push_bytecode_frame(span, frame);
						self.enqueue_generator("nix_eq", gen_span, |co| {
							a.nix_eq_owned_genco(b, co, PointerEquality::ForbidAll, gen_span)
						});
						return Ok(false);
					}
				},

				// These assertion operations error out if the stack
				// top is not of the expected type. This is necessary
				// to implement some specific behaviours of Nix
				// exactly.
				Op::AssertBool => {
					let val = self.stack_peek(0);
					// TODO(edef): propagate this into is_bool, since bottom values *are* values of any type
					if !val.is_catchable() && !val.is_bool() {
						return frame.error(
							self,
							ErrorKind::TypeError {
								expected: "bool",
								actual: val.type_of(),
								value: Some(Box::new(val.clone())),
							},
						);
					}
				}

				Op::AssertAttrs => {
					let val = self.stack_peek(0);
					// TODO(edef): propagate this into is_attrs, since bottom values *are* values of any type
					if !val.is_catchable() && !val.is_attrs() {
						return frame.error(
							self,
							ErrorKind::TypeError {
								expected: "set",
								actual: val.type_of(),
								value: Some(Box::new(val.clone())),
							},
						);
					}
				}

				Op::Attrs => self.run_attrset(frame.read_uvarint() as usize, &frame)?,

				Op::AttrsUpdate => lifted_pop! {
					self(rhs, lhs) => {
						let rhs = rhs.to_attrs().with_span(&frame, self)?;
						let lhs = lhs.to_attrs().with_span(&frame, self)?;
						self.stack.push(Value::attrs(lhs.update(rhs)))
					}
				},

				Op::Invert => lifted_pop! {
					self(v) => {
						let v = v.as_bool().with_span(&frame, self)?;
						self.stack.push(Value::Bool(!v));
					}
				},

				Op::List => {
					let count = frame.read_uvarint() as usize;
					let list =
						NixList::construct(count, self.stack.split_off(self.stack.len() - count));

					self.stack.push(Value::List(list));
				}

				Op::JumpIfTrue => {
					let offset = frame.read_u16() as usize;
					debug_assert!(offset != 0);
					if self
						.stack_peek(0)
						.as_bool()
						.map_err(|e| ErrorKind::Positioned(Box::new(e)))
						.with_span(&frame, self)?
					{
						frame.ip += offset;
					}
				}

				Op::HasAttr => lifted_pop! {
					self(key, attrs) => {
						let key = key.to_str().with_span(&frame, self)?;
						let result = match attrs {
							Value::Attrs(attrs) => attrs.contains(&key),

							// Nix allows use of `?` on non-set types, but
							// always returns false in those cases.
							_ => false,
						};

						self.stack.push(Value::Bool(result));
					}
				},

				Op::Concat => lifted_pop! {
					self(rhs, lhs) => {
						let context = "while evaluating one of the elements to concatenate";
						let mut lhs = lhs.to_list().trace(Some(frame.current_span()), context).with_span(&frame, self)?.into_inner();
						let rhs = rhs.to_list().trace(Some(frame.current_span()), context).with_span(&frame, self)?.into_inner();
						lhs.extend(rhs.into_iter());
						self.stack.push(Value::List(lhs.into()))
					}
				},

				Op::ResolveWith => {
					let ident = self.stack_pop().to_str().with_span(&frame, self)?;

					// Re-enqueue this frame.
					let op_span = frame.current_span();
					self.push_bytecode_frame(span, frame);

					// Construct a generator frame doing the lookup in constant
					// stack space.
					let with_stack_len = self.with_stack.len();
					let closed_with_stack_len = self
						.last_bytecode_frame()
						.map(|frame| frame.upvalues.with_stack_len())
						.unwrap_or(0);

					self.enqueue_generator("resolve_with", op_span, |co| {
						resolve_with(co, ident.into(), with_stack_len, closed_with_stack_len)
					});

					return Ok(false);
				}

				Op::Finalise => {
					let idx = frame.read_uvarint() as usize;
					match &self.stack[frame.stack_offset + idx] {
						Value::Closure(_) => panic!("attempted to finalise a closure"),
						Value::Thunk(thunk) => thunk.finalise(&self.stack[frame.stack_offset..]),
						_ => panic!("attempted to finalise a non-thunk"),
					}
				}

				Op::CoerceToString => {
					let kind: CoercionKind = frame.chunk().code[frame.ip.0].into();
					frame.ip.0 += 1;

					let value = self.stack_pop();
					if let Some(value) = native::coerce(&value, kind) {
						self.stack.push(value);
						continue;
					}
					let gen_span = frame.current_span();
					self.push_bytecode_frame(span, frame);

					self.enqueue_generator("coerce_to_string", gen_span, |co| async move {
						value
							.coerce_to_string(co, kind, gen_span)
							.await
							.trace(Some(gen_span), "while evaluating a path segment")
					});

					return Ok(false);
				}

				Op::Interpolate => self.run_interpolate(frame.read_uvarint(), &frame)?,

				Op::ValidateClosedFormals => {
					let formals = frame.lambda.formals.as_ref().expect(
						"OpValidateClosedFormals called within the frame of a lambda without formals",
					);

					let peeked = self.stack_peek(0);
					if peeked.is_catchable() {
						continue;
					}

					let args = peeked.to_attrs().with_span(&frame, self)?;
					for (arg, has_default) in &formals.arguments {
						if !has_default && args.select(arg).is_none() {
							return frame.error(
								self,
								ErrorKind::MissingArgument {
									arg: arg.clone(),
									lambda: frame.lambda.clone(),
								},
							);
						}
					}
					for arg in args.keys() {
						if !formals.contains(arg) {
							return frame.error(
								self,
								ErrorKind::UnexpectedArgumentFormals {
									arg: arg.clone(),
									formals_span: formals.span,
									lambda: Some(frame.lambda.clone()),
								},
							);
						}
					}
				}

				Op::Add => lifted_pop! {
					self(b, a) => {
						if let Some(value) = native::add(&a, &b) {
							self.stack.push(value);
							continue;
						}
						let gen_span = frame.current_span();
						self.push_bytecode_frame(span, frame);

						// OpAdd can add not just numbers, but also string-like
						// things, which requires more VM logic. This operation is
						// evaluated in a generator frame.
						self.enqueue_generator("add_values", gen_span, |co| async move {
							if matches!(&a, Value::String(_) | Value::Path(_)) && !matches!(&b, Value::String(_)) {
								generators::request_builtin_context(&co, vec![Trace::new(Some(gen_span), "while evaluating a path segment")]).await;
							}
							add_values(co, a, b).await.map_err(|error| {
								if matches!(error, ErrorKind::NotCoercibleToString { .. }) {
									Err::<Value, _>(error).trace(Some(gen_span), "while evaluating a path segment").unwrap_err()
								} else { error }
							})
						});
						return Ok(false);
					}
				},

				Op::Sub => lifted_pop! {
					self(b, a) => {
						let result = arithmetic_op!(&a, &b, -).with_span(&frame, self)?;
						self.stack.push(result);
					}
				},

				Op::Mul => lifted_pop! {
					self(b, a) => {
						let result = arithmetic_op!(&a, &b, *).with_span(&frame, self)?;
						self.stack.push(result);
					}
				},

				Op::Div => lifted_pop! {
					self(b, a) => {
						let result = arithmetic_op!(&a, &b, /).with_span(&frame, self)?;
						self.stack.push(result);
					}
				},

				Op::Negate => match self.stack_pop() {
					Value::Integer(i) => {
						let result = arithmetic_op!(&Value::Integer(0), &Value::Integer(i), -)
							.with_span(&frame, self)?;
						self.stack.push(result);
					}
					Value::Float(f) => self.stack.push(Value::Float(-f)),
					Value::Catchable(cex) => self.stack.push(Value::Catchable(cex)),
					v => {
						return frame.error(
							self,
							ErrorKind::TypeError {
								expected: "number (either int or float)",
								actual: v.type_of(),
								value: Some(Box::new(v.clone())),
							},
						);
					}
				},

				Op::Less => cmp_op!(self, frame, span, <),
				Op::LessOrEq => cmp_op!(self, frame, span, <=),
				Op::More => cmp_op!(self, frame, span, >),
				Op::MoreOrEq => cmp_op!(self, frame, span, >=),

				Op::FindFile => match self.stack_pop() {
					Value::UnresolvedPath(path) => {
						let resolved = self
							.nix_search_path
							.resolve(&self.io_handle, &path.to_string_lossy());
						self.stack.push(resolved.into());
					}

					_ => panic!("noxi bug: OpFindFile called on non-UnresolvedPath"),
				},

				Op::ResolveHomePath => match self.stack_pop() {
					Value::UnresolvedPath(path) => {
						// FUTUREWORK: this only works on Linux and Darwin. Other platforms?
						let home_dir = self
							.io_handle
							.as_ref()
							.get_env(OsStr::new("HOME"))
							.filter(|h| !h.is_empty())
							.map(PathBuf::from);

						match home_dir {
							None => {
								return frame.error(
									self,
									ErrorKind::RelativePathResolution(
										"failed to determine home directory".into(),
									),
								);
							}
							Some(mut buf) => {
								buf.push(*path);
								self.stack.push(buf.into());
							}
						};
					}

					_ => {
						panic!("noxi bug: OpResolveHomePath called on non-UnresolvedPath")
					}
				},

				Op::InterpolatePath => self.run_interpolate_path(frame.read_uvarint(), &frame)?,

				Op::PushWith => self
					.with_stack
					.push(frame.stack_offset + frame.read_uvarint() as usize),

				Op::PopWith => {
					self.with_stack.pop();
				}

				Op::AssertFail => {
					let text = self
						.stack_pop()
						.to_contextful_str()
						.map(|s| bstr::ByteSlice::to_str_lossy(s.as_bytes()).into_owned())
						.unwrap_or_default();
					let failed = Value::from(CatchableErrorKind::AssertionFailed);
					let failed = self.trace_catchable_in(
						failed,
						Some(&frame.trace),
						Some(frame.current_span()),
						Some(format!("assertion '{text}' failed")),
					);
					self.stack.push(failed);
				}

				Op::AssertEqFail => {
					let text = self
						.stack_pop()
						.to_contextful_str()
						.map(|s| bstr::ByteSlice::to_str_lossy(s.as_bytes()).into_owned())
						.unwrap_or_default();
					let rhs = self.stack_pop();
					let lhs = self.stack_pop();
					let gen_span = frame.current_span();
					self.push_bytecode_frame(span, frame);
					let hint = format!("while evaluating the condition of the assertion '{text}'");
					self.enqueue_generator("assert_eq", gen_span, |co| {
						assertions::assert_eq_failed(co, lhs, rhs, text)
					});
					if let Some(Frame::Generator { trace, .. }) = self.frames.last_mut() {
						*trace = FrameTrace::Frame(Trace::new(Some(gen_span), hint));
					}
					return Ok(false);
				}

				// Encountering an invalid opcode is a critical error in the
				// VM/compiler.
				Op::Invalid => {
					panic!("noxi bug: attempted to execute invalid opcode")
				}
			}
		}
	}
}

/// Implementation of helper functions for the runtime logic above.
impl<IO> VM<'_, IO>
where
	IO: AsRef<dyn EvalIO> + 'static,
{
	pub(crate) fn stack_pop(&mut self) -> Value {
		self.stack.pop().expect("runtime stack empty")
	}

	fn stack_peek(&self, offset: usize) -> &Value {
		&self.stack[self.stack.len() - 1 - offset]
	}

	fn run_attrset(&mut self, count: usize, frame: &BytecodeFrame) -> EvalResult<()> {
		let attrs = NixAttrs::construct_with_positions(
			count,
			self.stack.split_off(self.stack.len() - count * 2),
			&frame.chunk().attrsets_pos_spans,
			Some(&frame.chunk().attrset_positions),
		)
		.with_span(frame, self)?
		.map(Value::attrs)
		.into();

		self.stack.push(attrs);
		Ok(())
	}
}

impl<IO> VM<'_, IO> {
	/// The CppNix trace of the running evaluation, outermost first: the
	/// frames on the stack, then the running frame unless it was already
	/// pushed back. `error` is the error being raised, if any.
	/// `running` is the running frame's trace if the error has it at hand,
	/// which is newer than the copy taken when the frame was entered.
	fn traces(&self, error: Option<&ErrorKind>, running: Option<&FrameTrace>) -> Vec<Trace> {
		let current =
			(self.frames.len() <= self.current.0).then(|| running.unwrap_or(&self.current.1));
		let frames: Vec<&FrameTrace> = self
			.frames
			.iter()
			.map(Frame::trace)
			.chain(current)
			.collect();
		// CppNix checks a call's arguments against the function's formals
		// before it evaluates the body, so only `from call site` applies.
		let formals_error = matches!(
			error,
			Some(ErrorKind::UnexpectedArgumentFormals { .. } | ErrorKind::MissingArgument { .. })
		);
		let hidden = |span: &Span| self.source.is_hidden(*span);
		let mut traces = vec![];
		fn flatten<'a>(frame: &'a FrameTrace, out: &mut Vec<&'a FrameTrace>) {
			if let FrameTrace::Scoped { parent, context } = frame {
				flatten(parent, out);
				flatten(context, out);
			} else {
				out.push(frame);
			}
		}
		let mut flat = vec![];
		for frame in &frames {
			flatten(frame, &mut flat);
		}
		for (i, frame) in flat.iter().enumerate() {
			match frame {
				FrameTrace::Scoped { .. } => unreachable!("scopes flattened"),
				FrameTrace::None => {}
				FrameTrace::Dynamic(frames) => traces.extend(frames.borrow().iter().cloned()),
				FrameTrace::Context(hint) => traces.push(Trace::new(None, *hint)),
				FrameTrace::Frame(trace) => {
					if !trace.span.as_ref().is_some_and(hidden) {
						traces.push(trace.clone());
					}
				}
				FrameTrace::Select { path, attrs, key } => {
					if let Some(span) = attrs.get_attr_pos(key)
						&& !hidden(&span)
					{
						traces.push(Trace::new(
							Some(span),
							format!("while evaluating the attribute '{path}'"),
						));
					}
				}
				FrameTrace::Builtin {
					name,
					call_site,
					contexts,
					path,
				} => {
					if !call_site.as_ref().is_some_and(hidden) {
						traces.push(Trace::new(
							*call_site,
							format!("while calling the '{name}' builtin"),
						));
					}
					traces.extend(contexts.iter().cloned());
					if let Some(path) = path {
						traces.extend(path.borrow().iter().cloned());
					}
				}

				FrameTrace::Lambda { lambda, call_site } => {
					if lambda.span.as_ref().is_none_or(hidden)
						|| lambda.span.is_some_and(|span| {
							self.source
								.get_file(span)
								.name()
								.starts_with("«nix-internal»/")
						}) {
						continue;
					}
					let checking_formals = formals_error && i == frames.len() - 1;
					if let Some(call_site) = call_site.filter(|span| !hidden(span)) {
						traces.push(Trace {
							show_trace_only: !checking_formals,
							..Trace::new(Some(call_site), "from call site")
						});
					}
					if !checking_formals {
						let name = match &lambda.name {
							Some(name) => format!("'{name}'"),
							None => "anonymous lambda".to_owned(),
						};
						traces.push(Trace {
							show_trace_only: true,
							..Trace::new(lambda.span, format!("while calling {name}"))
						});
					}
				}
			}
		}
		traces
	}
}

impl<IO> VM<'_, IO>
where
	IO: AsRef<dyn EvalIO> + 'static,
{
	/// Record where a catchable error produced by the running frame was
	/// raised, unless `builtins.tryEval` will discard it.
	#[inline]
	pub(crate) fn trace_catchable(
		&self,
		value: Value,
		span: Option<Span>,
		message: Option<String>,
	) -> Value {
		self.trace_catchable_in(value, None, span, message)
	}

	/// [`trace_catchable`](Self::trace_catchable) with the running frame's
	/// trace, if it is at hand. A catchable error that carries only the
	/// frames of the generator that raised it gets the others prepended.
	#[inline]
	fn trace_catchable_in(
		&self,
		value: Value,
		running: Option<&FrameTrace>,
		span: Option<Span>,
		message: Option<String>,
	) -> Value {
		// Nearly every value a frame produces is not an error.
		if !matches!(value, Value::Catchable(_)) {
			return value;
		}
		self.trace_catchable_error(value, running, span, message)
	}

	#[cold]
	fn trace_catchable_error(
		&self,
		value: Value,
		running: Option<&FrameTrace>,
		span: Option<Span>,
		message: Option<String>,
	) -> Value {
		match value {
			Value::Catchable(kind) => match *kind {
				CatchableErrorKind::Traced { kind, info } if info.relative => {
					let mut traces = self.traces(None, running);
					traces.extend(info.traces.borrow().iter().cloned());
					Value::Catchable(Box::new(CatchableErrorKind::Traced {
						kind,
						info: Rc::new(CatchableTrace {
							traces: traces.into(),
							memoized_prefix: Default::default(),
							span: info.span,
							message: info.message.clone(),
							relative: false,
						}),
					}))
				}
				kind @ CatchableErrorKind::Traced { .. } => Value::Catchable(Box::new(kind)),
				kind => Value::Catchable(Box::new(CatchableErrorKind::Traced {
					kind: Box::new(kind),
					info: Rc::new(CatchableTrace {
						traces: self.traces(None, running).into(),
						memoized_prefix: Default::default(),
						span,
						message,
						relative: false,
					}),
				})),
			},
			value => value,
		}
	}

	/// Access the last bytecode frame present in the frame stack.
	fn last_bytecode_frame(&self) -> Option<&BytecodeFrame> {
		for frame in self.frames.iter().rev() {
			if let Frame::BytecodeFrame { bytecode_frame, .. } = frame {
				return Some(bytecode_frame);
			}
		}

		None
	}

	/// Push an already constructed warning.
	pub fn push_warning(&mut self, warning: EvalWarning) {
		self.warnings.push(warning);
	}

	/// Emit a warning with the given WarningKind and the source span
	/// of the current instruction.
	pub fn emit_warning(&mut self, kind: WarningKind) {
		self.push_warning(EvalWarning {
			kind,
			span: self.get_span(),
		});
	}

	/// Interpolate string fragments by popping the specified number of
	/// fragments of the stack, evaluating them to strings, and pushing
	/// the concatenated result string back on the stack.
	fn run_interpolate(&mut self, count: u64, frame: &BytecodeFrame) -> EvalResult<()> {
		let mut out = BString::default();
		// Interpolation propagates the context and union them.
		let mut context: NixContext = NixContext::new();

		for i in 0..count {
			let val = self.stack_pop();
			if val.is_catchable() {
				for _ in (i + 1)..count {
					self.stack.pop();
				}
				self.stack.push(val);
				return Ok(());
			}
			let mut nix_string = val.to_contextful_str().with_span(frame, self)?;
			out.push_str(nix_string.as_bstr());
			if let Some(nix_string_ctx) = nix_string.take_context() {
				context.extend(*nix_string_ctx)
			}
		}

		self.stack
			.push(Value::String(NixString::new_context_from(context, out)));
		Ok(())
	}

	/// Interpolate path fragments by popping the specified number of
	/// fragments off the stack, evaluating them into a single path,
	/// and pushing a Path value back on the stack.
	fn run_interpolate_path(&mut self, count: u64, frame: &BytecodeFrame) -> EvalResult<()> {
		// Similar pattern to run_interpolate for strings but simpler
		let mut path_str = String::new();

		for i in 0..count {
			let val = self.stack_pop();
			if val.is_catchable() {
				// If we encounter an error, discard remaining parts and propagate the error
				for _ in (i + 1)..count {
					self.stack.pop();
				}
				self.stack.push(val);
				return Ok(());
			}

			// For path interpolation, we only accept string and path values
			match val {
				Value::String(s) => {
					path_str.push_str(&s.as_bytes().to_str_lossy());
				}
				Value::Path(p) => {
					path_str.push_str(&p.to_string_lossy());
				}
				_ => {
					return frame.error(
						self,
						ErrorKind::TypeError {
							expected: "string or path",
							actual: val.type_of(),
							value: Some(Box::new(val.clone())),
						},
					);
				}
			}
		}

		// Create a canonical path from the concatenated string
		let path = canon_path(PathBuf::from(path_str));
		self.stack.push(Value::Path(Box::new(path)));

		Ok(())
	}

	/// Apply an argument from the stack to a builtin, and attempt to call it.
	///
	/// All calls are tail-calls in noxi, as every function application is a
	/// separate thunk and OpCall is thus the last result in the thunk.
	///
	/// Due to this, once control flow exits this function, the generator will
	/// automatically be run by the VM.
	fn call_builtin(
		&mut self,
		span: Span,
		call_site: Option<Span>,
		builtin: Builtin,
	) -> EvalResult<()> {
		let builtin_name = builtin.name();
		let result = self.apply_builtin(builtin, || span);
		self.finish_builtin(span, call_site, builtin_name, result)
	}

	fn apply_builtin(&mut self, builtin: Builtin, span: impl FnOnce() -> Span) -> BuiltinResult {
		self.observer.observe_enter_builtin(builtin.name());
		builtin.apply_at(self.stack_pop(), span)
	}

	fn finish_builtin(
		&mut self,
		span: Span,
		call_site: Option<Span>,
		builtin_name: &'static str,
		result: BuiltinResult,
	) -> EvalResult<()> {
		match result {
			BuiltinResult::Pending(name, pending) => self.frames.push(Frame::Builtin {
				name,
				pending,
				span,
				depth: self.call_depth,
				trace: FrameTrace::Builtin {
					name,
					call_site,
					contexts: vec![],
					path: None,
				},
			}),
			// Partially applied builtin is just pushed back on the stack.
			BuiltinResult::Partial(partial) => self.stack.push(Value::Builtin(partial)),
			BuiltinResult::Synchronous(result) => {
				// A ready builtin has no generator frame, but its failures
				// must carry the same trace as the asynchronous path.
				let previous = std::mem::replace(
					&mut self.current,
					(
						self.frames.len(),
						FrameTrace::Builtin {
							name: builtin_name,
							call_site,
							contexts: vec![],
							path: None,
						},
					),
				);
				let result = result
					.with_span(span, self)
					.map(|value| self.trace_catchable(value, None, None));
				self.current = previous;
				self.stack.push(result?);
			}

			// Builtin is fully applied and the generator needs to be run by the VM.
			BuiltinResult::Called(name, generator) => self.frames.push(Frame::Generator {
				generator,
				span,
				name,
				state: GeneratorState::Running,
				depth: self.call_depth,
				trace: FrameTrace::Builtin {
					name: builtin_name,
					call_site,
					contexts: vec![],
					path: None,
				},
			}),
		}

		Ok(())
	}

	fn call_error<T>(
		&self,
		span: Span,
		parent: Option<&(Span, BytecodeFrame)>,
		kind: ErrorKind,
	) -> EvalResult<T> {
		match parent {
			Some((_, frame)) => Err(kind).with_span((span, frame), self),
			None => Err(kind).with_span(span, self),
		}
	}

	fn call_value(
		&mut self,
		span: Span,
		parent: Option<(Span, BytecodeFrame)>,
		callable: Value,
	) -> EvalResult<()> {
		match callable {
			// The parent frame resumes after the call: a call in tail
			// position of a conditional or `let` is followed by more code.
			Value::Builtin(builtin) => {
				let call_site = parent.is_some().then_some(span);
				if let Some((parent_span, parent_frame)) = parent {
					self.push_bytecode_frame(parent_span, parent_frame);
				}
				self.call_builtin(span, call_site, builtin)
			}
			Value::Thunk(thunk) => self.call_value(span, parent, thunk.value().clone()),

			Value::Closure(closure) => {
				let lambda = closure.lambda();
				let call_site = parent
					.as_ref()
					.filter(|(_, f)| !f.lambda.omit_call_position)
					.map(|_| span);
				self.observer.observe_tail_call(self.frames.len(), &lambda);

				// The stack offset is always `stack.len() - arg_count`, and
				// since this branch handles native Nix functions (which always
				// take only a single argument and are curried), the offset is
				// `stack_len - 1`.
				let stack_offset = self.stack.len() - 1;

				// Like CppNix, count nested calls, tail calls included, so
				// unbounded recursion fails instead of running forever.
				if self.call_depth > self.max_call_depth {
					return self.call_error(
						span,
						parent.as_ref(),
						ErrorKind::Positioned(Box::new(ErrorKind::EvalError(
							"stack overflow; max-call-depth exceeded".into(),
						))),
					);
				}

				// Reenqueue the parent frame, which resumes after the call.
				// Keeping it also leads to more useful error traces.
				if let Some((parent_span, parent_frame)) = parent {
					self.push_bytecode_frame(parent_span, parent_frame);
				}

				self.push_bytecode_frame(
					span,
					BytecodeFrame {
						trace: FrameTrace::Lambda {
							lambda: lambda.clone(),
							call_site,
						},
						lambda,
						upvalues: closure.upvalues().into(),
						ip: CodeIdx(0),
						stack_offset,
						depth: self.call_depth
							+ usize::from(
								closure
									.lambda
									.span
									.is_some_and(|s| !self.source.is_hidden(s)),
							),
					},
				);

				Ok(())
			}

			// Attribute sets with a __functor attribute are callable.
			val @ Value::Attrs(_) => {
				if let Some((parent_span, parent_frame)) = parent {
					self.push_bytecode_frame(parent_span, parent_frame);
				}

				self.enqueue_generator("__functor call", span, |co| call_functor(co, val));
				Ok(())
			}

			val @ Value::Catchable(_) => {
				// the argument that we tried to apply a catchable to
				self.stack.pop();
				// applying a `throw` to anything is still a `throw`, so we just
				// push it back on the stack.
				self.stack.push(val);
				if let Some((parent_span, parent_frame)) = parent {
					self.push_bytecode_frame(parent_span, parent_frame);
				}
				Ok(())
			}

			v => self.call_error(span, parent.as_ref(), {
				let kind = ErrorKind::NotCallable(v.type_of(), Some(Box::new(v)));
				if parent.as_ref().is_some_and(|(_, frame)| {
					frame.lambda.span.is_some() || frame.lambda.generated_call_position
				}) {
					ErrorKind::Positioned(Box::new(kind))
				} else {
					kind
				}
			}),
		}
	}

	/// Populate the upvalue fields of a thunk or closure under construction.
	///
	/// See the closely tied function `emit_upvalue_data` in the compiler
	/// implementation for details on the argument processing.
	fn populate_upvalues(&self, frame: &mut BytecodeFrame) -> EvalResult<Upvalues> {
		let data = UpvalueData::from_raw(frame.read_uvarint());
		let count = data.count();
		let capture_with = data.captures_with();

		let mut static_upvalues = Vec::with_capacity(count);

		let with_stack = if !capture_with {
			None
		} else if self.with_stack.is_empty() {
			// No `with` was entered since the frame's own captures, so share them.
			frame.upvalues.shared_with_stack().cloned()
		} else {
			// Start the captured with_stack off of the
			// current bytecode frame's captured with_stack, ...
			let captured = frame.upvalues.with_stack();
			let mut captured_with_stack =
				Vec::with_capacity(captured.len() + self.with_stack.len());
			captured_with_stack.extend_from_slice(captured);

			// and extend it with the current with_stack
			for idx in &self.with_stack {
				captured_with_stack.push(self.stack[*idx].clone());
			}

			Some(captured_with_stack.into())
		};

		for _ in 0..count {
			let pos = Position(frame.read_uvarint());

			if let Some(stack_idx) = pos.runtime_stack_index() {
				let idx = frame.stack_offset + stack_idx.0;

				let val = match self.stack.get(idx) {
					Some(val) => val.clone(),
					None => {
						// TODO: maybe panic here?
						return frame.error(
							self,
							ErrorKind::NoxiBug {
								msg: "upvalue to be captured was missing on stack",
								metadata: Some(Rc::new(json!({
									"ip": format!("{:#x}", frame.ip.0 - 1),
									"stack_idx(relative)": stack_idx.0,
									"stack_idx(absolute)": idx,
								}))),
							},
						);
					}
				};

				static_upvalues.push(val);
			} else if let Some(idx) = pos.runtime_deferred_local() {
				static_upvalues.push(Value::DeferredUpvalue(idx));
			} else if let Some(idx) = pos.runtime_upvalue_index() {
				static_upvalues.push(frame.upvalue(idx).clone());
			} else {
				panic!("noxi bug: invalid capture position emitted")
			}
		}

		Ok(Upvalues::from_raw_parts(static_upvalues, with_stack))
	}
}

// TODO(amjoseph): de-asyncify this
/// Resolve a dynamically bound identifier (through `with`) by looking
/// for matching values in the with-stacks carried at runtime.
async fn resolve_with(
	co: GenCo,
	ident: BString,
	vm_with_len: usize,
	upvalue_with_len: usize,
) -> Result<Value, ErrorKind> {
	/// Fetch and force a value on the with-stack from the VM.
	async fn fetch_forced_with(co: &GenCo, idx: usize) -> Value {
		match co.yield_(VMRequest::WithValue(idx)).await {
			VMResponse::Value(value) => value,
			msg => panic!("noxi bug: VM responded with incorrect generator message: {msg}"),
		}
	}

	/// Fetch and force a value on the *captured* with-stack from the VM.
	async fn fetch_captured_with(co: &GenCo, idx: usize) -> Value {
		match co.yield_(VMRequest::CapturedWithValue(idx)).await {
			VMResponse::Value(value) => value,
			msg => panic!("noxi bug: VM responded with incorrect generator message: {msg}"),
		}
	}

	for with_stack_idx in (0..vm_with_len).rev() {
		// TODO(tazjin): is this branch still live with the current with-thunking?
		let with = fetch_forced_with(&co, with_stack_idx).await;

		if with.is_catchable() {
			return Ok(with);
		}

		match with.to_attrs()?.select(&ident) {
			None => continue,
			Some(val) => return Ok(val.clone()),
		}
	}

	for upvalue_with_idx in (0..upvalue_with_len).rev() {
		let with = fetch_captured_with(&co, upvalue_with_idx).await;

		if with.is_catchable() {
			return Ok(with);
		}

		match with.to_attrs()?.select(&ident) {
			None => continue,
			Some(val) => return Ok(val.clone()),
		}
	}

	Err(ErrorKind::UnknownDynamicVariable(ident.to_string()))
}

// TODO(amjoseph): de-asyncify this
async fn add_values(co: GenCo, a: Value, b: Value) -> Result<Value, ErrorKind> {
	// What we try to do is solely determined by the type of the first value!
	let result = match (a, b) {
		(Value::Path(p), v) => {
			let mut path = p.into_os_string();
			match generators::request_string_coerce(
				&co,
				v,
				CoercionKind {
					strong: false,

					// Concatenating a Path with something else results in a
					// Path, so we don't need to import any paths (paths
					// imported by Nix always exist as a string, unless
					// converted by the user). In C++ Nix they even may not
					// contain any string context, the resulting error of such a
					// case can not be replicated by us.
					import_paths: false,
					// FIXME(raitobezarius): per https://b.tvl.fyi/issues/364, this is a usecase
					// for having a `reject_context: true` option here. This didn't occur yet in
					// nixpkgs during my evaluations, therefore, I skipped it.
				},
			)
			.await
			{
				Ok(vs) => {
					path.push(vs.to_os_str()?);
					crate::value::canon_path(PathBuf::from(path)).into()
				}
				Err(c) => Value::Catchable(Box::new(c)),
			}
		}
		(Value::String(s1), Value::String(s2)) => Value::String(s1.concat(&s2)),
		(Value::String(s1), v) => generators::request_string_coerce(
			&co,
			v,
			CoercionKind {
				strong: false,
				// Behaves the same as string interpolation
				import_paths: true,
			},
		)
		.await
		.map(|s2| Value::String(s1.concat(&s2)))
		.into(),
		(a @ Value::Integer(_), b) | (a @ Value::Float(_), b) => {
			if !matches!(b, Value::Integer(_) | Value::Float(_)) {
				return Err(ErrorKind::Positioned(Box::new(ErrorKind::EvalError(
					format!(
						"cannot add {} to {}",
						crate::diagnostics::show_type(&b),
						crate::diagnostics::show_type(&a)
					),
				))));
			}
			arithmetic_op!(&a, &b, +).map_err(|e| ErrorKind::Positioned(Box::new(e)))?
		}
		(a, b) => {
			let r1 = generators::request_string_coerce(
				&co,
				a,
				CoercionKind {
					strong: false,
					import_paths: false,
				},
			)
			.await;
			let r2 = generators::request_string_coerce(
				&co,
				b,
				CoercionKind {
					strong: false,
					import_paths: false,
				},
			)
			.await;
			match (r1, r2) {
				(Ok(s1), Ok(s2)) => Value::String(s1.concat(&s2)),
				(Err(c), _) => return Ok(Value::from(c)),
				(_, Err(c)) => return Ok(Value::from(c)),
			}
		}
	};

	Ok(result)
}

/// The result of a VM's runtime evaluation.
pub struct RuntimeResult {
	pub value: Value,
	pub warnings: Vec<EvalWarning>,
}

// TODO(amjoseph): de-asyncify this
/// Generator that retrieves the final value from the stack, and deep-forces it
/// before returning. When the code is the caller's own (`hidden`), as a
/// command's list of results is, each element of a list result is forced on
/// its own, without a trace frame for its index.
async fn final_deep_force(co: GenCo, hidden: bool) -> Result<Value, ErrorKind> {
	let value = generators::request_stack_pop(&co).await;
	if hidden {
		let value = generators::request_force(&co, value).await;
		if let Value::List(list) = &value {
			for element in list.iter() {
				let element = generators::request_deep_force(&co, element.clone()).await;
				if element.is_catchable() {
					return Ok(element);
				}
			}
			return Ok(value);
		}
		return Ok(generators::request_deep_force(&co, value).await);
	}
	Ok(generators::request_deep_force(&co, value).await)
}

/// Specification for how to handle top-level values returned by evaluation
#[derive(Debug, Clone, Copy, Default)]
pub enum EvalMode {
	/// The default. Values are returned from evaluations as-is, without any extra forcing or
	/// special handling.
	#[default]
	Lazy,

	/// Strictly and deeply evaluate top-level values returned by evaluation.
	Strict,
}

pub fn run_lambda<IO>(
	nix_search_path: NixSearchPath,
	io_handle: IO,
	observer: Option<&mut dyn RuntimeObserver>,
	source: SourceCode,
	globals: Rc<GlobalsMap>,
	lambda: Rc<Lambda>,
	mode: EvalMode,
) -> EvalResult<RuntimeResult>
where
	IO: AsRef<dyn EvalIO> + 'static,
{
	// Retain the top-level span of the expression in this lambda, as
	// synthetic "calls" in deep_force will otherwise not have a span
	// to fall back to.
	//
	// We exploit the fact that the compiler emits a final instruction
	// with the span of the entire file for top-level expressions.
	let root_span = lambda.chunk.get_span(CodeIdx(lambda.chunk.code.len() - 1));

	let mut vm = VM::new(
		nix_search_path,
		io_handle,
		OptionalRuntimeObserver(observer),
		source,
		globals,
		root_span,
	);

	// When evaluating strictly, synthesise a frame that will instruct
	// the VM to deep-force the final value before returning it.
	match mode {
		EvalMode::Lazy => {}
		EvalMode::Strict => {
			let hidden = vm.source.is_hidden(root_span);
			vm.enqueue_generator("final_deep_force", root_span, move |co| {
				final_deep_force(co, hidden)
			})
		}
	}

	vm.frames.push(Frame::BytecodeFrame {
		span: root_span,
		bytecode_frame: BytecodeFrame {
			trace: FrameTrace::None,
			lambda,
			upvalues: Upvalues::with_capacity(0).into(),
			ip: CodeIdx(0),
			stack_offset: 0,
			depth: 0,
		},
	});

	vm.execute()
}

/// CppNix's `getName` for a computed attribute name: it must be a string,
/// and a failure is reported at the name.
fn attribute_name(key: Value, span: impl FnOnce() -> Span) -> Result<NixString, ErrorKind> {
	match key {
		Value::String(name) => Ok(name),
		Value::Thunk(ref thunk) if thunk.is_evaluated() => {
			attribute_name(thunk.value().clone(), span)
		}
		key => Err(ErrorKind::Positioned(Box::new(
			ErrorKind::TypeError {
				expected: "string",
				actual: key.type_of(),
				value: Some(Box::new(key.clone())),
			}
			.trace(Some(span()), "while evaluating an attribute name"),
		))),
	}
}
