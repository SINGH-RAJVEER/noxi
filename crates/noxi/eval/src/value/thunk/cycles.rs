//! Bounded trial deletion for cycles composed of direct thunk references.

use std::{
	cell::RefCell,
	collections::VecDeque,
	rc::{Rc, Weak},
};

use crate::opcode::UpvalueIdx;

use super::{Thunk, ThunkRepr, Upvalues, Value};
use crate::value::NixAttrs;

const MAX_CANDIDATES: usize = 4096;
const TRIALS_PER_PASS: usize = 4;
const MAX_ATTEMPTS: usize = 8;
const MAX_NODES: usize = 32;
const MAX_EDGES: usize = 128;
const MAX_SLOTS: usize = 256;

type Cell = RefCell<ThunkRepr>;

struct Candidate {
	thunk: Weak<Cell>,
	attempts: usize,
}

thread_local! {
	static CANDIDATES: RefCell<VecDeque<Candidate>> = const { RefCell::new(VecDeque::new()) };
}

fn enqueue(candidate: Candidate) {
	CANDIDATES.with_borrow_mut(|candidates| {
		if candidates.len() == MAX_CANDIDATES {
			candidates.pop_front();
		}
		candidates.push_back(candidate);
	});
}

pub(super) fn register(thunk: &Thunk) {
	enqueue(Candidate {
		thunk: Rc::downgrade(&thunk.0),
		attempts: 0,
	});
}

pub(super) fn collect() {
	let trials = CANDIDATES.with_borrow(|candidates| candidates.len().min(TRIALS_PER_PASS));
	for _ in 0..trials {
		let Some(mut candidate) = CANDIDATES.with_borrow_mut(VecDeque::pop_front) else {
			break;
		};
		let Some(root) = candidate.thunk.upgrade() else {
			continue;
		};
		// Dropping a removed representation can release other candidates.
		// Do not borrow the queue while inspecting or releasing a trial.
		let outcome = trial(root);
		#[cfg(feature = "eval-diagnostics")]
		record_outcome(&outcome);
		#[cfg(not(feature = "eval-diagnostics"))]
		let _ = outcome;
		candidate.attempts += 1;
		if candidate.attempts < MAX_ATTEMPTS && candidate.thunk.strong_count() != 0 {
			enqueue(candidate);
		}
	}
}

struct Node {
	cell: Rc<Cell>,
	incoming: usize,
	keep: bool,
}

impl Node {
	fn new(cell: Rc<Cell>) -> Self {
		Self {
			cell,
			incoming: 0,
			keep: false,
		}
	}
}

#[derive(Debug, PartialEq, Eq)]
enum TrialAbort {
	Budget,
	Borrow,
	Blackhole,
	Count,
}

#[cfg(feature = "eval-diagnostics")]
fn record_outcome(outcome: &Result<usize, TrialAbort>) {
	use crate::perf_counters::frame_transition;

	match outcome {
		Ok(0) => frame_transition("cycle_trial_rooted"),
		Ok(count) => {
			for _ in 0..*count {
				frame_transition("cycle_trial_collected_node");
			}
		}
		Err(TrialAbort::Budget) => frame_transition("cycle_trial_budget"),
		Err(TrialAbort::Borrow) => frame_transition("cycle_trial_borrowed"),
		Err(TrialAbort::Blackhole) => frame_transition("cycle_trial_blackhole"),
		Err(TrialAbort::Count) => frame_transition("cycle_trial_count_mismatch"),
	}
}

fn walk_value(
	value: &Value,
	slots: &mut usize,
	visit: &mut impl FnMut(&Thunk) -> Result<(), TrialAbort>,
) -> Result<(), TrialAbort> {
	*slots += 1;
	if *slots > MAX_SLOTS {
		return Err(TrialAbort::Budget);
	}

	// Only uniquely owned containers contribute internal edges. An owner
	// outside the trial could keep every value in a shared container alive.
	match value {
		Value::Thunk(thunk) => visit(thunk)?,
		Value::Attrs(attrs) => walk_attrs(attrs, slots, visit)?,
		Value::List(list) if list.is_uniquely_owned() => {
			for value in list {
				walk_value(value, slots, visit)?;
			}
		}
		Value::Closure(closure)
			if Rc::strong_count(closure) == 1 && Rc::strong_count(&closure.upvalues) == 1 =>
		{
			walk_captures(&closure.upvalues, slots, visit)?;
		}
		_ => {}
	}
	Ok(())
}

/// The values a uniquely owned set holds. A layered set holds its layers,
/// which are owned, and so walked, separately.
fn walk_attrs(
	attrs: &NixAttrs,
	slots: &mut usize,
	visit: &mut impl FnMut(&Thunk) -> Result<(), TrialAbort>,
) -> Result<(), TrialAbort> {
	if Rc::strong_count(&attrs.0) != 1 {
		return Ok(());
	}
	if let Some((base, top)) = attrs.layers() {
		walk_attrs(base, slots, visit)?;
		return walk_attrs(top, slots, visit);
	}
	for (_, value) in attrs.iter() {
		walk_value(value, slots, visit)?;
	}
	Ok(())
}

fn walk_captures(
	upvalues: &Upvalues,
	slots: &mut usize,
	visit: &mut impl FnMut(&Thunk) -> Result<(), TrialAbort>,
) -> Result<(), TrialAbort> {
	for i in 0..upvalues.len() {
		walk_value(&upvalues[UpvalueIdx(i)], slots, visit)?;
	}
	if let Some(stack) = upvalues.shared_with_stack()
		&& Rc::strong_count(stack) == 1
	{
		for value in stack.iter() {
			walk_value(value, slots, visit)?;
		}
	}
	Ok(())
}

fn walk_edges(
	repr: &ThunkRepr,
	slots: &mut usize,
	mut visit: impl FnMut(&Thunk) -> Result<(), TrialAbort>,
) -> Result<(), TrialAbort> {
	match repr {
		ThunkRepr::Suspended { upvalues, .. } => walk_captures(upvalues, slots, &mut visit),
		ThunkRepr::Evaluated(value) => walk_value(value, slots, &mut visit),
		ThunkRepr::Blackhole { .. } => Err(TrialAbort::Blackhole),
		ThunkRepr::Native(_) => Ok(()),
	}
}

fn trial(root: Rc<Cell>) -> Result<usize, TrialAbort> {
	let mut nodes = vec![Node::new(root)];
	let mut edges = Vec::<(usize, usize)>::new();
	let mut slots = 0;

	let mut from = 0;
	while from < nodes.len() {
		// This temporary keeps the borrow independent of the vector, and
		// drops before strong references are counted below.
		let cell = Rc::clone(&nodes[from].cell);
		let repr = cell.try_borrow().map_err(|_| TrialAbort::Borrow)?;
		walk_edges(&repr, &mut slots, |to| {
			if edges.len() == MAX_EDGES {
				return Err(TrialAbort::Budget);
			}
			let to = match nodes.iter().position(|node| Rc::ptr_eq(&node.cell, &to.0)) {
				Some(index) => index,
				None => {
					if nodes.len() == MAX_NODES {
						return Err(TrialAbort::Budget);
					}
					nodes.push(Node::new(Rc::clone(&to.0)));
					nodes.len() - 1
				}
			};
			nodes[to].incoming += 1;
			edges.push((from, to));
			Ok(())
		})?;
		from += 1;
	}

	for node in &mut nodes {
		// Every represented slot contributes one strong reference, and the
		// trial holds exactly one more. Unknown or shared edges contribute
		// excess references and therefore keep their targets alive.
		let external = Rc::strong_count(&node.cell)
			.checked_sub(node.incoming + 1)
			.ok_or(TrialAbort::Count)?;
		node.keep = external != 0
			|| matches!(
				*node.cell.try_borrow().map_err(|_| TrialAbort::Borrow)?,
				ThunkRepr::Native(_)
			);
	}

	let mut changed = true;
	while changed {
		changed = false;
		for &(from, to) in &edges {
			if nodes[from].keep && !nodes[to].keep {
				nodes[to].keep = true;
				changed = true;
			}
		}
	}

	// Acquire all mutation borrows before touching any representation. If
	// a borrower exists, the whole trial leaves the graph unchanged.
	let mut dead = Vec::new();
	for node in nodes.iter().filter(|node| !node.keep) {
		dead.push(node.cell.try_borrow_mut().map_err(|_| TrialAbort::Borrow)?);
	}
	let mut removed = Vec::with_capacity(dead.len());
	for repr in &mut dead {
		removed.push(std::mem::replace(
			&mut **repr,
			ThunkRepr::Evaluated(Value::Null),
		));
	}
	drop(dead);
	let count = removed.len();
	drop(removed);
	Ok(count)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{SourceCode, value::Lambda};

	fn thunk() -> Thunk {
		let source = SourceCode::default();
		let file = source.add_file("cycle-test".into(), "".into());
		Thunk::new_suspended(Rc::new(Lambda::default()), file.span)
	}

	fn capture(from: &Thunk, to: &[&Thunk]) {
		*from.upvalues_mut() = Upvalues::from_raw_parts(
			to.iter()
				.map(|thunk| Value::Thunk((*thunk).clone()))
				.collect(),
			None,
		);
	}

	#[test]
	fn collects_self_cycle_with_duplicate_edges() {
		let a = thunk();
		capture(&a, &[&a, &a]);
		let weak = Rc::downgrade(&a.0);
		drop(a);
		assert_eq!(trial(weak.upgrade().unwrap()), Ok(1));
		assert_eq!(weak.strong_count(), 0);
	}

	#[test]
	fn collects_mutual_cycles_only_after_external_alias_drops() {
		let a = thunk();
		let b = thunk();
		capture(&a, &[&b]);
		capture(&b, &[&a]);
		let a_weak = Rc::downgrade(&a.0);
		let b_weak = Rc::downgrade(&b.0);
		drop(a);
		assert_eq!(trial(a_weak.upgrade().unwrap()), Ok(0));
		assert!(b.is_suspended());
		drop(b);
		assert_eq!(trial(a_weak.upgrade().unwrap()), Ok(2));
		assert_eq!(a_weak.strong_count(), 0);
		assert_eq!(b_weak.strong_count(), 0);
	}

	#[test]
	fn shared_container_keeps_cycle_alive_until_its_alias_drops() {
		let root = thunk();
		let list = Value::List(vec![Value::Thunk(root.clone())].into());
		let attrs = Value::Attrs([("list", list.clone())].into_iter().collect());
		*root.0.borrow_mut() = ThunkRepr::Evaluated(attrs);
		let weak = Rc::downgrade(&root.0);
		drop(root);

		assert_eq!(trial(weak.upgrade().unwrap()), Ok(0));
		drop(list);
		assert_eq!(trial(weak.upgrade().unwrap()), Ok(1));
		assert_eq!(weak.strong_count(), 0);
	}

	#[test]
	fn opaque_native_capture_keeps_referenced_cycle_alive() {
		let a = thunk();
		capture(&a, &[&a]);
		let weak = Rc::downgrade(&a.0);
		let native = Thunk::new_suspended_native(Box::new(move || Ok(Value::Thunk(a.clone()))));
		assert_eq!(trial(weak.upgrade().unwrap()), Ok(0));
		drop(native);
		assert_eq!(trial(weak.upgrade().unwrap()), Ok(1));
		assert_eq!(weak.strong_count(), 0);
	}

	#[test]
	fn borrow_conflict_leaves_entire_trial_unchanged() {
		let a = thunk();
		let b = thunk();
		capture(&a, &[&b]);
		capture(&b, &[&a]);
		let a_weak = Rc::downgrade(&a.0);
		let b_weak = Rc::downgrade(&b.0);
		let guard = b.0.borrow_mut();
		drop(a);
		assert_eq!(trial(a_weak.upgrade().unwrap()), Err(TrialAbort::Borrow));
		drop(guard);
		drop(b);
		assert_eq!(trial(a_weak.upgrade().unwrap()), Ok(2));
		assert_eq!(b_weak.strong_count(), 0);
	}
}
