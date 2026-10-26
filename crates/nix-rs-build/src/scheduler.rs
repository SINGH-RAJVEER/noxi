//! One invocation's local build graph. Ready goals are ordered by name/path;
//! shared dependencies run once, and only completed inputs release a goal.

use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::time::Duration;

use super::*;

#[derive(Debug, Default)]
pub struct BuildBatch {
	pub outputs: BTreeMap<StorePath, BTreeMap<String, StorePath>>,
	pub failures: BTreeMap<StorePath, BuildError>,
}

impl BuildBatch {
	pub fn exit_status(&self) -> u8 {
		if self.failures.is_empty() {
			return 0;
		}
		let mut build_failure = false;
		let mut flags = 0;
		for error in self.failures.values() {
			match error.status() {
				FailureStatus::PermanentFailure | FailureStatus::InputRejected => {
					build_failure = true
				}
				FailureStatus::TimedOut => {
					build_failure = true;
					flags |= 1;
				}
				FailureStatus::HashMismatch => {
					build_failure = true;
					flags |= 2;
				}
				FailureStatus::NotDeterministic => {
					build_failure = true;
					flags |= 8;
				}
				_ => {}
			}
		}
		if build_failure { 100 | flags } else { 1 }
	}
}

#[derive(Default)]
struct Goal {
	wanted: BTreeSet<String>,
	inputs: BTreeSet<StorePath>,
}

enum Message {
	Started(StorePath),
	Output(StorePath, Vec<u8>),
	Finished(StorePath, Option<BuildError>),
	Done(StorePath, Result<BTreeMap<String, StorePath>>),
}

struct StopOnDrop<'a>(&'a AtomicBool);
impl Drop for StopOnDrop<'_> {
	fn drop(&mut self) {
		self.0.store(true, Ordering::Relaxed);
	}
}

impl LocalBuilder<'_> {
	fn collect_goals(
		&self,
		path: &StorePath,
		wanted: Option<&BTreeSet<String>>,
		goals: &mut BTreeMap<StorePath, Goal>,
		visiting: &mut BTreeSet<StorePath>,
	) -> Result<()> {
		self.check_cancelled()?;
		if visiting.contains(path) {
			return Err(BuildError::Other(format!(
				"cycle detected in the input derivations of '{}'",
				print(path)
			)));
		}
		let drv = self.store.read_build_derivation(path)?;
		let all: BTreeSet<String> = drv
			.basic
			.outputs()
			.names()
			.map(ToString::to_string)
			.collect();
		let wanted = wanted.cloned().unwrap_or(all.clone());
		if let Some(missing) = wanted.difference(&all).next() {
			return Err(BuildError::Other(format!(
				"derivation '{}' does not have wanted outputs '{missing}'",
				print(path)
			)));
		}
		let node = goals.entry(path.clone()).or_default();
		let expanded = !node.inputs.is_empty();
		node.wanted.extend(wanted);
		if expanded {
			return Ok(());
		}
		// Match the sequential builder's pre-input substitution policy. A
		// cached output must not cause its otherwise unused inputs to build.
		let needed = if drv.basic.outputs().is_deferred() && drv.floating.is_empty() {
			true
		} else {
			let outputs: BTreeMap<_, _> = self
				.known_outputs(path, &drv)?
				.into_iter()
				.filter(|(name, _)| node.wanted.contains(name))
				.collect();
			if !self.all_valid(&outputs)? && self.substitutes_allowed(&drv.basic)? {
				for output in outputs.values().filter(|o| o.floating.is_none()) {
					if !self.store.is_valid_path(&output.path)? {
						self.try_substitute(&output.path)?;
					}
				}
			}
			!self.all_valid(&outputs)?
		};
		if !needed {
			return Ok(());
		}
		visiting.insert(path.clone());
		for (input, names) in drv.basic.input_derivations() {
			let names = names.iter().map(ToString::to_string).collect();
			self.collect_goals(input, Some(&names), goals, visiting)?;
			goals.get_mut(path).unwrap().inputs.insert(input.clone());
		}
		visiting.remove(path);
		Ok(())
	}

	/// Realise a set of derivations with one bounded worker graph. `None`
	/// requests all outputs. Callbacks run on the calling thread, with an
	/// explicit derivation identity for interleaved output. Failed goals and
	/// their dependents are returned in `failures`; successful goals remain
	/// available when `keep-going` is enabled.
	pub fn realise_many_with_events(
		&self,
		targets: &[(StorePath, Option<BTreeSet<String>>)],
		events: &mut dyn FnMut(&StorePath, BuildEvent<'_>),
	) -> Result<BuildBatch> {
		let mut goals = BTreeMap::new();
		for (path, wanted) in targets {
			self.collect_goals(path, wanted.as_ref(), &mut goals, &mut BTreeSet::new())?;
		}
		let key = |path: &StorePath| (path.name().to_owned(), path.clone());
		let mut ready = BTreeSet::new();
		let mut waiting = BTreeMap::new();
		let mut dependents: BTreeMap<StorePath, Vec<StorePath>> = BTreeMap::new();
		for (path, node) in &goals {
			waiting.insert(path.clone(), node.inputs.len());
			if node.inputs.is_empty() {
				ready.insert(key(path));
			}
			for input in &node.inputs {
				dependents
					.entry(input.clone())
					.or_default()
					.push(path.clone());
			}
		}
		let stop = AtomicBool::new(false);
		let limit = usize::try_from(self.settings.max_build_jobs.max(1)).unwrap_or(usize::MAX);
		std::thread::scope(|scope| {
			let _stop_on_drop = StopOnDrop(&stop);
			let (tx, rx) = mpsc::sync_channel(64);
			let mut active = BTreeMap::new();
			let mut batch = BuildBatch::default();
			loop {
				if self.is_cancelled() {
					stop.store(true, Ordering::Relaxed);
				}
				while active.len() < limit && !stop.load(Ordering::Relaxed) {
					let Some((_, path)) = ready.pop_first() else {
						break;
					};
					let node = &goals[&path];
					if node.inputs.iter().any(|p| batch.failures.contains_key(p)) {
						batch.failures.insert(
							path.clone(),
							BuildError::failed(
								FailureStatus::DependencyFailed,
								format!(
									"Cannot build '{}'.\nReason: 1 dependency failed.",
									print(&path)
								),
							),
						);
						if let Some(parents) = dependents.get(&path) {
							for parent in parents {
								let count = waiting.get_mut(parent).unwrap();
								*count -= 1;
								if *count == 0 {
									ready.insert(key(parent));
								}
							}
						}
						continue;
					}
					let tx = tx.clone();
					let worker_path = path.clone();
					let stop = &stop;
					let handle = scope.spawn(move || {
						let builder = LocalBuilder::new(self.store, self.settings)
							.with_substituters(self.substituters)
							.with_cancellation(stop);
						let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
							let mut current = worker_path.clone();
							builder.realise_inner(
								&worker_path,
								Some(&node.wanted),
								&mut BTreeMap::new(),
								&mut Vec::new(),
								&mut |event| {
									let message = match event {
										BuildEvent::Started { drv_path } => {
											current = drv_path.clone();
											Message::Started(drv_path.clone())
										}
										BuildEvent::Output(data) => {
											Message::Output(current.clone(), data.to_vec())
										}
										BuildEvent::Finished { drv_path, error } => {
											Message::Finished(
												drv_path.clone(),
												error.map(|e| match e {
													BuildError::Cancelled => BuildError::Cancelled,
													_ => BuildError::failed(
														e.status(),
														e.to_string(),
													),
												}),
											)
										}
									};
									let _ = tx.send(message);
								},
							)
						}))
						.unwrap_or_else(|_| {
							Err(BuildError::Other("local build worker panicked".to_owned()))
						});
						let _ = tx.send(Message::Done(worker_path, result));
					});
					active.insert(path, handle);
				}
				if active.is_empty() {
					break;
				}
				match rx.recv_timeout(Duration::from_millis(50)) {
					Ok(Message::Started(path)) => {
						events(&path, BuildEvent::Started { drv_path: &path })
					}
					Ok(Message::Output(path, data)) => events(&path, BuildEvent::Output(&data)),
					Ok(Message::Finished(path, error)) => events(
						&path,
						BuildEvent::Finished {
							drv_path: &path,
							error: error.as_ref(),
						},
					),
					Ok(Message::Done(path, result)) => {
						active
							.remove(&path)
							.unwrap()
							.join()
							.expect("worker catches panics");
						match result {
							Ok(outputs) => {
								batch.outputs.insert(path.clone(), outputs);
							}
							Err(error) => {
								batch.failures.insert(path.clone(), error);
								if !self.settings.keep_going {
									stop.store(true, Ordering::Relaxed);
								}
							}
						}
						if let Some(parents) = dependents.get(&path) {
							for parent in parents {
								let count = waiting.get_mut(parent).unwrap();
								*count -= 1;
								if *count == 0 {
									ready.insert(key(parent));
								}
							}
						}
					}
					Err(mpsc::RecvTimeoutError::Timeout) => {}
					Err(mpsc::RecvTimeoutError::Disconnected) => {
						unreachable!("scheduler retains a sender")
					}
				}
			}
			for path in goals.keys() {
				if !batch.outputs.contains_key(path) && !batch.failures.contains_key(path) {
					let error = if self.is_cancelled() {
						BuildError::Cancelled
					} else {
						BuildError::failed(
							FailureStatus::DependencyFailed,
							format!(
								"Cannot build '{}'.\nReason: 1 dependency failed.",
								print(path)
							),
						)
					};
					batch.failures.insert(path.clone(), error);
				}
			}
			Ok(batch)
		})
	}
}
