//! Nix-aware JSON conversion with an explicit traversal stack.

use super::{CoercionKind, Value};
use crate::errors::ErrorKind;
use crate::generators::{self, GenCo};
use crate::{NixContext, NixContextElement};
use bstr::ByteSlice;
use serde_json::{Map, Number, Value as Json};

enum Work {
	Value(Value, usize, Option<crate::errors::Trace>),
	Exit,
	List(usize),
	Attrs(Vec<String>),
}

/// Drop completed JSON values without recursive container destruction.
struct Results(Vec<Json>);

impl Drop for Results {
	fn drop(&mut self) {
		while let Some(value) = self.0.pop() {
			match value {
				Json::Array(values) => self.0.extend(values),
				Json::Object(values) => self.0.extend(values.into_values()),
				_ => {}
			}
		}
	}
}

/// Serialize and release a JSON tree without recursive traversal.
pub(crate) fn into_json_string(value: Json) -> Result<String, ErrorKind> {
	enum Part<'a> {
		Value(&'a Json),
		Text(&'static str),
		Key(&'a str),
	}
	let root = Results(vec![value]);
	let mut pending = vec![Part::Value(&root.0[0])];
	let mut output = String::new();
	while let Some(part) = pending.pop() {
		match part {
			Part::Text(text) => output.push_str(text),
			Part::Key(key) => output.push_str(&serde_json::to_string(key)?),
			Part::Value(Json::Array(values)) => {
				output.push('[');
				pending.push(Part::Text("]"));
				for (index, value) in values.iter().enumerate().rev() {
					pending.push(Part::Value(value));
					if index > 0 {
						pending.push(Part::Text(","));
					}
				}
			}
			Part::Value(Json::Object(values)) => {
				output.push('{');
				pending.push(Part::Text("}"));
				for (index, (key, value)) in values.iter().enumerate().rev() {
					pending.push(Part::Value(value));
					pending.push(Part::Text(":"));
					pending.push(Part::Key(key));
					if index > 0 {
						pending.push(Part::Text(","));
					}
				}
			}
			Part::Value(value) => output.push_str(&serde_json::to_string(value)?),
		}
	}
	Ok(output)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn serializes_and_drops_deep_json_on_a_small_stack() {
		std::thread::Builder::new()
			.stack_size(256 * 1024)
			.spawn(|| {
				let mut value = Json::Null;
				for _ in 0..20_000 {
					value = Json::Array(vec![value]);
				}
				let output = into_json_string(value).unwrap();
				assert_eq!(output.len(), 40_004);
				assert!(output.starts_with("[[[["));
				assert!(output.ends_with("]]]]"));
			})
			.unwrap()
			.join()
			.unwrap();
	}

	#[test]
	fn preserves_json_escaping_and_container_order() {
		let value = serde_json::json!({"z": [1, true, null], "a": "\"\n\\"});
		let expected = serde_json::to_string(&value).unwrap();
		assert_eq!(into_json_string(value).unwrap(), expected);
	}
}

impl Value {
	/// Convert values in reference evaluation order, preserving contexts and
	/// limiting depth before Rust's call stack can overflow.
	pub async fn into_contextful_json(self, co: &GenCo) -> Result<(Json, NixContext), ErrorKind> {
		let mut work = vec![Work::Value(self, 0, None)];
		let mut results = Results(Vec::new());
		let mut context = NixContext::new();
		let path = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
		generators::request_trace_path(co, path.clone()).await;
		let budget = generators::request_recursion_budget(co).await;
		while let Some(item) = work.pop() {
			match item {
				Work::Exit => {
					path.borrow_mut().pop();
				}
				Work::List(count) => {
					let values = results.0.split_off(results.0.len() - count);
					results.0.push(Json::Array(values));
				}
				Work::Attrs(names) => {
					let values = results.0.split_off(results.0.len() - names.len());
					results.0.push(Json::Object(
						names.into_iter().zip(values).collect::<Map<_, _>>(),
					));
				}
				Work::Value(value, depth, trace) => {
					if let Some(trace) = trace {
						path.borrow_mut().push(trace);
						work.push(Work::Exit);
					}
					if depth >= budget {
						return Err(ErrorKind::At {
							message: "stack overflow; max-call-depth exceeded".into(),
							span: path
								.borrow()
								.last()
								.and_then(|t| t.span)
								.unwrap_or(generators::request_span(co).await),
						});
					}
					let value = generators::request_force(co, value).await;
					let json = match value {
						Value::Null => Json::Null,
						Value::Bool(value) => Json::Bool(value),
						Value::Integer(value) => Json::Number(Number::from(value)),
						Value::Float(value) => serde_json::value::to_value(value)?,
						Value::String(value) => {
							context.mimic(&value);
							Json::String(match value.to_str() {
								Ok(text) => text.to_owned(),
								Err(_) => {
									let bytes = value.as_bytes();
									let error = std::str::from_utf8(bytes).unwrap_err();
									let index = error.valid_up_to();
									return Err(ErrorKind::EvalError(format!(
										"JSON serialization error: [json.exception.type_error.316] invalid UTF-8 byte at index {index}: 0x{:02X}",
										bytes[index]
									)));
								}
							})
						}
						Value::Path(path) => {
							let path = generators::request_path_import(co, *path)
								.await
								.to_string_lossy()
								.into_owned();
							context = context.append(NixContextElement::Plain(path.clone()));
							Json::String(path)
						}
						Value::List(values) => {
							work.push(Work::List(values.len()));
							let values: Vec<_> = values.into_iter().collect();
							let count = values.len();
							work.extend(values.into_iter().rev().enumerate().map(
								|(index, value)| {
									Work::Value(
										value,
										depth + 1,
										Some(crate::errors::Trace::new(
											path.borrow().last().and_then(|trace| trace.span),
											format!(
												"while evaluating list element at index {}",
												count - index - 1
											),
										)),
									)
								},
							));
							continue;
						}
						Value::Attrs(attrs) => {
							if attrs.select("__toString").is_some() {
								let span = generators::request_span(co).await;
								match Value::Attrs(attrs)
									.coerce_to_string_(
										co,
										CoercionKind {
											strong: false,
											import_paths: false,
										},
										span,
									)
									.await?
								{
									Value::Catchable(error) => {
										return Err(ErrorKind::CatchableError(*error));
									}
									Value::String(value) => {
										context.mimic(&value);
										Json::String(match value.to_str() {
											Ok(text) => text.to_owned(),
											Err(_) => {
												let bytes = value.as_bytes();
												let error = std::str::from_utf8(bytes).unwrap_err();
												let index = error.valid_up_to();
												return Err(ErrorKind::EvalError(format!(
													"JSON serialization error: [json.exception.type_error.316] invalid UTF-8 byte at index {index}: 0x{:02X}",
													bytes[index]
												)));
											}
										})
									}
									_ => unreachable!("string coercion returned a non-string"),
								}
							} else if let Some(path) = attrs.select("outPath") {
								work.push(Work::Value(path.clone(), depth + 1, None));
								continue;
							} else {
								let positions = attrs.clone();
								let entries: Vec<_> = attrs.into_iter_sorted().collect();
								let names = entries
									.iter()
									.map(|(name, _)| name.to_str().map(str::to_owned))
									.collect::<Result<Vec<_>, _>>()?;
								work.push(Work::Attrs(names));
								work.extend(entries.into_iter().rev().map(|(name, value)| {
									Work::Value(
										value,
										depth + 1,
										Some(crate::errors::Trace::new(
											positions.get_attr_pos(&name),
											format!(
												"while evaluating attribute '{}'",
												name.to_str_lossy()
											),
										)),
									)
								}));
								continue;
							}
						}
						Value::Catchable(error) => return Err(ErrorKind::CatchableError(*error)),
						value => return Err(ErrorKind::NotSerialisableToJson(value.type_of())),
					};
					results.0.push(json);
				}
			}
		}
		Ok((results.0.pop().expect("root JSON value"), context))
	}
}
