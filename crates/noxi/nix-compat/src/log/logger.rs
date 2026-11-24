//! CppNix's process logger: the simple logger of `logging.cc` (`raw` and
//! `raw-with-logs`), its `JSONLogger` (`internal-json`) and the
//! `ProgressBar` of `progress-bar.cc` (`bar` and `bar-with-logs`).
//!
//! Producers report activities, results and messages here; the selected
//! format renders them. This is the compatibility contract the reference
//! commands print, and is separate from the versioned `noxi-events` schema
//! of the native commands.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{ActivityType, ResultType, VerbosityLevel};

pub const ANSI_NORMAL: &str = "\x1b[0m";
pub const ANSI_BOLD: &str = "\x1b[1m";
pub const ANSI_FAINT: &str = "\x1b[2m";
pub const ANSI_RED: &str = "\x1b[31;1m";
pub const ANSI_GREEN: &str = "\x1b[32;1m";
pub const ANSI_WARNING: &str = "\x1b[35;1m";
pub const ANSI_BLUE: &str = "\x1b[34;1m";
pub const ANSI_MAGENTA: &str = "\x1b[35;1m";

/// The values of `--log-format`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum LogFormat {
	Raw = 0,
	RawWithLogs = 1,
	InternalJson = 2,
	Bar = 3,
	BarWithLogs = 4,
}

impl LogFormat {
	/// `parseLogFormat`. CppNix 2.35.2 has no `multiline` formats.
	pub fn parse(format: &str) -> Result<Self, String> {
		Ok(match format {
			"raw" => LogFormat::Raw,
			"raw-with-logs" => LogFormat::RawWithLogs,
			"internal-json" => LogFormat::InternalJson,
			"bar" => LogFormat::Bar,
			"bar-with-logs" => LogFormat::BarWithLogs,
			_ => {
				return Err(format!(
					"option 'log-format' has an invalid value '{format}'"
				));
			}
		})
	}

	fn from_u8(value: u8) -> Self {
		match value {
			1 => LogFormat::RawWithLogs,
			2 => LogFormat::InternalJson,
			3 => LogFormat::Bar,
			4 => LogFormat::BarWithLogs,
			_ => LogFormat::Raw,
		}
	}
}

/// A field of an activity or result: CppNix's `Logger::Field`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LogField {
	Int(u64),
	Str(String),
}

impl From<u64> for LogField {
	fn from(value: u64) -> Self {
		LogField::Int(value)
	}
}

impl From<&str> for LogField {
	fn from(value: &str) -> Self {
		LogField::Str(value.to_owned())
	}
}

impl From<String> for LogField {
	fn from(value: String) -> Self {
		LogField::Str(value)
	}
}

/// The legacy commands start with the simple logger.
static FORMAT: AtomicU8 = AtomicU8::new(LogFormat::Raw as u8);
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// A logger that replaces the selected format: everything the producers
/// report goes to it unrendered. A daemon worker installs CppNix's
/// `TunnelLogger` this way, so the activities, results and messages of
/// the builds and substitutions it runs reach its client.
pub trait LogSink: Send + Sync {
	/// `Logger::log`, after `printMsg`'s verbosity check.
	fn log(&self, level: VerbosityLevel, message: &str);
	/// `Logger::startActivity`.
	fn start_activity(
		&self,
		id: u64,
		level: VerbosityLevel,
		kind: ActivityType,
		text: &str,
		fields: &[LogField],
		parent: u64,
	);
	/// `Logger::stopActivity`.
	fn stop_activity(&self, id: u64);
	/// `Logger::result`.
	fn result(&self, id: u64, kind: ResultType, fields: &[LogField]);
}

static SINK: std::sync::RwLock<Option<std::sync::Arc<dyn LogSink>>> = std::sync::RwLock::new(None);

/// Send everything this process logs to `sink`, or back to the selected
/// format with `None`.
pub fn set_sink(sink: Option<std::sync::Arc<dyn LogSink>>) {
	*SINK.write().unwrap_or_else(|e| e.into_inner()) = sink;
}

fn sink() -> Option<std::sync::Arc<dyn LogSink>> {
	SINK.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The selected logger.
pub fn log_format() -> LogFormat {
	LogFormat::from_u8(FORMAT.load(Ordering::Relaxed))
}

/// Whether `internal-json` is selected.
pub fn is_json() -> bool {
	log_format() == LogFormat::InternalJson
}

/// `setLogFormat`: replace the logger. The new logger prints build logs
/// exactly when its format says so, as CppNix's new logger does: a
/// `--print-build-logs` given before `--log-format` is lost. The JSON
/// logger is always verbose.
pub fn set_log_format(format: LogFormat) {
	stop();
	FORMAT.store(format as u8, Ordering::Relaxed);
	super::set_print_build_logs(matches!(
		format,
		LogFormat::RawWithLogs | LogFormat::InternalJson | LogFormat::BarWithLogs
	));
	if matches!(format, LogFormat::Bar | LogFormat::BarWithLogs) {
		bar_start();
	}
}

/// CppNix's `isTTY()`: standard error is a terminal, `TERM` is set and not
/// `dumb`, and neither `NO_COLOR` nor `NOCOLOR` is set.
pub fn is_tty() -> bool {
	static TTY: OnceLock<bool> = OnceLock::new();
	*TTY.get_or_init(|| {
		std::io::IsTerminal::is_terminal(&std::io::stderr())
			&& std::env::var_os("TERM").is_some_and(|term| term != "dumb")
			&& std::env::var_os("NO_COLOR").is_none()
			&& std::env::var_os("NOCOLOR").is_none()
	})
}

fn write_stderr(bytes: &[u8]) {
	let _ = std::io::stderr().lock().write_all(bytes);
}

/// `printMsg`: print `message` at `level` if the verbosity admits it.
pub fn log(level: VerbosityLevel, message: &str) {
	if !super::log_enabled(level.clone()) {
		return;
	}
	log_unfiltered(level, message);
}

/// `Logger::log`, which the callers of `printMsg` reach after filtering.
fn log_unfiltered(level: VerbosityLevel, message: &str) {
	if let Some(sink) = sink() {
		return sink.log(level, message);
	}
	match log_format() {
		LogFormat::InternalJson => {
			let mut json = JsonObject::new("msg");
			json.int("level", level as u64);
			json.string("msg", message.as_bytes());
			json.write();
		}
		LogFormat::Bar | LogFormat::BarWithLogs => bar_log(message),
		LogFormat::Raw | LogFormat::RawWithLogs => simple_log(level, message),
	}
}

/// `SimpleLogger::log`, without the verbosity check.
fn simple_log(level: VerbosityLevel, message: &str) {
	let mut line = Vec::new();
	if std::env::var_os("IN_SYSTEMD").is_some_and(|v| v == "1") {
		let c = match level {
			VerbosityLevel::Error => '3',
			VerbosityLevel::Warn => '4',
			VerbosityLevel::Notice | VerbosityLevel::Info => '5',
			VerbosityLevel::Talkative | VerbosityLevel::Chatty => '6',
			VerbosityLevel::Debug | VerbosityLevel::Vomit => '7',
		};
		line.extend_from_slice(format!("<{c}>").as_bytes());
	}
	line.extend(super::filter_ansi_escapes(message.as_bytes(), !is_tty()));
	line.push(b'\n');
	write_stderr(&line);
}

/// `Logger::warn`: `warning:` and the message at `lvlWarn`.
pub fn warn(message: &str) {
	log(
		VerbosityLevel::Warn,
		&format!("{ANSI_WARNING}warning:{ANSI_NORMAL} {message}"),
	);
}

/// `logError`: `logEI` of an error without a position. `rendered` is the
/// whole message as `showErrorInfo` prints it, `raw` the bare message.
pub fn log_error(rendered: &str, raw: &str) {
	log_error_info(VerbosityLevel::Error, rendered, raw);
}

/// `logEI` at `level`.
pub fn log_error_info(level: VerbosityLevel, rendered: &str, raw: &str) {
	match log_format() {
		LogFormat::InternalJson if sink().is_none() => {
			if !super::log_enabled(level.clone()) {
				return;
			}
			let mut json = JsonObject::new("msg");
			json.null("column");
			json.null("file");
			json.int("level", level as u64);
			json.null("line");
			json.string("msg", rendered.as_bytes());
			json.string("raw_msg", raw.as_bytes());
			json.write();
		}
		_ => log(level, rendered),
	}
}

/// An activity: started when created and stopped when dropped, with the
/// id `nextId++ + (pid << 32)` CppNix gives it.
#[derive(Debug)]
pub struct Activity {
	id: u64,
}

impl Activity {
	pub fn new(
		level: VerbosityLevel,
		kind: ActivityType,
		text: impl Into<String>,
		fields: Vec<LogField>,
		parent: u64,
	) -> Self {
		let id = NEXT_ID.fetch_add(1, Ordering::Relaxed) + ((std::process::id() as u64) << 32);
		start_activity(id, level, kind, text.into(), fields, parent);
		Activity { id }
	}

	pub fn id(&self) -> u64 {
		self.id
	}

	pub fn result(&self, kind: ResultType, fields: &[LogField]) {
		result(self.id, kind, fields);
	}

	pub fn progress(&self, done: u64, expected: u64, running: u64, failed: u64) {
		self.result(
			ResultType::Progress,
			&[done.into(), expected.into(), running.into(), failed.into()],
		);
	}

	pub fn set_expected(&self, kind: ActivityType, expected: u64) {
		let kind: u8 = kind.into();
		self.result(
			ResultType::SetExpected,
			&[(kind as u64).into(), expected.into()],
		);
	}
}

impl Drop for Activity {
	fn drop(&mut self) {
		stop_activity(self.id);
	}
}

/// `Logger::stopActivity`: stop activity `id`, which [`Activity`] does when
/// dropped. A daemon client calls it for the daemon's activities.
pub fn stop_activity(id: u64) {
	if let Some(sink) = sink() {
		return sink.stop_activity(id);
	}
	match log_format() {
		LogFormat::InternalJson => {
			let mut json = JsonObject::new("stop");
			json.int("id", id);
			json.write();
		}
		LogFormat::Bar | LogFormat::BarWithLogs => bar_stop_activity(id),
		LogFormat::Raw | LogFormat::RawWithLogs => {}
	}
}

/// `Logger::startActivity` with a given id, which [`Activity::new`] calls
/// with a fresh one. A daemon client calls it for the activities the
/// daemon starts, keeping the daemon's ids and parents as CppNix's
/// `processStderr` does.
pub fn start_activity(
	id: u64,
	level: VerbosityLevel,
	kind: ActivityType,
	text: String,
	fields: Vec<LogField>,
	parent: u64,
) {
	if let Some(sink) = sink() {
		return sink.start_activity(id, level, kind, &text, &fields, parent);
	}
	match log_format() {
		LogFormat::InternalJson => {
			let mut json = JsonObject::new("start");
			json.fields(&fields);
			json.int("id", id);
			json.int("level", level as u64);
			json.int("parent", parent);
			json.string("text", text.as_bytes());
			let kind: u8 = kind.into();
			json.int("type", kind as u64);
			json.write();
		}
		LogFormat::Bar | LogFormat::BarWithLogs => {
			bar_start_activity(id, level, kind, text, &fields, parent)
		}
		LogFormat::Raw | LogFormat::RawWithLogs => {
			if super::log_enabled(level.clone()) && !text.is_empty() {
				simple_log(level, &format!("{text}..."));
			}
		}
	}
}

/// `Logger::result` for activity `id`, which [`Activity::result`] calls.
/// A daemon client calls it for the results the daemon reports.
pub fn result(id: u64, kind: ResultType, fields: &[LogField]) {
	if let Some(sink) = sink() {
		return sink.result(id, kind, fields);
	}
	match log_format() {
		LogFormat::InternalJson => {
			let mut json = JsonObject::new("result");
			json.fields(fields);
			json.int("id", id);
			let kind: u8 = kind.into();
			json.int("type", kind as u64);
			json.write();
		}
		LogFormat::Bar | LogFormat::BarWithLogs => bar_result(id, kind, fields),
		LogFormat::Raw | LogFormat::RawWithLogs => {
			let line = match fields.first() {
				Some(LogField::Str(line)) => line.as_str(),
				_ => return,
			};
			if !super::print_build_logs() {
				return;
			}
			// `printError`.
			match kind {
				ResultType::BuildLogLine => log(VerbosityLevel::Error, line),
				ResultType::PostBuildLogLine => {
					log(VerbosityLevel::Error, &format!("post-build-hook: {line}"))
				}
				_ => {}
			}
		}
	}
}

/// The counters of CppNix's `Worker` that `updateProgress` reports.
#[derive(Clone, Copy, Debug, Default)]
pub struct WorkerCounters {
	pub done_builds: u64,
	pub expected_builds: u64,
	pub running_builds: u64,
	pub failed_builds: u64,
	pub done_substitutions: u64,
	pub expected_substitutions: u64,
	pub running_substitutions: u64,
	pub failed_substitutions: u64,
	pub expected_download_size: u64,
	pub done_download_size: u64,
	pub expected_nar_size: u64,
	pub done_nar_size: u64,
}

/// A running `Worker`'s activities. Fields drop in order, so the copy and
/// build activities stop before `realise`, as the C++ members do.
struct WorkerActivities {
	copy_paths: Activity,
	builds: Activity,
	realise: Activity,
	counters: WorkerCounters,
}

static WORKER: Mutex<Option<WorkerActivities>> = Mutex::new(None);

/// The `Worker` of one `buildPaths` call: its `realise`, `builds` and
/// `copy-paths` activities run until the guard is dropped. A guard made
/// while another is alive does nothing, so nested store calls share the
/// outer worker.
#[must_use]
pub struct WorkerGuard {
	owner: bool,
}

impl WorkerGuard {
	pub fn new() -> Self {
		let mut worker = WORKER.lock().unwrap();
		if worker.is_some() {
			return WorkerGuard { owner: false };
		}
		let realise = Activity::new(VerbosityLevel::Error, ActivityType::Realise, "", vec![], 0);
		let builds = Activity::new(VerbosityLevel::Error, ActivityType::Builds, "", vec![], 0);
		let copy_paths = Activity::new(
			VerbosityLevel::Error,
			ActivityType::CopyPaths,
			"",
			vec![],
			0,
		);
		*worker = Some(WorkerActivities {
			copy_paths,
			builds,
			realise,
			counters: WorkerCounters::default(),
		});
		WorkerGuard { owner: true }
	}
}

impl Default for WorkerGuard {
	fn default() -> Self {
		Self::new()
	}
}

impl Drop for WorkerGuard {
	fn drop(&mut self) {
		if self.owner {
			let worker = WORKER.lock().unwrap().take();
			drop(worker);
		}
	}
}

/// Change the running worker's counters and report them, as the goals do
/// before calling `updateProgress`. Without a worker this does nothing.
pub fn worker_update(change: impl FnOnce(&mut WorkerCounters)) {
	let mut worker = WORKER.lock().unwrap();
	let Some(worker) = worker.as_mut() else {
		return;
	};
	change(&mut worker.counters);
	let c = worker.counters;
	worker.builds.progress(
		c.done_builds,
		c.expected_builds + c.done_builds,
		c.running_builds,
		c.failed_builds,
	);
	worker.copy_paths.progress(
		c.done_substitutions,
		c.expected_substitutions + c.done_substitutions,
		c.running_substitutions,
		c.failed_substitutions,
	);
	worker.realise.set_expected(
		ActivityType::FileTransfer,
		c.expected_download_size + c.done_download_size,
	);
	worker.realise.set_expected(
		ActivityType::CopyPath,
		c.expected_nar_size + c.done_nar_size,
	);
}

/// Whether a worker is running, so a store operation is one of its goals.
pub fn worker_active() -> bool {
	WORKER.lock().unwrap().is_some()
}

/// Change the running worker's counters without reporting them, as a goal
/// releasing its `MaintainCount` does.
pub fn worker_change(change: impl FnOnce(&mut WorkerCounters)) {
	if let Some(worker) = WORKER.lock().unwrap().as_mut() {
		change(&mut worker.counters);
	}
}

/// `Logger::writeToStdout`: a line of command output. The progress bar
/// clears its status line first and redraws it after.
pub fn write_stdout(text: &str) {
	let write = || {
		let mut stdout = std::io::stdout().lock();
		let _ = stdout.write_all(text.as_bytes());
		let _ = stdout.write_all(b"\n");
		let _ = stdout.flush();
	};
	let Some(bar) = BAR.get() else {
		return write();
	};
	let mut state = bar.state.lock().unwrap();
	if state.active {
		state.last_output.clear();
		write_stderr(b"\r\x1b[K");
		write();
		draw(&mut state);
	} else {
		write();
	}
}

/// Stop the logger before the process exits: the progress bar clears its
/// line and shows the cursor again.
pub fn stop() {
	let Some(bar) = BAR.get() else {
		return;
	};
	let thread = {
		let mut state = bar.state.lock().unwrap();
		if state.active {
			state.active = false;
			state.last_output.clear();
			write_stderr(b"\r\x1b[K");
			if bar.tty {
				write_stderr(b"\x1b[?25h");
			}
			bar.update.notify_one();
			bar.quit.notify_one();
		}
		state.thread.take()
	};
	if let Some(thread) = thread {
		let _ = thread.join();
	}
}

/// A builder's `@nix` line, as `handleJSONLogMessage` handles it for the
/// build activity `act`: `setPhase` becomes a phase result and `msg` a
/// message. Returns whether the line was a JSON message.
pub fn handle_json_log_message(line: &[u8], act: &Activity) -> bool {
	let Some(json) = line.strip_prefix(b"@nix ") else {
		return false;
	};
	match parse_builder_message(json) {
		Some(BuilderMessage::SetPhase(phase)) => {
			act.result(ResultType::SetPhase, &[LogField::Str(phase)]);
		}
		Some(BuilderMessage::Msg(level, msg)) => {
			let level = VerbosityLevel::try_from(level).unwrap_or(VerbosityLevel::Error);
			log_unfiltered(level, &msg);
		}
		Some(BuilderMessage::Other) => {}
		None => {
			// `parseJSONMessage` reports the parse failure and the line is
			// then treated as output.
			log(
				VerbosityLevel::Error,
				"bad JSON log message from the derivation builder",
			);
			return false;
		}
	}
	true
}

/// CppNix's `BuildLog`: a build's output assembled into lines, each sent
/// as a log-line result of its activity unless it is a builder `@nix`
/// message. A carriage return restarts the line.
pub struct BuildLog {
	act: Activity,
	kind: ResultType,
	line: Vec<u8>,
	pos: usize,
}

impl BuildLog {
	pub fn new(act: Activity) -> Self {
		BuildLog {
			act,
			kind: ResultType::BuildLogLine,
			line: Vec::new(),
			pos: 0,
		}
	}

	/// The log of a `post-build-hook`.
	pub fn post_build(mut self) -> Self {
		self.kind = ResultType::PostBuildLogLine;
		self
	}

	pub fn activity(&self) -> &Activity {
		&self.act
	}

	pub fn push(&mut self, data: &[u8]) {
		for &c in data {
			match c {
				b'\r' => self.pos = 0,
				b'\n' => self.flush_line(),
				c => {
					if self.pos >= self.line.len() {
						self.line.resize(self.pos + 1, 0);
					}
					self.line[self.pos] = c;
					self.pos += 1;
				}
			}
		}
	}

	/// The unterminated last line, when the builder's output closes.
	pub fn flush(&mut self) {
		if !self.line.is_empty() {
			self.flush_line();
		}
	}

	fn flush_line(&mut self) {
		self.line.truncate(self.pos);
		if !handle_json_log_message(&self.line, &self.act) {
			let line = String::from_utf8_lossy(&self.line).into_owned();
			self.act.result(self.kind.clone(), &[LogField::Str(line)]);
		}
		self.line.clear();
		self.pos = 0;
	}
}

enum BuilderMessage {
	SetPhase(String),
	Msg(u64, String),
	Other,
}

/// A minimal JSON reader for the builder messages the log handles.
fn parse_builder_message(json: &[u8]) -> Option<BuilderMessage> {
	let text = std::str::from_utf8(json).ok()?;
	let mut parser = JsonParser {
		s: text.as_bytes(),
		i: 0,
	};
	let object = parser.object()?;
	parser.ws();
	if parser.i != parser.s.len() {
		return None;
	}
	let get = |key: &str| object.iter().find(|(k, _)| k == key).map(|(_, v)| v);
	Some(match get("action") {
		Some(JsonValue::Str(action)) if action == "setPhase" => match get("phase") {
			Some(JsonValue::Str(phase)) => BuilderMessage::SetPhase(phase.clone()),
			_ => BuilderMessage::Other,
		},
		Some(JsonValue::Str(action)) if action == "msg" => match (get("level"), get("msg")) {
			(Some(JsonValue::Num(level)), Some(JsonValue::Str(msg))) => {
				BuilderMessage::Msg(*level, msg.clone())
			}
			_ => BuilderMessage::Other,
		},
		_ => BuilderMessage::Other,
	})
}

enum JsonValue {
	Str(String),
	Num(u64),
	Other,
}

struct JsonParser<'a> {
	s: &'a [u8],
	i: usize,
}

impl JsonParser<'_> {
	fn ws(&mut self) {
		while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
			self.i += 1;
		}
	}

	fn eat(&mut self, c: u8) -> Option<()> {
		self.ws();
		(self.s.get(self.i) == Some(&c)).then(|| self.i += 1)
	}

	fn object(&mut self) -> Option<Vec<(String, JsonValue)>> {
		self.eat(b'{')?;
		let mut entries = Vec::new();
		if self.eat(b'}').is_some() {
			return Some(entries);
		}
		loop {
			self.ws();
			let key = self.string()?;
			self.eat(b':')?;
			let value = self.value()?;
			entries.push((key, value));
			if self.eat(b',').is_some() {
				continue;
			}
			self.eat(b'}')?;
			return Some(entries);
		}
	}

	fn value(&mut self) -> Option<JsonValue> {
		self.ws();
		match *self.s.get(self.i)? {
			b'"' => self.string().map(JsonValue::Str),
			b'{' => self.object().map(|_| JsonValue::Other),
			b'[' => {
				self.i += 1;
				if self.eat(b']').is_some() {
					return Some(JsonValue::Other);
				}
				loop {
					self.value()?;
					if self.eat(b',').is_some() {
						continue;
					}
					self.eat(b']')?;
					return Some(JsonValue::Other);
				}
			}
			b'0'..=b'9' => {
				let start = self.i;
				while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
					self.i += 1;
				}
				let digits = std::str::from_utf8(&self.s[start..self.i]).ok()?;
				if matches!(self.s.get(self.i), Some(b'.' | b'e' | b'E')) {
					self.skip_number();
					return Some(JsonValue::Other);
				}
				Some(digits.parse().map_or(JsonValue::Other, JsonValue::Num))
			}
			b'-' => {
				self.i += 1;
				self.skip_number();
				Some(JsonValue::Other)
			}
			_ => {
				for word in [&b"true"[..], b"false", b"null"] {
					if self.s[self.i..].starts_with(word) {
						self.i += word.len();
						return Some(JsonValue::Other);
					}
				}
				None
			}
		}
	}

	fn skip_number(&mut self) {
		while self.i < self.s.len()
			&& matches!(
				self.s[self.i],
				b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-'
			) {
			self.i += 1;
		}
	}

	fn string(&mut self) -> Option<String> {
		if self.s.get(self.i) != Some(&b'"') {
			return None;
		}
		self.i += 1;
		let mut out = String::new();
		loop {
			let c = *self.s.get(self.i)?;
			self.i += 1;
			match c {
				b'"' => return Some(out),
				b'\\' => {
					let e = *self.s.get(self.i)?;
					self.i += 1;
					match e {
						b'"' => out.push('"'),
						b'\\' => out.push('\\'),
						b'/' => out.push('/'),
						b'b' => out.push('\u{8}'),
						b'f' => out.push('\u{c}'),
						b'n' => out.push('\n'),
						b'r' => out.push('\r'),
						b't' => out.push('\t'),
						b'u' => {
							let mut code = self.hex4()?;
							if (0xd800..0xdc00).contains(&code)
								&& self.s.get(self.i..self.i + 2) == Some(b"\\u")
							{
								self.i += 2;
								let low = self.hex4()?;
								code = 0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00);
							}
							out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
						}
						_ => return None,
					}
				}
				_ => {
					// Copy one UTF-8 sequence; the input was checked.
					let start = self.i - 1;
					let len = match c {
						0xc0..=0xdf => 2,
						0xe0..=0xef => 3,
						0xf0..=0xf7 => 4,
						_ => 1,
					};
					self.i = start + len;
					out.push_str(std::str::from_utf8(self.s.get(start..self.i)?).ok()?);
				}
			}
		}
	}

	fn hex4(&mut self) -> Option<u32> {
		let digits = std::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
		self.i += 4;
		u32::from_str_radix(digits, 16).ok()
	}
}

/// A JSON object written with nlohmann's `dump()`: keys in the order
/// given (callers pass them sorted, as `nlohmann::json` stores them),
/// non-ASCII text unescaped and invalid UTF-8 replaced.
struct JsonObject {
	out: String,
}

impl JsonObject {
	fn new(action: &str) -> Self {
		let mut object = JsonObject {
			out: String::from("@nix {"),
		};
		object.string("action", action.as_bytes());
		object
	}

	fn key(&mut self, key: &str) {
		if !self.out.ends_with('{') {
			self.out.push(',');
		}
		self.out.push('"');
		self.out.push_str(key);
		self.out.push_str("\":");
	}

	fn int(&mut self, key: &str, value: u64) {
		self.key(key);
		self.out.push_str(&value.to_string());
	}

	fn null(&mut self, key: &str) {
		self.key(key);
		self.out.push_str("null");
	}

	fn string(&mut self, key: &str, value: &[u8]) {
		self.key(key);
		push_json_string(&mut self.out, value);
	}

	/// `addFields`: omitted when empty.
	fn fields(&mut self, fields: &[LogField]) {
		if fields.is_empty() {
			return;
		}
		self.key("fields");
		self.out.push('[');
		for (i, field) in fields.iter().enumerate() {
			if i > 0 {
				self.out.push(',');
			}
			match field {
				LogField::Int(n) => self.out.push_str(&n.to_string()),
				LogField::Str(s) => push_json_string(&mut self.out, s.as_bytes()),
			}
		}
		self.out.push(']');
	}

	fn write(mut self) {
		self.out.push_str("}\n");
		write_stderr(self.out.as_bytes());
	}
}

/// A JSON string as nlohmann dumps it with `error_handler_t::replace`.
pub fn push_json_string(out: &mut String, value: &[u8]) {
	out.push('"');
	for c in String::from_utf8_lossy(value).chars() {
		match c {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			'\u{8}' => out.push_str("\\b"),
			'\u{c}' => out.push_str("\\f"),
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
			c => out.push(c),
		}
	}
	out.push('"');
}

// The progress bar.

struct ActInfo {
	id: u64,
	s: String,
	last_line: String,
	phase: String,
	kind: ActivityType,
	done: u64,
	expected: u64,
	running: u64,
	failed: u64,
	expected_by_type: BTreeMap<u8, u64>,
	visible: bool,
	parent: u64,
	name: Option<String>,
	start_time: Instant,
}

#[derive(Default)]
struct ActivitiesByType {
	ids: Vec<u64>,
	done: u64,
	expected: u64,
	failed: u64,
}

struct BarState {
	/// In display order: the most recently started or updated last.
	activities: Vec<ActInfo>,
	by_type: BTreeMap<u8, ActivitiesByType>,
	files_linked: u64,
	bytes_linked: u64,
	corrupted_paths: u64,
	untrusted_paths: u64,
	active: bool,
	have_update: bool,
	last_output: String,
	thread: Option<std::thread::JoinHandle<()>>,
}

struct Bar {
	state: Mutex<BarState>,
	update: Condvar,
	quit: Condvar,
	tty: bool,
}

static BAR: OnceLock<Bar> = OnceLock::new();

fn bar() -> &'static Bar {
	BAR.get_or_init(|| Bar {
		state: Mutex::new(BarState {
			activities: Vec::new(),
			by_type: BTreeMap::new(),
			files_linked: 0,
			bytes_linked: 0,
			corrupted_paths: 0,
			untrusted_paths: 0,
			active: false,
			have_update: true,
			last_output: String::new(),
			thread: None,
		}),
		update: Condvar::new(),
		quit: Condvar::new(),
		tty: is_tty(),
	})
}

/// `ProgressBar::ProgressBar`: hide the cursor and start the update thread
/// on a terminal.
fn bar_start() {
	let bar = bar();
	let mut state = bar.state.lock().unwrap();
	state.activities.clear();
	state.by_type.clear();
	state.have_update = true;
	state.last_output.clear();
	if !bar.tty {
		state.active = false;
		return;
	}
	write_stderr(b"\x1b[?25l");
	state.active = true;
	state.thread = Some(std::thread::spawn(|| {
		let bar = self::bar();
		let mut state = bar.state.lock().unwrap();
		let mut next_wakeup: Option<Duration> = None;
		while state.active {
			if !state.have_update {
				state = match next_wakeup {
					Some(timeout) => bar.update.wait_timeout(state, timeout).unwrap().0,
					None => bar.update.wait(state).unwrap(),
				};
			}
			next_wakeup = draw(&mut state);
			state = bar
				.quit
				.wait_timeout(state, Duration::from_millis(50))
				.unwrap()
				.0;
		}
	}));
}

fn bar_log(message: &str) {
	let bar = bar();
	let mut state = bar.state.lock().unwrap();
	bar_log_locked(bar, &mut state, message);
}

fn bar_log_locked(bar: &Bar, state: &mut BarState, message: &str) {
	if state.active {
		state.last_output.clear();
		let mut line = b"\r\x1b[K".to_vec();
		line.extend(super::filter_ansi_escapes(message.as_bytes(), !bar.tty));
		line.extend_from_slice(ANSI_NORMAL.as_bytes());
		line.push(b'\n');
		write_stderr(&line);
		draw(state);
	} else {
		let mut line = super::filter_ansi_escapes(message.as_bytes(), !bar.tty);
		line.push(b'\n');
		write_stderr(&line);
	}
}

fn update(state: &mut BarState) {
	state.have_update = true;
	bar().update.notify_one();
}

fn store_path_to_name(path: &str) -> &str {
	let base = path.rsplit('/').next().unwrap_or(path);
	match base.find('-') {
		Some(i) => &base[i + 1..],
		None => "",
	}
}

fn store_path_to_name_without_drv_suffix(path: &str) -> &str {
	let name = store_path_to_name(path);
	name.strip_suffix(".drv").unwrap_or(name)
}

/// `DrvName(name).name`: the name up to the first dash followed by a
/// character that is not a letter.
fn drv_name(full: &str) -> String {
	let bytes = full.as_bytes();
	let end = (0..bytes.len())
		.find(|&i| bytes[i] == b'-' && i + 1 < bytes.len() && !bytes[i + 1].is_ascii_alphabetic())
		.unwrap_or(bytes.len());
	full[..end].to_owned()
}

fn get_s(fields: &[LogField], n: usize) -> &str {
	match fields.get(n) {
		Some(LogField::Str(s)) => s,
		_ => "",
	}
}

fn bar_start_activity(
	id: u64,
	level: VerbosityLevel,
	kind: ActivityType,
	text: String,
	fields: &[LogField],
	parent: u64,
) {
	let bar = bar();
	let mut state = bar.state.lock().unwrap();
	if super::log_enabled(level) && !text.is_empty() && kind != ActivityType::BuildWaiting {
		bar_log_locked(bar, &mut state, &format!("{text}..."));
	}
	let mut info = ActInfo {
		id,
		s: text,
		last_line: String::new(),
		phase: String::new(),
		kind: kind.clone(),
		done: 0,
		expected: 0,
		running: 0,
		failed: 0,
		expected_by_type: BTreeMap::new(),
		visible: true,
		parent,
		name: None,
		start_time: Instant::now(),
	};
	match kind {
		ActivityType::Build => {
			let name = store_path_to_name_without_drv_suffix(get_s(fields, 0));
			info.s = format!("building {ANSI_BOLD}{name}{ANSI_NORMAL}");
			let machine = get_s(fields, 1);
			if !machine.is_empty() {
				info.s += &format!(" on {ANSI_BOLD}{machine}{ANSI_NORMAL}");
			}
			info.name = Some(drv_name(name));
		}
		ActivityType::Substitute => {
			let name = store_path_to_name(get_s(fields, 0));
			let sub = get_s(fields, 1);
			info.s = if sub.starts_with("local") {
				format!("copying {ANSI_BOLD}{name}{ANSI_NORMAL} from {sub}")
			} else {
				format!("fetching {ANSI_BOLD}{name}{ANSI_NORMAL} from {sub}")
			};
		}
		ActivityType::PostBuildHook => {
			let name = store_path_to_name_without_drv_suffix(get_s(fields, 0));
			info.s = format!("post-build {ANSI_BOLD}{name}{ANSI_NORMAL}");
			info.name = Some(drv_name(name));
		}
		ActivityType::QueryPathInfo => {
			let name = store_path_to_name(get_s(fields, 0));
			info.s = format!(
				"querying {ANSI_BOLD}{name}{ANSI_NORMAL} on {}",
				get_s(fields, 1)
			);
		}
		_ => {}
	}
	if (kind == ActivityType::FileTransfer
		&& (has_ancestor(&state, ActivityType::CopyPath, parent)
			|| has_ancestor(&state, ActivityType::QueryPathInfo, parent)))
		|| (kind == ActivityType::CopyPath
			&& has_ancestor(&state, ActivityType::Substitute, parent))
	{
		info.visible = false;
	}
	let kind: u8 = kind.into();
	state.activities.push(info);
	state.by_type.entry(kind).or_default().ids.push(id);
	update(&mut state);
}

fn has_ancestor(state: &BarState, kind: ActivityType, mut act: u64) -> bool {
	while act != 0 {
		let Some(info) = state.activities.iter().find(|a| a.id == act) else {
			break;
		};
		if info.kind == kind {
			return true;
		}
		act = info.parent;
	}
	false
}

fn bar_stop_activity(id: u64) {
	let Some(bar) = BAR.get() else {
		return;
	};
	let mut state = bar.state.lock().unwrap();
	if let Some(pos) = state.activities.iter().position(|a| a.id == id) {
		let info = state.activities.remove(pos);
		let kind: u8 = info.kind.clone().into();
		let by_type = state.by_type.entry(kind).or_default();
		by_type.done += info.done;
		by_type.failed += info.failed;
		by_type.ids.retain(|&i| i != id);
		for (kind, expected) in &info.expected_by_type {
			let by_type = state.by_type.entry(*kind).or_default();
			by_type.expected = by_type.expected.wrapping_sub(*expected);
		}
	}
	update(&mut state);
}

fn get_i(fields: &[LogField], n: usize) -> u64 {
	match fields.get(n) {
		Some(LogField::Int(i)) => *i,
		_ => 0,
	}
}

fn bar_result(id: u64, kind: ResultType, fields: &[LogField]) {
	let bar = bar();
	let mut state = bar.state.lock().unwrap();
	let Some(pos) = state.activities.iter().position(|a| a.id == id) else {
		if kind == ResultType::FileLinked {
			state.files_linked += 1;
			state.bytes_linked += get_i(fields, 0);
			update(&mut state);
		}
		return;
	};
	match kind {
		ResultType::FileLinked => {
			state.files_linked += 1;
			state.bytes_linked += get_i(fields, 0);
		}
		ResultType::BuildLogLine | ResultType::PostBuildLogLine => {
			let line = get_s(fields, 0).trim_end_matches(['\n', '\r', ' ', '\t']);
			if super::print_build_logs() {
				let suffix = if kind == ResultType::PostBuildLogLine {
					" (post)> "
				} else {
					"> "
				};
				let name = state.activities[pos]
					.name
					.clone()
					.unwrap_or_else(|| "unnamed".into());
				let message = format!("{ANSI_FAINT}{name}{suffix}{ANSI_NORMAL}{line}");
				bar_log_locked(bar, &mut state, &message);
				return;
			}
			let mut info = state.activities.remove(pos);
			info.last_line = line.to_owned();
			state.activities.push(info);
		}
		ResultType::UntrustedPath => state.untrusted_paths += 1,
		ResultType::CorruptedPath => state.corrupted_paths += 1,
		ResultType::SetPhase => state.activities[pos].phase = get_s(fields, 0).to_owned(),
		ResultType::Progress => {
			let info = &mut state.activities[pos];
			info.done = get_i(fields, 0);
			info.expected = get_i(fields, 1);
			info.running = get_i(fields, 2);
			info.failed = get_i(fields, 3);
		}
		ResultType::SetExpected => {
			let kind = get_i(fields, 0) as u8;
			let expected = get_i(fields, 1);
			let old = state.activities[pos]
				.expected_by_type
				.insert(kind, expected)
				.unwrap_or(0);
			let by_type = state.by_type.entry(kind).or_default();
			by_type.expected = by_type.expected.wrapping_sub(old).wrapping_add(expected);
		}
		ResultType::FetchStatus => state.activities[pos].last_line = get_s(fields, 0).to_owned(),
	}
	update(&mut state);
}

/// The terminal's width, as `getWindowSize` reads it from standard error.
fn window_width() -> usize {
	// SAFETY: TIOCGWINSZ writes a `winsize` into the struct passed.
	unsafe {
		let mut ws: libc::winsize = std::mem::zeroed();
		if libc::ioctl(2, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
			return ws.ws_col as usize;
		}
	}
	usize::MAX
}

/// `ProgressBar::draw`: the status line, redrawn when it changes. Returns
/// when an activity too young to show becomes old enough.
fn draw(state: &mut BarState) -> Option<Duration> {
	let mut next_wakeup: Option<Duration> = None;
	state.have_update = false;
	if !state.active {
		return next_wakeup;
	}
	let mut line = String::new();
	let status = get_status(state);
	if !status.is_empty() {
		line.push('[');
		line.push_str(&status);
		line.push(']');
	}
	let now = Instant::now();
	if !state.activities.is_empty() {
		if !status.is_empty() {
			line.push(' ');
		}
		// Activities are shown only once some time has passed, so very
		// short ones never appear.
		let delay = Duration::from_millis(10);
		let mut shown = None;
		for info in state.activities.iter().rev() {
			if info.visible && (!info.s.is_empty() || !info.last_line.is_empty()) {
				if info.start_time + delay < now {
					shown = Some(info);
					break;
				}
				let wait = delay.saturating_sub(now - info.start_time);
				next_wakeup = Some(next_wakeup.map_or(wait, |w| w.min(wait)));
			}
		}
		if let Some(info) = shown {
			line.push_str(&info.s);
			if !info.phase.is_empty() {
				line.push_str(" (");
				line.push_str(&info.phase);
				line.push(')');
			}
			if !info.last_line.is_empty() {
				if !info.s.is_empty() {
					line.push_str(": ");
				}
				line.push_str(&info.last_line);
			}
		}
	}
	let width = window_width();
	let mut output = b"\r".to_vec();
	output.extend(super::filter_ansi_escapes_width(
		line.as_bytes(),
		false,
		width,
	));
	output.extend_from_slice(ANSI_NORMAL.as_bytes());
	output.extend_from_slice(b"\x1b[K");
	let output = String::from_utf8_lossy(&output).into_owned();
	if output != state.last_output {
		write_stderr(output.as_bytes());
		state.last_output = output;
	}
	next_wakeup
}

struct Totals {
	done: u64,
	expected: u64,
	running: u64,
	failed: u64,
}

fn totals(state: &BarState, kind: ActivityType) -> Totals {
	let kind: u8 = kind.into();
	let Some(by_type) = state.by_type.get(&kind) else {
		return Totals {
			done: 0,
			expected: 0,
			running: 0,
			failed: 0,
		};
	};
	let mut t = Totals {
		done: by_type.done,
		expected: by_type.done,
		running: 0,
		failed: by_type.failed,
	};
	for id in &by_type.ids {
		if let Some(info) = state.activities.iter().find(|a| a.id == *id) {
			t.done += info.done;
			t.expected += info.expected;
			t.running += info.running;
			t.failed += info.failed;
		}
	}
	t.expected = t.expected.max(by_type.expected);
	t
}

fn render_activity(state: &BarState, kind: ActivityType, item: &str) -> String {
	let t = totals(state, kind);
	if t.running == 0 && t.done == 0 && t.expected == 0 && t.failed == 0 {
		return String::new();
	}
	let s = if t.running != 0 {
		if t.expected != 0 {
			format!(
				"{ANSI_BLUE}{}{ANSI_NORMAL}/{ANSI_GREEN}{}{ANSI_NORMAL}/{}",
				t.running, t.done, t.expected
			)
		} else {
			format!(
				"{ANSI_BLUE}{}{ANSI_NORMAL}/{ANSI_GREEN}{}{ANSI_NORMAL}",
				t.running, t.done
			)
		}
	} else if t.expected != t.done {
		if t.expected != 0 {
			format!("{ANSI_GREEN}{}{ANSI_NORMAL}/{}", t.done, t.expected)
		} else {
			format!("{ANSI_GREEN}{}{ANSI_NORMAL}", t.done)
		}
	} else if t.done != 0 {
		format!("{ANSI_GREEN}{}{ANSI_NORMAL}", t.done)
	} else {
		format!("{}", t.done)
	};
	let mut s = item.replace("%s", &s);
	if t.failed != 0 {
		s += &format!(" ({ANSI_RED}{} failed{ANSI_NORMAL})", t.failed);
	}
	s
}

const SIZE_SUFFIXES: [char; 9] = ['K', 'K', 'M', 'G', 'T', 'P', 'E', 'Z', 'Y'];

/// `getSizeUnit`: the index of the unit.
fn size_unit(value: u64) -> usize {
	let mut unit = 0;
	let mut value = value;
	while value > 1024 && unit < SIZE_SUFFIXES.len() - 1 {
		unit += 1;
		value /= 1024;
	}
	unit
}

fn common_size_unit(values: &[u64]) -> Option<usize> {
	let unit = size_unit(values[0]);
	values[1..]
		.iter()
		.all(|v| size_unit(*v) == unit)
		.then_some(unit)
}

/// `renderSizeWithoutUnit`: bytes count as KiB.
fn render_size_without_unit(value: u64, unit: usize) -> String {
	let power = unit.max(1) as i32;
	format!("{:.1}", value as f64 / 1024f64.powi(power))
}

fn render_size(value: u64) -> String {
	let unit = size_unit(value);
	format!(
		"{} {}iB",
		render_size_without_unit(value, unit),
		SIZE_SUFFIXES[unit]
	)
}

fn render_size_activity(state: &BarState, kind: ActivityType, item: &str) -> String {
	let t = totals(state, kind);
	if t.running == 0 && t.done == 0 && t.expected == 0 && t.failed == 0 {
		return String::new();
	}
	let sized = |v: u64, unit: Option<usize>| match unit {
		Some(unit) => render_size_without_unit(v, unit),
		None => render_size(v),
	};
	let (s, unit) = if t.running != 0 {
		if t.expected != 0 {
			let unit = common_size_unit(&[t.running, t.done, t.expected]);
			(
				format!(
					"{ANSI_BLUE}{}{ANSI_NORMAL}/{ANSI_GREEN}{}{ANSI_NORMAL}/{}",
					sized(t.running, unit),
					sized(t.done, unit),
					sized(t.expected, unit)
				),
				unit,
			)
		} else {
			let unit = common_size_unit(&[t.running, t.done]);
			(
				format!(
					"{ANSI_BLUE}{}{ANSI_NORMAL}/{ANSI_GREEN}{}{ANSI_NORMAL}",
					sized(t.running, unit),
					sized(t.done, unit)
				),
				unit,
			)
		}
	} else if t.expected != t.done {
		if t.expected != 0 {
			let unit = common_size_unit(&[t.done, t.expected]);
			(
				format!(
					"{ANSI_GREEN}{}{ANSI_NORMAL}/{}",
					sized(t.done, unit),
					sized(t.expected, unit)
				),
				unit,
			)
		} else {
			let unit = size_unit(t.done);
			(
				format!(
					"{ANSI_GREEN}{}{ANSI_NORMAL}",
					render_size_without_unit(t.done, unit)
				),
				Some(unit),
			)
		}
	} else {
		let unit = size_unit(t.done);
		let n = render_size_without_unit(t.done, unit);
		(
			if t.done != 0 {
				format!("{ANSI_GREEN}{n}{ANSI_NORMAL}")
			} else {
				n
			},
			Some(unit),
		)
	};
	let s = match unit {
		Some(unit) => format!("{s} {}iB", SIZE_SUFFIXES[unit]),
		None => s,
	};
	let mut s = item.replace("%s", &s);
	if t.failed != 0 {
		s += &format!(" ({ANSI_RED}{} failed{ANSI_NORMAL})", render_size(t.failed));
	}
	s
}

/// `ProgressBar::getStatus`.
fn get_status(state: &BarState) -> String {
	let mut res = String::new();
	let append = |res: &mut String, s: &str| {
		if s.is_empty() {
			return;
		}
		if !res.is_empty() {
			res.push_str(", ");
		}
		res.push_str(s);
	};
	append(
		&mut res,
		&render_activity(state, ActivityType::Builds, "%s built"),
	);
	let s1 = render_activity(state, ActivityType::CopyPaths, "%s copied");
	let s2 = render_size_activity(state, ActivityType::CopyPath, "%s");
	if !s1.is_empty() || !s2.is_empty() {
		if !res.is_empty() {
			res.push_str(", ");
		}
		if s1.is_empty() {
			res.push_str("0 copied");
		} else {
			res.push_str(&s1);
		}
		if !s2.is_empty() {
			res.push_str(" (");
			res.push_str(&s2);
			res.push(')');
		}
	}
	append(
		&mut res,
		&render_size_activity(state, ActivityType::FileTransfer, "%s DL"),
	);
	let mut optimised = render_activity(state, ActivityType::OptimiseStore, "%s paths optimised");
	if !optimised.is_empty() {
		optimised += &format!(
			", {} / {} inodes freed",
			render_size(state.bytes_linked),
			state.files_linked
		);
		append(&mut res, &optimised);
	}
	append(
		&mut res,
		&render_activity(state, ActivityType::VerifyPaths, "%s paths verified"),
	);
	if state.corrupted_paths != 0 {
		append(
			&mut res,
			&format!("{ANSI_RED}{} corrupted{ANSI_NORMAL}", state.corrupted_paths),
		);
	}
	if state.untrusted_paths != 0 {
		append(
			&mut res,
			&format!("{ANSI_RED}{} untrusted{ANSI_NORMAL}", state.untrusted_paths),
		);
	}
	res
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_the_reference_formats_only() {
		assert_eq!(LogFormat::parse("bar"), Ok(LogFormat::Bar));
		assert_eq!(
			LogFormat::parse("internal-json"),
			Ok(LogFormat::InternalJson)
		);
		assert_eq!(
			LogFormat::parse("multiline"),
			Err("option 'log-format' has an invalid value 'multiline'".into())
		);
	}

	#[test]
	fn writes_json_like_nlohmann() {
		let mut out = String::new();
		push_json_string(&mut out, b"a\"\\\x1b[0m\n\t\x01\xff\xc3\xa9");
		assert_eq!(out, "\"a\\\"\\\\\\u001b[0m\\n\\t\\u0001\u{fffd}\u{e9}\"");
		let mut json = JsonObject::new("start");
		json.fields(&["p".into(), 1u64.into()]);
		json.int("id", 7);
		assert_eq!(
			json.out,
			"@nix {\"action\":\"start\",\"fields\":[\"p\",1],\"id\":7"
		);
	}

	#[test]
	fn reads_builder_messages() {
		assert!(matches!(
			parse_builder_message(br#"{"action":"setPhase","phase":"buildPhase"}"#),
			Some(BuilderMessage::SetPhase(p)) if p == "buildPhase"
		));
		assert!(matches!(
			parse_builder_message(
				br#" {"msg":"h\u00e9 \"x\"","level":3,"action":"msg","x":[1,{"y":null}]} "#
			),
			Some(BuilderMessage::Msg(3, m)) if m == "h\u{e9} \"x\""
		));
		assert!(parse_builder_message(b"{nope").is_none());
	}

	#[test]
	fn renders_sizes_like_cppnix() {
		assert_eq!(render_size(120), "0.1 KiB");
		assert_eq!(render_size(3_000_000), "2.9 MiB");
		assert_eq!(drv_name("hello-2.12.1"), "hello");
		assert_eq!(drv_name("foo-bar"), "foo-bar");
	}
}
