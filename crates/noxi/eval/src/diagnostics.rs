//! Error rendering in CppNix 2.35.2's format: `showErrorInfo`,
//! `printCodeLines` and `printTrace` from `libutil/error.cc`, and the line
//! splitting and positions of `libutil/position.cc` and `pos-table.cc`.
//!
//! The legacy commands print evaluation errors this way. The codespan
//! renderer ([`Error::fancy_format_str`]) stays available for interfaces
//! that select it explicitly.

use std::collections::HashSet;
use std::fmt::Write as _;

use bstr::ByteSlice;
use codemap::Span;

use crate::SourceCode;
use crate::errors::{CatchableErrorKind, Error, ErrorKind, Trace};
use crate::value::{Lambda, PrintOptions, Value, ValuePrinter};

/// The origin CppNix prints for code given on the command line.
pub const STRING_ORIGIN: &str = "«string»";

/// The origin CppNix prints for code read from standard input.
pub const STDIN_ORIGIN: &str = "«stdin»";

/// A resolved source position, as CppNix's `Pos`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Pos {
	pub origin: String,
	/// 1-based line, counting `\r\n`, `\r` and `\n` as one line break each.
	pub line: usize,
	/// 1-based byte column.
	pub column: usize,
	source: Option<std::sync::Arc<codemap::File>>,
}

impl Pos {
	fn key(&self) -> (&str, usize, usize) {
		(&self.origin, self.line, self.column)
	}
}

impl std::fmt::Display for Pos {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}:{}", self.origin, self.line)?;
		if self.column > 0 {
			write!(f, ":{}", self.column)?;
		}
		Ok(())
	}
}

/// The lines of `input` as CppNix's `Pos::LinesIterator` yields them, with
/// the byte offset each starts at.
pub fn lines(input: &str) -> Vec<(usize, &str)> {
	let bytes = input.as_bytes();
	let mut out = vec![];
	if bytes.is_empty() {
		return out;
	}
	let mut start = 0;
	loop {
		let eol = bytes[start..]
			.iter()
			.position(|&b| b == b'\r' || b == b'\n')
			.map_or(bytes.len(), |eol| start + eol);
		out.push((start, &input[start..eol]));
		// `bump`: past the end once the input is used up; otherwise skip
		// one `\r`, then one `\n`.
		if eol == bytes.len() {
			break;
		}
		let mut next = eol;
		if bytes[next] == b'\r' {
			next += 1;
		}
		if next < bytes.len() && bytes[next] == b'\n' {
			next += 1;
		}
		start = next;
		if start == bytes.len() {
			out.push((start, ""));
			break;
		}
	}
	out
}

/// The origin CppNix names a source file by.
fn origin(name: &str) -> String {
	match name {
		crate::REPL_LOCATION => STRING_ORIGIN.to_owned(),
		"<stdin>" => STDIN_ORIGIN.to_owned(),
		name => name.to_owned(),
	}
}

/// Whether `name` is a file noxi generates, whose positions CppNix does
/// not show.
pub(crate) fn is_internal_file(name: &str) -> bool {
	name.starts_with("<src-builtins/") || name == "<dummy>"
}

/// The position of the start of `span`, or `None` if it is in generated
/// code.
pub fn position(source: &SourceCode, span: Span) -> Option<Pos> {
	let span = source.original_span(span);
	if source.is_hidden(span) {
		return None;
	}
	let file = source.get_file(span);
	if is_internal_file(file.name()) {
		return None;
	}
	let offset = (span.low() - file.span.low()) as usize;
	let lines = source.line_starts(&file);
	let index = match lines.partition_point(|start| *start <= offset) {
		0 => 0,
		n => n - 1,
	};
	let line_start = lines.get(index).copied().unwrap_or(0);
	Some(Pos {
		origin: origin(file.name()),
		line: index + 1,
		column: source
			.decode_literal(&file, &file.source()[line_start..offset])
			.len()
			+ 1,
		source: Some(file),
	})
}

/// `printCodeLines`: the line before, the line of, and the line after
/// `pos`, with a caret under its column.
fn print_code_lines(out: &mut String, pos: &Pos) {
	let Some(file) = &pos.source else { return };
	let lines = lines(file.source());
	let line = |n: usize| lines.get(n.wrapping_sub(1)).map(|(_, text)| *text);
	if pos.line > 1
		&& let Some(prev) = line(pos.line - 1)
	{
		let _ = write!(out, "\n {:>5}| {prev}", pos.line - 1);
	}
	if let Some(text) = line(pos.line) {
		let _ = write!(out, "\n {:>5}| {text}", pos.line);
		if pos.column > 0 {
			let _ = write!(out, "\n      |{}^", " ".repeat(pos.column));
		}
		if let Some(next) = line(pos.line + 1) {
			let _ = write!(out, "\n {:>5}| {next}", pos.line + 1);
		}
	}
}

/// `printPosMaybe`: `at POS:` and the code around it.
fn print_pos(out: &mut String, indent: &str, pos: Option<&Pos>) -> bool {
	let Some(pos) = pos else { return false };
	let _ = write!(out, "{indent}at {pos}:");
	if pos.source.is_some() {
		print_code_lines(out, pos);
		out.push('\n');
	}
	true
}

struct ResolvedTrace {
	always: bool,
	pos: Option<Pos>,
	hint: String,
}

impl ResolvedTrace {
	fn key(&self) -> (Option<(&str, usize, usize)>, &str) {
		(self.pos.as_ref().map(Pos::key), &self.hint)
	}
}

fn print_trace(out: &mut String, count: &mut usize, trace: &ResolvedTrace) {
	let _ = write!(out, "\n… {}\n", trace.hint);
	if print_pos(out, "  ", trace.pos.as_ref()) {
		*count += 1;
	}
}

/// `printSkippedTracesMaybe`: up to five repeated frames print normally;
/// more fold into one line. The reference passes its seen set by value.
fn print_skipped<'a>(
	out: &mut String,
	count: &mut usize,
	skipped: &mut Vec<&'a ResolvedTrace>,
	_seen: &mut HashSet<(Option<(&'a str, usize, usize)>, &'a str)>,
) {
	if skipped.len() <= 5 {
		for trace in skipped.iter() {
			print_trace(out, count, trace);
		}
	} else {
		let _ = write!(out, "\n({} duplicate frames omitted)\n", skipped.len());
		// The reference clears only its by-value copy of the seen set.
	}
	skipped.clear();
}

/// `indent`: the first line after `first`, the others after `rest`, each
/// with trailing whitespace removed.
fn indent(first: &str, rest: &str, s: &str) -> String {
	let mut res = String::new();
	for (i, line) in s.split('\n').enumerate() {
		if i > 0 {
			res.push('\n');
		}
		let line = format!("{}{line}", if i == 0 { first } else { rest });
		res.push_str(line.trim_end());
	}
	res
}

/// A diagnostic ready for CppNix's `showErrorInfo`.
pub struct ErrorInfo<'a> {
	pub source: &'a SourceCode,
	/// Outermost first.
	pub traces: Vec<Trace>,
	pub message: String,
	pub span: Option<Span>,
	pub suggestions: Vec<String>,
}

impl ErrorInfo<'_> {
	/// The text CppNix's logger prints for this error, without the final
	/// newline.
	pub fn render(&self, show_trace: bool) -> String {
		let traces: Vec<ResolvedTrace> = self
			.traces
			.iter()
			.filter(|trace| show_trace || !trace.show_trace_only)
			.map(|trace| ResolvedTrace {
				always: trace.always,
				pos: trace.span.and_then(|span| position(self.source, span)),
				hint: trace.hint.clone(),
			})
			.collect();
		let prefix = "error: ";
		let mut out = String::new();
		if !traces.is_empty() {
			let mut seen = HashSet::new();
			let mut skipped = vec![];
			let mut count = 0;
			let mut truncate = false;
			for trace in &traces {
				if trace.hint.is_empty() {
					continue;
				}
				if !show_trace && count > 3 {
					truncate = true;
				}
				if !truncate || trace.always {
					if seen.contains(&trace.key()) {
						skipped.push(trace);
						continue;
					}
					seen.insert(trace.key());
					print_skipped(&mut out, &mut count, &mut skipped, &mut seen);
					count += 1;
					print_trace(&mut out, &mut count, trace);
				}
			}
			print_skipped(&mut out, &mut count, &mut skipped, &mut seen);
			if truncate {
				out.push_str(
					"\n(stack trace truncated; use '--show-trace' to show the full, detailed trace)\n",
				);
			}
			out.push('\n');
			out.push_str(prefix);
		}
		out.push_str(&self.message);
		out.push('\n');
		print_pos(
			&mut out,
			"",
			self.span
				.and_then(|span| position(self.source, span))
				.as_ref(),
		);
		if !self.suggestions.is_empty() {
			let _ = writeln!(out, "Did you mean {}?", show_suggestions(&self.suggestions));
		}
		let mut rendered = indent(prefix, &" ".repeat(prefix.len()), out.trim_end());
		if self.message.ends_with('\n') {
			rendered.push('\n');
			rendered.push_str(&" ".repeat(prefix.len()));
		}
		rendered
	}
}

/// `Suggestions::to_string`.
fn show_suggestions(suggestions: &[String]) -> String {
	match suggestions {
		[] => String::new(),
		[one] => one.clone(),
		[first, rest @ ..] => {
			let mut res = format!("one of {first}");
			for (i, s) in rest.iter().enumerate() {
				res.push_str(if i == rest.len() - 1 { " or " } else { ", " });
				res.push_str(s);
			}
			res
		}
	}
}

/// `levenshteinDistance`, over bytes.
fn levenshtein(first: &[u8], second: &[u8]) -> usize {
	let mut v0: Vec<usize> = (0..=second.len()).collect();
	let mut v1 = vec![0; second.len() + 1];
	for (i, a) in first.iter().enumerate() {
		v1[0] = i + 1;
		for (j, b) in second.iter().enumerate() {
			let substitution = if a == b { v0[j] } else { v0[j] + 1 };
			v1[j + 1] = (v0[j + 1] + 1).min(v1[j] + 1).min(substitution);
		}
		std::mem::swap(&mut v0, &mut v1);
	}
	v0[second.len()]
}

/// `Suggestions::bestMatches(candidates, query).trim()`: at most five
/// candidates within an edit distance of two, closest first.
pub fn best_matches<'a>(candidates: impl IntoIterator<Item = &'a str>, query: &str) -> Vec<String> {
	let mut matches: Vec<(usize, &str)> = candidates
		.into_iter()
		.map(|candidate| {
			(
				levenshtein(query.as_bytes(), candidate.as_bytes()),
				candidate,
			)
		})
		.collect();
	matches.sort();
	matches.dedup();
	matches
		.into_iter()
		.take(5)
		.take_while(|(distance, _)| *distance <= 2)
		.map(|(_, candidate)| candidate.to_owned())
		.collect()
}

impl Error {
	/// Render this error as CppNix prints it, with traces of function
	/// calls only if `show_trace` is set, as `--show-trace` does.
	pub fn cppnix_format(&self, show_trace: bool) -> String {
		String::from_utf8_lossy(&self.cppnix_format_bytes(show_trace)).into_owned()
	}

	/// Error streams preserve non-UTF-8 source excerpts like CppNix's logger.
	pub fn cppnix_format_bytes(&self, show_trace: bool) -> Vec<u8> {
		self.source
			.decode_diagnostic(&error_info(self).render(show_trace))
	}
}

/// CppNix's `showType`: the type of `value` with an article.
pub fn show_type(value: &Value) -> String {
	match value {
		Value::Thunk(thunk) if thunk.is_evaluated() => show_type(&thunk.value()),
		Value::Null => "null".into(),
		Value::Bool(_) => "a Boolean".into(),
		Value::Integer(_) => "an integer".into(),
		Value::Float(_) => "a float".into(),
		Value::String(s) if s.has_context() => "a string with context".into(),
		Value::String(_) => "a string".into(),
		Value::Path(_) => "a path".into(),
		Value::Attrs(_) => "a set".into(),
		Value::List(_) => "a list".into(),
		Value::Closure(_) => "a function".into(),
		Value::Builtin(builtin) if builtin.has_partials() => {
			format!(
				"the partially applied built-in function '{}'",
				builtin.name()
			)
		}
		Value::Builtin(builtin) => format!("the built-in function '{}'", builtin.name()),
		Value::Thunk(_) => "a thunk".into(),
		_ => "an error".into(),
	}
}

/// The CppNix name, with an article, of a type noxi names `name`.
fn expected_type(name: &str) -> &str {
	match name {
		"bool" => "a Boolean",
		"int" | "number (either int or float)" => "an integer",
		"float" => "a float",
		"string" | "contextless strings" | "contextful string" => "a string",
		"path" => "a path",
		"set" => "a set",
		"list" => "a list",
		"lambda" => "a function",
		"null" => "null",
		name => name,
	}
}

/// `ValuePrinter` with `errorPrintOptions`.
fn print_value(source: &SourceCode, value: &Value) -> String {
	String::from_utf8_lossy(&ValuePrinter::print(source, PrintOptions::ERROR, value)).into_owned()
}

/// The name CppNix gives a function in messages.
fn lambda_name(lambda: &Lambda) -> String {
	match &lambda.name {
		Some(name) => name.to_string(),
		None => "anonymous lambda".to_owned(),
	}
}

/// CppNix's message for `error`, the position it prints with it, and any
/// suggestions.
fn message(error: &Error) -> (String, Option<Span>, Vec<String>) {
	let (message, span, suggestions) = kind_message(error);
	(
		message,
		span.or(error.show_position.then_some(error.span)),
		suggestions,
	)
}

/// Translate parser tokens to the names used by the pinned Bison grammar.
fn parse_message(
	error: &Error,
	errors: &[rnix::ParseError],
) -> (String, Option<Span>, Vec<String>) {
	use rnix::SyntaxKind::*;
	let file = error.source.get_file(error.span);
	let input = file.source();
	let (mut offset, wanted, extra) = match errors.first() {
		Some(rnix::ParseError::Unexpected(range)) => {
			(u32::from(range.start()) as usize, vec![], false)
		}
		Some(rnix::ParseError::UnexpectedExtra(range)) => {
			(u32::from(range.start()) as usize, vec![], true)
		}
		Some(rnix::ParseError::UnexpectedWanted(_, range, wanted)) => {
			(u32::from(range.start()) as usize, wanted.to_vec(), false)
		}
		Some(rnix::ParseError::UnexpectedEOF | rnix::ParseError::UnexpectedEOFWanted(_)) => {
			(input.len(), vec![], false)
		}
		_ => return (error.kind.to_string(), None, vec![]),
	};
	// The lexer can consume an unterminated string before reporting
	// its terminal error. Bison reports its opening token in an attrpath.
	if let Some(token) = rnix::ast::Root::parse(input)
		.syntax()
		.descendants_with_tokens()
		.filter_map(|x| x.into_token())
		.find(|token| {
			token.kind() == TOKEN_ERROR
				&& token.text().ends_with('/')
				&& token.text().len() > 1
				&& !token.text().chars().any(char::is_whitespace)
		}) {
		let end = u32::from(token.text_range().end()) as u64;
		return (
			"path has a trailing slash".into(),
			Some(file.span.subspan(end, end)),
			vec![],
		);
	}
	let prefix = input[..offset].trim_end();
	let indented_attr = input[..offset]
		.rfind(".''")
		.filter(|start| input[*start + 3..offset].trim().is_empty());
	if let Some(start) = indented_attr {
		offset = start + 1;
	}
	let root = rnix::Root::parse(input).syntax();
	let unclosed_string = root
		.descendants_with_tokens()
		.filter_map(|node| node.into_token())
		.fold(0isize, |depth, token| match token.kind() {
			TOKEN_STRING_START => depth + 1,
			TOKEN_STRING_END => depth - 1,
			_ => depth,
		})
		> 0;
	if unclosed_string && indented_attr.is_none() && input[offset..].trim().len() <= 1 {
		offset = input.len();
	}
	let token = root
		.descendants_with_tokens()
		.filter_map(|node| node.into_token())
		.find(|token| usize::from(token.text_range().end()) > offset);
	let label = token
		.as_ref()
		.map(|token| match token.kind() {
			TOKEN_IDENT => "identifier".into(),
			TOKEN_INTEGER => "integer".into(),
			TOKEN_FLOAT => "floating-point literal".into(),
			TOKEN_PATH_HOME => "'~/…' path".into(),
			TOKEN_PATH_ABS | TOKEN_PATH_REL => "path".into(),
			TOKEN_PATH_SEARCH => "'<…>' path".into(),
			TOKEN_URI => "URI".into(),
			TOKEN_ERROR => "invalid token".into(),
			TOKEN_STRING_START if token.text() == "''" => "start of an indented string".into(),
			_ => format!("'{}'", token.text()),
		})
		.unwrap_or_else(|| "end of file".into());
	let expecting = if offset == input.len() && unclosed_string {
		", expecting '\"'"
	} else if extra {
		", expecting end of file"
	} else if prefix.ends_with('.') || prefix.ends_with('?') || indented_attr.is_some() {
		", expecting identifier or 'or' or '${' or '\"'"
	} else if wanted == [TOKEN_THEN] {
		", expecting 'then'"
	} else if wanted.contains(&TOKEN_ASSIGN) {
		", expecting '.' or '='"
	} else if token
		.as_ref()
		.is_some_and(|token| token.kind() == TOKEN_COLON)
		&& input[..offset].contains('{')
	{
		", expecting '}' or ','"
	} else if token
		.as_ref()
		.is_some_and(|token| matches!(token.kind(), TOKEN_INTEGER | TOKEN_FLOAT))
		&& wanted.contains(&TOKEN_IDENT)
	{
		", expecting 'inherit'"
	} else {
		""
	};
	(
		format!("syntax error, unexpected {label}{expecting}"),
		Some(file.span.subspan(offset as u64, offset as u64)),
		vec![],
	)
}

/// [`message`] by error kind, before an explicit position applies.
fn kind_message(error: &Error) -> (String, Option<Span>, Vec<String>) {
	let at = Some(error.span);
	match &error.kind {
		ErrorKind::ParseErrors(errors) => parse_message(error, errors),
		ErrorKind::ImportParseError { errors, file, .. } => {
			let nested = Error::new(
				ErrorKind::ParseErrors(errors.clone()),
				file.span,
				error.source.clone(),
			);
			parse_message(&nested, errors)
		}
		ErrorKind::DuplicateDefinition {
			key,
			original,
			duplicate,
			dynamic,
		} => (
			format!(
				"{}attribute '{}' already defined at {}",
				if *dynamic { "dynamic " } else { "" },
				key,
				position(&error.source, *original)
					.map(|p| p.to_string())
					.unwrap_or_default()
			),
			Some(*duplicate),
			vec![],
		),
		ErrorKind::Abort(message) => (
			format!("evaluation aborted with the following error message: '{message}'"),
			None,
			vec![],
		),
		ErrorKind::CatchableError(cek) => match cek.untraced() {
			CatchableErrorKind::Throw(message) => {
				(message.to_str_lossy().into_owned(), None, vec![])
			}
			CatchableErrorKind::NixPathResolution(message) => (message.to_string(), at, vec![]),
			kind => (kind.to_string(), None, vec![]),
		},
		ErrorKind::TypeError {
			expected,
			actual,
			value,
		} => {
			let message = match value {
				Some(value) => format!(
					"expected {} but found {}: {}",
					expected_type(expected),
					show_type(value),
					print_value(&error.source, value)
				),
				None => format!(
					"expected {} but found {}",
					expected_type(expected),
					expected_type(actual)
				),
			};
			(message, None, vec![])
		}
		ErrorKind::UnknownStaticVariable(name) | ErrorKind::UnknownDynamicVariable(name) => {
			(format!("undefined variable '{name}'"), at, vec![])
		}
		ErrorKind::AttributeNotFound { name, suggestions } => (
			format!("attribute '{name}' missing"),
			None,
			suggestions.clone(),
		),
		ErrorKind::Incomparable { values, .. } => {
			let (lhs, rhs) = &**values;
			let left = show_type(lhs);
			let right = show_type(rhs);
			let values = format!(
				"{} and {}",
				print_value(&error.source, lhs),
				print_value(&error.source, rhs)
			);
			let detail = if lhs.type_of() == rhs.type_of() {
				format!("values of that type are incomparable (values are {values})")
			} else {
				format!("values are {values}")
			};
			(
				format!("cannot compare {left} with {right}; {detail}"),
				None,
				vec![],
			)
		}
		ErrorKind::MissingArgument { arg, lambda } => (
			format!(
				"function '{}' called without required argument '{arg}'",
				lambda_name(lambda),
				arg = arg.to_str_lossy()
			),
			lambda.span,
			vec![],
		),
		ErrorKind::UnexpectedArgumentFormals {
			arg,
			lambda: Some(lambda),
			..
		} => {
			let formals = lambda
				.formals
				.as_ref()
				.map(|formals| {
					formals
						.arguments
						.keys()
						.map(|name| name.to_str_lossy().into_owned())
						.collect::<Vec<_>>()
				})
				.unwrap_or_default();
			let arg = arg.to_str_lossy();
			(
				format!(
					"function '{}' called with unexpected argument '{arg}'",
					lambda_name(lambda)
				),
				lambda.span,
				best_matches(formals.iter().map(String::as_str), &arg),
			)
		}
		ErrorKind::IntegerOverflow { .. } | ErrorKind::DivisionByZero => {
			(error.kind.to_string(), None, vec![])
		}
		ErrorKind::InfiniteRecursion { .. } => (error.kind.to_string(), at, vec![]),
		ErrorKind::NotCallable(_, Some(value)) => (
			format!(
				"attempt to call something which is not a function but {}: {}",
				show_type(value),
				print_value(&error.source, value)
			),
			None,
			vec![],
		),
		ErrorKind::IO {
			path: Some(path),
			error: io,
		} if io.kind() == std::io::ErrorKind::NotFound => (
			format!("path '{}' does not exist", path.display()),
			None,
			vec![],
		),
		ErrorKind::IO {
			path: Some(path),
			error: io,
		} if io.kind() == std::io::ErrorKind::NotADirectory => (
			format!("'{}' is not a directory", path.display()),
			None,
			vec![],
		),
		ErrorKind::NotCoercibleToString {
			value: Some(value), ..
		} => (
			format!(
				"cannot coerce {} to a string: {}",
				show_type(value),
				print_value(&error.source, value)
			),
			None,
			vec![],
		),
		ErrorKind::VariableAlreadyDefined(_, name) => (
			format!("duplicate formal function argument '{name}'"),
			at,
			vec![],
		),
		ErrorKind::DynamicKeyInScope(scope) => (
			format!(
				"dynamic attributes not allowed in {}",
				match *scope {
					"let-expression" => "let",
					scope => scope,
				}
			),
			at,
			vec![],
		),
		ErrorKind::NotAnAbsolutePath(path) => (
			format!(
				"string '{}' doesn't represent an absolute path",
				path.display()
			),
			None,
			vec![],
		),
		ErrorKind::UnknownHashType(name) => (
			format!(
				"unknown hash algorithm '{name}', expect 'blake3', 'md5', 'sha1', 'sha256', or 'sha512'"
			),
			None,
			vec![],
		),
		ErrorKind::TailEmptyList => (
			"'builtins.tail' called on an empty list".into(),
			None,
			vec![],
		),

		ErrorKind::FromTomlError(message) if message.starts_with("while parsing TOML:") => {
			(message.clone(), None, vec![])
		}
		ErrorKind::At { message, span } => (message.clone(), Some(*span), vec![]),
		ErrorKind::EvalError(message) => (message.clone(), None, vec![]),
		kind => (kind.to_string(), None, vec![]),
	}
}

/// The diagnostic CppNix prints for `error`.
fn error_info(error: &Error) -> ErrorInfo<'_> {
	let inner = error.innermost();
	// CppNix stops at the first error of a file that does not compile.
	let inner = match &inner.kind {
		ErrorKind::ImportCompilerError { errors, .. } if !errors.is_empty() => {
			errors[0].innermost()
		}
		_ => inner,
	};
	let (mut message, mut span, suggestions) = message(inner);
	let mut traces = error.traces.clone();
	if let ErrorKind::CatchableError(cek) = &inner.kind
		&& let Some(info) = cek.trace()
	{
		traces = info.traces.borrow().clone();
		span = info.span;
		if let Some(text) = &info.message {
			message = text.clone();
		}
	}
	traces.extend(inner.contexts.iter().cloned());
	ErrorInfo {
		source: &error.source,
		traces,
		message,
		span,
		suggestions,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn splits_lines_like_cppnix() {
		assert_eq!(lines(""), vec![]);
		assert_eq!(lines("a"), vec![(0, "a")]);
		assert_eq!(lines("a\n"), vec![(0, "a"), (2, "")]);
		assert_eq!(
			lines("a\r\nb\rc\n\nd"),
			vec![(0, "a"), (3, "b"), (5, "c"), (7, ""), (8, "d")]
		);
	}

	#[test]
	fn suggests_close_names() {
		assert_eq!(best_matches(["x", "z"], "y"), vec!["x", "z"]);
		assert_eq!(best_matches(["foo", "barbaz"], "fo"), vec!["foo"]);
	}

	#[test]
	fn renders_traces_and_positions() {
		let source = SourceCode::default();
		let file = source.add_file(
			"/pwd/lang/eval-fail-add-1.nix".into(),
			"builtins.add \"foo\" 1\n".into(),
		);
		let info = ErrorInfo {
			source: &source,
			traces: vec![
				Trace::new(Some(file.span), "while calling the 'add' builtin"),
				Trace::new(None, "while evaluating the first argument of the addition"),
			],
			message: "expected an integer but found a string: \"foo\"".into(),
			span: None,
			suggestions: vec![],
		};
		assert_eq!(
			info.render(true) + "\n",
			std::fs::read_to_string(concat!(
				env!("CARGO_MANIFEST_DIR"),
				"/../../../tests/nix/tests/functional/lang/eval-fail-add-1.err.exp"
			))
			.unwrap()
		);
	}

	#[test]
	fn renders_an_error_position_and_suggestions() {
		let source = SourceCode::default();
		let code = "({ x, z }: x + z) {\n  x = \"foo\";\n  y = \"bla\";\n  z = \"bar\";\n}\n";
		let file = source.add_file("/pwd/lang/eval-fail-undeclared-arg.nix".into(), code.into());
		let at = |offset: u64| file.span.subspan(offset, offset);
		let info = ErrorInfo {
			source: &source,
			traces: vec![Trace::new(Some(at(0)), "from call site")],
			message: "function 'anonymous lambda' called with unexpected argument 'y'".into(),
			span: Some(at(1)),
			suggestions: best_matches(["x", "z"], "y"),
		};
		assert_eq!(
			info.render(true) + "\n",
			std::fs::read_to_string(concat!(
				env!("CARGO_MANIFEST_DIR"),
				"/../../../tests/nix/tests/functional/lang/eval-fail-undeclared-arg.err.exp"
			))
			.unwrap()
		);
	}

	#[test]
	fn folds_repeated_frames() {
		let source = SourceCode::default();
		let traces = std::iter::repeat_n(Trace::new(None, "while calling 'f'"), 8).collect();
		let info = ErrorInfo {
			source: &source,
			traces,
			message: "boom".into(),
			span: None,
			suggestions: vec![],
		};
		assert_eq!(
			info.render(true),
			"error:\n       … while calling 'f'\n\n       (7 duplicate frames omitted)\n\n       error: boom"
		);
	}
}
