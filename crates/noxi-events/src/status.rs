//! Cargo-style status lines: a right-aligned verb, then what it applies to.
//!
//! ```text
//!    Resolving flake /etc/nixos#host, revision 3f9a1c2
//!   Evaluating nixosConfigurations.host.config.system.build.toplevel
//!        Built /nix/store/...-nixos-system-host-25.11
//!     Finished switch in 2m 14s
//! ```

use std::io::{self, Write};
use std::time::Duration;

use crate::style::{self, Styling};

/// The column the verbs are right-aligned to, as in cargo.
pub const VERB_WIDTH: usize = 12;

/// The meaning of a status line, which picks the verb's color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
	/// Work starting, under way, or done.
	Progress,
	/// Work that finished with warnings.
	Warning,
	/// Work that failed or was cancelled.
	Failure,
}

/// Write one status line.
pub fn status(
	out: &mut dyn Write,
	styling: &Styling,
	verb: &str,
	tone: Tone,
	text: &str,
) -> io::Result<()> {
	let verb_style = match tone {
		Tone::Progress => style::VERB,
		Tone::Warning => style::VERB_WARNING,
		Tone::Failure => style::VERB_FAILED,
	};
	let padded = format!("{verb:>VERB_WIDTH$}");
	let mut lines = text.lines();
	writeln!(
		out,
		"{} {}",
		styling.paint(verb_style, &padded),
		lines.next().unwrap_or("")
	)?;
	// Continuation lines align under the text.
	for line in lines {
		writeln!(out, "{:VERB_WIDTH$} {line}", "")?;
	}
	Ok(())
}

/// The closing line of an operation, `Finished switch in 2.81s`, with the
/// duration dimmed. `preposition` is `in` or `after`.
pub fn timed(
	out: &mut dyn Write,
	styling: &Styling,
	verb: &str,
	tone: Tone,
	subject: &str,
	preposition: &str,
	elapsed: Duration,
) -> io::Result<()> {
	let elapsed = styling.paint(style::DIM, &format_duration(elapsed));
	status(
		out,
		styling,
		verb,
		tone,
		&format!("{subject} {preposition} {elapsed}"),
	)
}

/// A duration as a person reads it: `340ms`, `2.81s`, `1m 42s`, `1h 03m`.
pub fn format_duration(duration: Duration) -> String {
	let millis = duration.as_millis();
	let secs = duration.as_secs();
	if millis < 1000 {
		format!("{millis}ms")
	} else if secs < 60 {
		format!("{:.2}s", duration.as_secs_f64())
	} else if secs < 3600 {
		format!("{}m {:02}s", secs / 60, secs % 60)
	} else {
		format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
	}
}

/// A byte count with a binary unit: `512 B`, `1.5 KiB`, `412.0 MiB`.
pub fn format_bytes(bytes: u64) -> String {
	const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
	if bytes < 1024 {
		return format!("{bytes} B");
	}
	let mut value = bytes as f64 / 1024.0;
	let mut unit = 0;
	while value >= 1024.0 && unit + 1 < UNITS.len() {
		value /= 1024.0;
		unit += 1;
	}
	format!("{value:.1} {}", UNITS[unit])
}

/// `count` followed by `singular` or `plural`.
pub fn plural(count: usize, singular: &str, plural: &str) -> String {
	format!("{count} {}", if count == 1 { singular } else { plural })
}

#[cfg(test)]
mod tests {
	use super::*;

	fn render(styling: Styling, verb: &str, tone: Tone, text: &str) -> String {
		let mut out = Vec::new();
		status(&mut out, &styling, verb, tone, text).unwrap();
		String::from_utf8(out).unwrap()
	}

	#[test]
	fn verbs_align_like_cargo() {
		let plain = Styling::PLAIN;
		let lines = [
			render(plain, "Resolving", Tone::Progress, "flake /etc/nixos#host"),
			render(plain, "Built", Tone::Progress, "/nix/store/x-system"),
			render(plain, "Failed", Tone::Failure, "switch after 2.81s\nsecond"),
		]
		.concat();
		// Exact text: expect-test would strip the alignment padding.
		assert_eq!(
			lines,
			concat!(
				"   Resolving flake /etc/nixos#host\n",
				"       Built /nix/store/x-system\n",
				"      Failed switch after 2.81s\n",
				"             second\n",
			)
		);
	}

	#[test]
	fn color_wraps_only_the_verb() {
		let styling = Styling {
			color: true,
			width: 80,
		};
		let line = render(styling, "Built", Tone::Progress, "x");
		assert_eq!(line, "\u{1b}[1m\u{1b}[32m       Built\u{1b}[0m x\n");
		let failed = render(styling, "Failed", Tone::Failure, "x");
		assert!(failed.starts_with("\u{1b}[1m\u{1b}[31m"), "{failed:?}");
	}

	#[test]
	fn closing_lines_dim_the_duration() {
		let elapsed = Duration::from_millis(2810);
		let mut plain = Vec::new();
		timed(
			&mut plain,
			&Styling::PLAIN,
			"Finished",
			Tone::Progress,
			"switch",
			"in",
			elapsed,
		)
		.unwrap();
		assert_eq!(plain, b"    Finished switch in 2.81s\n");
		let styling = Styling {
			color: true,
			width: 80,
		};
		let mut styled = Vec::new();
		timed(
			&mut styled,
			&styling,
			"Failed",
			Tone::Failure,
			"switch",
			"after",
			elapsed,
		)
		.unwrap();
		let styled = String::from_utf8(styled).unwrap();
		assert!(
			styled.ends_with(" switch after \u{1b}[2m2.81s\u{1b}[0m\n"),
			"{styled:?}"
		);
	}

	#[test]
	fn durations_and_sizes_read_naturally() {
		assert_eq!(format_duration(Duration::from_millis(340)), "340ms");
		assert_eq!(format_duration(Duration::from_millis(2810)), "2.81s");
		assert_eq!(format_duration(Duration::from_secs(102)), "1m 42s");
		assert_eq!(format_duration(Duration::from_secs(3780)), "1h 03m");
		assert_eq!(format_bytes(512), "512 B");
		assert_eq!(format_bytes(1536), "1.5 KiB");
		assert_eq!(format_bytes(412 * 1024 * 1024), "412.0 MiB");
		assert_eq!(plural(1, "build", "builds"), "1 build");
		assert_eq!(plural(3, "build", "builds"), "3 builds");
	}
}
