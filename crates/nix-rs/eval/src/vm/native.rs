//! Operations on evaluated operands that do not need to suspend.
//!
//! Unsupported operands and failures use the generator path, retaining its
//! forcing order and error frames.

use std::cmp::Ordering;
use std::rc::Rc;

use bstr::ByteSlice;

use crate::value::{PointerEquality, canon_path};
use crate::{CoercionKind, Value, arithmetic_op};

pub(crate) fn equal(a: &Value, b: &Value, ptr_eq: PointerEquality) -> Option<bool> {
	Some(match (a, b) {
		(Value::Thunk(_) | Value::List(_) | Value::Attrs(_) | Value::Catchable(_), _)
		| (_, Value::Thunk(_) | Value::List(_) | Value::Attrs(_) | Value::Catchable(_)) => return None,
		(Value::Null, Value::Null) => true,
		(Value::Bool(a), Value::Bool(b)) => a == b,
		(Value::Integer(a), Value::Integer(b)) => a == b,
		(Value::Integer(a), Value::Float(b)) => *a as f64 == *b,
		(Value::Float(a), Value::Integer(b)) => *a == *b as f64,
		(Value::Float(a), Value::Float(b)) => a == b,
		(Value::String(a), Value::String(b)) => a == b,
		(Value::Path(a), Value::Path(b)) => a == b,
		(Value::Closure(a), Value::Closure(b)) => {
			ptr_eq >= PointerEquality::AllowNested && Rc::ptr_eq(a, b)
		}
		_ => false,
	})
}

pub(crate) fn compare(a: &Value, b: &Value) -> Option<Ordering> {
	Some(match (a, b) {
		(Value::Integer(a), Value::Integer(b)) => a.cmp(b),
		(Value::Float(a), Value::Float(b)) => a.total_cmp(b),
		(Value::Integer(a), Value::Float(b)) => (*a as f64).total_cmp(b),
		(Value::Float(a), Value::Integer(b)) => a.total_cmp(&(*b as f64)),
		(Value::String(a), Value::String(b)) => a.cmp(b),
		(Value::Path(a), Value::Path(b)) => a
			.as_os_str()
			.as_encoded_bytes()
			.cmp(b.as_os_str().as_encoded_bytes()),
		_ => return None,
	})
}

pub(super) fn add(a: &Value, b: &Value) -> Option<Value> {
	match (a, b) {
		(Value::String(a), Value::String(b)) => Some(Value::String(a.concat(b))),
		(Value::Path(a), Value::String(b)) => {
			let suffix = b.to_os_str().ok()?;
			let mut path = a.as_os_str().to_os_string();
			path.push(suffix);
			Some(canon_path(path.into()).into())
		}
		(Value::Integer(_) | Value::Float(_), _) => arithmetic_op!(a, b, +).ok(),
		_ => None,
	}
}

pub(crate) fn coerce(value: &Value, kind: CoercionKind) -> Option<Value> {
	Some(match (value, kind) {
		(Value::String(s), _) => Value::String(s.clone()),
		(
			Value::Path(path),
			CoercionKind {
				import_paths: false,
				..
			},
		) => Value::from(path.as_os_str().as_encoded_bytes()),
		(Value::Null | Value::Bool(false), CoercionKind { strong: true, .. }) => Value::from(""),
		(Value::Bool(true), CoercionKind { strong: true, .. }) => Value::from("1"),
		(Value::Integer(i), CoercionKind { strong: true, .. }) => Value::from(i.to_string()),
		(Value::Float(f), CoercionKind { strong: true, .. }) => Value::from(format!("{f:.6}")),
		_ => return None,
	})
}
