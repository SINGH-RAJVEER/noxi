//! Opt-in, thread-local evaluator measurements, absent without `eval-diagnostics`.
//!
//! Counters accumulate across VMs until [`reset`]. Snapshots contain bounded
//! numeric data and static labels only. Allocation events describe instrumented
//! sites; payload bytes exclude allocator metadata, capacity slack and Rc headers.
//! They are neither total allocator traffic nor peak/live memory measurements.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

const MAX_LABELS: usize = 64;
const CAPTURE_BUCKETS: usize = 65;

#[derive(Clone, Debug, Default, Serialize)]
pub struct Allocation {
	pub events: u64,
	pub payload_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
	pub schema_version: u32,
	pub scope: &'static str,
	pub opcodes: BTreeMap<u8, u64>,
	/// Names for observed numeric opcodes, resolved only when taking a snapshot.
	pub opcode_names: BTreeMap<u8, String>,
	/// Pairs use `previous * 256 + current`; frame transitions break adjacency.
	pub opcode_pairs: BTreeMap<u16, u64>,
	pub frame_transitions: BTreeMap<&'static str, u64>,
	pub thunk_creations: BTreeMap<&'static str, u64>,
	pub thunk_first_forces: u64,
	pub thunk_cached_forces: u64,
	/// Finalized capture sets only, excluding clones and empty placeholders.
	/// Exact lengths 0..63; bucket 64 includes every length >= 64.
	pub static_capture_histogram: Vec<u64>,
	pub with_capture_histogram: Vec<u64>,
	pub capture_static_buffer_estimated_events: u64,
	pub capture_static_buffer_estimated_payload_bytes: u64,
	pub instrumented_allocations: BTreeMap<&'static str, Allocation>,
	pub layouts: BTreeMap<&'static str, usize>,
	pub dropped_labels: u64,
}

impl Default for Snapshot {
	fn default() -> Self {
		Self {
			schema_version: 1,
			scope: "current_thread_since_reset",
			opcodes: BTreeMap::new(),
			opcode_names: BTreeMap::new(),
			opcode_pairs: BTreeMap::new(),
			frame_transitions: BTreeMap::new(),
			thunk_creations: BTreeMap::new(),
			thunk_first_forces: 0,
			thunk_cached_forces: 0,
			static_capture_histogram: vec![0; CAPTURE_BUCKETS],
			with_capture_histogram: vec![0; CAPTURE_BUCKETS],
			capture_static_buffer_estimated_events: 0,
			capture_static_buffer_estimated_payload_bytes: 0,
			instrumented_allocations: BTreeMap::new(),
			layouts: BTreeMap::new(),
			dropped_labels: 0,
		}
	}
}

#[derive(Default)]
struct Counters {
	data: Snapshot,
	previous: Option<u8>,
}
thread_local! {
	static COUNTERS: RefCell<Counters> = RefCell::new(Counters::default());
}

/// Begin an independent measurement on this thread. Existing VMs are unaffected.
pub fn reset() {
	COUNTERS.with(|c| *c.borrow_mut() = Counters::default());
}

pub fn snapshot() -> Snapshot {
	let mut snapshot = COUNTERS.with(|c| c.borrow().data.clone());
	snapshot.opcode_names = snapshot
		.opcodes
		.keys()
		.map(|&opcode| (opcode, format!("{:?}", crate::opcode::Op::from(opcode))))
		.collect();
	snapshot
}

pub fn opcode(op: u8) {
	COUNTERS.with(|c| {
		let mut c = c.borrow_mut();
		let count = c.data.opcodes.entry(op).or_default();
		*count = count.saturating_add(1);
		if let Some(previous) = c.previous {
			let count = c
				.data
				.opcode_pairs
				.entry(u16::from(previous) * 256 + u16::from(op))
				.or_default();
			*count = count.saturating_add(1);
		}
		c.previous = Some(op);
	});
}

/// Break adjacency when switching frames or VMs; retain aggregate counts.
pub fn boundary() {
	COUNTERS.with(|c| c.borrow_mut().previous = None);
}

fn increment(map: &mut BTreeMap<&'static str, u64>, name: &'static str) -> bool {
	if name.len() > 80 || (!map.contains_key(name) && map.len() >= MAX_LABELS) {
		return false;
	}
	let count = map.entry(name).or_default();
	*count = count.saturating_add(1);
	true
}

pub fn frame_transition(name: &'static str) {
	COUNTERS.with(|c| {
		let mut c = c.borrow_mut();
		c.previous = None;
		if !increment(&mut c.data.frame_transitions, name) {
			c.data.dropped_labels = c.data.dropped_labels.saturating_add(1);
		}
	});
}

pub fn thunk_created(kind: &'static str) {
	COUNTERS.with(|c| {
		let mut c = c.borrow_mut();
		if !increment(&mut c.data.thunk_creations, kind) {
			c.data.dropped_labels = c.data.dropped_labels.saturating_add(1);
		}
	});
}

pub fn thunk_force(first: bool) {
	COUNTERS.with(|c| {
		let mut c = c.borrow_mut();
		let count = if first {
			&mut c.data.thunk_first_forces
		} else {
			&mut c.data.thunk_cached_forces
		};
		*count = count.saturating_add(1);
	});
}

pub fn capture_created(static_count: usize, with_count: usize, static_buffer_alloc: bool) {
	COUNTERS.with(|c| {
		let mut c = c.borrow_mut();
		let count = &mut c.data.static_capture_histogram[static_count.min(CAPTURE_BUCKETS - 1)];
		*count = count.saturating_add(1);
		let count = &mut c.data.with_capture_histogram[with_count.min(CAPTURE_BUCKETS - 1)];
		*count = count.saturating_add(1);
		if static_buffer_alloc {
			c.data.capture_static_buffer_estimated_events = c
				.data
				.capture_static_buffer_estimated_events
				.saturating_add(1);
			c.data.capture_static_buffer_estimated_payload_bytes = c
				.data
				.capture_static_buffer_estimated_payload_bytes
				.saturating_add(
					static_count.saturating_mul(std::mem::size_of::<crate::Value>()) as u64,
				);
		}
	});
}

/// Record one known allocation site and its requested payload, excluding headers.
pub fn allocation(kind: &'static str, payload_bytes: usize) {
	COUNTERS.with(|c| {
		let mut c = c.borrow_mut();
		let map = &mut c.data.instrumented_allocations;
		if kind.len() > 80 || (!map.contains_key(kind) && map.len() >= MAX_LABELS) {
			c.data.dropped_labels = c.data.dropped_labels.saturating_add(1);
			return;
		}
		let allocation = map.entry(kind).or_default();
		allocation.events = allocation.events.saturating_add(1);
		allocation.payload_bytes = allocation
			.payload_bytes
			.saturating_add(payload_bytes as u64);
	});
}

pub fn layout(name: &'static str, size: usize) {
	COUNTERS.with(|c| {
		let mut c = c.borrow_mut();
		if name.len() > 80
			|| (!c.data.layouts.contains_key(name) && c.data.layouts.len() >= MAX_LABELS)
		{
			c.data.dropped_labels = c.data.dropped_labels.saturating_add(1);
		} else {
			c.data.layouts.insert(name, size);
		}
	});
}

/// Atomically replace the explicitly selected output with the cumulative snapshot.
/// Different measuring threads/processes must select different output paths.
pub fn write_json(path: impl AsRef<Path>) -> io::Result<()> {
	static SERIAL: AtomicU64 = AtomicU64::new(0);
	let path = path.as_ref();
	let filename = path.file_name().ok_or_else(|| {
		io::Error::new(
			io::ErrorKind::InvalidInput,
			"diagnostic output requires a filename",
		)
	})?;
	let mut temporary_name = filename.to_os_string();
	temporary_name.push(format!(
		".{}.{}.tmp",
		std::process::id(),
		SERIAL.fetch_add(1, Ordering::Relaxed)
	));
	let temporary = path.with_file_name(temporary_name);
	let mut file = std::fs::OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(&temporary)?;
	let result = (|| {
		serde_json::to_writer(&mut file, &snapshot())?;
		file.write_all(b"\n")?;
		file.flush()?;
		std::fs::rename(&temporary, path)
	})();
	if result.is_err() {
		let _ = std::fs::remove_file(&temporary);
	}
	result
}

pub fn write_from_env() -> io::Result<()> {
	match std::env::var_os("NOXI_EVAL_DIAGNOSTICS") {
		Some(path) => write_json(PathBuf::from(path)),
		None => Ok(()),
	}
}

/// Flush on evaluation success or failure, retaining counts for subsequent VMs.
pub struct FlushGuard(Option<PathBuf>);
impl FlushGuard {
	pub fn from_env() -> Self {
		Self(std::env::var_os("NOXI_EVAL_DIAGNOSTICS").map(PathBuf::from))
	}
}
impl Drop for FlushGuard {
	fn drop(&mut self) {
		if let Some(path) = &self.0 {
			if let Err(error) = write_json(path) {
				eprintln!(
					"cannot write evaluator diagnostics to {}: {error}",
					path.display()
				);
			}
		}
	}
}
