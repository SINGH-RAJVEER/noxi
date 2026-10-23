//! CppNix 2.35.2's `Expr::show`, including parser desugaring.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use rnix::ast::{self, AstToken, HasEntry, InterpolPart, PathContent};

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

	fn str(&self, s: &ast::Str) -> String {
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
			ast::Expr::PathSearch(p) => format!(
				"(__findFile __nixPath {})",
				string(
					p.content()
						.unwrap()
						.syntax()
						.text()
						.trim_start_matches('<')
						.trim_end_matches('>')
				)
			),
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
				ast::UnaryOpKind::Negate => format!("(__sub 0 {})", show(u.expr().unwrap())),
			},
			ast::Expr::BinOp(b) => {
				use ast::BinOpKind::*;
				let left = self.expr(b.lhs().unwrap());
				let right = self.expr(b.rhs().unwrap());
				let (l, r) = (left.show(), right.show());
				match b.operator().unwrap() {
					Sub => format!("(__sub {l} {r})"),
					Mul => format!("(__mul {l} {r})"),
					Div => format!("(__div {l} {r})"),
					Less => format!("(__lessThan {l} {r})"),
					More => format!("(__lessThan {r} {l})"),
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
