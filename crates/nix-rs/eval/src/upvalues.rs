//! This module encapsulates some logic for upvalue handling, which is
//! relevant to both thunks (delayed computations for lazy-evaluation)
//! as well as closures (lambdas that capture variables from the
//! surrounding scope).
//!
//! The upvalues of a scope are whatever data are needed at runtime
//! in order to resolve each free variable in the scope to a value.
//! "Upvalue" is a term taken from Lua.

use std::{ops::Index, rc::Rc};

use crate::{Value, opcode::UpvalueIdx};

/// Wrapper type to assist with assembling [Upvalues].
/// The first bit is used to signify that data for a `with` scope is
/// being used, the other 63 bits define the upvalue count.
pub struct UpvalueData(u64);

impl UpvalueData {
	#[inline]
	pub fn new(count: usize, captures_with: bool) -> Self {
		debug_assert!(
			count < (i64::MAX as usize),
			"upvalue count exceeds storage size"
		);

		let raw = ((count as u64) << 1) | (if captures_with { 1 } else { 0 });
		Self(raw)
	}

	#[inline]
	pub fn from_raw(value: u64) -> Self {
		Self(value)
	}

	#[inline]
	pub fn count(&self) -> usize {
		(self.0 as usize) >> 1
	}

	#[inline]
	pub fn captures_with(&self) -> bool {
		self.0 & 0b1 == 1
	}

	#[inline]
	pub fn into_raw(self) -> u64 {
		self.0
	}
}

/// Structure for carrying upvalues of an UpvalueCarrier.  The
/// implementation of this struct encapsulates the logic for
/// capturing and accessing upvalues.
///
/// Nix's `with` cannot be used to shadow an enclosing binding --
/// like Rust's `use xyz::*` construct, but unlike Javascript's
/// `with (xyz)`.  This means that Nix has two kinds of identifiers,
/// which can be distinguished at compile time:
///
/// - Static identifiers, which are bound in some enclosing scope by
///   `let`, `name:` or `{name}:`
/// - Dynamic identifiers, which are not bound in any enclosing
///   scope
#[derive(Clone, Debug)]
pub struct Upvalues {
	/// The upvalues of static identifiers.  Each static identifier
	/// is assigned an integer identifier at compile time, which is
	/// an index into this Vec.
	static_upvalues: Box<[Value]>,

	/// The upvalues of dynamic identifiers, if any exist.  This
	/// consists of the value passed to each enclosing `with val;`,
	/// from outermost to innermost. Thunks and closures created in the same
	/// `with` scopes share it.
	with_stack: Option<Rc<[Value]>>,
}

impl Upvalues {
	/// Upvalues are never added after construction, so the count only
	/// matters to callers constructing them whole.
	pub fn with_capacity(_count: usize) -> Self {
		Upvalues {
			static_upvalues: Box::default(),
			with_stack: None,
		}
	}

	/// Construct an [Upvalues] instance from the raw static and with stacks.
	pub fn from_raw_parts(static_upvalues: Vec<Value>, with_stack: Option<Rc<[Value]>>) -> Self {
		Self {
			static_upvalues: static_upvalues.into_boxed_slice(),
			with_stack,
		}
	}

	/// Get the number of static upvalues
	pub fn len(&self) -> usize {
		self.static_upvalues.len()
	}

	/// Retrieve a single value from the `with_stack`. Returns `None`
	/// if the stack doesn't exist or the value isn't in range.
	pub fn get_from_with_stack(&self, index: usize) -> Option<Value> {
		self.with_stack().get(index).cloned()
	}

	pub fn with_stack(&self) -> &[Value] {
		self.with_stack.as_deref().unwrap_or_default()
	}

	/// The `with` stack, to share with upvalues captured in the same scopes.
	pub fn shared_with_stack(&self) -> Option<&Rc<[Value]>> {
		self.with_stack.as_ref()
	}

	pub fn with_stack_len(&self) -> usize {
		self.with_stack().len()
	}

	pub fn into_static_upvalues(self) -> Vec<Value> {
		self.static_upvalues.into_vec()
	}

	/// Resolve deferred upvalues from the provided stack slice,
	/// mutating them in the internal upvalue slots.
	pub fn resolve_deferred_upvalues(&mut self, stack: &[Value]) {
		for upvalue in self.static_upvalues.iter_mut() {
			if let Value::DeferredUpvalue(update_from_idx) = upvalue {
				*upvalue = stack[update_from_idx.0].clone();
			}
		}
	}
}

impl Index<UpvalueIdx> for Upvalues {
	type Output = Value;

	fn index(&self, index: UpvalueIdx) -> &Self::Output {
		&self.static_upvalues[index.0]
	}
}

#[cfg(test)]
mod tests {
	use super::UpvalueData;
	#[test]
	fn test_upvalue() {
		assert_eq!(
			std::mem::size_of::<UpvalueData>(),
			std::mem::size_of::<usize>(),
			"The size of the UpvalueData should match the size of usize/u64."
		);
		let value = UpvalueData::new(456, false);
		assert!(!value.captures_with());
		assert_eq!(value.count(), 456);

		let value = UpvalueData::from_raw(value.into_raw());
		assert!(!value.captures_with());
		assert_eq!(value.count(), 456);

		let value = UpvalueData::new(1024, true);
		assert!(value.captures_with());
		assert_eq!(value.count(), 1024);

		let value = UpvalueData::from_raw(value.into_raw());
		assert!(value.captures_with());
		assert_eq!(value.count(), 1024);
	}
}
