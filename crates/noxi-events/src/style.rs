//! Terminal styling: whether to color, how wide the terminal is, and the
//! palette, which follows rustc and cargo so the meaning of each color is
//! already familiar.

use std::io::IsTerminal;

use anstyle::{AnsiColor, Effects, Style};

/// Errors and error codes.
pub const ERROR: Style = AnsiColor::BrightRed.on_default().effects(Effects::BOLD);
pub const WARNING: Style = AnsiColor::BrightYellow.on_default().effects(Effects::BOLD);
pub const NOTE: Style = AnsiColor::BrightGreen.on_default().effects(Effects::BOLD);
pub const HELP: Style = AnsiColor::BrightCyan.on_default().effects(Effects::BOLD);
/// Gutters, `-->` arrows and line numbers.
pub const GUTTER: Style = AnsiColor::BrightBlue.on_default().effects(Effects::BOLD);
/// Status verbs for work in progress or done, as cargo prints them.
pub const VERB: Style = AnsiColor::Green.on_default().effects(Effects::BOLD);
/// Status verbs for failed or cancelled work.
pub const VERB_FAILED: Style = AnsiColor::Red.on_default().effects(Effects::BOLD);
/// Status verbs for work that finished with warnings.
pub const VERB_WARNING: Style = AnsiColor::Yellow.on_default().effects(Effects::BOLD);
/// Package and artifact names.
pub const NAME: Style = Style::new().effects(Effects::BOLD);
/// Store hashes, paths and timings.
pub const DIM: Style = Style::new().effects(Effects::DIMMED);

/// Colors for per-build line prefixes when output cannot be redrawn. Red
/// and yellow are left out: they mean error and warning.
pub const PREFIX_COLORS: [AnsiColor; 6] = [
	AnsiColor::Cyan,
	AnsiColor::Magenta,
	AnsiColor::Blue,
	AnsiColor::Green,
	AnsiColor::BrightMagenta,
	AnsiColor::BrightCyan,
];

/// The narrowest width rendering assumes.
pub const MIN_WIDTH: usize = 40;
/// The width when it cannot be detected.
pub const DEFAULT_WIDTH: usize = 100;

/// How output is rendered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Styling {
	pub color: bool,
	pub width: usize,
}

impl Default for Styling {
	fn default() -> Self {
		Styling::PLAIN
	}
}

impl Styling {
	/// No color at the default width: tests, files and pipes.
	pub const PLAIN: Styling = Styling {
		color: false,
		width: DEFAULT_WIDTH,
	};

	/// Styling for standard error, from the terminal and the environment.
	pub fn for_stderr() -> Styling {
		let stderr = std::io::stderr();
		Styling::decide(
			stderr.is_terminal(),
			&|name| std::env::var(name).ok(),
			terminal_width(libc::STDERR_FILENO),
		)
	}

	/// Color when `is_terminal`, unless `NO_COLOR` is set or `TERM` is
	/// `dumb`; `CLICOLOR_FORCE` forces it on. `COLUMNS` overrides the
	/// detected width.
	pub fn decide(
		is_terminal: bool,
		env: &dyn Fn(&str) -> Option<String>,
		detected_width: Option<usize>,
	) -> Styling {
		let set = |name: &str| env(name).is_some_and(|value| !value.is_empty());
		let color = if set("NO_COLOR") {
			false
		} else if env("CLICOLOR_FORCE").is_some_and(|value| !value.is_empty() && value != "0") {
			true
		} else {
			is_terminal && env("TERM").as_deref() != Some("dumb")
		};
		let width = env("COLUMNS")
			.and_then(|columns| columns.trim().parse().ok())
			.or(detected_width)
			.unwrap_or(DEFAULT_WIDTH)
			.max(MIN_WIDTH);
		Styling { color, width }
	}

	/// `text` in `style` when coloring.
	pub fn paint(&self, style: Style, text: &str) -> String {
		if self.color && style != Style::new() {
			format!("{style}{text}{style:#}")
		} else {
			text.to_owned()
		}
	}
}

/// The width of the terminal on `fd`, if it is one.
pub fn terminal_width(fd: libc::c_int) -> Option<usize> {
	let mut size = libc::winsize {
		ws_row: 0,
		ws_col: 0,
		ws_xpixel: 0,
		ws_ypixel: 0,
	};
	// SAFETY: TIOCGWINSZ writes a `winsize` through a valid pointer and
	// fails harmlessly on a descriptor that is not a terminal.
	let result = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) };
	(result == 0 && size.ws_col > 0).then_some(usize::from(size.ws_col))
}

/// A stable color for a build's line prefix, from its name.
pub fn prefix_color(name: &str) -> AnsiColor {
	// FNV-1a: stable across runs and platforms, unlike the std hasher.
	let mut hash: u32 = 0x811c_9dc5;
	for byte in name.bytes() {
		hash ^= u32::from(byte);
		hash = hash.wrapping_mul(0x0100_0193);
	}
	PREFIX_COLORS[hash as usize % PREFIX_COLORS.len()]
}

#[cfg(test)]
mod tests {
	use super::*;

	fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
		move |name| {
			pairs
				.iter()
				.find(|(key, _)| *key == name)
				.map(|(_, value)| (*value).to_owned())
		}
	}

	#[test]
	fn color_follows_terminal_and_environment() {
		assert!(Styling::decide(true, &env(&[]), Some(120)).color);
		assert!(!Styling::decide(false, &env(&[]), None).color);
		assert!(!Styling::decide(true, &env(&[("NO_COLOR", "1")]), None).color);
		assert!(!Styling::decide(true, &env(&[("TERM", "dumb")]), None).color);
		assert!(Styling::decide(false, &env(&[("CLICOLOR_FORCE", "1")]), None).color);
		assert!(!Styling::decide(false, &env(&[("CLICOLOR_FORCE", "0")]), None).color);
	}

	#[test]
	fn width_prefers_columns_and_has_a_floor() {
		assert_eq!(Styling::decide(true, &env(&[]), Some(120)).width, 120);
		assert_eq!(
			Styling::decide(true, &env(&[("COLUMNS", "90")]), Some(120)).width,
			90
		);
		assert_eq!(Styling::decide(true, &env(&[]), Some(10)).width, MIN_WIDTH);
		assert_eq!(Styling::decide(false, &env(&[]), None).width, DEFAULT_WIDTH);
	}

	#[test]
	fn prefix_colors_are_stable_and_never_red_or_yellow() {
		assert_eq!(prefix_color("openssl"), prefix_color("openssl"));
		for name in ["openssl", "nginx", "python3", "glibc", "bash"] {
			let color = prefix_color(name);
			assert!(!matches!(
				color,
				AnsiColor::Red | AnsiColor::BrightRed | AnsiColor::Yellow | AnsiColor::BrightYellow
			));
		}
	}
}
