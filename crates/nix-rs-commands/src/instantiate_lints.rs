//! The legacy command's parser diagnostics, independent of evaluator error rendering.

use std::cell::Cell;
use std::rc::Rc;

use nix_rs_eval::parser_lints::{Diagnostic, Settings, Severity};
use nix_rs_eval::{ErrorKind, SourceCode};

pub(crate) struct Lints {
	settings: Settings,
	pub(crate) failed: Cell<bool>,
	warnings: bool,
}

impl Lints {
	pub(crate) fn new(settings: &nix_compat::nixcpp::settings::Settings) -> Rc<Self> {
		let severity = |name| Severity::from_setting(settings.get(name).unwrap_or("ignore"));
		Rc::new(Self {
			settings: Settings {
				absolute_paths: severity("lint-absolute-path-literals"),
				short_paths: severity("lint-short-path-literals"),
				urls: severity("lint-url-literals"),
			},
			failed: Cell::new(false),
			warnings: nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn),
		})
	}

	fn emit(&self, diagnostics: Vec<Diagnostic>, code: &str, name: &str) -> Result<(), ErrorKind> {
		for diagnostic in diagnostics {
			if diagnostic.severity == Severity::Fatal {
				eprint!("{}", diagnostic.render(code, name));
				self.failed.set(true);
				// The command has emitted this parser diagnostic. Abort compilation;
				// the caller uses `failed` to avoid rendering it a second time.
				return Err(ErrorKind::EvalError(diagnostic.message));
			}
			if self.warnings {
				eprint!("{}", diagnostic.render(code, name));
			}
		}
		Ok(())
	}

	/// Check primary input before store initialization when lint policy can reject it.
	/// Warnings for files are emitted when the evaluator actually parses the file.
	pub(crate) fn preflight(
		&self,
		code: &str,
		name: &str,
		expressions: bool,
	) -> Result<(), ErrorKind> {
		let diagnostics = self.settings.check_text(code);
		if expressions || diagnostics.iter().any(|d| d.severity == Severity::Fatal) {
			self.emit(diagnostics, code, name)?;
		}
		Ok(())
	}

	pub(crate) fn source_map(self: &Rc<Self>) -> SourceCode {
		let lints = self.clone();
		SourceCode::default().with_source_check(move |expr, file| {
			// The command driver and the evaluator's builtin definitions are generated
			// sources. Original --expr/--arg sources are checked by preflight.
			if file.name() == "[code]" || file.name().starts_with("<src-builtins/") {
				return Ok(());
			}
			lints.emit(lints.settings.check(expr), file.source(), file.name())
		})
	}
}
