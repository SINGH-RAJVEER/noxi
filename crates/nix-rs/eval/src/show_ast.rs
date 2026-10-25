//! CppNix 2.35.2's `Expr::show`, including parser desugaring.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use rnix::ast::{self, AstToken, HasEntry, InterpolPart, PathContent};
use rowan::ast::AstNode;

use crate::compiler::normalized_str_parts;

fn string(value: &str) -> String {
	let mut out = String::from("\"");
	let mut chars = value.chars().peekable();
	while let Some(c) = chars.next() {
		match c {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			'$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
			c => out.push(c),
		}
	}
	out.push('"');
	out
}

fn identifier(value: &str) -> String {
	if value.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
		&& value
			.chars()
			.all(|c| c.is_ascii_alphanumeric() || "_'-".contains(c))
		&& !matches!(
			value,
			"if" | "then" | "else" | "assert" | "with" | "let" | "in" | "rec" | "inherit"
		) {
		value.to_owned()
	} else {
		string(value)
	}
}

/// One `IND_STR` token the lexer produces and whether it takes part in
/// indentation (escapes such as `''$` do not).
struct IndToken {
	text: String,
	has_indentation: bool,
}

/// Splits the literal text of an indented string into the lexer's tokens, so
/// escapes such as `''$` and `'''` stay separate strings as in CppNix.
fn ind_tokens(raw: &str) -> Vec<IndToken> {
	let chars: Vec<char> = raw.chars().collect();
	let mut tokens = Vec::new();
	let mut push = |text: String, has_indentation| {
		tokens.push(IndToken {
			text,
			has_indentation,
		})
	};
	let mut i = 0;
	while i < chars.len() {
		let rest = &chars[i..];
		if rest.starts_with(&['\'', '\'', '$']) {
			push("$".to_owned(), false);
			i += 3;
		} else if rest.starts_with(&['\'', '\'', '\'']) {
			push("''".to_owned(), false);
			i += 3;
		} else if rest.starts_with(&['\'', '\'', '\\']) && rest.len() > 3 {
			let escaped = match rest[3] {
				'n' => '\n',
				'r' => '\r',
				't' => '\t',
				c => c,
			};
			push(escaped.to_string(), false);
			i += 4;
		} else {
			let start = i;
			while i < chars.len() {
				let next = chars.get(i + 1).copied();
				match chars[i] {
					'$' if next.is_some_and(|n| n != '{' && n != '\'') => i += 2,
					'\'' if next.is_some_and(|n| n != '\'' && n != '$') => i += 2,
					'$' | '\'' => break,
					_ => i += 1,
				}
			}
			if i > start {
				push(chars[start..i].iter().collect(), true);
			} else if chars[i] == '$' {
				push("$".to_owned(), false);
				i += 1;
			} else {
				push("'".to_owned(), false);
				i += 1;
			}
		}
	}
	tokens
}

/// A piece of an indented string: a lexer token or an interpolation, with
/// whether the interpolated expression is itself a string literal.
enum IndPart {
	Token(IndToken),
	Code { text: String, is_string: bool },
}

/// `ParserState::stripIndentation`: the parts that remain, as text pieces
/// (`Err` holds interpolated code).
fn strip_indentation(es: Vec<IndPart>) -> Vec<IndPart> {
	let mut at_start_of_line = true;
	let mut min_indent = 1_000_000usize;
	let mut cur_indent = 0usize;
	for part in &es {
		let IndPart::Token(token) = part else {
			if at_start_of_line {
				at_start_of_line = false;
				min_indent = min_indent.min(cur_indent);
			}
			continue;
		};
		if !token.has_indentation {
			if at_start_of_line {
				at_start_of_line = false;
				min_indent = min_indent.min(cur_indent);
			}
			continue;
		}
		for byte in token.text.bytes() {
			if at_start_of_line {
				match byte {
					b' ' => cur_indent += 1,
					b'\n' => cur_indent = 0,
					_ => {
						at_start_of_line = false;
						min_indent = min_indent.min(cur_indent);
					}
				}
			} else if byte == b'\n' {
				at_start_of_line = true;
				cur_indent = 0;
			}
		}
	}

	let mut out = Vec::new();
	at_start_of_line = true;
	let mut cur_dropped = 0usize;
	let mut remaining = es.len();
	let mut first_part = true;
	for part in es {
		match part {
			IndPart::Code { text, is_string } => {
				at_start_of_line = false;
				cur_dropped = 0;
				out.push(IndPart::Code { text, is_string });
				first_part = false;
			}
			IndPart::Token(token) => {
				let mut kept = Vec::new();
				for byte in token.text.bytes() {
					if at_start_of_line {
						match byte {
							b' ' => {
								if cur_dropped >= min_indent {
									kept.push(byte);
								}
								cur_dropped += 1;
							}
							b'\n' => {
								cur_dropped = 0;
								kept.push(byte);
							}
							_ => {
								at_start_of_line = false;
								cur_dropped = 0;
								kept.push(byte);
							}
						}
					} else {
						kept.push(byte);
						if byte == b'\n' {
							at_start_of_line = true;
						}
					}
				}
				if first_part && kept.first() == Some(&b'\n') {
					kept.remove(0);
				}
				first_part = false;
				if remaining == 1
					&& let Some(last_newline) = kept.iter().rposition(|&b| b == b'\n')
					&& kept[last_newline + 1..].iter().all(|&b| b == b' ')
				{
					kept.truncate(last_newline + 1);
				}
				if !kept.is_empty() {
					out.push(IndPart::Token(IndToken {
						text: String::from_utf8_lossy(&kept).into_owned(),
						has_indentation: token.has_indentation,
					}));
				}
			}
		}
		remaining -= 1;
	}
	out
}

enum Name {
	Static(String),
	Dynamic(String),
}

impl Name {
	fn show(&self) -> String {
		match self {
			Self::Static(name) => identifier(name),
			Self::Dynamic(expr) => format!("\"${{{expr}}}\""),
		}
	}
}

enum Expr {
	Text(String),
	Set(bool, Bindings),
	Call(Box<Expr>, Vec<Expr>),
}

impl Expr {
	fn show(&self) -> String {
		match self {
			Self::Text(text) => text.clone(),
			Self::Set(recursive, bindings) => format!(
				"{}{{ {}}}",
				if *recursive { "rec " } else { "" },
				bindings.show()
			),
			Self::Call(fun, args) => format!(
				"({} {})",
				fun.show(),
				args.iter().map(Self::show).collect::<Vec<_>>().join(" ")
			),
		}
	}

	fn call(self, arg: Expr) -> Self {
		match self {
			Self::Call(fun, mut args) => {
				args.push(arg);
				Self::Call(fun, args)
			}
			fun => Self::Call(Box::new(fun), vec![arg]),
		}
	}
}

#[derive(Default)]
struct Bindings {
	plain: BTreeMap<String, Expr>,
	inherited: Vec<String>,
	from: Vec<(Expr, Vec<String>)>,
	dynamic: Vec<(String, Expr)>,
}

impl Bindings {
	fn insert(&mut self, mut path: impl Iterator<Item = Name>, value: Expr) {
		let Some(name) = path.next() else { return };
		let rest: Vec<_> = path.collect();
		let value = if rest.is_empty() {
			value
		} else {
			let mut nested = Self::default();
			nested.insert(rest.into_iter(), value);
			Expr::Set(false, nested)
		};
		match name {
			Name::Static(name) => {
				if let Some(existing) = self.plain.get_mut(&name) {
					if let (Expr::Set(_, left), Expr::Set(_, right)) = (existing, value) {
						left.merge(right);
					}
				} else {
					self.plain.insert(name, value);
				}
			}
			Name::Dynamic(name) => self.dynamic.push((name, value)),
		}
	}

	fn merge(&mut self, other: Self) {
		for (name, value) in other.plain {
			self.insert([Name::Static(name)].into_iter(), value);
		}
		self.inherited.extend(other.inherited);
		self.from.extend(other.from);
		self.dynamic.extend(other.dynamic);
	}

	fn show(&self) -> String {
		let mut out = String::new();
		let names = |names: &[String]| {
			let mut names = names.to_vec();
			names.sort();
			names
				.iter()
				.map(|name| identifier(name))
				.collect::<Vec<_>>()
				.join(" ")
		};
		if !self.inherited.is_empty() {
			write!(out, "inherit {}; ", names(&self.inherited)).unwrap();
		}
		for (from, attrs) in &self.from {
			if !attrs.is_empty() {
				write!(out, "inherit ({}) {}; ", from.show(), names(attrs)).unwrap();
			}
		}
		for (name, value) in &self.plain {
			write!(out, "{} = {}; ", identifier(name), value.show()).unwrap();
		}
		for (name, value) in &self.dynamic {
			write!(out, "\"${{{name}}}\" = {}; ", value.show()).unwrap();
		}
		out
	}
}

struct Printer<'a> {
	base: &'a Path,
}

impl Printer<'_> {
	fn name(&self, attr: ast::Attr) -> Name {
		match attr {
			ast::Attr::Ident(ident) => Name::Static(ident.to_string()),
			ast::Attr::Str(s) => self.string_name(&s),
			ast::Attr::Dynamic(dynamic) => match dynamic.expr().unwrap() {
				ast::Expr::Str(s) => self.string_name(&s),
				expr => Name::Dynamic(self.expr(expr).show()),
			},
		}
	}

	fn string_name(&self, s: &ast::Str) -> Name {
		let parts = normalized_str_parts(s);
		if parts.is_empty() {
			return Name::Static(String::new());
		}
		if let [InterpolPart::Literal(value)] = parts.as_slice() {
			return Name::Static(value.clone());
		}
		Name::Dynamic(self.str(s))
	}

	fn bindings(&self, entries: &impl HasEntry) -> Bindings {
		let mut out = Bindings::default();
		for entry in entries.entries() {
			match entry {
				ast::Entry::AttrpathValue(binding) => out.insert(
					binding
						.attrpath()
						.unwrap()
						.attrs()
						.map(|attr| self.name(attr)),
					self.expr(binding.value().unwrap()),
				),
				ast::Entry::Inherit(inherit) => {
					let names = inherit
						.attrs()
						.map(|attr| match self.name(attr) {
							Name::Static(name) => name,
							Name::Dynamic(_) => unreachable!("compiler rejects dynamic inherit"),
						})
						.collect();
					if let Some(from) = inherit.from() {
						out.from.push((self.expr(from.expr().unwrap()), names));
					} else {
						out.inherited.extend(names);
					}
				}
			}
		}
		out
	}

	fn is_indented(s: &ast::Str) -> bool {
		s.syntax().children_with_tokens().any(|child| {
			matches!(child, rowan::NodeOrToken::Token(t)
				if t.kind() == rnix::SyntaxKind::TOKEN_STRING_START && t.text() == "''")
		})
	}

	/// Whether `expr` is an `ExprString` once parsed.
	fn is_string(&self, expr: &ast::Expr) -> bool {
		match expr {
			ast::Expr::Paren(p) => p.expr().is_some_and(|e| self.is_string(&e)),
			ast::Expr::Literal(l) => matches!(l.kind(), ast::LiteralKind::Uri(_)),
			ast::Expr::Str(s) if Self::is_indented(s) => self.ind_str(s).1,
			ast::Expr::Str(s) => normalized_str_parts(s)
				.iter()
				.all(|part| matches!(part, InterpolPart::Literal(_))),
			_ => false,
		}
	}

	/// An indented string as CppNix builds it, and whether the result is a
	/// plain string rather than a concatenation.
	fn ind_str(&self, s: &ast::Str) -> (String, bool) {
		let mut es = Vec::new();
		let mut raw = String::new();
		let flush = |raw: &mut String, es: &mut Vec<IndPart>| {
			if !raw.is_empty() {
				es.extend(ind_tokens(raw).into_iter().map(IndPart::Token));
				raw.clear();
			}
		};
		for child in s.syntax().children_with_tokens() {
			match child {
				rowan::NodeOrToken::Token(t)
					if t.kind() == rnix::SyntaxKind::TOKEN_STRING_CONTENT =>
				{
					raw.push_str(t.text());
				}
				rowan::NodeOrToken::Node(n) => {
					flush(&mut raw, &mut es);
					let expr = ast::Interpol::cast(n).and_then(|i| i.expr()).unwrap();
					es.push(IndPart::Code {
						text: self.expr(expr.clone()).show(),
						is_string: self.is_string(&expr),
					});
				}
				rowan::NodeOrToken::Token(_) => {}
			}
		}
		flush(&mut raw, &mut es);
		if es.is_empty() {
			return (string(""), true);
		}
		let mut parts = strip_indentation(es);
		if parts.is_empty() {
			return (string(""), true);
		}
		if let [only] = parts.as_slice() {
			match only {
				IndPart::Token(t) => return (string(&t.text), true),
				IndPart::Code {
					text,
					is_string: true,
				} => return (text.clone(), true),
				IndPart::Code { .. } => {}
			}
		}
		let shown = parts
			.drain(..)
			.map(|part| match part {
				IndPart::Token(t) => string(&t.text),
				IndPart::Code { text, .. } => text,
			})
			.collect::<Vec<_>>()
			.join(" + ");
		(format!("({shown})"), false)
	}

	fn str(&self, s: &ast::Str) -> String {
		if Self::is_indented(s) {
			return self.ind_str(s).0;
		}
		let parts = normalized_str_parts(s);
		if parts
			.iter()
			.all(|part| matches!(part, InterpolPart::Literal(_)))
		{
			let text: String = parts
				.into_iter()
				.filter_map(|part| match part {
					InterpolPart::Literal(s) => Some(s),
					_ => None,
				})
				.collect();
			return string(&text);
		}
		format!(
			"({})",
			parts
				.into_iter()
				.filter(|part| !matches!(part, InterpolPart::Literal(s) if s.is_empty()))
				.map(|part| match part {
					InterpolPart::Literal(s) => string(&s),
					InterpolPart::Interpolation(i) => self.expr(i.expr().unwrap()).show(),
				})
				.collect::<Vec<_>>()
				.join(" + ")
		)
	}

	fn path(&self, parts: Vec<InterpolPart<PathContent>>, home: bool) -> String {
		let interpolated = parts
			.iter()
			.any(|p| matches!(p, InterpolPart::Interpolation(_)));
		let mut shown = Vec::new();
		for (index, part) in parts.into_iter().enumerate() {
			shown.push(match part {
				InterpolPart::Literal(part) if index == 0 => {
					let literal = part.syntax().text().to_string();
					let path = if home {
						PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
							.join(literal.trim_start_matches("~/"))
					} else {
						self.base.join(&literal)
					};
					let mut path = crate::value::canon_path(path)
						.to_string_lossy()
						.into_owned();
					if literal.len() > 1 && literal.ends_with('/') && !path.ends_with('/') {
						path.push('/');
					}
					path
				}
				InterpolPart::Literal(part) => string(&part.syntax().text().to_string()),
				InterpolPart::Interpolation(i) => self.expr(i.expr().unwrap()).show(),
			});
		}
		if interpolated {
			format!("({})", shown.join(" + "))
		} else {
			shown.concat()
		}
	}

	fn expr(&self, expr: ast::Expr) -> Expr {
		let show = |expr| self.expr(expr).show();
		let text = match expr {
			ast::Expr::Paren(p) => return self.expr(p.expr().unwrap()),
			ast::Expr::Root(r) => return self.expr(r.expr().unwrap()),
			ast::Expr::Ident(i) => identifier(&i.to_string()),
			ast::Expr::CurPos(_) => "__curPos".to_owned(),
			ast::Expr::Literal(l) => match l.kind() {
				ast::LiteralKind::Integer(i) => i.value().unwrap().to_string(),
				ast::LiteralKind::Float(f) => {
					let mut out = String::new();
					write!(out, "{}", crate::Value::Float(f.value().unwrap())).unwrap();
					if let Some(index) = out.find("e-")
						&& out.len() - index == 3
					{
						out.insert(index + 2, '0');
					}
					out
				}
				ast::LiteralKind::Uri(u) => string(u.syntax().text()),
			},
			ast::Expr::Str(s) => self.str(&s),
			ast::Expr::PathAbs(p) => self.path(p.parts(), false),
			ast::Expr::PathRel(p) => self.path(p.parts(), false),
			ast::Expr::PathHome(p) => self.path(p.parts(), true),
			ast::Expr::PathSearch(p) => {
				return Expr::Text("__findFile".to_owned())
					.call(Expr::Text("__nixPath".to_owned()))
					.call(Expr::Text(string(
						p.content()
							.unwrap()
							.syntax()
							.text()
							.trim_start_matches('<')
							.trim_end_matches('>'),
					)));
			}
			ast::Expr::AttrSet(s) => return Expr::Set(s.rec_token().is_some(), self.bindings(&s)),
			ast::Expr::LegacyLet(l) => {
				format!("({}).body", Expr::Set(true, self.bindings(&l)).show())
			}
			ast::Expr::LetIn(l) => format!(
				"(let {}in {})",
				self.bindings(&l).show(),
				show(l.body().unwrap())
			),
			ast::Expr::With(w) => format!(
				"(with {}; {})",
				show(w.namespace().unwrap()),
				show(w.body().unwrap())
			),
			ast::Expr::Assert(a) => format!(
				"assert {}; {}",
				show(a.condition().unwrap()),
				show(a.body().unwrap())
			),
			ast::Expr::IfElse(i) => format!(
				"(if {} then {} else {})",
				show(i.condition().unwrap()),
				show(i.body().unwrap()),
				show(i.else_body().unwrap())
			),
			ast::Expr::List(l) => format!(
				"[ {}]",
				l.items()
					.map(|e| format!("({}) ", show(e)))
					.collect::<String>()
			),
			ast::Expr::Apply(a) => {
				return self
					.expr(a.lambda().unwrap())
					.call(self.expr(a.argument().unwrap()));
			}
			ast::Expr::Select(s) => {
				let mut out = format!(
					"({}).{}",
					show(s.expr().unwrap()),
					s.attrpath()
						.unwrap()
						.attrs()
						.map(|a| self.name(a).show())
						.collect::<Vec<_>>()
						.join(".")
				);
				if let Some(default) = s.default_expr() {
					write!(out, " or ({})", show(default)).unwrap();
				}
				out
			}
			ast::Expr::HasAttr(h) => format!(
				"(({}) ? {})",
				show(h.expr().unwrap()),
				h.attrpath()
					.unwrap()
					.attrs()
					.map(|a| self.name(a).show())
					.collect::<Vec<_>>()
					.join(".")
			),
			ast::Expr::Lambda(l) => {
				let param = match l.param().unwrap() {
					ast::Param::IdentParam(p) => p.ident().unwrap().to_string(),
					ast::Param::Pattern(p) => {
						let mut entries: Vec<_> = p
							.pat_entries()
							.map(|entry| {
								let name = entry.ident().unwrap().to_string();
								let value = match entry.default() {
									Some(e) => format!("{name} ? {}", show(e)),
									None => name.clone(),
								};
								(name, value)
							})
							.collect();
						entries.sort_by(|a, b| a.0.cmp(&b.0));
						let mut entries: Vec<_> =
							entries.into_iter().map(|(_, value)| value).collect();
						if p.ellipsis_token().is_some() {
							entries.push("...".to_owned());
						}
						let mut param = format!("{{ {} }}", entries.join(", "));
						if let Some(bind) = p.pat_bind() {
							write!(param, " @ {}", bind.ident().unwrap()).unwrap();
						}
						param
					}
				};
				format!("({param}: {})", show(l.body().unwrap()))
			}
			ast::Expr::UnaryOp(u) => match u.operator().unwrap() {
				ast::UnaryOpKind::Invert => format!("(! {})", show(u.expr().unwrap())),
				ast::UnaryOpKind::Negate => {
					return Expr::Text("__sub".to_owned())
						.call(Expr::Text("0".to_owned()))
						.call(self.expr(u.expr().unwrap()));
				}
			},
			ast::Expr::BinOp(b) => {
				use ast::BinOpKind::*;
				let left = self.expr(b.lhs().unwrap());
				let right = self.expr(b.rhs().unwrap());
				let (l, r) = (left.show(), right.show());
				match b.operator().unwrap() {
					Sub | Mul | Div | Less => {
						return Expr::Text(
							match b.operator().unwrap() {
								Sub => "__sub",
								Mul => "__mul",
								Div => "__div",
								_ => "__lessThan",
							}
							.to_owned(),
						)
						.call(left)
						.call(right);
					}
					More => return Expr::Text("__lessThan".to_owned()).call(right).call(left),
					LessOrEq => format!("(! (__lessThan {r} {l}))"),
					MoreOrEq => format!("(! (__lessThan {l} {r}))"),
					PipeRight => return right.call(left),
					PipeLeft => return left.call(right),
					op => format!(
						"({l} {} {r})",
						match op {
							Add => "+",
							Concat => "++",
							Update => "//",
							And => "&&",
							Or => "||",
							Equal => "==",
							NotEqual => "!=",
							Implication => "->",
							_ => unreachable!(),
						}
					),
				}
			}
			ast::Expr::Error(_) => unreachable!("only print validated expressions"),
		};
		Expr::Text(text)
	}
}

/// Print a successfully compiled expression as CppNix's `--parse` does.
/// `base` is the absolute source directory, or the working directory for stdin.
pub fn show_expr(expr: &ast::Expr, base: &Path) -> String {
	Printer { base }.expr(expr.clone()).show()
}
