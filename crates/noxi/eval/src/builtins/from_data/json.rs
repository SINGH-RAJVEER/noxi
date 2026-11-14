//! JSON lexical compatibility with the pinned nlohmann SAX parser.
use crate::{AddContext, ErrorKind, Value};

pub(crate) fn parse_json(input: &[u8]) -> Result<Value, ErrorKind> {
	// serde_json treats an integral negative zero as a float. nlohmann
	// invokes its signed-integer callback for that token, including in
	// nested arrays/objects. Keep byte offsets intact for later errors.
	let mut normalized = input.to_vec();
	let mut quoted = false;
	let mut escaped = false;
	for i in 0..input.len() {
		let byte = input[i];
		if quoted {
			if escaped {
				escaped = false;
			} else if byte == b'\\' {
				escaped = true;
			} else if byte == b'"' {
				quoted = false;
			}
		} else if byte == b'"' {
			quoted = true;
		} else if byte == b'-'
			&& input.get(i + 1) == Some(&b'0')
			&& input
				.get(i + 2)
				.is_none_or(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}'))
		{
			normalized[i] = b' ';
		}
	}
	let json = serde_json::from_slice(&normalized).map_err(|error| {
		let message = if error.to_string().starts_with("recursion limit") {
			None
		} else {
			Parser::new(input).diagnostic()
		}
		.unwrap_or_else(|| {
			format!(
				"[json.exception.parse_error.101] parse error at line {}, column {}: {}",
				error.line(),
				error.column(),
				error
			)
		});
		ErrorKind::EvalError(message).context("decoding a JSON string")
	})?;
	super::json_to_value(json)
}

#[derive(Clone)]
enum Token {
	Symbol(u8),
	String,
	Number,
	Literal(&'static str),
	End,
}

struct Parser<'a> {
	input: &'a [u8],
	pos: usize,
}

impl<'a> Parser<'a> {
	fn new(input: &'a [u8]) -> Self {
		Self { input, pos: 0 }
	}

	fn diagnostic(mut self) -> Option<String> {
		if self.input.is_empty() {
			return Some(self.error("attempting to parse an empty input; check that your input string or stream contains the expected JSON"));
		}
		let result = self.value().and_then(|()| {
			let token = self.token()?;
			if matches!(token, Token::End) {
				Ok(())
			} else {
				Err(self.unexpected(&token, "value", "end of input"))
			}
		});
		result.err()
	}

	fn error(&self, message: &str) -> String {
		let at = self.pos.min(self.input.len());
		let line = self.input[..at].iter().filter(|&&b| b == b'\n').count() + 1;
		let line_start = self.input[..at]
			.iter()
			.rposition(|&b| b == b'\n')
			.map_or(0, |i| i + 1);
		let column = self.pos.saturating_sub(line_start).max(1);
		format!(
			"[json.exception.parse_error.101] parse error at line {line}, column {column}: {message}"
		)
	}

	fn unexpected(&self, token: &Token, context: &str, expected: &str) -> String {
		let token = match token {
			Token::Symbol(b) => format!("'{}'", char::from(*b)),
			Token::String => "string literal".into(),
			Token::Number => "number literal".into(),
			Token::Literal(name) => format!("{name} literal"),
			Token::End => "end of input".into(),
		};
		self.error(&format!(
			"syntax error while parsing {context} - unexpected {token}; expected {expected}"
		))
	}

	fn value(&mut self) -> Result<(), String> {
		let token = self.token()?;
		self.value_token(token)
	}

	fn value_token(&mut self, token: Token) -> Result<(), String> {
		match token {
			Token::String | Token::Number | Token::Literal(_) => Ok(()),
			Token::Symbol(b'[') => {
				let token = self.token()?;
				if matches!(token, Token::Symbol(b']')) {
					return Ok(());
				}
				self.value_token(token)?;
				loop {
					let token = self.token()?;
					match token {
						Token::Symbol(b']') => return Ok(()),
						Token::Symbol(b',') => self.value()?,
						_ => return Err(self.unexpected(&token, "array", "']'")),
					}
				}
			}
			Token::Symbol(b'{') => {
				let mut token = self.token()?;
				if matches!(token, Token::Symbol(b'}')) {
					return Ok(());
				}
				loop {
					if !matches!(token, Token::String) {
						return Err(self.unexpected(&token, "object key", "string literal"));
					}
					let colon = self.token()?;
					if !matches!(colon, Token::Symbol(b':')) {
						return Err(self.unexpected(&colon, "object separator", "':'"));
					}
					self.value()?;
					token = self.token()?;
					match token {
						Token::Symbol(b'}') => return Ok(()),
						Token::Symbol(b',') => token = self.token()?,
						_ => return Err(self.unexpected(&token, "object", "'}'")),
					}
				}
			}
			_ => Err(self.unexpected(&token, "value", "'[', '{', or a literal")),
		}
	}

	fn token(&mut self) -> Result<Token, String> {
		while self
			.input
			.get(self.pos)
			.is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
		{
			self.pos += 1;
		}
		let Some(&b) = self.input.get(self.pos) else {
			self.pos += 1;
			return Ok(Token::End);
		};
		let start = self.pos;
		self.pos += 1;
		match b {
			b'{' | b'}' | b'[' | b']' | b',' | b':' => Ok(Token::Symbol(b)),
			b'"' => self.string(start),
			b't' | b'f' | b'n' => {
				let literal = match b {
					b't' => "true",
					b'f' => "false",
					_ => "null",
				};
				for &expected in &literal.as_bytes()[1..] {
					let actual = self.input.get(self.pos).copied();
					self.pos += 1;
					if actual != Some(expected) {
						return Err(self.invalid(start, "invalid literal"));
					}
				}
				Ok(Token::Literal(literal))
			}
			b'-' | b'0'..=b'9' => {
				if b == b'-' {
					if !self.input.get(self.pos).is_some_and(u8::is_ascii_digit) {
						return Err(self.invalid(start, "invalid number; expected digit after '-'"));
					}
					self.pos += 1;
				}
				if self.input[self.pos - 1] != b'0' {
					while self.input.get(self.pos).is_some_and(u8::is_ascii_digit) {
						self.pos += 1;
					}
				}
				if self.input.get(self.pos) == Some(&b'.') {
					self.pos += 1;
					if !self.input.get(self.pos).is_some_and(u8::is_ascii_digit) {
						return Err(self.invalid(start, "invalid number; expected digit after '.'"));
					}
					while self.input.get(self.pos).is_some_and(u8::is_ascii_digit) {
						self.pos += 1;
					}
				}
				if matches!(self.input.get(self.pos), Some(b'e' | b'E')) {
					self.pos += 1;
					if matches!(self.input.get(self.pos), Some(b'+' | b'-')) {
						self.pos += 1;
					}
					if !self.input.get(self.pos).is_some_and(u8::is_ascii_digit) {
						return Err(self
							.invalid(start, "invalid number; expected digit after exponent sign"));
					}
					while self.input.get(self.pos).is_some_and(u8::is_ascii_digit) {
						self.pos += 1;
					}
				}
				let number = String::from_utf8_lossy(&self.input[start..self.pos]);
				if number.parse::<f64>().is_ok_and(|n| n.is_infinite()) {
					return Err(format!(
						"[json.exception.out_of_range.406] number overflow parsing '{number}'"
					));
				}
				Ok(Token::Number)
			}
			_ => Err(self.invalid(start, "invalid literal")),
		}
	}

	fn invalid(&self, start: usize, message: &str) -> String {
		let read = String::from_utf8_lossy(&self.input[start..self.pos.min(self.input.len())]);
		self.error(&format!(
			"syntax error while parsing value - {message}; last read: '{read}'"
		))
	}

	fn string(&mut self, start: usize) -> Result<Token, String> {
		while let Some(&b) = self.input.get(self.pos) {
			self.pos += 1;
			match b {
				b'"' => return Ok(Token::String),
				b'\\' => {
					let Some(&escaped) = self.input.get(self.pos) else {
						break;
					};
					self.pos += 1;
					if escaped == b'u' {
						let code = self.hex4(start)?;
						if (0xd800..=0xdbff).contains(&code) {
							let paired = self.input.get(self.pos..self.pos + 2) == Some(b"\\u");
							self.pos += if paired { 2 } else { 1 };
							if !paired {
								return Err(self.invalid(
									start,
									"invalid string: surrogate U+D800..U+DBFF must be followed by U+DC00..U+DFFF",
								));
							}
							let low = self.hex4(start)?;
							if !(0xdc00..=0xdfff).contains(&low) {
								return Err(self.invalid(
									start,
									"invalid string: surrogate U+D800..U+DBFF must be followed by U+DC00..U+DFFF",
								));
							}
						} else if (0xdc00..=0xdfff).contains(&code) {
							return Err(self.invalid(
								start,
								"invalid string: surrogate U+DC00..U+DFFF must follow U+D800..U+DBFF",
							));
						}
					} else if !b"\"\\/bfnrt".contains(&escaped) {
						return Err(self.invalid(
							start,
							"invalid string: forbidden character after backslash",
						));
					}
				}
				_ => {}
			}
		}
		self.pos += 1;
		Err(self.invalid(start, "invalid string: missing closing quote"))
	}

	fn hex4(&mut self, start: usize) -> Result<u16, String> {
		let mut result = 0;
		for _ in 0..4 {
			let byte = self.input.get(self.pos).copied();
			self.pos += 1;
			let Some(digit) = byte.and_then(|b| char::from(b).to_digit(16)) else {
				return Err(self.invalid(
					start,
					"invalid string: '\\u' must be followed by 4 hex digits",
				));
			};
			result = result * 16 + digit as u16;
		}
		Ok(result)
	}
}
