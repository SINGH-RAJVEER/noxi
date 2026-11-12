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

/// Tracks all source code in a noxi evaluation for accurate error
/// reporting.
#[derive(Clone)]
pub struct SourceCode(
	Rc<RefCell<CodeMap>>,
	Rc<RefCell<Vec<Span>>>,
	Option<Rc<SourceCheck>>,
	Rc<RefCell<Vec<(Span, Span)>>>,
	Rc<RefCell<Vec<(Span, u32)>>>,
	Rc<RefCell<std::collections::BTreeSet<String>>>,
	/// The line start offsets of each file positions were requested in, by
	/// the file's starting position.
	Rc<RefCell<rustc_hash::FxHashMap<codemap::Pos, Rc<[usize]>>>>,
);

type SourceCheck = dyn Fn(&rnix::ast::Expr, &codemap::File) -> Result<(), crate::ErrorKind>;

impl std::fmt::Debug for SourceCode {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_tuple("SourceCode").field(&self.0).finish()
	}
}

impl SourceCode {
	pub(crate) fn set_features(&self, features: Vec<String>) {
		*self.5.borrow_mut() = features.into_iter().collect();
	}
	pub(crate) fn feature_enabled(&self, name: &str) -> bool {
		self.5.borrow().contains(name)
	}

	/// Preserve arbitrary source bytes through rnix's UTF-8 syntax tree. Choose a
	/// private Unicode block absent from the input, so valid source cannot collide.
	pub(crate) fn encode_bytes(bytes: &[u8]) -> (String, Option<u32>) {
		if let Ok(text) = std::str::from_utf8(bytes) {
			return (text.to_owned(), None);
		}
		let lossy = String::from_utf8_lossy(bytes);
		let base = (0xF0000..0xFFF00)
			.step_by(256)
			.find(|base| {
				!lossy
					.chars()
					.any(|c| (*base..*base + 256).contains(&(c as u32)))
			})
			.expect("available source byte encoding");
		let mut text = String::new();
		let mut rest = bytes;
		while !rest.is_empty() {
			match std::str::from_utf8(rest) {
				Ok(valid) => {
					text.push_str(valid);
					break;
				}
				Err(error) => {
					let valid = error.valid_up_to();
					text.push_str(std::str::from_utf8(&rest[..valid]).unwrap());
					let count = error.error_len().unwrap_or(rest.len() - valid);
					for byte in &rest[valid..valid + count] {
						text.push(char::from_u32(base + u32::from(*byte)).unwrap());
					}
					rest = &rest[valid + count..];
				}
			}
		}
		(text, Some(base))
	}

	pub(crate) fn set_byte_encoding(&self, span: Span, base: Option<u32>) {
		if let Some(base) = base {
			self.4.borrow_mut().push((span, base));
		}
	}

	pub(crate) fn has_byte_encoding(&self, file: &codemap::File) -> bool {
		self.4.borrow().iter().any(|(span, _)| *span == file.span)
	}

	pub(crate) fn decode_literal(&self, file: &codemap::File, text: &str) -> Vec<u8> {
		let base = self
			.4
			.borrow()
			.iter()
			.find(|(span, _)| *span == file.span)
			.map(|(_, base)| *base);
		let mut bytes = Vec::new();
		for c in text.chars() {
			if let Some(base) = base
				&& (base..base + 256).contains(&(c as u32))
			{
				bytes.push((c as u32 - base) as u8);
			} else {
				let mut buffer = [0; 4];
				bytes.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
			}
		}
		bytes
	}

	pub(crate) fn decode_diagnostic(&self, text: &str) -> Vec<u8> {
		let encodings = self.4.borrow();
		if encodings.is_empty() {
			return text.as_bytes().to_vec();
		}
		let mut out = Vec::new();
		let mut encoding = None;
		for line in text.split_inclusive('\n') {
			let source_line = line
				.trim_start()
				.split_once("| ")
				.is_some_and(|(number, _)| {
					!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
				});
			if let Some(location) = line.trim_start().strip_prefix("at ") {
				encoding = encodings.iter().find_map(|(span, base)| {
					let file = self.get_file(*span);
					location
						.strip_prefix(file.name())
						.is_some_and(|suffix| suffix.starts_with(':'))
						.then_some(*base)
				});
			}
			for c in line.chars() {
				if source_line
					&& let Some(base) = encoding
					&& (base..base + 256).contains(&(c as u32))
				{
					out.push((c as u32 - base) as u8);
				} else {
					let mut buf = [0; 4];
					out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
				}
			}
		}
		out
	}

	/// Validate each parsed source before compilation, including imported files.
	/// The callback belongs to this source map, so independent evaluations do not share policy.
	pub fn with_source_check(
		mut self,
		check: impl Fn(&rnix::ast::Expr, &codemap::File) -> Result<(), crate::ErrorKind> + 'static,
	) -> Self {
		self.2 = Some(Rc::new(check));
		self
	}

	pub(crate) fn check(
		&self,
		expr: &rnix::ast::Expr,
		file: &codemap::File,
	) -> crate::EvalResult<()> {
		if let Some(check) = &self.2 {
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
	/// The start offsets of the lines of `file`, as
	/// [`lines`](crate::diagnostics::lines) splits them, computed once per file.
	pub(crate) fn line_starts(&self, file: &codemap::File) -> Rc<[usize]> {
		self.6
			.borrow_mut()
			.entry(file.span.low())
			.or_insert_with(|| {
				crate::diagnostics::lines(file.source())
					.into_iter()
					.map(|(start, _)| start)
					.collect()
			})
			.clone()
	}

	pub fn get_file(&self, span: Span) -> Arc<codemap::File> {
		self.codemap().look_up_span(span).file
	}

	/// Mark a file as generated by the caller, such as a command's driver
	/// expression: CppNix-format traces leave out its frames and positions.
	pub fn hide_file(&self, file: &codemap::File) {
		self.1.borrow_mut().push(file.span);
	}

	/// Map a fragment of generated code back to the original input.
	pub(crate) fn map_fragment(&self, generated: Span, original: Span) {
		self.3.borrow_mut().push((generated, original));
	}

	/// Resolve a generated-code span to its original input, when present.
	pub(crate) fn original_span(&self, span: Span) -> Span {
		self.3
			.borrow()
			.iter()
			.find_map(|(generated, original)| {
				(generated.low() <= span.low() && span.high() <= generated.high()).then(|| {
					original.subspan(span.low() - generated.low(), span.high() - generated.low())
				})
			})
			.unwrap_or(span)
	}

	/// Whether `span` is in a file marked with [`hide_file`](Self::hide_file).
	pub fn is_hidden(&self, span: Span) -> bool {
		let span = self.original_span(span);
		self.1
			.borrow()
			.iter()
			.any(|file| file.low() <= span.low() && span.high() <= file.high())
	}
}

impl Default for SourceCode {
	/// Create a new SourceCode instance.
	fn default() -> Self {
		Self(
			Rc::new(RefCell::new(CodeMap::new())),
			Rc::new(RefCell::new(vec![])),
			None,
			Rc::new(RefCell::new(vec![])),
			Rc::new(RefCell::new(vec![])),
			Rc::new(RefCell::new(Default::default())),
			Rc::new(RefCell::new(Default::default())),
		)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn byte_source_roundtrips_without_colliding_with_private_unicode() {
		let source = SourceCode::default();
		let mut bytes = "\u{f00ff}\n\"".as_bytes().to_vec();
		bytes.extend_from_slice(b"\xff\xc3_\"\n");
		let (encoded, base) = SourceCode::encode_bytes(&bytes);
		assert_ne!(base, Some(0xf0000));
		let file = source.add_file("bytes.nix".into(), encoded);
		source.set_byte_encoding(file.span, base);
		assert_eq!(source.decode_literal(&file, file.source()), bytes);
	}

	#[test]
	fn raw_excerpts_do_not_decode_private_unicode_from_another_file() {
		let source = SourceCode::default();
		let (encoded, base) = SourceCode::encode_bytes(b"\"\xff\"\n");
		let file = source.add_file("bytes.nix".into(), encoded.clone());
		source.set_byte_encoding(file.span, base);
		let private = char::from_u32(base.unwrap() + 255).unwrap();
		let diagnostic =
			format!("at bytes.nix:1:1:\n 1| {encoded}at unicode.nix:1:1:\n 1| {private}\n");
		let expected = [
			b"at bytes.nix:1:1:\n 1| \"\xff\"\nat unicode.nix:1:1:\n 1| ".as_slice(),
			private.to_string().as_bytes(),
			b"\n",
		]
		.concat();
		assert_eq!(source.decode_diagnostic(&diagnostic), expected);
	}
}
