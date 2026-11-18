//! This module implements the runtime representation of functions.
use std::{collections::BTreeMap, hash::Hash, rc::Rc};

use codemap::Span;

use crate::{chunk::Chunk, upvalues::Upvalues};

use super::NixString;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Formals {
	/// Map from argument name, to whether that argument is required
	pub(crate) arguments: BTreeMap<NixString, bool>,

	/// The span of each argument's name, which `builtins.functionArgs`
	/// gives its attributes as positions.
	pub(crate) argument_spans: BTreeMap<NixString, Span>,

	/// Do the formals of this function accept extra arguments
	pub(crate) ellipsis: bool,

	/// The span of the formals themselves, to use to emit errors
	pub(crate) span: Span,

	/// Optionally tracks a name for all function arguments (args@ style).
	/// Used by toXML.
	pub(crate) name: Option<String>,
}

impl Formals {
	/// Returns true if the given arg is a valid argument to these formals.
	///
	/// This is true if it is either listed in the list of arguments, or the formals have an
	/// ellipsis
	pub(crate) fn contains<Q>(&self, arg: &Q) -> bool
	where
		Q: ?Sized + Hash + Ord + Eq,
		NixString: std::borrow::Borrow<Q>,
	{
		self.ellipsis || self.arguments.contains_key(arg)
	}
}

/// The opcodes for a thunk or closure, plus the number of
/// non-executable opcodes which are allowed after an OpThunkClosure or
/// OpThunkSuspended referencing it.  At runtime `Lambda` is usually wrapped
/// in `Rc` to avoid copying the `Chunk` it holds (which can be
/// quite large).
///
/// In order to correctly reproduce cppnix's "pointer equality"
/// semantics it is important that we never clone a Lambda --
/// use `Rc<Lambda>::clone()` instead.  This struct deliberately
/// does not `derive(Clone)` in order to prevent this from being
/// done accidentally.
///
#[derive(/* do not add Clone here */ Debug, Default)]
pub struct Lambda {
	pub(crate) chunk: Chunk,

	/// Name of the function (equivalent to the name of the
	/// identifier (e.g. a value in a let-expression or an attribute
	/// set entry) it is located in).
	pub(crate) name: Option<LambdaName>,

	/// Number of upvalues which the code in this Lambda closes
	/// over, and which need to be initialised at
	/// runtime.
	pub(crate) upvalue_count: u32,
	pub(crate) formals: Option<Box<Formals>>,

	/// Parameter name for simple functions (e.g., `name: ...`); `None` for
	/// attribute-pattern functions and thunks. Used by builtins.toXML.
	pub(crate) param_name: Option<LambdaName>,

	/// Where the function starts, CppNix's `ExprLambda::pos`; `None` for
	/// thunks.
	pub(crate) span: Option<codemap::Span>,

	/// Diagnostic data that few lambdas have, allocated only when present.
	extra: Option<Box<LambdaExtra>>,

	/// For the blueprint of a suspended thunk, the span its creating
	/// instruction records, fixed when it is compiled.
	pub(crate) suspended_span: Option<Span>,
	/// Generated applications that use the function definition as their call site.
	pub(crate) generated_call_position: bool,
	pub(crate) omit_call_position: bool,
	pub(crate) synthetic_call: bool,
}

// A compiled lambda lives as long as any thunk or closure made from it, and
// evaluations keep about a million alive. With its two reference counts it
// takes 176 bytes, which mimalloc places in a 192-byte block; at 192 bytes or
// more it would take a 224-byte block.
const _: () = assert!(std::mem::size_of::<Lambda>() <= 160);

/// A function or parameter name. Names are identifiers, short enough to be
/// interned, so lambdas share their storage instead of each holding a copy.
#[derive(Clone, PartialEq)]
pub(crate) struct LambdaName(NixString);

impl LambdaName {
	pub(crate) fn new(name: &str) -> Self {
		Self(NixString::from(name))
	}

	pub(crate) fn as_str(&self) -> &str {
		std::str::from_utf8(self.0.as_bytes()).expect("lambda names are constructed from strings")
	}
}

impl std::ops::Deref for LambdaName {
	type Target = str;

	fn deref(&self) -> &str {
		self.as_str()
	}
}

impl std::fmt::Display for LambdaName {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		std::fmt::Display::fmt(self.as_str(), f)
	}
}

impl std::fmt::Debug for LambdaName {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		std::fmt::Debug::fmt(self.as_str(), f)
	}
}

#[derive(Debug)]
struct Selection {
	path: Rc<str>,
	span: Option<Span>,
}

#[derive(Debug, Default)]
struct LambdaExtra {
	/// For the thunk of an attribute selection, the selected path as CppNix
	/// shows it in `while evaluating the attribute 'PATH'`.
	selection: Option<Selection>,
	/// The left operand of additions that can fall back to path coercion.
	addition_left_spans: Option<Vec<(usize, Span)>>,
}

impl Lambda {
	fn extra_mut(&mut self) -> &mut LambdaExtra {
		self.extra.get_or_insert_with(Default::default)
	}

	fn selection(&self) -> Option<&Selection> {
		self.extra
			.as_ref()
			.and_then(|extra| extra.selection.as_ref())
	}

	pub(crate) fn set_selection(&mut self, path: Rc<str>, span: Option<Span>) {
		self.extra_mut().selection = Some(Selection { path, span });
	}

	pub(crate) fn selection_path(&self) -> Option<&Rc<str>> {
		self.selection().map(|selection| &selection.path)
	}

	pub(crate) fn selection_span(&self) -> Option<Span> {
		self.selection().and_then(|selection| selection.span)
	}

	/// Code positions of additions that can fall back to path coercion,
	/// with the span of each one's left operand.
	pub(crate) fn addition_left_spans(&self) -> Option<&Vec<(usize, Span)>> {
		self.extra
			.as_ref()
			.and_then(|extra| extra.addition_left_spans.as_ref())
	}

	pub(crate) fn push_addition_left_span(&mut self, code: usize, span: Span) {
		self.extra_mut()
			.addition_left_spans
			.get_or_insert_with(Default::default)
			.push((code, span));
	}

	/// The parameter of a plain lambda; attribute-pattern lambdas return `None`.
	pub fn parameter_name(&self) -> Option<&str> {
		self.formals
			.is_none()
			.then_some(self.param_name.as_deref().unwrap_or_default())
	}
	pub fn chunk(&mut self) -> &mut Chunk {
		&mut self.chunk
	}

	/// For a function taking an attribute set pattern, whether the pattern
	/// accepts extra attributes (`...`); `None` for a plain `name: ...`.
	pub fn formals_ellipsis(&self) -> Option<bool> {
		self.formals.as_ref().map(|formals| formals.ellipsis)
	}
}

///
/// In order to correctly reproduce cppnix's "pointer equality"
/// semantics it is important that we never clone a Lambda --
/// use `Rc<Lambda>::clone()` instead.  This struct deliberately
/// does not `derive(Clone)` in order to prevent this from being
/// done accidentally.
///
#[derive(/* do not add Clone here */ Debug)]
pub struct Closure {
	pub lambda: Rc<Lambda>,
	pub upvalues: Rc<Upvalues>,
}

impl Closure {
	pub fn new(lambda: Rc<Lambda>) -> Self {
		Self::new_with_upvalues(
			Rc::new(Upvalues::with_capacity(lambda.upvalue_count as usize)),
			lambda,
		)
	}

	pub fn new_with_upvalues(upvalues: Rc<Upvalues>, lambda: Rc<Lambda>) -> Self {
		Closure { upvalues, lambda }
	}

	pub fn chunk(&self) -> &Chunk {
		&self.lambda.chunk
	}

	pub fn lambda(&self) -> Rc<Lambda> {
		self.lambda.clone()
	}

	pub fn upvalues(&self) -> Rc<Upvalues> {
		self.upvalues.clone()
	}
}
