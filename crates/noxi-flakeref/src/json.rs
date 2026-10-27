//! JSON rendering byte-compatible with nlohmann's `json::dump`, which CppNix
//! uses for lock files, registries and attribute sets in messages. Object
//! keys are sorted, as nlohmann's default `std::map` storage sorts them.

use serde_json::Value;

/// `json.dump(indent)`; `None` is the compact `dump()` form.
pub fn dump(value: &Value, indent: Option<usize>) -> String {
	let mut out = String::new();
	write_value(&mut out, value, indent, 0);
	out
}

fn write_string(out: &mut String, s: &str) {
	out.push('"');
	for c in s.chars() {
		match c {
			'"' => out.push_str("\\\""),
			'\\' => out.push_str("\\\\"),
			'\u{08}' => out.push_str("\\b"),
			'\u{0c}' => out.push_str("\\f"),
			'\n' => out.push_str("\\n"),
			'\r' => out.push_str("\\r"),
			'\t' => out.push_str("\\t"),
			c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
			c => out.push(c),
		}
	}
	out.push('"');
}

fn newline(out: &mut String, indent: Option<usize>, level: usize) {
	if let Some(indent) = indent {
		out.push('\n');
		out.extend(std::iter::repeat_n(' ', indent * level));
	}
}

fn write_value(out: &mut String, value: &Value, indent: Option<usize>, level: usize) {
	match value {
		Value::Null => out.push_str("null"),
		Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
		Value::Number(n) => out.push_str(&n.to_string()),
		Value::String(s) => write_string(out, s),
		Value::Array(items) => {
			if items.is_empty() {
				out.push_str("[]");
				return;
			}
			out.push('[');
			for (i, item) in items.iter().enumerate() {
				if i > 0 {
					out.push(',');
				}
				newline(out, indent, level + 1);
				write_value(out, item, indent, level + 1);
			}
			newline(out, indent, level);
			out.push(']');
		}
		Value::Object(map) => {
			if map.is_empty() {
				out.push_str("{}");
				return;
			}
			let mut entries: Vec<_> = map.iter().collect();
			entries.sort_by(|a, b| a.0.cmp(b.0));
			out.push('{');
			for (i, (key, item)) in entries.into_iter().enumerate() {
				if i > 0 {
					out.push(',');
				}
				newline(out, indent, level + 1);
				write_string(out, key);
				out.push(':');
				if indent.is_some() {
					out.push(' ');
				}
				write_value(out, item, indent, level + 1);
			}
			newline(out, indent, level);
			out.push('}');
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn matches_nlohmann() {
		let value: Value =
			serde_json::from_str(r#"{"b":[1,"x\u0001"],"a":{},"c":[],"d":{"e":true}}"#).unwrap();
		assert_eq!(
			dump(&value, None),
			r#"{"a":{},"b":[1,"x\u0001"],"c":[],"d":{"e":true}}"#
		);
		assert_eq!(
			dump(&value, Some(2)),
			"{\n  \"a\": {},\n  \"b\": [\n    1,\n    \"x\\u0001\"\n  ],\n  \"c\": [],\n  \"d\": {\n    \"e\": true\n  }\n}"
		);
	}
}
