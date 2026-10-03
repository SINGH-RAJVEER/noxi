//! Carriage returns as CppNix's lexer treats them.
//!
//! CppNix ends a `#` comment at a carriage return, counts a lone carriage
//! return as a line break, and reads one in a `"` string as a line feed.
//! `rnix` knows only line feeds, so a lone carriage return outside an
//! indented string is replaced by a line feed before parsing. The
//! replacement keeps every byte offset, so spans stay valid. Escaped
//! carriage returns and those in `''` strings are kept, as CppNix keeps them.

use std::borrow::Cow;

enum Context {
	/// Code, with the depth of `{` opened in it. The outermost code and the
	/// inside of each interpolation are separate contexts.
	Code(usize),
	Str,
	IndStr,
}

pub(crate) fn normalize(code: &str) -> Cow<'_, str> {
	if !code.contains('\r') {
		return Cow::Borrowed(code);
	}
	let mut bytes = code.as_bytes().to_vec();
	let mut stack = vec![Context::Code(0)];
	let mut i = 0;
	let lone_cr = |bytes: &[u8], i: usize| bytes[i] == b'\r' && bytes.get(i + 1) != Some(&b'\n');

	while i < bytes.len() {
		match stack
			.last_mut()
			.expect("the outermost context is never left")
		{
			Context::Code(depth) => match bytes[i] {
				b'#' => {
					while i < bytes.len() && bytes[i] != b'\n' {
						if lone_cr(&bytes, i) {
							bytes[i] = b'\n';
							break;
						}
						i += 1;
					}
				}
				b'/' if bytes.get(i + 1) == Some(&b'*') => {
					i += 2;
					while i < bytes.len() && !bytes[i..].starts_with(b"*/") {
						if lone_cr(&bytes, i) {
							bytes[i] = b'\n';
						}
						i += 1;
					}
					i += 1;
				}
				b'"' => stack.push(Context::Str),
				b'\'' if bytes.get(i + 1) == Some(&b'\'') => {
					i += 1;
					stack.push(Context::IndStr);
				}
				b'{' => *depth += 1,
				b'}' if *depth > 0 => *depth -= 1,
				// The end of an interpolation.
				b'}' if stack.len() > 1 => {
					stack.pop();
				}
				b'\r' if lone_cr(&bytes, i) => bytes[i] = b'\n',
				_ => {}
			},
			Context::Str => match bytes[i] {
				b'\\' => i += 1,
				b'$' if bytes.get(i + 1) == Some(&b'$') => i += 1,
				b'$' if bytes.get(i + 1) == Some(&b'{') => {
					i += 1;
					stack.push(Context::Code(0));
				}
				b'"' => {
					stack.pop();
				}
				b'\r' if lone_cr(&bytes, i) => bytes[i] = b'\n',
				_ => {}
			},
			Context::IndStr => {
				if bytes[i..].starts_with(b"''$") || bytes[i..].starts_with(b"'''") {
					i += 2;
				} else if bytes[i..].starts_with(b"''\\") {
					i += 3;
				} else if bytes[i..].starts_with(b"''") {
					i += 1;
					stack.pop();
				} else if bytes[i..].starts_with(b"$$") {
					i += 1;
				} else if bytes[i..].starts_with(b"${") {
					i += 1;
					stack.push(Context::Code(0));
				}
			}
		}
		i += 1;
	}

	Cow::Owned(String::from_utf8(bytes).expect("only ASCII bytes are replaced"))
}

#[cfg(test)]
mod tests {
	use super::normalize;

	#[test]
	fn replaces_lone_carriage_returns_outside_indented_strings() {
		let cases = [
			("a # c\rb", "a # c\nb"),
			("a # c\r\nb", "a # c\r\nb"),
			("1 +\r\r x", "1 +\n\n x"),
			("\"a\rb\"", "\"a\nb\""),
			("\"a\\\rb\"", "\"a\\\rb\""),
			("\"${ \"x\r\" }\r\"", "\"${ \"x\n\" }\n\""),
			("''a\r''", "''a\r''"),
			("''${ \"x\r\" }\r'' # c\r", "''${ \"x\n\" }\r'' # c\n"),
			("''''\\\r'' \r", "''''\\\r'' \n"),
			("/* a\rb */\r", "/* a\nb */\n"),
			("{ a = \"$${\r\"; }\r", "{ a = \"$${\n\"; }\n"),
			("\"a$\" # c\r", "\"a$\" # c\n"),
		];
		for (input, expected) in cases {
			assert_eq!(normalize(input), expected, "{input:?}");
		}
	}
}
