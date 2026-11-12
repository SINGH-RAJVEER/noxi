#![cfg(feature = "eval-diagnostics")]

use noxi_eval::perf_counters as counters;

#[test]
fn opcode_pairs_stop_at_frame_and_vm_boundaries() {
	counters::reset();
	counters::opcode(1);
	counters::opcode(2);
	counters::frame_transition("bytecode_suspend");
	counters::opcode(3);
	counters::boundary();
	counters::opcode(4);
	let snapshot = counters::snapshot();
	assert_eq!(snapshot.opcodes.values().sum::<u64>(), 4);
	assert_eq!(snapshot.opcode_names.len(), snapshot.opcodes.len());
	assert_eq!(snapshot.opcode_names[&1], "Pop");
	assert_eq!(snapshot.opcode_pairs.len(), 1);
	assert_eq!(snapshot.opcode_pairs[&258], 1);
	assert_eq!(snapshot.frame_transitions["bytecode_suspend"], 1);
}

#[test]
fn captures_are_bounded_and_estimates_are_separate() {
	counters::reset();
	counters::capture_created(0, 0, false);
	counters::capture_created(1000, 2000, true);
	counters::allocation("thunk_rc", 24);
	counters::thunk_created("suspended");
	counters::thunk_force(true);
	counters::thunk_force(false);
	let snapshot = counters::snapshot();
	assert_eq!(snapshot.static_capture_histogram.len(), 65);
	assert_eq!(snapshot.static_capture_histogram[0], 1);
	assert_eq!(snapshot.static_capture_histogram[64], 1);
	assert_eq!(snapshot.with_capture_histogram[64], 1);
	assert_eq!(snapshot.capture_static_buffer_estimated_events, 1);
	assert_eq!(
		snapshot.capture_static_buffer_estimated_payload_bytes,
		(1000 * std::mem::size_of::<noxi_eval::Value>()) as u64
	);
	assert_eq!(snapshot.instrumented_allocations["thunk_rc"].events, 1);
	assert_eq!(
		snapshot.instrumented_allocations["thunk_rc"].payload_bytes,
		24
	);
	assert_eq!(snapshot.thunk_first_forces, 1);
	assert_eq!(snapshot.thunk_cached_forces, 1);
	counters::reset();
	assert!(counters::snapshot().opcodes.is_empty());
	assert_eq!(counters::snapshot().thunk_first_forces, 0);
}

#[test]
fn snapshots_accumulate_and_explicit_output_is_valid_json() {
	counters::reset();
	let directory = tempfile::tempdir().unwrap();
	let path = directory.path().join("counters.json");
	counters::opcode(7);
	counters::write_json(&path).unwrap();
	counters::boundary();
	counters::opcode(7);
	counters::write_json(&path).unwrap();
	let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
	assert_eq!(value["opcodes"]["7"], 2);
	assert!(
		value["opcode_names"]["7"]
			.as_str()
			.is_some_and(|name| !name.is_empty())
	);
	assert!(value["opcode_pairs"].as_object().unwrap().is_empty());
	assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
	let failed_path = directory.path().join("missing").join("counters.json");
	assert!(counters::write_json(failed_path).is_err());
	assert_eq!(counters::snapshot().opcodes[&7], 2);
}

#[test]
fn measurement_state_is_thread_local() {
	counters::reset();
	counters::opcode(9);
	std::thread::spawn(|| {
		assert!(counters::snapshot().opcodes.is_empty());
		counters::opcode(10);
		assert_eq!(counters::snapshot().opcodes[&10], 1);
	})
	.join()
	.unwrap();
	assert_eq!(counters::snapshot().opcodes[&9], 1);
	assert!(!counters::snapshot().opcodes.contains_key(&10));
}

#[test]
fn multiple_evaluations_keep_cumulative_vm_counts() {
	counters::reset();
	for expected in 1..=2 {
		let result = noxi_eval::Evaluation::builder_pure()
			.build()
			.evaluate("1 + 2", None);
		assert!(result.errors.is_empty(), "{:?}", result.errors);
		assert!(result.value.is_some());
		let snapshot = counters::snapshot();
		assert_eq!(snapshot.frame_transitions["vm_created"], expected);
		assert!(!snapshot.opcodes.is_empty());
	}
}
