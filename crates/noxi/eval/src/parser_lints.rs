//! Parser warnings from CppNix 2.35.2's `parser.y` and `ExprCall::warnIfCursedOr`.
//! These diagnostics are separate from evaluator errors and trace rendering.

use std::fmt::Write;

use rnix::{
	SyntaxKind::*,
	ast::{self, AstToken, InterpolPart},
};
use rowan::ast::AstNode;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Severity {
	#[default]
	Ignore,
	Warn,
	Fatal,
}

impl Severity {
	pub fn from_setting(value: &str) -> Self {
		match value {
			"warn" => Self::Warn,
			"fatal" => Self::Fatal,
			_ => Self::Ignore,
		}
	}
}

#[derive(Clone, Debug, Default)]
pub struct Settings {
	pub absolute_paths: Severity,
	pub short_paths: Severity,
	pub urls: Severity,
}

#[derive(Debug)]
pub struct Diagnostic {
	pub severity: Severity,
	pub message: String,
	pub offset: usize,
	/// Cursed-or warnings use a single position without a source excerpt.
	pub excerpt: bool,
}

impl Diagnostic {
	pub fn render(&self, code: &str, name: &str) -> String {
		let before = &code[..self.offset];
		let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
		let column = before.rsplit('\n').next().unwrap_or_default().len() + 1;
		let prefix = if self.severity == Severity::Fatal {
			"error: "
		} else {
			"warning: "
		};
		if !self.excerpt {
			return format!("{prefix}at {name}:{line}:{column}: {}\n", self.message);
		}
		let indent = " ".repeat(prefix.len());
		let mut out = format!(
			"{prefix}{}\n{indent}at {name}:{line}:{column}:\n",
			self.message
		);
		let lines: Vec<_> = code.split('\n').collect();
		for number in line.saturating_sub(1).max(1)..=(line + 1).min(lines.len()) {
			let text = lines[number - 1].trim_end_matches('\r');
			write!(out, "{indent}{number:>6}|").unwrap();
			if !text.is_empty() {
				write!(out, " {text}").unwrap();
			}
			out.push('\n');
			if number == line {
				writeln!(out, "{indent}      | {}^", " ".repeat(column - 1)).unwrap();
			}
		}
		out
	}
}

impl Settings {
	/// Check text without compiling or evaluating it. Malformed syntax is left to the parser.
	pub fn check_text(&self, code: &str) -> Vec<Diagnostic> {
		let code = crate::line_endings::normalize(code);
		let parsed = ast::Root::parse(&code);
		if !parsed.errors().is_empty() {
			return Vec::new();
		}
		parsed
			.tree()
			.expr()
			.map_or_else(Vec::new, |expr| self.check(&expr))
	}

	pub fn check(&self, expr: &ast::Expr) -> Vec<Diagnostic> {
		if self.absolute_paths == Severity::Ignore
			&& self.short_paths == Severity::Ignore
			&& self.urls == Severity::Ignore
			&& !has_or_ident(expr)
		{
			return Vec::new();
		}
		let mut out = Vec::new();
		for node in expr.syntax().descendants() {
			let offset = || u32::from(node.text_range().start()) as usize;
			let diagnostic = match node.kind() {
				NODE_PATH_ABS if self.absolute_paths != Severity::Ignore => {
					let path = ast::PathAbs::cast(node.clone()).unwrap();
					self.path(path.parts(), self.absolute_paths, "absolute", offset())
				}
				NODE_PATH_HOME if self.absolute_paths != Severity::Ignore => {
					let path = ast::PathHome::cast(node.clone()).unwrap();
					self.path(path.parts(), self.absolute_paths, "home", offset())
				}
				NODE_PATH_REL if self.short_paths != Severity::Ignore => {
					let path = ast::PathRel::cast(node.clone()).unwrap();
					self.path(path.parts(), self.short_paths, "relative", offset())
				}
				NODE_LITERAL if self.urls != Severity::Ignore => {
					match ast::Literal::cast(node.clone()).unwrap().kind() {
						ast::LiteralKind::Uri(uri) => Some(Diagnostic {
							severity: self.urls,
							message: format!(
								"URL literals are {}. Consider using a string literal \"{}\" instead (lint-url-literals)",
								if self.urls == Severity::Fatal {
									"disallowed"
								} else {
									"discouraged"
								},
								uri.syntax().text()
							),
							offset: offset(),
							excerpt: true,
						}),
						_ => None,
					}
				}
				NODE_APPLY => {
					let apply = ast::Apply::cast(node.clone()).unwrap();
					if !matches!(apply.argument(), Some(ast::Expr::Ident(ref ident)) if is_or_ident(&ident.syntax().green()))
					{
						continue;
					}
					let consumed = node.parent().is_some_and(|parent| {
						if ast::List::can_cast(parent.kind()) {
							return true;
						}
						if let Some(outer) = ast::Apply::cast(parent.clone()) {
							return outer.argument().is_some_and(|arg| arg.syntax() == &node);
						}
						if let Some(select) = ast::Select::cast(parent) {
							return select
								.default_expr()
								.is_some_and(|arg| arg.syntax() == &node);
						}
						false
					});
					consumed.then(|| Diagnostic {
						severity: Severity::Warn,
						message: format!("This expression uses `or` as an identifier in a way that will change in a future Nix release.\nWrap this entire expression in parentheses to preserve its current meaning:\n    ({apply})\nGive feedback at https://github.com/NixOS/nix/pull/11121"),
						offset: offset(),
						excerpt: false,
					})
				}
				_ => None,
			};
			if let Some(diagnostic) = diagnostic {
				out.push(diagnostic);
			}
		}
		out.sort_by_key(|diagnostic| diagnostic.offset);
		out
	}

	fn path(
		&self,
		parts: Vec<InterpolPart<ast::PathContent>>,
		severity: Severity,
		kind: &str,
		offset: usize,
	) -> Option<Diagnostic> {
		if severity == Severity::Ignore {
			return None;
		}
		let Some(InterpolPart::Literal(first)) = parts.first() else {
			return None;
		};
		let literal = first.syntax().text();
		let message = if kind == "relative" {
			if literal.starts_with('.') {
				return None;
			}
			format!(
				"relative path literal '{literal}' should be prefixed with '.' for clarity: './{literal}' (lint-short-path-literals)"
			)
		} else {
			format!(
				"{kind} path literals are not portable. Consider replacing path literal '{literal}' by a string, relative path, or parameter (lint-absolute-path-literals)"
			)
		};
		Some(Diagnostic {
			severity,
			message,
			offset,
			excerpt: true,
		})
	}
}

fn is_or_ident(node: &rowan::GreenNodeData) -> bool {
	if u32::from(node.text_len()) != 2 {
		return false;
	}
	let mut children = node.children();
	if let Some(rowan::NodeOrToken::Token(token)) = children.next()
		&& children.next().is_none()
	{
		return token.text() == "or";
	}
	false
}

fn has_or_ident(expr: &ast::Expr) -> bool {
	let root = expr.syntax().green();
	let mut stack = vec![root.children()];
	while let Some(children) = stack.last_mut() {
		match children.next() {
			Some(rowan::NodeOrToken::Node(node)) => {
				if node.kind().0 == NODE_IDENT as u16 && is_or_ident(node) {
					return true;
				}
				stack.push(node.children());
			}
			Some(rowan::NodeOrToken::Token(_)) => {}
			None => {
				stack.pop();
			}
		}
	}
	false
}
