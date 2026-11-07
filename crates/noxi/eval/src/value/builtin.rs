//! This module implements the runtime representation of a Nix
//! builtin.
//!
//! Builtins are directly backed by Rust code operating on Nix values.

use crate::errors::ErrorKind;
use crate::vm::generators::Generator;
use codemap::Span;

use super::{Thunk, Value};

use std::{
	fmt::{Debug, Display},
	rc::Rc,
};

/// Trait for closure types of builtins.
///
/// Builtins are expected to yield a generator which can be run by the VM to
/// produce the final value.
///
/// Implementors should use the builtins-macros to create these functions
/// instead of handling the argument-passing logic manually.
pub trait BuiltinGen: Fn(Vec<Value>) -> Generator {}
impl<F: Fn(Vec<Value>) -> Generator> BuiltinGen for F {}

#[derive(Clone)]
pub struct BuiltinRepr {
	name: &'static str,
	/// Optional documentation for the builtin.
	documentation: Option<&'static str>,
	arg_count: usize,

	func: Rc<dyn BuiltinGen>,
	synchronous: Option<SynchronousBuiltin>,

	/// Partially applied function arguments.
	partials: Vec<Value>,
}

#[derive(Clone)]
struct SynchronousBuiltin {
	strict: &'static [bool],
	func: SynchronousBody,
}

#[derive(Clone)]
enum SynchronousBody {
	Plain(Rc<dyn Fn(Vec<Value>) -> Result<Value, ErrorKind>>),
	Spanned(Rc<dyn Fn(Span, Vec<Value>) -> Result<Value, ErrorKind>>),
}

impl SynchronousBody {
	fn call(&self, args: Vec<Value>, span: impl FnOnce() -> Span) -> Result<Value, ErrorKind> {
		match self {
			Self::Plain(func) => func(args),
			Self::Spanned(func) => func(span(), args),
		}
	}
}

pub enum BuiltinResult {
	/// Builtin was not ready to be called (arguments missing) and remains
	/// partially applied.
	Partial(Builtin),

	/// Builtin was called and constructed a generator that the VM must run.
	Called(&'static str, Generator),

	/// A builtin whose body and arguments do not need to suspend.
	Synchronous(Result<Value, ErrorKind>),

	/// A synchronous body whose strict arguments still need evaluation.
	Pending(&'static str, PendingBuiltin),
}

/// Argument forcing for a synchronous builtin, resumed by the VM frame loop.
pub struct PendingBuiltin {
	builtin: Builtin,
	args: Vec<Value>,
	next: usize,
}

impl PendingBuiltin {
	pub(crate) fn awaiting_value(&self) -> bool {
		self.next != 0
	}

	/// Force strict arguments from left to right. Catchable arguments go back
	/// through the generated wrapper, which implements `#[catch]` and stops
	/// before forcing later arguments when the error must propagate.
	pub(crate) fn advance(
		&mut self,
		value: Option<Value>,
		span: Span,
	) -> Result<Thunk, BuiltinResult> {
		if let Some(value) = value {
			self.args[self.next - 1] = value;
			if self.args[self.next - 1].is_catchable() {
				return Err(self.fallback());
			}
		}
		let sync = self.builtin.0.synchronous.as_ref().unwrap();
		while self.next < self.args.len() {
			let index = self.next;
			self.next += 1;
			if !sync.strict[index] {
				continue;
			}
			match std::mem::replace(&mut self.args[index], Value::Null) {
				Value::Thunk(thunk) => match thunk.force_if_evaluated() {
					Ok(value) => self.args[index] = value,
					Err(thunk) => return Ok(thunk),
				},
				value => self.args[index] = value,
			}
			if self.args[index].is_catchable() {
				return Err(self.fallback());
			}
		}
		Err(BuiltinResult::Synchronous(
			sync.func.call(std::mem::take(&mut self.args), || span),
		))
	}

	fn fallback(&mut self) -> BuiltinResult {
		BuiltinResult::Called(
			self.builtin.name(),
			(self.builtin.0.func)(std::mem::take(&mut self.args)),
		)
	}
}

/// Represents a single built-in function which directly executes Rust
/// code that operates on a Nix value.
///
/// Builtins are the only functions in Nix that have varying arities
/// (for example, `hasAttr` has an arity of 2, but `isAttrs` an arity
/// of 1). To facilitate this generically, builtins expect to be
/// called with a vector of Nix values corresponding to their
/// arguments in order.
///
/// Partially applied builtins act similar to closures in that they
/// "capture" the partially applied arguments, and are treated
/// specially when printing their representation etc.
#[derive(Clone)]
pub struct Builtin(Rc<BuiltinRepr>);

impl From<BuiltinRepr> for Builtin {
	fn from(value: BuiltinRepr) -> Self {
		Builtin(Rc::new(value))
	}
}

impl Builtin {
	pub fn new<F: BuiltinGen + 'static>(
		name: &'static str,
		documentation: Option<&'static str>,
		arg_count: usize,
		func: F,
	) -> Self {
		BuiltinRepr {
			name,
			documentation,
			arg_count,
			func: Rc::new(func),
			synchronous: None,
			partials: vec![],
		}
		.into()
	}

	/// Register a synchronous body alongside its ordinary forcing generator.
	/// The macro supplies the same argument order and catchable propagation to
	/// both bodies. The VM forces strict arguments before calling the body.
	pub fn new_with_sync<F, S>(
		name: &'static str,
		documentation: Option<&'static str>,
		arg_count: usize,
		func: F,
		strict: &'static [bool],
		synchronous: S,
	) -> Self
	where
		F: BuiltinGen + 'static,
		S: Fn(Vec<Value>) -> Result<Value, ErrorKind> + 'static,
	{
		assert_eq!(strict.len(), arg_count);
		let mut builtin = Self::new(name, documentation, arg_count, func);
		Rc::get_mut(&mut builtin.0).unwrap().synchronous = Some(SynchronousBuiltin {
			strict,
			func: SynchronousBody::Plain(Rc::new(synchronous)),
		});
		builtin
	}

	/// Register a synchronous body that needs the call's source position.
	/// Builtins without a position parameter avoid looking it up.
	pub fn new_with_sync_span<F, S>(
		name: &'static str,
		documentation: Option<&'static str>,
		arg_count: usize,
		func: F,
		strict: &'static [bool],
		synchronous: S,
	) -> Self
	where
		F: BuiltinGen + 'static,
		S: Fn(Span, Vec<Value>) -> Result<Value, ErrorKind> + 'static,
	{
		assert_eq!(strict.len(), arg_count);
		let mut builtin = Self::new(name, documentation, arg_count, func);
		Rc::get_mut(&mut builtin.0).unwrap().synchronous = Some(SynchronousBuiltin {
			strict,
			func: SynchronousBody::Spanned(Rc::new(synchronous)),
		});
		builtin
	}

	pub fn name(&self) -> &'static str {
		self.0.name
	}

	/// Whether the builtin has been applied to some of its arguments.
	pub fn has_partials(&self) -> bool {
		!self.0.partials.is_empty()
	}

	pub fn documentation(&self) -> Option<&'static str> {
		self.0.documentation
	}

	/// Apply an additional argument to the builtin.
	/// After this, [`Builtin::call`] *must* be called, otherwise it may leave
	/// the builtin in an incorrect state.
	pub fn apply_arg(&mut self, arg: Value) {
		Rc::make_mut(&mut self.0).partials.push(arg);

		debug_assert!(
			self.0.partials.len() <= self.0.arg_count,
			"noxi bug: pushed too many arguments to builtin"
		);
	}

	/// Attempt to call a builtin, which will produce a generator if it is fully
	/// applied or return the builtin if it is partially applied.
	pub fn call(self) -> BuiltinResult {
		if self.0.partials.len() == self.0.arg_count {
			let repr = Rc::unwrap_or_clone(self.0);
			BuiltinResult::Called(repr.name, (repr.func)(repr.partials))
		} else {
			BuiltinResult::Partial(self)
		}
	}

	/// Apply and call without copying shared metadata on the final argument.
	/// Cloned partial applications retain independent argument vectors.
	#[cfg(test)]
	pub(crate) fn apply(self, arg: Value) -> BuiltinResult {
		self.apply_at(arg, || panic!("builtin did not request a span"))
	}

	pub(crate) fn apply_at(mut self, arg: Value, span: impl FnOnce() -> Span) -> BuiltinResult {
		if self.0.partials.len() + 1 == self.0.arg_count {
			let mut args = match Rc::get_mut(&mut self.0) {
				Some(repr) => std::mem::take(&mut repr.partials),
				None => self.0.partials.clone(),
			};
			args.push(arg);
			if let Some(sync) = &self.0.synchronous
				&& args.iter().zip(sync.strict).all(|(arg, strict)| {
					!strict || !matches!(arg, Value::Thunk(thunk) if !thunk.is_forced())
				}) {
				for (arg, strict) in args.iter_mut().zip(sync.strict) {
					if *strict && matches!(arg, Value::Thunk(_)) {
						let Value::Thunk(thunk) = std::mem::replace(arg, Value::Null) else {
							unreachable!();
						};
						*arg = thunk
							.force_if_evaluated()
							.expect("checked memoized argument");
					}
				}
				BuiltinResult::Synchronous(sync.func.call(args, span))
			} else if self.0.synchronous.is_some() {
				BuiltinResult::Pending(
					self.0.name,
					PendingBuiltin {
						builtin: self,
						args,
						next: 0,
					},
				)
			} else {
				BuiltinResult::Called(self.0.name, (self.0.func)(args))
			}
		} else {
			self.apply_arg(arg);
			BuiltinResult::Partial(self)
		}
	}
}

impl Debug for Builtin {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "builtin[{}]", self.0.name)
	}
}

impl Display for Builtin {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if !self.0.partials.is_empty() {
			f.write_str("<PRIMOP-APP>")
		} else {
			f.write_str("<PRIMOP>")
		}
	}
}

/// Builtins are uniquely identified by their name
impl PartialEq for Builtin {
	fn eq(&self, other: &Self) -> bool {
		self.0.name == other.0.name
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ready_arguments_skip_generator_construction() {
		let builtin = Builtin::new_with_sync(
			"identity",
			None,
			1,
			|_| panic!("ready argument constructed a generator"),
			&[true],
			|mut values| Ok(values.pop().unwrap()),
		);
		let BuiltinResult::Synchronous(Ok(Value::Integer(value))) = builtin.apply(42.into()) else {
			panic!("ready argument did not use synchronous builtin");
		};
		assert_eq!(value, 42);
	}
}
