//! The versioned event every producer emits, and the sinks that receive it.
//!
//! Producers hold an [`Emitter`] for one operation. It numbers events,
//! timestamps them against the operation start, redacts secrets, and hands
//! them to a [`Sink`]. Renderers, log writers and tests are sinks; none of
//! them infers state from human-readable text.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;

/// The version of the event schema. Consumers reject a major version they
/// do not know.
pub const SCHEMA_VERSION: u32 = 1;

/// One activity inside an operation: a build, a substitution, a fetch, or
/// the activation program. Output streams are keyed by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ActivityId(pub u64);

/// How much attention an event deserves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
	Debug,
	Info,
	Notice,
	Warning,
	Error,
}

/// What an activity does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityKind {
	Build,
	Substitute,
	Fetch,
	Copy,
	Activation,
	Other,
}

/// How an activity ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivityOutcome {
	Succeeded,
	/// The result was already present, or came from a cache, without work.
	Cached,
	Failed,
	Cancelled,
}

/// The unit a progress count is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unit {
	Bytes,
	Tasks,
}

/// The payload of an event.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Kind {
	/// An operation phase (resolve, evaluate, build, ...) began.
	PhaseStarted { phase: String, text: String },
	/// An operation phase completed; `text` says what it produced.
	PhaseFinished { phase: String, text: String },
	ActivityStarted {
		activity: ActivityId,
		parent: Option<ActivityId>,
		activity_kind: ActivityKind,
		/// A short name, such as `openssl-3.4.1`.
		name: String,
		/// The full artifact identity, such as a derivation path.
		artifact: Option<String>,
		/// The builder or cache host doing the work.
		host: Option<String>,
	},
	/// The step an activity is in, such as a stdenv phase.
	ActivityStep { activity: ActivityId, step: String },
	Progress {
		activity: ActivityId,
		done: u64,
		expected: Option<u64>,
		unit: Unit,
	},
	/// One complete line of an activity's own output.
	Output { activity: ActivityId, line: String },
	ActivityFinished {
		activity: ActivityId,
		outcome: ActivityOutcome,
		/// Where the activity's full output is kept.
		log: Option<String>,
	},
	/// A free-standing message.
	Message { text: String },
}

/// One event of one operation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Event {
	pub schema_version: u32,
	pub operation_id: String,
	/// Monotonic within the operation, starting at 0.
	pub seq: u64,
	/// Milliseconds since the operation started.
	pub elapsed_ms: u64,
	pub severity: Severity,
	/// A stable diagnostic code from [`crate::codes`], when one applies.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub code: Option<&'static str>,
	#[serde(flatten)]
	pub kind: Kind,
}

impl Event {
	/// The activity the event belongs to, if any.
	pub fn activity(&self) -> Option<ActivityId> {
		match &self.kind {
			Kind::ActivityStarted { activity, .. }
			| Kind::ActivityStep { activity, .. }
			| Kind::Progress { activity, .. }
			| Kind::Output { activity, .. }
			| Kind::ActivityFinished { activity, .. } => Some(*activity),
			Kind::PhaseStarted { .. } | Kind::PhaseFinished { .. } | Kind::Message { .. } => None,
		}
	}

	fn redact(&mut self, redactor: &Redactor) {
		match &mut self.kind {
			Kind::PhaseStarted { text, .. }
			| Kind::PhaseFinished { text, .. }
			| Kind::Message { text } => redactor.apply(text),
			Kind::Output { line, .. } => redactor.apply(line),
			Kind::ActivityStep { step, .. } => redactor.apply(step),
			Kind::ActivityStarted { name, artifact, .. } => {
				redactor.apply(name);
				if let Some(artifact) = artifact {
					redactor.apply(artifact);
				}
			}
			Kind::Progress { .. } | Kind::ActivityFinished { .. } => {}
		}
	}
}

/// Receives the events of an operation. Builds run in parallel, so a sink
/// is shared between threads.
pub trait Sink: Send + Sync {
	fn emit(&self, event: &Event);
}

/// Drops every event.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullSink;

impl Sink for NullSink {
	fn emit(&self, _: &Event) {}
}

/// Keeps every event, for tests.
#[derive(Debug, Default)]
pub struct RecordingSink {
	events: Mutex<Vec<Event>>,
}

impl RecordingSink {
	pub fn new() -> Self {
		Self::default()
	}

	/// The events received so far, in order.
	pub fn events(&self) -> Vec<Event> {
		self.events
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner())
			.clone()
	}
}

impl Sink for RecordingSink {
	fn emit(&self, event: &Event) {
		self.events
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner())
			.push(event.clone());
	}
}

/// Writes each event as one JSON object per line.
pub struct JsonSink {
	out: Mutex<Box<dyn Write + Send>>,
}

impl JsonSink {
	pub fn new(out: Box<dyn Write + Send>) -> Self {
		JsonSink {
			out: Mutex::new(out),
		}
	}
}

impl Sink for JsonSink {
	fn emit(&self, event: &Event) {
		let mut out = self
			.out
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner());
		if let Ok(mut line) = serde_json::to_vec(event) {
			line.push(b'\n');
			let _ = out.write_all(&line);
			let _ = out.flush();
		}
	}
}

/// Delivers each event to several sinks in order.
#[derive(Default)]
pub struct Fanout {
	sinks: Vec<Arc<dyn Sink>>,
}

impl Fanout {
	pub fn new(sinks: Vec<Arc<dyn Sink>>) -> Self {
		Fanout { sinks }
	}
}

impl Sink for Fanout {
	fn emit(&self, event: &Event) {
		for sink in &self.sinks {
			sink.emit(event);
		}
	}
}

/// Replaces secret values before any sink sees them.
#[derive(Clone, Debug, Default)]
pub struct Redactor {
	secrets: Vec<String>,
}

/// What a redacted value is replaced with.
pub const REDACTED: &str = "[redacted]";

impl Redactor {
	/// Redact `secret` wherever it appears. Empty values are ignored, since
	/// they would match everywhere.
	pub fn add(&mut self, secret: impl Into<String>) {
		let secret = secret.into();
		if !secret.is_empty() && !self.secrets.contains(&secret) {
			self.secrets.push(secret);
			// Longest first, so a secret containing another is replaced whole.
			self.secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
		}
	}

	pub fn apply(&self, text: &mut String) {
		for secret in &self.secrets {
			if text.contains(secret.as_str()) {
				*text = text.replace(secret.as_str(), REDACTED);
			}
		}
	}
}

/// Emits the events of one operation.
pub struct Emitter {
	operation_id: String,
	start: Instant,
	seq: AtomicU64,
	next_activity: AtomicU64,
	redactor: Redactor,
	sink: Arc<dyn Sink>,
}

impl Emitter {
	pub fn new(operation_id: impl Into<String>, sink: Arc<dyn Sink>) -> Self {
		Emitter {
			operation_id: operation_id.into(),
			start: Instant::now(),
			seq: AtomicU64::new(0),
			next_activity: AtomicU64::new(1),
			redactor: Redactor::default(),
			sink,
		}
	}

	/// An emitter whose events go nowhere.
	pub fn null() -> Self {
		Emitter::new("", Arc::new(NullSink))
	}

	pub fn with_redactor(mut self, redactor: Redactor) -> Self {
		self.redactor = redactor;
		self
	}

	pub fn operation_id(&self) -> &str {
		&self.operation_id
	}

	/// Time since the operation started.
	pub fn elapsed(&self) -> std::time::Duration {
		self.start.elapsed()
	}

	/// A fresh activity id, unique within the operation.
	pub fn activity(&self) -> ActivityId {
		ActivityId(self.next_activity.fetch_add(1, Ordering::Relaxed))
	}

	pub fn emit(&self, severity: Severity, code: Option<&'static str>, kind: Kind) {
		let mut event = Event {
			schema_version: SCHEMA_VERSION,
			operation_id: self.operation_id.clone(),
			seq: self.seq.fetch_add(1, Ordering::Relaxed),
			elapsed_ms: u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX),
			severity,
			code,
			kind,
		};
		event.redact(&self.redactor);
		self.sink.emit(&event);
	}

	pub fn info(&self, kind: Kind) {
		self.emit(Severity::Info, None, kind);
	}
}

/// A short, unique-enough operation id: the start time in milliseconds and
/// the process id, in hex.
pub fn new_operation_id() -> String {
	let millis = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_or(0, |d| d.as_millis());
	format!("{millis:x}-{:x}", std::process::id())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn emitter_numbers_redacts_and_serializes() {
		let sink = Arc::new(RecordingSink::new());
		let mut redactor = Redactor::default();
		redactor.add("hunter2");
		let emitter = Emitter::new("op", sink.clone()).with_redactor(redactor);
		let build = emitter.activity();
		emitter.info(Kind::ActivityStarted {
			activity: build,
			parent: None,
			activity_kind: ActivityKind::Build,
			name: "hello-2.12".into(),
			artifact: Some("/nix/store/aaaa-hello-2.12.drv".into()),
			host: None,
		});
		emitter.info(Kind::Output {
			activity: build,
			line: "token=hunter2".into(),
		});
		let events = sink.events();
		assert_eq!(events.len(), 2);
		assert_eq!((events[0].seq, events[1].seq), (0, 1));
		assert_eq!(events[1].activity(), Some(build));
		assert_eq!(
			events[1].kind,
			Kind::Output {
				activity: build,
				line: "token=[redacted]".into()
			}
		);
		let json: serde_json::Value = serde_json::to_value(&events[1]).unwrap();
		assert_eq!(json["schema_version"], 1);
		assert_eq!(json["kind"], "output");
		assert_eq!(json["activity"], build.0);
		assert!(json.get("code").is_none());
	}
}
