//! Diagnostics in the shape rustc prints them, rendered by
//! `annotate-snippets`, the crate rustc itself uses.
//!
//! ```text
//! error[E0001]: undefined variable 'pkgz'
//!  --> /etc/nixos/configuration.nix:3:24
//!   |
//! 3 |   environment.systemPackages = [ pkgz.git ];
//!   |                                  ^^^^ not defined here
//!   |
//! help: a variable with a similar name exists
//!   |
//! 3 -   environment.systemPackages = [ pkgz.git ];
//! 3 +   environment.systemPackages = [ pkgs.git ];
//!   |
//! ```

use std::ops::Range;

use annotate_snippets::{AnnotationKind, Group, Level, Patch, Renderer, Snippet};

use crate::style::Styling;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticLevel {
	Error,
	Warning,
}

/// A labelled span of source text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
	/// Byte range into [`Source::text`].
	pub range: Range<usize>,
	pub text: Option<String>,
}

/// The source a diagnostic points into: a whole file, or an excerpt of it
/// starting at `line_start`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
	pub path: String,
	pub text: String,
	/// The 1-based line number of the first line of `text`.
	pub line_start: usize,
	pub primary: Label,
	pub secondary: Vec<Label>,
}

/// A suggestion; with a replacement, it is shown applied to the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Help {
	pub message: String,
	pub replacement: Option<(Range<usize>, String)>,
}

/// An excerpt of an activity's output, such as the failing lines of a
/// build log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogExcerpt {
	/// Where the full log is.
	pub path: String,
	/// The 1-based line number of the first line in the full log.
	pub line_start: usize,
	pub lines: Vec<String>,
	/// Indexes into `lines` worth highlighting.
	pub highlight: Vec<usize>,
}

/// One diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
	pub level: DiagnosticLevel,
	pub code: Option<&'static str>,
	pub title: String,
	pub source: Option<Source>,
	pub log: Option<LogExcerpt>,
	/// `= note:` lines: causes, context, and the state left behind.
	pub notes: Vec<String>,
	pub help: Vec<Help>,
}

impl Diagnostic {
	pub fn error(code: Option<&'static str>, title: impl Into<String>) -> Self {
		Diagnostic {
			level: DiagnosticLevel::Error,
			code,
			title: title.into(),
			source: None,
			log: None,
			notes: Vec::new(),
			help: Vec::new(),
		}
	}

	pub fn warning(code: Option<&'static str>, title: impl Into<String>) -> Self {
		Diagnostic {
			level: DiagnosticLevel::Warning,
			..Diagnostic::error(code, title)
		}
	}

	pub fn with_source(mut self, source: Source) -> Self {
		self.source = Some(source);
		self
	}

	pub fn with_log(mut self, log: LogExcerpt) -> Self {
		self.log = Some(log);
		self
	}

	pub fn note(mut self, note: impl Into<String>) -> Self {
		self.notes.push(note.into());
		self
	}

	pub fn help(mut self, message: impl Into<String>) -> Self {
		self.help.push(Help {
			message: message.into(),
			replacement: None,
		});
		self
	}

	/// A suggestion shown as `replacement` applied to `range` of the source.
	pub fn suggest(
		mut self,
		message: impl Into<String>,
		range: Range<usize>,
		replacement: impl Into<String>,
	) -> Self {
		self.help.push(Help {
			message: message.into(),
			replacement: Some((range, replacement.into())),
		});
		self
	}

	/// The rendered diagnostic, ending in a newline.
	pub fn render(&self, styling: &Styling) -> String {
		let level = match self.level {
			DiagnosticLevel::Error => Level::ERROR,
			DiagnosticLevel::Warning => Level::WARNING,
		};
		let mut title = level.primary_title(self.title.as_str());
		if let Some(code) = self.code {
			title = title.id(code);
		}
		let mut main = Group::with_title(title);
		if let Some(source) = &self.source {
			let mut snippet = Snippet::source(source.text.as_str())
				.path(source.path.as_str())
				.line_start(source.line_start)
				.fold(true);
			let mut primary = AnnotationKind::Primary.span(source.primary.range.clone());
			if let Some(text) = &source.primary.text {
				primary = primary.label(text.as_str());
			}
			snippet = snippet.annotation(primary);
			for label in &source.secondary {
				let mut annotation = AnnotationKind::Context.span(label.range.clone());
				if let Some(text) = &label.text {
					annotation = annotation.label(text.as_str());
				}
				snippet = snippet.annotation(annotation);
			}
			main = main.element(snippet);
		}
		let log_text;
		if let Some(log) = &self.log
			&& !log.lines.is_empty()
		{
			log_text = log.lines.join("\n");
			let mut snippet = Snippet::source(log_text.as_str())
				.path(log.path.as_str())
				.line_start(log.line_start)
				.fold(false);
			let mut offset = 0;
			for (index, line) in log.lines.iter().enumerate() {
				if log.highlight.contains(&index) && !line.is_empty() {
					snippet = snippet.annotation(
						AnnotationKind::Primary
							.span(offset..offset + line.len())
							.highlight_source(true),
					);
				}
				offset += line.len() + 1;
			}
			// A snippet needs an annotation to be shown; mark the last line
			// when nothing was singled out.
			if log.highlight.is_empty() {
				let last = log.lines.last().map_or(0, String::len);
				snippet =
					snippet.annotation(AnnotationKind::Visible.span(offset - last - 1..offset - 1));
			}
			main = main.element(snippet);
		}
		for note in &self.notes {
			main = main.element(Level::NOTE.message(note.as_str()));
		}
		// Plain help joins the main group as `= help:`; a replacement gets
		// its own group showing the source with the change applied.
		let mut suggestions = Vec::new();
		for help in &self.help {
			match (&help.replacement, &self.source) {
				(Some((range, replacement)), Some(source)) => suggestions.push(
					Level::HELP.secondary_title(help.message.as_str()).element(
						Snippet::source(source.text.as_str())
							.path(source.path.as_str())
							.line_start(source.line_start)
							.patch(Patch::new(range.clone(), replacement.as_str())),
					),
				),
				_ => main = main.element(Level::HELP.message(help.message.as_str())),
			}
		}
		let mut groups = vec![main];
		groups.extend(suggestions);
		let renderer = if styling.color {
			Renderer::styled()
		} else {
			Renderer::plain()
		}
		.term_width(styling.width);
		let mut out = renderer.render(&groups);
		out.push('\n');
		out
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::codes;

	fn plain(diagnostic: &Diagnostic) -> String {
		diagnostic.render(&Styling::PLAIN)
	}

	#[test]
	fn evaluation_error_with_span_and_suggestion() {
		let text = "{ pkgs, ... }:\n{\n  environment.systemPackages = [ pkgz.git ];\n}\n";
		let start = text.find("pkgz").unwrap();
		let diagnostic = Diagnostic::error(Some(codes::EVALUATION), "undefined variable 'pkgz'")
			.with_source(Source {
				path: "/etc/nixos/configuration.nix".into(),
				text: text.into(),
				line_start: 1,
				primary: Label {
					range: start..start + 4,
					text: Some("not defined here".into()),
				},
				secondary: vec![Label {
					range: 2..6,
					text: Some("did you mean this argument?".into()),
				}],
			})
			.note("evaluation stopped before any build or system change")
			.suggest(
				"a variable with a similar name exists",
				start..start + 4,
				"pkgs",
			);
		expect_test::expect![[r#"
    error[E0001]: undefined variable 'pkgz'
     --> /etc/nixos/configuration.nix:3:34
      |
    1 | { pkgs, ... }:
      |   ---- did you mean this argument?
    2 | {
    3 |   environment.systemPackages = [ pkgz.git ];
      |                                  ^^^^ not defined here
      |
      = note: evaluation stopped before any build or system change
    help: a variable with a similar name exists
      |
    3 -   environment.systemPackages = [ pkgz.git ];
    3 +   environment.systemPackages = [ pkgs.git ];
      |
"#]]
		.assert_eq(&plain(&diagnostic));
	}

	#[test]
	fn build_failure_with_log_excerpt() {
		let diagnostic = Diagnostic::error(
			Some(codes::BUILD),
			"build of `openssl-3.4.1` failed in buildPhase",
		)
		.with_log(LogExcerpt {
			path: "/nix/var/log/noxi/ops/1/builds/openssl-3.4.1.log".into(),
			line_start: 4410,
			lines: vec![
				"CC crypto/evp/e_aes.o".into(),
				"crypto/evp/e_aes.c:812:5: error: implicit declaration of function 'foo'".into(),
				"make[1]: *** [Makefile:4412: crypto/evp/e_aes.o] Error 1".into(),
			],
			highlight: vec![1, 2],
		})
		.note("builder exited with status 2 after 1m 42s")
		.note("required by `nginx-1.27.3` -> `nixos-system-host-25.11`")
		.help("see the full log with `noxi logs openssl-3.4.1`");
		expect_test::expect![[r#"
    error[B0001]: build of `openssl-3.4.1` failed in buildPhase
        --> /nix/var/log/noxi/ops/1/builds/openssl-3.4.1.log:4411:1
         |
    4410 | CC crypto/evp/e_aes.o
    4411 | crypto/evp/e_aes.c:812:5: error: implicit declaration of function 'foo'
         | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
    4412 | make[1]: *** [Makefile:4412: crypto/evp/e_aes.o] Error 1
         | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
         |
         = note: builder exited with status 2 after 1m 42s
         = note: required by `nginx-1.27.3` -> `nixos-system-host-25.11`
         = help: see the full log with `noxi logs openssl-3.4.1`
"#]]
		.assert_eq(&plain(&diagnostic));
	}

	#[test]
	fn title_with_notes_only() {
		let diagnostic = Diagnostic::error(
			Some(codes::ACTIVATION_FAILED),
			"\"switch-to-configuration switch\" exited with status 4",
		)
		.note("the running system changed; the boot default is generation 119")
		.help("roll back with `noxi system rollback`");
		expect_test::expect![[r#"
    error[A0003]: "switch-to-configuration switch" exited with status 4
     |
     = note: the running system changed; the boot default is generation 119
     = help: roll back with `noxi system rollback`
"#]]
		.assert_eq(&plain(&diagnostic));
	}

	#[test]
	fn styled_output_uses_the_rustc_palette() {
		let styling = Styling {
			color: true,
			width: 100,
		};
		let rendered = Diagnostic::error(Some(codes::BUILD), "x").render(&styling);
		assert!(
			rendered.contains("\u{1b}[1m\u{1b}[91merror[B0001]"),
			"{rendered:?}"
		);
	}
}
