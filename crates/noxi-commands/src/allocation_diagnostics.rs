//! Process-wide requested allocation accounting for diagnostic builds only.
//! This wraps the normal mimalloc allocator. Bytes exclude allocator metadata,
//! fragmentation and retained free pages. Reallocation changes logical live
//! bytes by new size minus old size, not any temporary copying storage.

use std::alloc::{GlobalAlloc, Layout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[derive(Default)]
struct Accounting {
	allocations: AtomicU64,
	allocation_failures: AtomicU64,
	allocation_bytes: AtomicU64,
	reallocations: AtomicU64,
	reallocation_failures: AtomicU64,
	reallocation_old_bytes: AtomicU64,
	reallocation_new_bytes: AtomicU64,
	deallocations: AtomicU64,
	deallocation_bytes: AtomicU64,
	live_bytes: AtomicU64,
	peak_live_bytes: AtomicU64,
}

static ACCOUNTING: Accounting = Accounting {
	allocations: AtomicU64::new(0),
	allocation_failures: AtomicU64::new(0),
	allocation_bytes: AtomicU64::new(0),
	reallocations: AtomicU64::new(0),
	reallocation_failures: AtomicU64::new(0),
	reallocation_old_bytes: AtomicU64::new(0),
	reallocation_new_bytes: AtomicU64::new(0),
	deallocations: AtomicU64::new(0),
	deallocation_bytes: AtomicU64::new(0),
	live_bytes: AtomicU64::new(0),
	peak_live_bytes: AtomicU64::new(0),
};

/// Exact bins for requested sizes up to this bound, power-of-two bins above.
const EXACT_BIN_LIMIT: usize = 1024;
const BIN_COUNT: usize = EXACT_BIN_LIMIT + 1 + 64;
/// The live size bins are copied whenever live bytes pass the previous
/// snapshot's level by this step, so the copy is within one step of the peak.
const SNAPSHOT_STEP: u64 = 4 << 20;

struct Bins {
	live_count: [AtomicU64; BIN_COUNT],
	live_bytes: [AtomicU64; BIN_COUNT],
	peak_count: [AtomicU64; BIN_COUNT],
	peak_bytes: [AtomicU64; BIN_COUNT],
	peak_level: AtomicU64,
	next_snapshot: AtomicU64,
}

static BINS: Bins = Bins {
	live_count: [const { AtomicU64::new(0) }; BIN_COUNT],
	live_bytes: [const { AtomicU64::new(0) }; BIN_COUNT],
	peak_count: [const { AtomicU64::new(0) }; BIN_COUNT],
	peak_bytes: [const { AtomicU64::new(0) }; BIN_COUNT],
	peak_level: AtomicU64::new(0),
	next_snapshot: AtomicU64::new(SNAPSHOT_STEP),
};

fn bin(size: usize) -> usize {
	if size <= EXACT_BIN_LIMIT {
		size
	} else {
		EXACT_BIN_LIMIT + 1 + (usize::BITS - 1 - size.leading_zeros()) as usize
	}
}

fn bin_label(index: usize) -> u64 {
	if index <= EXACT_BIN_LIMIT {
		index as u64
	} else {
		1 << (index - EXACT_BIN_LIMIT - 1)
	}
}

impl Bins {
	fn add(&self, size: usize) {
		let index = bin(size);
		self.live_count[index].fetch_add(1, Relaxed);
		self.live_bytes[index].fetch_add(size as u64, Relaxed);
	}

	fn remove(&self, size: usize) {
		let index = bin(size);
		self.live_count[index].fetch_sub(1, Relaxed);
		self.live_bytes[index].fetch_sub(size as u64, Relaxed);
	}

	/// Copy the live bins when live bytes reach the next snapshot level.
	fn observe(&self, live: u64) {
		if live < self.next_snapshot.load(Relaxed) {
			return;
		}
		self.next_snapshot.store(live + SNAPSHOT_STEP, Relaxed);
		self.peak_level.store(live, Relaxed);
		for index in 0..BIN_COUNT {
			self.peak_count[index].store(self.live_count[index].load(Relaxed), Relaxed);
			self.peak_bytes[index].store(self.live_bytes[index].load(Relaxed), Relaxed);
		}
	}

	/// Nonempty bins, largest byte total first. Bins above the exact limit
	/// are labelled by their power-of-two lower bound.
	fn report(
		&self,
		counts: &[AtomicU64; BIN_COUNT],
		bytes: &[AtomicU64; BIN_COUNT],
	) -> serde_json::Value {
		let mut rows: Vec<_> = (0..BIN_COUNT)
			.map(|index| {
				(
					bin_label(index),
					index <= EXACT_BIN_LIMIT,
					counts[index].load(Relaxed),
					bytes[index].load(Relaxed),
				)
			})
			.filter(|(_, _, count, _)| *count != 0)
			.collect();
		rows.sort_by(|left, right| right.3.cmp(&left.3));
		rows.into_iter()
			.map(|(size, exact, count, bytes)| {
				serde_json::json!({"size": size, "exact": exact, "count": count, "bytes": bytes})
			})
			.collect()
	}
}

impl Accounting {
	fn grow(&self, bytes: u64) {
		let live = self
			.live_bytes
			.fetch_add(bytes, Relaxed)
			.wrapping_add(bytes);
		self.peak_live_bytes.fetch_max(live, Relaxed);
		BINS.observe(live);
	}

	fn allocated(&self, bytes: usize, succeeded: bool) {
		if !succeeded {
			self.allocation_failures.fetch_add(1, Relaxed);
			return;
		}
		self.allocations.fetch_add(1, Relaxed);
		self.allocation_bytes.fetch_add(bytes as u64, Relaxed);
		BINS.add(bytes);
		self.grow(bytes as u64);
	}

	fn deallocated(&self, bytes: usize) {
		BINS.remove(bytes);
		self.deallocations.fetch_add(1, Relaxed);
		self.deallocation_bytes.fetch_add(bytes as u64, Relaxed);
		self.live_bytes.fetch_sub(bytes as u64, Relaxed);
	}

	fn reallocated(&self, old: usize, new: usize, succeeded: bool) {
		if !succeeded {
			self.reallocation_failures.fetch_add(1, Relaxed);
			return;
		}
		self.reallocations.fetch_add(1, Relaxed);
		BINS.remove(old);
		BINS.add(new);
		self.reallocation_old_bytes.fetch_add(old as u64, Relaxed);
		self.reallocation_new_bytes.fetch_add(new as u64, Relaxed);
		if new >= old {
			self.grow((new - old) as u64);
		} else {
			self.live_bytes.fetch_sub((old - new) as u64, Relaxed);
		}
	}

	fn snapshot(&self) -> [u64; 11] {
		[
			self.allocations.load(Relaxed),
			self.allocation_failures.load(Relaxed),
			self.allocation_bytes.load(Relaxed),
			self.reallocations.load(Relaxed),
			self.reallocation_failures.load(Relaxed),
			self.reallocation_old_bytes.load(Relaxed),
			self.reallocation_new_bytes.load(Relaxed),
			self.deallocations.load(Relaxed),
			self.deallocation_bytes.load(Relaxed),
			self.live_bytes.load(Relaxed),
			self.peak_live_bytes.load(Relaxed),
		]
	}
}

pub(crate) struct CountingMiMalloc;

// The underlying allocator supplies exactly the same allocation operations as
// production. Accounting performs only atomic operations and cannot allocate.
unsafe impl GlobalAlloc for CountingMiMalloc {
	unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
		let ptr = unsafe { mimalloc::MiMalloc.alloc(layout) };
		ACCOUNTING.allocated(layout.size(), !ptr.is_null());
		ptr
	}

	unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
		let ptr = unsafe { mimalloc::MiMalloc.alloc_zeroed(layout) };
		ACCOUNTING.allocated(layout.size(), !ptr.is_null());
		ptr
	}

	unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
		// Record before making the storage available to another thread.
		ACCOUNTING.deallocated(layout.size());
		unsafe { mimalloc::MiMalloc.dealloc(ptr, layout) };
	}

	unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
		let result = unsafe { mimalloc::MiMalloc.realloc(ptr, layout, new_size) };
		ACCOUNTING.reallocated(layout.size(), new_size, !result.is_null());
		result
	}
}

pub(crate) struct ReportGuard(Option<PathBuf>);

impl ReportGuard {
	pub(crate) fn new() -> Self {
		Self(std::env::var_os("NOXI_EVAL_ALLOCATION_DIAGNOSTICS").map(PathBuf::from))
	}
}

impl Drop for ReportGuard {
	fn drop(&mut self) {
		let Some(path) = &self.0 else { return };
		// Freeze counters before JSON construction and file IO allocate. This is
		// a process-lifetime snapshot after the command thread has joined. Other
		// background threads, if any, can make its fields non-transactional.
		let [
			allocations,
			allocation_failures,
			allocation_bytes,
			reallocations,
			reallocation_failures,
			reallocation_old_bytes,
			reallocation_new_bytes,
			deallocations,
			deallocation_bytes,
			live_bytes,
			peak_live_bytes,
		] = ACCOUNTING.snapshot();
		let report = serde_json::json!({
			"schema": 1,
			"scope": "process_requested_allocation_bytes",
			"allocator": "mimalloc",
			"diagnostic_overhead": true,
			"allocation_count": allocations,
			"allocation_failure_count": allocation_failures,
			"allocation_requested_bytes": allocation_bytes,
			"reallocation_count": reallocations,
			"reallocation_failure_count": reallocation_failures,
			"reallocation_old_requested_bytes": reallocation_old_bytes,
			"reallocation_new_requested_bytes": reallocation_new_bytes,
			"deallocation_count": deallocations,
			"deallocation_requested_bytes": deallocation_bytes,
			"current_live_requested_bytes": live_bytes,
			"peak_live_requested_bytes": peak_live_bytes,
			"peak_snapshot_live_requested_bytes": BINS.peak_level.load(Relaxed),
			"peak_snapshot_bins": BINS.report(&BINS.peak_count, &BINS.peak_bytes),
			"final_live_bins": BINS.report(&BINS.live_count, &BINS.live_bytes),
		});
		let result = std::fs::File::create(path)
			.and_then(|file| serde_json::to_writer(file, &report).map_err(std::io::Error::other));
		if let Err(error) = result {
			eprintln!("could not write allocation diagnostics: {error}");
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn failed_operations_leave_live_accounting_unchanged() {
		let accounting = Accounting::default();
		accounting.allocated(32, true);
		accounting.allocated(4096, false);
		accounting.reallocated(32, 8192, false);
		assert_eq!(accounting.snapshot(), [1, 1, 32, 0, 1, 0, 0, 0, 0, 32, 32]);
		accounting.deallocated(32);
		assert_eq!(accounting.live_bytes.load(Relaxed), 0);
	}

	#[test]
	fn reallocations_account_for_growth_shrink_and_unchanged_sizes() {
		let accounting = Accounting::default();
		accounting.allocated(32, true);
		accounting.reallocated(32, 80, true);
		accounting.reallocated(80, 16, true);
		accounting.reallocated(16, 16, true);
		accounting.deallocated(16);
		assert_eq!(
			accounting.snapshot(),
			[1, 0, 32, 3, 0, 128, 112, 1, 16, 0, 80]
		);
	}
}
