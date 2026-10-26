//! Errors carrying CppNix's messages for flake references and fetchers.

use std::fmt;
use std::sync::OnceLock;

/// Whether an error is CppNix's `BadURL`, which some parsers catch to try
/// another syntax, or any other error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
	BadUrl,
	Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
	pub kind: ErrorKind,
	pub message: String,
	/// Context added while the error propagated, innermost first, as
	/// CppNix's `addTrace` records it.
	pub traces: Vec<String>,
	/// The process exit status CppNix uses for this error, if not 1.
	pub status: Option<u8>,
}

impl Error {
	pub fn new(message: impl Into<String>) -> Self {
		Self {
			kind: ErrorKind::Other,
			message: message.into(),
			traces: Vec::new(),
			status: None,
		}
	}

	pub fn bad_url(message: impl Into<String>) -> Self {
		Self {
			kind: ErrorKind::BadUrl,
			..Self::new(message)
		}
	}

	pub fn with_status(mut self, status: u8) -> Self {
		self.status = Some(status);
		self
	}

	/// Record the context the error passed through.
	pub fn trace(mut self, context: impl Into<String>) -> Self {
		self.traces.push(context.into());
		self
	}

	pub fn is_bad_url(&self) -> bool {
		self.kind == ErrorKind::BadUrl
	}
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.message)
	}
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
	fn from(error: std::io::Error) -> Self {
		Error::new(error.to_string())
	}
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

static WARNING_HANDLER: OnceLock<fn(&str)> = OnceLock::new();

/// Route warnings through the caller's logger; the default prints
/// `warning: MESSAGE` on standard error.
pub fn set_warning_handler(handler: fn(&str)) {
	let _ = WARNING_HANDLER.set(handler);
}

/// Emit a warning as CppNix's `warn` does.
pub fn warn(message: &str) {
	match WARNING_HANDLER.get() {
		Some(handler) => handler(message),
		None => eprintln!("warning: {message}"),
	}
}

static NOTICE_HANDLER: OnceLock<fn(&str)> = OnceLock::new();

/// Route notices (CppNix's `notice`, shown at the default verbosity)
/// through the caller's logger; the default prints them on standard error.
pub fn set_notice_handler(handler: fn(&str)) {
	let _ = NOTICE_HANDLER.set(handler);
}

pub fn notice(message: &str) {
	match NOTICE_HANDLER.get() {
		Some(handler) => handler(message),
		None => eprintln!("{message}"),
	}
}
