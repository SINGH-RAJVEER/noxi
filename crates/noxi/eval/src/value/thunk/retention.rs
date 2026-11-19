//! Bounded, non-forcing search for closed paths through surviving thunk roots.

use rustc_hash::FxHashSet;

use crate::opcode::UpvalueIdx;

use super::{RcCell, ThunkRepr, Upvalues, Value};

const STATIC: u8 = 1;
const DYNAMIC: u8 = 2;
const MEMOIZED: u8 = 4;
const ATTRS: u8 = 8;
const LIST: u8 = 16;
const CLOSURE: u8 = 32;
const MAX_DEPTH: usize = 32;
const MAX_VISITS: usize = 1024;

type Cell = RcCell<ThunkRepr>;

pub(super) struct Probe {
	pub witness: Option<u8>,
	pub exhausted: bool,
}

struct Search {
	root: *const ThunkRepr,
	seen: FxHashSet<*const ThunkRepr>,
	visits: usize,
	exhausted: bool,
}

pub(super) fn probe(root: &Cell) -> Probe {
	let mut search = Search {
		root: RcCell::as_ptr(root),
		seen: FxHashSet::default(),
		visits: 0,
		exhausted: false,
	};
	let witness = search.cell(root, 0, 0);
	Probe {
		witness,
		exhausted: search.exhausted,
	}
}

impl Search {
	fn enter(&mut self, depth: usize) -> bool {
		if depth == MAX_DEPTH || self.visits == MAX_VISITS {
			self.exhausted = true;
			return false;
		}
		self.visits += 1;
		true
	}

	fn cell(&mut self, cell: &Cell, edges: u8, depth: usize) -> Option<u8> {
		let ptr = RcCell::as_ptr(cell);
		if depth != 0 && ptr == self.root {
			return Some(edges);
		}
		if !self.enter(depth) || !self.seen.insert(ptr) {
			return None;
		}
		match &*cell.try_borrow().ok()? {
			ThunkRepr::Suspended { upvalues, .. } => self.captures(upvalues, edges, depth),
			ThunkRepr::Evaluated(value) => self.value(value, edges | MEMOIZED, depth + 1),
			ThunkRepr::Blackhole { .. } | ThunkRepr::Native(_) => None,
		}
	}

	fn captures(&mut self, upvalues: &Upvalues, edges: u8, depth: usize) -> Option<u8> {
		let captures = (0..upvalues.len())
			.map(|i| (&upvalues[UpvalueIdx(i)], STATIC))
			.chain(upvalues.with_stack().iter().map(|value| (value, DYNAMIC)));
		self.values(captures, edges, depth)
	}

	fn values<'a>(
		&mut self,
		values: impl Iterator<Item = (&'a Value, u8)>,
		edges: u8,
		depth: usize,
	) -> Option<u8> {
		for (value, kind) in values {
			if depth + 1 == MAX_DEPTH || self.visits == MAX_VISITS {
				self.exhausted = true;
				break;
			}
			if let Some(witness) = self.value(value, edges | kind, depth + 1) {
				return Some(witness);
			}
		}
		None
	}

	fn value(&mut self, value: &Value, edges: u8, depth: usize) -> Option<u8> {
		if !self.enter(depth) {
			return None;
		}
		match value {
			Value::Thunk(thunk) => self.cell(&thunk.0, edges, depth + 1),
			Value::Attrs(attrs) => {
				self.values(attrs.iter().map(|(_, value)| (value, ATTRS)), edges, depth)
			}
			Value::List(list) => self.values(list.iter().map(|value| (value, LIST)), edges, depth),
			Value::Closure(closure) => self.captures(&closure.upvalues, edges | CLOSURE, depth),
			_ => None,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::value::{NixAttrs, Thunk};

	#[test]
	fn reports_closed_paths_through_memoized_attribute_values() {
		let root = Thunk::new_evaluated(Value::Null);
		assert!(probe(&root.0).witness.is_none());
		let attrs: NixAttrs = [("root", Value::Thunk(root.clone()))].into_iter().collect();
		*root.0.borrow_mut() = ThunkRepr::Evaluated(Value::Attrs(attrs));
		assert_eq!(probe(&root.0).witness, Some(MEMOIZED | ATTRS));
		*root.0.borrow_mut() = ThunkRepr::Evaluated(Value::Null);
	}
}
