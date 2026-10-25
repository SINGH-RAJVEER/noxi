//! Adapt CppNix's byte-oriented `std::regex::extended` grammar to Rust regexes.
//! Capture priority follows libstdc++'s ordered alternatives, not libc regexec.

use regex::bytes::{Regex, RegexBuilder};

pub(super) fn compile(pattern: &str) -> Result<Regex, ()> {
	let mut out = String::new();
	let mut chars = pattern.chars().peekable();
	let mut groups = Vec::new();
	let mut atom = None;
	let mut repeated = false;
	while let Some(c) = chars.next() {
		let start = out.len();
		match c {
			'\\' => {
				let escaped = chars.next().ok_or(())?;
				// libstdc++ accepts escaped ERE metacharacters only. In particular,
				// \d, \w, \n and backreferences are not extended-regex escapes.
				if !".^$|()[]*+?{\\".contains(escaped) || escaped == ']' {
					return Err(());
				}
				out.push('\\');
				out.push(escaped);
				atom = Some(start);
				repeated = false;
			}
			'[' => {
				out.push('[');
				if chars.peek() == Some(&'^') {
					out.push(chars.next().unwrap());
				}
				if chars.peek() == Some(&']') {
					chars.next();
					out.push_str(r"\x5d");
				}
				let mut closed = false;
				while let Some(c) = chars.next() {
					match c {
						']' => {
							out.push(']');
							closed = true;
							break;
						}
						'\\' => out.push_str(r"\x5c"),
						'&' => out.push_str(r"\x26"),
						'~' => out.push_str(r"\x7e"),
						'[' if matches!(chars.peek(), Some(':' | '.' | '=')) => {
							let delimiter = chars.next().unwrap();
							let mut name = String::new();
							loop {
								let c = chars.next().ok_or(())?;
								if c == delimiter && chars.peek() == Some(&']') {
									chars.next();
									break;
								}
								name.push(c);
							}
							if delimiter == ':' {
								match name.as_str() {
									"d" => out.push_str("[:digit:]"),
									"s" => out.push_str("[:space:]"),
									"w" => out.push_str("[:alnum:]_"),
									"alnum" | "alpha" | "blank" | "cntrl" | "digit" | "graph"
									| "lower" | "print" | "punct" | "space" | "upper"
									| "xdigit" => out.push_str(&format!("[:{name}:]")),
									_ => return Err(()),
								}
							} else if name.len() == 1 {
								out.push_str(&format!(r"\x{:02x}", name.as_bytes()[0]));
							} else {
								let byte = match name.as_str() {
									"NUL" => 0,
									"tab" => b'\t',
									"newline" => b'\n',
									"vertical-tab" => 11,
									"form-feed" => 12,
									"carriage-return" => b'\r',
									"space" => b' ',
									_ => return Err(()),
								};
								out.push_str(&format!(r"\x{byte:02x}"));
							}
						}
						'[' => out.push_str(r"\x5b"),
						'-' if chars.peek() == Some(&'-') => out.push_str(r"\x2d"),
						c => out.push(c),
					}
				}
				if !closed {
					return Err(());
				}
				atom = Some(start);
				repeated = false;
			}
			'(' => {
				groups.push(start);
				out.push('(');
				atom = None;
				repeated = false;
			}
			')' => {
				atom = Some(groups.pop().ok_or(())?);
				out.push(')');
				repeated = false;
			}
			'*' | '+' | '?' | '{' => {
				let start = atom.ok_or(())?;
				let quantifier = if c == '{' {
					let mut quantifier = String::from("{");
					let mut lower = 0;
					while chars.peek().is_some_and(char::is_ascii_digit) {
						lower += 1;
						quantifier.push(chars.next().unwrap());
					}
					if lower == 0 {
						return Err(());
					}
					if chars.peek() == Some(&',') {
						quantifier.push(chars.next().unwrap());
						while chars.peek().is_some_and(char::is_ascii_digit) {
							quantifier.push(chars.next().unwrap());
						}
					}
					if chars.next() != Some('}') {
						return Err(());
					}
					quantifier.push('}');
					quantifier
				} else {
					c.to_string()
				};
				// In extended grammar repeated quantifiers nest greedily. They do
				// not switch to Rust's lazy/possessive quantifier syntax.
				if repeated {
					out.insert_str(start, "(?:");
					out.push(')');
				}
				out.push_str(&quantifier);
				repeated = true;
			}
			'|' | '^' | '$' => {
				out.push(c);
				atom = None;
				repeated = false;
			}
			c => {
				out.push(c);
				atom = Some(start);
				repeated = false;
			}
		}
	}
	if !groups.is_empty() {
		return Err(());
	}
	RegexBuilder::new(&format!(r"\A(?:{out})\z"))
		.unicode(false)
		.dot_matches_new_line(true)
		.build()
		.map_err(|_| ())
}
