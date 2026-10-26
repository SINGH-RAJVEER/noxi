//! CppNix's report for a failed equality assertion, `assert a == b`:
//! `assertEqValues` in `libexpr/eval.cc` compares both sides again and
//! describes the first difference, with frames for the list elements and
//! attributes that led to it.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use bstr::ByteSlice;

use super::generators::{self, GenCo};
use crate::errors::{CatchableErrorKind, CatchableTrace, ErrorKind, Trace};
use crate::value::{PrintOptions, Value, cppnix_attribute_order};

/// The first difference between two values: CppNix's message and the
/// frames leading to it, outermost first.
type Difference = Option<(String, Vec<Trace>)>;

async fn print(co: &GenCo, value: &Value) -> String {
	generators::request_print(co, value.clone(), PrintOptions::ERROR)
		.await
		.to_str_lossy()
		.into_owned()
}

fn is_derivation(value: &Value) -> bool {
	matches!(value, Value::Attrs(attrs) if attrs.is_derivation())
}

/// `assertEqValues`, returning the first difference if any.
fn difference<'a>(
	co: &'a GenCo,
	lhs: Value,
	rhs: Value,
) -> Pin<Box<dyn Future<Output = Result<Difference, ErrorKind>> + 'a>> {
	Box::pin(async move {
		let lhs = generators::request_force(co, lhs).await;
		let rhs = generators::request_force(co, rhs).await;
		let show_type = crate::diagnostics::show_type;
		let differ = |message: String| Ok(Some((message, vec![])));

		match (&lhs, &rhs) {
			(
				Value::Integer(_) | Value::Float(_),
				Value::Integer(_) | Value::Float(_),
			) => {
				let equal = match (&lhs, &rhs) {
					(Value::Integer(a), Value::Integer(b)) => a == b,
					(Value::Float(a), Value::Float(b)) => a == b,
					(Value::Integer(a), Value::Float(b)) | (Value::Float(b), Value::Integer(a)) => {
						*a as f64 == *b
					}
					_ => unreachable!(),
				};
				if equal {
					return Ok(None);
				}
				differ(format!(
					"{} with value '{}' is not equal to {} with value '{}'",
					show_type(&lhs),
					print(co, &lhs).await,
					show_type(&rhs),
					print(co, &rhs).await
				))
			}
			_ if std::mem::discriminant(&lhs) != std::mem::discriminant(&rhs)
				&& !matches!(
					(&lhs, &rhs),
					(
						Value::Closure(_) | Value::Builtin(_),
						Value::Closure(_) | Value::Builtin(_)
					)
				) =>
			{
				differ(format!(
					"{} of value '{}' is not equal to {} of value '{}'",
					show_type(&lhs),
					print(co, &lhs).await,
					show_type(&rhs),
					print(co, &rhs).await
				))
			}
			(Value::Bool(a), Value::Bool(b)) if a != b => differ(format!(
				"boolean '{}' is not equal to boolean '{}'",
				print(co, &lhs).await,
				print(co, &rhs).await
			)),
			(Value::String(a), Value::String(b)) if a.as_bytes() != b.as_bytes() => {
				differ(format!(
					"string '{}' is not equal to string '{}'",
					print(co, &lhs).await,
					print(co, &rhs).await
				))
			}
			(Value::Path(a), Value::Path(b)) if a != b => differ(format!(
				"path '{}' is not equal to path '{}'",
				print(co, &lhs).await,
				print(co, &rhs).await
			)),
			(Value::List(a), Value::List(b)) => {
				if a.len() != b.len() {
					return differ(format!(
						"list of size '{}' is not equal to list of size '{}', left hand side is '{}', right hand side is '{}'",
						a.len(),
						b.len(),
						print(co, &lhs).await,
						print(co, &rhs).await
					));
				}
				for (index, (a, b)) in a.iter().zip(b.iter()).enumerate() {
					if let Some((message, mut traces)) =
						difference(co, a.clone(), b.clone()).await?
					{
						traces.insert(
							0,
							Trace::new(None, format!("while comparing list element {index}")),
						);
						return Ok(Some((message, traces)));
					}
				}
				Ok(None)
			}
			(Value::Attrs(a), Value::Attrs(b)) => {
				if is_derivation(&lhs)
					&& is_derivation(&rhs)
					&& let (Some(a), Some(b)) = (a.select("outPath"), b.select("outPath"))
				{
					return Ok(difference(co, a.clone(), b.clone())
						.await?
						.map(|(message, mut traces)| {
							traces.insert(
								0,
								Trace::new(
									None,
									"while comparing a derivation by its 'outPath' attribute",
								),
							);
							(message, traces)
						}));
				}
				if a.len() != b.len() {
					return differ(format!(
						"attribute names of attribute set '{}' differs from attribute set '{}'",
						print(co, &lhs).await,
						print(co, &rhs).await
					));
				}
				let order = |attrs: &crate::NixAttrs| {
					let mut names: Vec<_> = attrs.keys().cloned().collect();
					names.sort_by(|a, b| cppnix_attribute_order(a.as_bytes(), b.as_bytes()));
					names
				};
				for (i, j) in order(a).into_iter().zip(order(b)) {
					if i != j {
						let (message, name) = if b.select(&i).is_none() {
							("is contained in '{lhs}', but not in '{rhs}'", i)
						} else {
							("is missing in '{lhs}', but is contained in '{rhs}'", j)
						};
						let message = message
							.replace("{lhs}", &print(co, &lhs).await)
							.replace("{rhs}", &print(co, &rhs).await);
						return differ(format!(
							"attribute name '{}' {message}",
							name.to_str_lossy()
						));
					}
					let (Some(x), Some(y)) = (a.select(&i), b.select(&j)) else {
						continue;
					};
					if let Some((message, traces)) = difference(co, x.clone(), y.clone()).await? {
						let mut frames = vec![Trace::new(
							None,
							format!("while comparing attribute '{}'", i.to_str_lossy()),
						)];
						if let Some(span) = a.get_attr_pos(&i) {
							frames.push(Trace::new(Some(span), "where left hand side is"));
						}
						if let Some(span) = b.get_attr_pos(&j) {
							frames.push(Trace::new(Some(span), "where right hand side is"));
						}
						frames.extend(traces);
						return Ok(Some((message, frames)));
					}
				}
				Ok(None)
			}
			(Value::Closure(_) | Value::Builtin(_), _) => differ(
				"distinct functions and immediate comparisons of identical functions compare as unequal"
					.to_owned(),
			),
			_ => Ok(None),
		}
	})
}

/// The catchable error of `assert lhs == rhs` failing, where `text` is the
/// condition as CppNix shows it. Its trace is completed by the VM with the
/// frames of the running evaluation.
pub(super) async fn assert_eq_failed(
	co: GenCo,
	lhs: Value,
	rhs: Value,
	text: String,
) -> Result<Value, ErrorKind> {
	let (message, traces) = match difference(&co, lhs, rhs).await? {
		Some(difference) => difference,
		None => (format!("assertion '{text}' failed"), vec![]),
	};
	Ok(Value::Catchable(Box::new(CatchableErrorKind::Traced {
		kind: Box::new(CatchableErrorKind::AssertionFailed),
		info: Rc::new(CatchableTrace {
			traces,
			span: None,
			message: Some(message),
			relative: true,
		}),
	})))
}
