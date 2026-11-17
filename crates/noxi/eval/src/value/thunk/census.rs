//! Read-only ownership census over values reachable from sampled surviving thunks.
//!
//! Each reference-counted allocation the census can enumerate becomes one
//! node: thunk cells, attribute sets, lists, closures, shared closure
//! captures and `with` stacks. A node's strong count is compared with the
//! references found inside the explored graph. Any unexplained reference
//! belongs to an owner outside the graph, such as the VM, an evaluator cache,
//! a builtin's partial application, a native thunk, a lambda, or a node the
//! census did not expand. Such nodes, and everything they reach, are live.
//! The remaining nodes are owned only by one another, so releasing any one
//! of their cycles would release all of them.
//!
//! Every opaque owner can only make a node look live, never collectible, so
//! truncation and unsupported value kinds keep the result conservative. The
//! census never forces, mutates or frees anything. It holds one temporary
//! strong reference per discovered but unexpanded node and corrects for it.

use std::{
	cell::RefCell,
	collections::{BTreeMap, VecDeque},
	mem::size_of,
	rc::Rc,
	time::Instant,
};

use rustc_hash::FxHashMap;
use serde::Serialize;

use crate::{
	opcode::UpvalueIdx,
	value::{Closure, attrs::AttrsRep},
};

use super::{ThunkRepr, Upvalues, Value};

type Cell = RefCell<ThunkRepr>;

/// Thunk node budget of the bounded trial in `cycles.rs`.
const TRIAL_THUNK_LIMIT: u64 = 32;
/// Value slot budget of the same trial. Each census edge is one slot, but the
/// trial also counts slots without reference-counted values, so census edges
/// are a lower bound on the slots it would visit.
const TRIAL_SLOT_LIMIT: u64 = 256;
/// Stays below the 2^25-bucket capacity of the pointer index, so the index
/// never doubles to about 1.1 GiB. Override with
/// `NOXI_EVAL_OWNERSHIP_CENSUS_MAX_NODES`.
const DEFAULT_MAX_NODES: usize = 28_000_000;
/// Four outgoing references per node on average; 448 MB of edge storage.
/// Override with `NOXI_EVAL_OWNERSHIP_CENSUS_MAX_EDGES`.
const DEFAULT_MAX_EDGES: usize = 112_000_000;

const SUSPENDED: u8 = 0;
const EVALUATED: u8 = 1;
const NATIVE: u8 = 2;
const BLACKHOLE: u8 = 3;
const BORROWED: u8 = 4;
const ATTRS: u8 = 5;
const LIST: u8 = 6;
const CLOSURE: u8 = 7;
const UPVALUES: u8 = 8;
const WITH_STACK: u8 = 9;
const KINDS: [&str; 10] = [
	"thunk_suspended",
	"thunk_evaluated",
	"thunk_native",
	"thunk_blackhole",
	"thunk_borrowed",
	"attrs",
	"list",
	"closure",
	"upvalues",
	"with_stack",
];

const STRONG_BUCKETS: [&str; 6] = ["1", "2", "3-4", "5-8", "9-64", "65+"];
const SIZE_BUCKETS: [&str; 6] = ["0", "1", "2-32", "33-256", "257-4096", "4097+"];

#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
	pub max_nodes: usize,
	pub max_edges: usize,
}

impl Limits {
	/// Enabled by a non-empty `NOXI_EVAL_OWNERSHIP_CENSUS` other than `0`.
	pub(super) fn from_env() -> Option<Self> {
		let enabled = std::env::var_os("NOXI_EVAL_OWNERSHIP_CENSUS")?;
		if enabled.is_empty() || enabled == "0" {
			return None;
		}
		let read = |name: &str, default: usize| {
			std::env::var(name)
				.ok()
				.and_then(|value| value.parse::<usize>().ok())
				.unwrap_or(default)
				.min(u32::MAX as usize - 1)
		};
		Some(Self {
			max_nodes: read("NOXI_EVAL_OWNERSHIP_CENSUS_MAX_NODES", DEFAULT_MAX_NODES),
			max_edges: read("NOXI_EVAL_OWNERSHIP_CENSUS_MAX_EDGES", DEFAULT_MAX_EDGES),
		})
	}
}

#[derive(Debug, Default, Serialize)]
struct RootCensus {
	/// Sampled thunks alive when the census started.
	roots: u64,
	/// Sampled thunks owned only by other explored nodes.
	collectible: u64,
	/// `collectible` scaled by the sampling rate.
	estimated_collectible: u64,
	collectible_by_state: BTreeMap<&'static str, u64>,
}

#[derive(Debug, Default, Serialize)]
struct TypeCensus {
	nodes: u64,
	expanded: u64,
	/// Strong count above one.
	shared: u64,
	/// Shared, with every strong reference found inside the explored graph.
	shared_explained: u64,
	/// Strong references not found inside the explored graph.
	external_references: u64,
	/// Counts per `strong_count_bucket_labels` entry.
	strong_count_buckets: [u64; 6],
	collectible: u64,
	/// Collectible despite a strong count above one; the bounded trial
	/// treats these containers as opaque.
	collectible_shared: u64,
}

#[derive(Debug, Default, Serialize)]
struct Components {
	/// Weakly connected components of the collectible subgraph.
	count: u64,
	/// Component counts per `size_bucket_labels` entry, by thunk nodes.
	components_by_thunks: [u64; 6],
	/// Thunk nodes per `size_bucket_labels` entry of their component.
	thunks_by_component_thunks: [u64; 6],
	largest_nodes: u64,
	largest_thunks: u64,
	/// Collectible thunks in components with more than 32 thunk nodes.
	thunks_over_thunk_limit: u64,
	/// Collectible thunks in components with more than 256 internal references.
	thunks_over_slot_limit: u64,
	/// Collectible thunks in components that contain a shared container.
	thunks_with_shared_container: u64,
	/// Collectible thunks in components over any of the three limits above.
	/// The trial also stops at 128 thunk-to-thunk edges, which is not modelled,
	/// so this count is a lower bound.
	thunks_beyond_trial: u64,
	/// Collectible thunks in the remaining components.
	thunks_within_trial: u64,
}

#[derive(Debug, Serialize)]
pub(super) struct Report {
	max_nodes: u64,
	max_edges: u64,
	/// The first limit reached, if any. Results remain conservative.
	truncated: Option<&'static str>,
	nodes: u64,
	expanded: u64,
	edges: u64,
	explore_ms: u64,
	analyze_ms: u64,
	/// Census data structures only, from container capacities.
	estimated_peak_bytes: u64,
	sample_rate_inverse: u64,
	/// Nodes with more internal than strong references; nonzero indicates a census defect.
	count_mismatches: u64,
	roots: BTreeMap<&'static str, RootCensus>,
	strong_count_bucket_labels: [&'static str; 6],
	types: BTreeMap<&'static str, TypeCensus>,
	/// References into values the census does not traverse, plus dropped edges.
	opaque: BTreeMap<&'static str, u64>,
	/// References between collectible nodes, by source and target kind.
	collectible_edges: BTreeMap<String, u64>,
	size_bucket_labels: [&'static str; 6],
	components: Components,
}

/// A discovered node awaiting expansion. The clone keeps it alive while the
/// census runs and is excluded from the recorded strong count.
enum Pending {
	Thunk(Rc<Cell>),
	Attrs(Rc<AttrsRep>),
	List(Rc<Vec<Value>>),
	Closure(Rc<Closure>),
	Upvalues(Rc<Upvalues>),
	WithStack(Rc<[Value]>),
}

struct Graph {
	limits: Limits,
	index: FxHashMap<usize, u32>,
	strong: Vec<u32>,
	internal: Vec<u32>,
	kind: Vec<u8>,
	/// Nodes are expanded in index order, so edges form compressed rows.
	offsets: Vec<u32>,
	edges: Vec<u32>,
	pending: VecDeque<Pending>,
	peak_pending_capacity: usize,
	truncated: Option<&'static str>,
	opaque: BTreeMap<&'static str, u64>,
}

fn key<T: ?Sized>(rc: &Rc<T>) -> usize {
	Rc::as_ptr(rc).cast::<()>() as usize
}

fn thunk_kind(cell: &Cell) -> u8 {
	match cell.try_borrow().as_deref() {
		Ok(ThunkRepr::Suspended { .. }) => SUSPENDED,
		Ok(ThunkRepr::Evaluated(_)) => EVALUATED,
		Ok(ThunkRepr::Native(_)) => NATIVE,
		Ok(ThunkRepr::Blackhole { .. }) => BLACKHOLE,
		Err(_) => BORROWED,
	}
}

fn is_thunk(kind: u8) -> bool {
	kind <= BORROWED
}

fn strong_bucket(strong: u32) -> usize {
	match strong {
		0..=1 => 0,
		2 => 1,
		3..=4 => 2,
		5..=8 => 3,
		9..=64 => 4,
		_ => 5,
	}
}

fn size_bucket(size: u64) -> usize {
	match size {
		0 => 0,
		1 => 1,
		2..=32 => 2,
		33..=256 => 3,
		257..=4096 => 4,
		_ => 5,
	}
}

impl Graph {
	fn new(limits: Limits) -> Self {
		Self {
			limits,
			index: FxHashMap::default(),
			strong: Vec::new(),
			internal: Vec::new(),
			kind: Vec::new(),
			offsets: Vec::new(),
			edges: Vec::new(),
			pending: VecDeque::new(),
			peak_pending_capacity: 0,
			truncated: None,
			opaque: BTreeMap::new(),
		}
	}

	fn note(&mut self, label: &'static str) {
		let count = self.opaque.entry(label).or_default();
		*count = count.saturating_add(1);
	}

	/// Register an absent node. `strong` must exclude the census's own clone.
	fn add(&mut self, key: usize, strong: usize, kind: u8, pending: Pending) -> Option<u32> {
		if self.strong.len() >= self.limits.max_nodes {
			self.truncated.get_or_insert("nodes");
			self.note("edges_beyond_node_limit");
			return None;
		}
		let id = self.strong.len() as u32;
		self.index.insert(key, id);
		self.strong.push(u32::try_from(strong).unwrap_or(u32::MAX));
		self.internal.push(0);
		self.kind.push(kind);
		self.pending.push_back(pending);
		self.peak_pending_capacity = self.peak_pending_capacity.max(self.pending.capacity());
		Some(id)
	}

	/// Record one strong reference held by the node being expanded.
	fn link<T: ?Sized>(
		&mut self,
		rc: &Rc<T>,
		kind: impl FnOnce() -> u8,
		pending: impl FnOnce() -> Pending,
	) {
		if self.edges.len() >= self.limits.max_edges {
			self.truncated.get_or_insert("edges");
			self.note("edges_beyond_edge_limit");
			return;
		}
		let key = key(rc);
		let id = match self.index.get(&key) {
			Some(&id) => id,
			None => {
				// Read the count before the census clones the reference.
				let strong = Rc::strong_count(rc);
				match self.add(key, strong, kind(), pending()) {
					Some(id) => id,
					None => return,
				}
			}
		};
		let internal = &mut self.internal[id as usize];
		*internal = internal.saturating_add(1);
		self.edges.push(id);
	}

	fn value(&mut self, value: &Value) {
		match value {
			Value::Thunk(thunk) => self.link(
				&thunk.0,
				|| thunk_kind(&thunk.0),
				|| Pending::Thunk(Rc::clone(&thunk.0)),
			),
			Value::Attrs(attrs) => {
				self.link(&attrs.0, || ATTRS, || Pending::Attrs(Rc::clone(&attrs.0)))
			}
			Value::List(list) => {
				let storage = list.shared_storage();
				self.link(storage, || LIST, || Pending::List(Rc::clone(storage)))
			}
			Value::Closure(closure) => {
				self.link(closure, || CLOSURE, || Pending::Closure(Rc::clone(closure)))
			}
			Value::Builtin(_) => self.note("builtin_values"),
			Value::Blueprint(_) => self.note("blueprint_values"),
			Value::Catchable(_) => self.note("catchable_values"),
			Value::DeferredUpvalue(_) => self.note("deferred_upvalues"),
			_ => {}
		}
	}

	fn captures(&mut self, upvalues: &Upvalues) {
		for i in 0..upvalues.len() {
			self.value(&upvalues[UpvalueIdx(i)]);
		}
		if let Some(stack) = upvalues.shared_with_stack() {
			self.link(
				stack,
				|| WITH_STACK,
				|| Pending::WithStack(Rc::clone(stack)),
			);
		}
	}

	fn expand(&mut self, pending: Pending) {
		self.offsets.push(self.edges.len() as u32);
		match &pending {
			Pending::Thunk(cell) => {
				// Borrowed and blackholed cells are live owners whose
				// references stay unexplained.
				let Ok(repr) = cell.try_borrow() else { return };
				match &*repr {
					ThunkRepr::Suspended { upvalues, .. } => self.captures(upvalues),
					ThunkRepr::Evaluated(value) => self.value(value),
					ThunkRepr::Native(_) => self.note("native_thunks"),
					ThunkRepr::Blackhole { .. } => {}
				}
			}
			Pending::Attrs(rep) => match &**rep {
				AttrsRep::Empty => {}
				AttrsRep::KV { name, value } => {
					self.value(name);
					self.value(value);
				}
				AttrsRep::Map { attrs, .. } => {
					for (_, value) in attrs.iter() {
						self.value(value);
					}
				}
			},
			Pending::List(list) => {
				for value in list.iter() {
					self.value(value);
				}
			}
			Pending::Closure(closure) => {
				let upvalues = &closure.upvalues;
				self.link(
					upvalues,
					|| UPVALUES,
					|| Pending::Upvalues(Rc::clone(upvalues)),
				);
			}
			Pending::Upvalues(upvalues) => self.captures(upvalues),
			Pending::WithStack(stack) => {
				for value in stack.iter() {
					self.value(value);
				}
			}
		}
	}

	fn bytes(&self) -> usize {
		self.index.capacity() * (size_of::<(usize, u32)>() + 1)
			+ self.strong.capacity() * size_of::<u32>()
			+ self.internal.capacity() * size_of::<u32>()
			+ self.kind.capacity()
			+ self.offsets.capacity() * size_of::<u32>()
			+ self.edges.capacity() * size_of::<u32>()
			+ self.peak_pending_capacity * size_of::<Pending>()
	}
}

#[derive(Default)]
struct Group {
	nodes: u64,
	thunks: u64,
	/// References between collectible nodes, attributed to the source.
	edges: u64,
	/// Contains a container with a strong count above one.
	shared: bool,
}

/// Outgoing references of `node`; empty for unexpanded nodes.
fn row<'a>(edges: &'a [u32], offsets: &[u32], expanded: usize, node: usize) -> &'a [u32] {
	if node < expanded {
		&edges[offsets[node] as usize..offsets[node + 1] as usize]
	} else {
		&[]
	}
}

fn find(parent: &mut [u32], mut node: u32) -> u32 {
	while parent[node as usize] != node {
		let grandparent = parent[parent[node as usize] as usize];
		parent[node as usize] = grandparent;
		node = grandparent;
	}
	node
}

/// Explore everything reachable from `roots`, within `limits`, and classify it.
pub(super) fn run(
	roots: Vec<(&'static str, Rc<Cell>)>,
	limits: Limits,
	sample_rate_inverse: u64,
) -> Report {
	let started = Instant::now();
	let mut graph = Graph::new(limits);
	let mut root_ids = Vec::with_capacity(roots.len());
	for (label, cell) in roots {
		let key = key(&cell);
		if graph.index.contains_key(&key) {
			continue;
		}
		// Exclude the upgraded sample reference held by this function.
		let strong = Rc::strong_count(&cell) - 1;
		let kind = thunk_kind(&cell);
		if let Some(id) = graph.add(key, strong, kind, Pending::Thunk(cell)) {
			root_ids.push((label, id));
		}
	}

	while let Some(pending) = graph.pending.pop_front() {
		if graph.edges.len() >= graph.limits.max_edges {
			graph.truncated.get_or_insert("edges");
			break;
		}
		graph.expand(pending);
	}
	// Release every census clone before classification.
	graph.pending = VecDeque::new();
	graph.offsets.push(graph.edges.len() as u32);
	let explore_bytes = graph.bytes();
	graph.index = FxHashMap::default();
	let explore_ms = started.elapsed().as_millis() as u64;

	let started = Instant::now();
	let mut report = analyze(graph, &root_ids, sample_rate_inverse);
	report.explore_ms = explore_ms;
	report.analyze_ms = started.elapsed().as_millis() as u64;
	report.estimated_peak_bytes = report.estimated_peak_bytes.max(explore_bytes as u64);
	report
}

fn analyze(graph: Graph, root_ids: &[(&'static str, u32)], sample_rate_inverse: u64) -> Report {
	let Graph {
		limits,
		strong,
		internal,
		kind,
		offsets,
		edges,
		truncated,
		opaque,
		..
	} = graph;
	let nodes = strong.len();
	let expanded = offsets.len() - 1;
	let targets = |node: usize| row(&edges, &offsets, expanded, node);

	// Unexplained owners and active cells root the live set.
	let mut live = vec![false; nodes];
	let mut stack = Vec::new();
	let mut count_mismatches = 0;
	for node in 0..nodes {
		if internal[node] > strong[node] {
			count_mismatches += 1;
		}
		if strong[node] != internal[node] || matches!(kind[node], BLACKHOLE | BORROWED) {
			live[node] = true;
			stack.push(node as u32);
		}
	}
	while let Some(node) = stack.pop() {
		for &target in targets(node as usize) {
			if !live[target as usize] {
				live[target as usize] = true;
				stack.push(target);
			}
		}
	}

	let mut types: BTreeMap<&'static str, TypeCensus> = BTreeMap::new();
	for node in 0..nodes {
		let census = types.entry(KINDS[kind[node] as usize]).or_default();
		let external = strong[node].saturating_sub(internal[node]);
		census.nodes += 1;
		census.expanded += u64::from(node < expanded);
		census.external_references += u64::from(external);
		census.strong_count_buckets[strong_bucket(strong[node])] += 1;
		if strong[node] > 1 {
			census.shared += 1;
			census.shared_explained += u64::from(external == 0);
		}
		if !live[node] {
			census.collectible += 1;
			census.collectible_shared += u64::from(strong[node] > 1);
		}
	}

	let mut roots: BTreeMap<&'static str, RootCensus> = BTreeMap::new();
	for &(label, id) in root_ids {
		let census = roots.entry(label).or_default();
		census.roots += 1;
		if !live[id as usize] {
			census.collectible += 1;
			census.estimated_collectible += sample_rate_inverse;
			*census
				.collectible_by_state
				.entry(KINDS[kind[id as usize] as usize])
				.or_default() += 1;
		}
	}

	let mut parent: Vec<u32> = (0..nodes as u32).collect();
	let mut families = [[0u64; KINDS.len()]; KINDS.len()];
	for from in 0..expanded {
		if live[from] {
			continue;
		}
		for &to in targets(from) {
			if live[to as usize] {
				continue;
			}
			families[kind[from] as usize][kind[to as usize] as usize] += 1;
			let (a, b) = (find(&mut parent, from as u32), find(&mut parent, to));
			if a != b {
				parent[a.max(b) as usize] = a.min(b);
			}
		}
	}
	let mut collectible_edges = BTreeMap::new();
	for (from, row) in families.iter().enumerate() {
		for (to, &count) in row.iter().enumerate() {
			if count != 0 {
				collectible_edges.insert(format!("{}->{}", KINDS[from], KINDS[to]), count);
			}
		}
	}

	let mut groups: FxHashMap<u32, Group> = FxHashMap::default();
	for node in 0..nodes {
		if live[node] {
			continue;
		}
		let group = groups.entry(find(&mut parent, node as u32)).or_default();
		group.nodes += 1;
		if is_thunk(kind[node]) {
			group.thunks += 1;
		} else if strong[node] > 1 {
			group.shared = true;
		}
	}
	for from in 0..expanded {
		if live[from] {
			continue;
		}
		let inside = targets(from)
			.iter()
			.filter(|&&to| !live[to as usize])
			.count() as u64;
		if inside != 0 {
			let root = find(&mut parent, from as u32);
			groups
				.get_mut(&root)
				.expect("collectible node has a group")
				.edges += inside;
		}
	}
	let mut components = Components {
		count: groups.len() as u64,
		..Components::default()
	};
	for group in groups.values() {
		let thunks = group.thunks;
		components.components_by_thunks[size_bucket(thunks)] += 1;
		components.thunks_by_component_thunks[size_bucket(thunks)] += thunks;
		components.largest_nodes = components.largest_nodes.max(group.nodes);
		components.largest_thunks = components.largest_thunks.max(thunks);
		if thunks > TRIAL_THUNK_LIMIT {
			components.thunks_over_thunk_limit += thunks;
		}
		if group.edges > TRIAL_SLOT_LIMIT {
			components.thunks_over_slot_limit += thunks;
		}
		if group.shared {
			components.thunks_with_shared_container += thunks;
		}
		if thunks > TRIAL_THUNK_LIMIT || group.edges > TRIAL_SLOT_LIMIT || group.shared {
			components.thunks_beyond_trial += thunks;
		} else {
			components.thunks_within_trial += thunks;
		}
	}

	let analyze_bytes = strong.capacity() * size_of::<u32>()
		+ internal.capacity() * size_of::<u32>()
		+ kind.capacity()
		+ offsets.capacity() * size_of::<u32>()
		+ edges.capacity() * size_of::<u32>()
		+ live.capacity()
		+ parent.capacity() * size_of::<u32>()
		+ groups.capacity() * (size_of::<(u32, Group)>() + 1);

	Report {
		max_nodes: limits.max_nodes as u64,
		max_edges: limits.max_edges as u64,
		truncated,
		nodes: nodes as u64,
		expanded: expanded as u64,
		edges: edges.len() as u64,
		explore_ms: 0,
		analyze_ms: 0,
		estimated_peak_bytes: analyze_bytes as u64,
		sample_rate_inverse,
		count_mismatches,
		roots,
		strong_count_bucket_labels: STRONG_BUCKETS,
		types,
		opaque,
		collectible_edges,
		size_bucket_labels: SIZE_BUCKETS,
		components,
	}
}

#[cfg(test)]
mod tests {
	use std::rc::Weak;

	use super::*;
	use crate::{
		SourceCode,
		value::{Lambda, Thunk},
	};

	const LIMITS: Limits = Limits {
		max_nodes: 1024,
		max_edges: 4096,
	};

	fn thunk() -> Thunk {
		let source = SourceCode::default();
		let file = source.add_file("census-test".into(), "".into());
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

	fn census(root: &Weak<Cell>, limits: Limits) -> Report {
		run(vec![("test", root.upgrade().unwrap())], limits, 1024)
	}

	/// Break the test cycle so the test does not leak it.
	fn release(root: &Weak<Cell>) {
		if let Some(cell) = root.upgrade() {
			*cell.borrow_mut() = ThunkRepr::Evaluated(Value::Null);
		}
	}

	#[test]
	fn mutual_cycle_is_collectible_only_after_external_alias_drops() {
		let a = thunk();
		let b = thunk();
		capture(&a, &[&b]);
		capture(&b, &[&a]);
		let a_weak = Rc::downgrade(&a.0);
		drop(a);
		assert_eq!(census(&a_weak, LIMITS).roots["test"].collectible, 0);

		drop(b);
		let report = census(&a_weak, LIMITS);
		assert_eq!(report.truncated, None);
		assert_eq!(report.count_mismatches, 0);
		assert_eq!(report.roots["test"].collectible, 1);
		assert_eq!(report.types["thunk_suspended"].collectible, 2);
		assert_eq!(report.components.count, 1);
		assert_eq!(report.components.largest_thunks, 2);
		assert_eq!(report.components.thunks_within_trial, 2);
		// The census released nothing.
		assert_eq!(a_weak.strong_count(), 1);
		release(&a_weak);
		assert_eq!(a_weak.strong_count(), 0);
	}

	#[test]
	fn shared_container_owned_inside_the_cycle_is_collectible() {
		let root = thunk();
		let list = Value::List(vec![Value::Thunk(root.clone())].into());
		let attrs = Value::Attrs(
			[("x", list.clone()), ("y", list.clone())]
				.into_iter()
				.collect(),
		);
		*root.0.borrow_mut() = ThunkRepr::Evaluated(attrs);
		let weak = Rc::downgrade(&root.0);
		drop(root);
		assert_eq!(census(&weak, LIMITS).roots["test"].collectible, 0);

		drop(list);
		let report = census(&weak, LIMITS);
		assert_eq!(report.roots["test"].collectible, 1);
		let lists = &report.types["list"];
		assert_eq!(
			(
				lists.shared,
				lists.shared_explained,
				lists.collectible_shared
			),
			(1, 1, 1)
		);
		assert_eq!(report.components.thunks_with_shared_container, 1);
		assert_eq!(report.components.thunks_beyond_trial, 1);
		release(&weak);
		assert_eq!(weak.strong_count(), 0);
	}

	#[test]
	fn node_limit_keeps_the_census_conservative() {
		let a = thunk();
		let b = thunk();
		capture(&a, &[&b]);
		capture(&b, &[&a]);
		let a_weak = Rc::downgrade(&a.0);
		drop((a, b));
		let report = census(
			&a_weak,
			Limits {
				max_nodes: 1,
				max_edges: 16,
			},
		);
		assert_eq!(report.truncated, Some("nodes"));
		assert_eq!(report.roots["test"].collectible, 0);
		release(&a_weak);
		assert_eq!(a_weak.strong_count(), 0);
	}
}
