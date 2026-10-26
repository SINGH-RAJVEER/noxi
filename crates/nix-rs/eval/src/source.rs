//! This module contains utilities for dealing with the codemap that
//! needs to be carried across different compiler instantiations in an
//! evaluation.
//!
//! The data type `SourceCode` should be carried through all relevant
//! places instead of copying the codemap structures directly.

use std::{
	cell::{Ref, RefCell, RefMut},
	rc::Rc,
	sync::Arc,
};

use codemap::{CodeMap, Span};

/// Tracks all source code in a nix-rs evaluation for accurate error
/// reporting.
#[derive(Clone)]
pub struct SourceCode(Rc<RefCell<CodeMap>>, Option<Rc<SourceCheck>>);

type SourceCheck = dyn Fn(&rnix::ast::Expr, &codemap::File) -> Result<(), crate::ErrorKind>;

impl std::fmt::Debug for SourceCode {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_tuple("SourceCode").field(&self.0).finish()
	}
}

impl SourceCode {
	/// Validate each parsed source before compilation, including imported files.
	/// The callback belongs to this source map, so independent evaluations do not share policy.
	pub fn with_source_check(
		mut self,
		check: impl Fn(&rnix::ast::Expr, &codemap::File) -> Result<(), crate::ErrorKind> + 'static,
	) -> Self {
		self.1 = Some(Rc::new(check));
		self
	}

	pub(crate) fn check(
		&self,
		expr: &rnix::ast::Expr,
		file: &codemap::File,
	) -> crate::EvalResult<()> {
		if let Some(check) = &self.1 {
			check(expr, file).map_err(|kind| crate::Error::new(kind, file.span, self.clone()))?;
		}
		Ok(())
	}
	/// Access a read-only reference to the codemap.
	pub fn codemap(&self) -> Ref<'_, CodeMap> {
		self.0.borrow()
	}

	/// Access a writable reference to the codemap.
	fn codemap_mut(&self) -> RefMut<'_, CodeMap> {
		self.0.borrow_mut()
	}

	/// Add a file to the codemap. The returned Arc is managed by the
	/// codemap internally and can be used like a normal reference.
	pub fn add_file(&self, name: String, code: String) -> Arc<codemap::File> {
		self.codemap_mut().add_file(name, code)
	}

	/// Retrieve the line number of the given span. If it spans
	/// multiple lines, the first line will be returned.
	pub fn get_line(&self, span: Span) -> usize {
		// lines are 0-indexed in the codemap, but users probably want
		// real line numbers
		self.codemap().look_up_span(span).begin.line + 1
	}

	/// Returns the literal source slice of the given span.
	pub fn source_slice(&self, span: Span) -> Ref<'_, str> {
		Ref::map(self.codemap(), |c| {
			c.find_file(span.low()).source_slice(span)
		})
	}

	/// Returns the reference to the file structure that a given span
	/// is in.
	pub fn get_file(&self, span: Span) -> Arc<codemap::File> {
		self.codemap().look_up_span(span).file
	}
}

impl Default for SourceCode {
	/// Create a new SourceCode instance.
	fn default() -> Self {
		Self(Rc::new(RefCell::new(CodeMap::new())), None)
	}
}
