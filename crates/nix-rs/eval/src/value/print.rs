//! The value printer behind CppNix's `nix-instantiate --eval`
//! (`printAmbiguous`).
//!
//! It differs from [`TotalDisplay`](super::TotalDisplay): unevaluated thunks
//! print as `<CODE>`, derivations print as their attribute sets, and a
//! non-empty attribute set or list that was already printed anywhere in the
//! value, not only on the current path, prints as `«repeated»`.
//!
//! CppNix identifies a repeated attribute set by its bindings and a repeated
//! list by the value cell holding it. Here a set is identified by its shared
//! storage and a list by the thunk it was reached through; a list reached
//! directly occupies its own cell and is never repeated.

use std::fmt::{self, Display, Formatter};

use codemap::Span;
use rustc_hash::FxHashSet;

use super::{NixAttrs, NixList, Value, thunk::ThunkRepr, total_fmt_float};
use crate::SourceCode;
use crate::opcode::CodeIdx;

/// Displays a value as `nix-instantiate --eval` prints it.
pub struct Ambiguous<'a>(pub &'a Value);

impl Display for Ambiguous<'_> {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		print(self.0, None, f, &mut FxHashSet::default())
	}
}

fn print(
	value: &Value,
	cell: Option<*const ()>,
	f: &mut Formatter<'_>,
	seen: &mut FxHashSet<*const ()>,
) -> fmt::Result {
	match value {
		Value::Attrs(attrs) => {
			if !attrs.is_empty() && !seen.insert(attrs.as_ptr()) {
				return f.write_str("«repeated»");
			}
			f.write_str("{ ")?;
			for (name, value) in attrs.iter_sorted() {
				write!(f, "{} = ", name.ident_str())?;
				print(value, None, f, seen)?;
				f.write_str("; ")?;
			}
			f.write_str("}")
		}
		Value::List(list) => {
			if !list.is_empty() && cell.is_some_and(|cell| !seen.insert(cell)) {
				return f.write_str("«repeated»");
			}
			f.write_str("[ ")?;
			for value in list {
				print(value, None, f, seen)?;
				f.write_str(" ")?;
			}
			f.write_str("]")
		}
		Value::Thunk(thunk) => match &*thunk.0.borrow() {
			ThunkRepr::Evaluated(value) => print(value, Some(thunk.as_ptr()), f, seen),
			ThunkRepr::Blackhole { .. } => f.write_str("«potential infinite recursion»"),
			ThunkRepr::Suspended { .. } | ThunkRepr::Native { .. } => f.write_str("<CODE>"),
		},
		Value::Float(num) => total_fmt_float(*num, f),
		value => write!(f, "{value}"),
	}
}

/// The limits of CppNix's `PrintOptions` that this printer supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrintOptions {
	pub max_depth: usize,
	pub max_attrs: usize,
	pub max_list_items: usize,
	pub max_string_length: usize,
}

impl PrintOptions {
	/// The default options, as `builtins.trace` uses them.
	pub const UNLIMITED: Self = PrintOptions {
		max_depth: usize::MAX,
		max_attrs: usize::MAX,
		max_list_items: usize::MAX,
		max_string_length: usize::MAX,
	};

	/// `errorPrintOptions`, for values shown in error messages.
	pub const ERROR: Self = PrintOptions {
		max_depth: 10,
		max_attrs: 10,
		max_list_items: 10,
		max_string_length: 1024,
	};
}

/// CppNix's `ValuePrinter` without forcing: thunks print as `«thunk»`,
/// functions with their names and positions, and repeated sets and lists as
/// `«repeated»`.
pub struct ValuePrinter<'a> {
	source: &'a SourceCode,
	options: PrintOptions,
	seen: FxHashSet<*const ()>,
	attrs_printed: usize,
	items_printed: usize,
	out: Vec<u8>,
}

impl<'a> ValuePrinter<'a> {
	/// Prints `value`, resolving function positions through `source`.
	pub fn print(source: &'a SourceCode, options: PrintOptions, value: &Value) -> Vec<u8> {
		let mut printer = ValuePrinter {
			source,
			options,
			seen: FxHashSet::default(),
			attrs_printed: 0,
			items_printed: 0,
			out: vec![],
		};
		printer.value(value, None, 0);
		printer.out
	}

	fn value(&mut self, value: &Value, cell: Option<*const ()>, depth: usize) {
		match value {
			Value::Null => self.out.extend_from_slice(b"null"),
			Value::Bool(b) => self
				.out
				.extend_from_slice(if *b { b"true" } else { b"false" }),
			Value::Integer(i) => self.out.extend_from_slice(i.to_string().as_bytes()),
			Value::Float(num) => {
				let mut s = String::new();
				let _ = total_fmt_float(*num, &mut s);
				self.out.extend_from_slice(s.as_bytes());
			}
			Value::String(s) => {
				literal_string(&mut self.out, s.as_bytes(), self.options.max_string_length)
			}
			Value::Path(p) => self.out.extend_from_slice(p.as_os_str().as_encoded_bytes()),
			Value::Attrs(attrs) => self.attrs(attrs, depth),
			Value::List(list) => self.list(list, cell, depth),
			Value::Closure(closure) => {
				let lambda = &closure.lambda;
				self.out.extend_from_slice("«lambda".as_bytes());
				if let Some(name) = &lambda.name {
					self.out.push(b' ');
					self.out.extend_from_slice(name.as_bytes());
				}
				if let Some((_, idx)) = lambda.chunk.last_op() {
					let span = lambda.chunk.get_span(CodeIdx(idx));
					self.out.extend_from_slice(b" @ ");
					self.out
						.extend_from_slice(position(self.source, span).as_bytes());
				}
				self.out.extend_from_slice("»".as_bytes());
			}
			Value::Builtin(builtin) => {
				let partial = if builtin.has_partials() {
					"partially applied "
				} else {
					""
				};
				self.out
					.extend_from_slice(format!("«{partial}primop {}»", builtin.name()).as_bytes());
			}
			Value::Thunk(thunk) => match &*thunk.0.borrow() {
				ThunkRepr::Evaluated(value) => self.value(value, Some(thunk.as_ptr()), depth),
				ThunkRepr::Blackhole { .. } => self
					.out
					.extend_from_slice("«potential infinite recursion»".as_bytes()),
				ThunkRepr::Suspended { .. } | ThunkRepr::Native { .. } => {
					self.out.extend_from_slice("«thunk»".as_bytes())
				}
			},
			// A failed value, like one `builtins.tryEval` discarded.
			Value::Catchable(_) => self.out.extend_from_slice("«thunk»".as_bytes()),
			_ => self.out.extend_from_slice("«unknown»".as_bytes()),
		}
	}

	fn attrs(&mut self, attrs: &NixAttrs, depth: usize) {
		if !self.seen.insert(attrs.as_ptr()) {
			self.out.extend_from_slice("«repeated»".as_bytes());
			return;
		}
		if depth >= self.options.max_depth {
			self.out.extend_from_slice(b"{ ... }");
			return;
		}

		let mut sorted: Vec<_> = attrs.iter().collect();
		if self.options.max_attrs == usize::MAX {
			sorted.sort_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));
		} else {
			let important = |name: &[u8]| name == b"type" || name == b"_type";
			sorted.sort_by(|(a, _), (b, _)| {
				(!important(a.as_bytes()), a.as_bytes())
					.cmp(&(!important(b.as_bytes()), b.as_bytes()))
			});
		}

		self.out.push(b'{');
		for (printed, (name, value)) in sorted.iter().enumerate() {
			self.out.push(b' ');
			if self.attrs_printed >= self.options.max_attrs {
				self.elided(sorted.len() - printed, "attribute", "attributes");
				break;
			}
			attribute_name(&mut self.out, name.as_bytes());
			self.out.extend_from_slice(b" = ");
			self.value(value, None, depth + 1);
			self.out.push(b';');
			self.attrs_printed += 1;
		}
		self.out.extend_from_slice(b" }");
	}

	fn list(&mut self, list: &NixList, cell: Option<*const ()>, depth: usize) {
		if !list.is_empty() && cell.is_some_and(|cell| !self.seen.insert(cell)) {
			self.out.extend_from_slice("«repeated»".as_bytes());
			return;
		}
		if depth >= self.options.max_depth {
			self.out.extend_from_slice(b"[ ... ]");
			return;
		}

		self.out.push(b'[');
		for (printed, value) in list.iter().enumerate() {
			self.out.push(b' ');
			if self.items_printed >= self.options.max_list_items {
				self.elided(list.len() - printed, "item", "items");
				break;
			}
			self.value(value, None, depth + 1);
			self.items_printed += 1;
		}
		self.out.extend_from_slice(b" ]");
	}

	fn elided(&mut self, count: usize, single: &str, plural: &str) {
		let noun = if count == 1 { single } else { plural };
		self.out
			.extend_from_slice(format!("«{count} {noun} elided»").as_bytes());
	}
}

/// A source position as CppNix prints it: `«string»:LINE:COLUMN` for an
/// expression given on the command line, otherwise `FILE:LINE:COLUMN`.
pub(crate) fn position(source: &SourceCode, span: Span) -> String {
	let file = source.get_file(span);
	let pos = file.find_line_col(span.low());
	let name = match file.name() {
		crate::REPL_LOCATION => "«string»",
		name => name,
	};
	format!("{name}:{}:{}", pos.line + 1, pos.column + 1)
}

/// `printLiteralString`: a quoted, escaped string, cut after `max_length`
/// bytes.
fn literal_string(out: &mut Vec<u8>, s: &[u8], max_length: usize) {
	out.push(b'"');
	for (printed, (i, &c)) in s.iter().enumerate().enumerate() {
		if printed >= max_length {
			let rest = s.len() - printed;
			let noun = if rest == 1 { "byte" } else { "bytes" };
			out.extend_from_slice(format!("\" «{rest} {noun} elided»").as_bytes());
			return;
		}
		match c {
			b'"' | b'\\' => out.extend_from_slice(&[b'\\', c]),
			b'\n' => out.extend_from_slice(b"\\n"),
			b'\r' => out.extend_from_slice(b"\\r"),
			b'\t' => out.extend_from_slice(b"\\t"),
			b'$' if s.get(i + 1) == Some(&b'{') => out.extend_from_slice(b"\\$"),
			c => out.push(c),
		}
	}
	out.push(b'"');
}

/// `printAttributeName`: unquoted when the name is a variable name.
fn attribute_name(out: &mut Vec<u8>, name: &[u8]) {
	const KEYWORDS: &[&[u8]] = &[
		b"if", b"then", b"else", b"assert", b"with", b"let", b"in", b"rec", b"inherit",
	];
	let var_name = match name.first() {
		None | Some(b'0'..=b'9' | b'-' | b'\'') => false,
		Some(_) => {
			!KEYWORDS.contains(&name)
				&& name
					.iter()
					.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'\''))
		}
	};
	if var_name {
		out.extend_from_slice(name);
	} else {
		literal_string(out, name, usize::MAX);
	}
}
