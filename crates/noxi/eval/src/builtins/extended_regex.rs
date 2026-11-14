//! Adapt CppNix's byte-oriented `std::regex::extended` grammar to Rust regexes.
//! Capture priority follows libstdc++'s ordered alternatives, not libc regexec.

use regex::bytes::{Regex, RegexBuilder};
use regex_automata::{Anchored, Input, MatchKind, nfa::thompson::pikevm::PikeVM, util::syntax};

fn translate(pattern: &[u8]) -> Result<String, ()> {
	let mut out = String::new();
	let mut chars = pattern.iter().copied().map(char::from).peekable();
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
							} else {
								let byte = collating_byte(&name).ok_or(())?;
								out.push_str(&format!(r"\x{byte:02x}"));
							}
						}
						'[' => out.push_str(r"\x5b"),
						'-' if chars.peek() == Some(&'-') => out.push_str(r"\x2d"),
						c if !c.is_ascii() => {
							out.push_str(&format!(r"\x{:02x}", c as u8));
						}
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
				if c.is_ascii() {
					out.push(c);
					atom = Some(start);
				} else {
					// Each literal is one byte, including UTF-8 and arbitrary
					// bytes read from a file or produced by substring.
					atom = Some(out.len());
					out.push_str(&format!(r"\x{:02x}", c as u8));
				}
				repeated = false;
			}
		}
	}
	if !groups.is_empty() {
		return Err(());
	}
	Ok(out)
}

pub(super) fn compile(pattern: &[u8]) -> Result<Regex, ()> {
	let out = translate(pattern)?;
	RegexBuilder::new(&format!(r"\A(?:{out})\z"))
		.unicode(false)
		.dot_matches_new_line(true)
		.build()
		.map_err(|_| ())
}

/// libstdc++'s classic-locale symbolic names, indexed by their ASCII value.
/// Punctuation and digits use their symbolic names, not their literal spelling.
fn collating_byte(name: &str) -> Option<u8> {
	const NAMES: &str = "NUL|SOH|STX|ETX|EOT|ENQ|ACK|alert|backspace|tab|newline|vertical-tab|form-feed|carriage-return|SO|SI|DLE|DC1|DC2|DC3|DC4|NAK|SYN|ETB|CAN|EM|SUB|ESC|IS4|IS3|IS2|IS1|space|exclamation-mark|quotation-mark|number-sign|dollar-sign|percent-sign|ampersand|apostrophe|left-parenthesis|right-parenthesis|asterisk|plus-sign|comma|hyphen|period|slash|zero|one|two|three|four|five|six|seven|eight|nine|colon|semicolon|less-than-sign|equals-sign|greater-than-sign|question-mark|commercial-at|A|B|C|D|E|F|G|H|I|J|K|L|M|N|O|P|Q|R|S|T|U|V|W|X|Y|Z|left-square-bracket|backslash|right-square-bracket|circumflex|underscore|grave-accent|a|b|c|d|e|f|g|h|i|j|k|l|m|n|o|p|q|r|s|t|u|v|w|x|y|z|left-curly-bracket|vertical-line|right-curly-bracket|tilde|DEL";
	NAMES
		.split('|')
		.position(|candidate| candidate == name)
		.map(|i| i as u8)
}

/// Search chooses the earliest start and longest whole match, as the
/// reference's extended-grammar regex_iterator does. Captures at that endpoint
/// retain the grammar's ordered-alternative priority.
pub(super) struct SearchRegex {
	pub search: Regex,
	pub longest: PikeVM,
}

pub(super) fn compile_search(pattern: &[u8]) -> Result<SearchRegex, ()> {
	let translated = translate(pattern)?;
	let search = RegexBuilder::new(&translated)
		.unicode(false)
		.dot_matches_new_line(true)
		.build()
		.map_err(|_| ())?;
	let longest = PikeVM::builder()
		.configure(PikeVM::config().match_kind(MatchKind::All))
		.thompson(regex_automata::nfa::thompson::Config::new().utf8(false))
		.syntax(
			syntax::Config::new()
				.unicode(false)
				.utf8(false)
				.dot_matches_new_line(true),
		)
		.build(&translated)
		.map_err(|_| ())?;
	Ok(SearchRegex { search, longest })
}

impl SearchRegex {
	pub fn captures_at(
		&self,
		text: &[u8],
		start: usize,
		cache: &mut regex_automata::nfa::thompson::pikevm::Cache,
		captures: &mut regex_automata::util::captures::Captures,
	) -> bool {
		let Some(found) = self.search.find_at(text, start) else {
			return false;
		};
		let input = Input::new(text)
			.range(found.start()..)
			.anchored(Anchored::Yes);
		self.longest.captures(cache, input, captures);
		captures.is_match()
	}
}
