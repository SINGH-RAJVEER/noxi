//! Structured operation events and their terminal rendering.
//!
//! Every producer (evaluator, store, builder, daemon, activation) reports
//! through an [`Emitter`] into a [`Sink`]. Renderers turn the same events
//! into cargo-style status lines, per-build output streams, rustc-style
//! [`Diagnostic`]s or versioned JSON; none of them parses human-readable
//! text. [docs/logging.md] is the specification.
//!
//! This crate sits low in the dependency graph so the build engine, the
//! daemon and the fetchers can depend on it: artifacts are plain strings,
//! not store types.
//!
//! [docs/logging.md]: https://github.com/SINGH-RAJVEER/nix-rs/blob/main/docs/logging.md

pub mod codes;
pub mod diagnostic;
pub mod event;
pub mod lines;
pub mod status;
pub mod style;

pub use diagnostic::{Diagnostic, DiagnosticLevel, Help, Label, LogExcerpt, Source};
pub use event::{
	ActivityId, ActivityKind, ActivityOutcome, Emitter, Event, Fanout, JsonSink, Kind, NullSink,
	RecordingSink, Redactor, SCHEMA_VERSION, Severity, Sink, Unit,
};
pub use lines::LineSplitter;
pub use status::{Tone, format_bytes, format_duration, status};
pub use style::Styling;
