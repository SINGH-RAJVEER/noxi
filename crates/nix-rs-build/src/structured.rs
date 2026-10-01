//! The `.attrs.json` and `.attrs.sh` files a builder with structured
//! attributes reads, following CppNix 2.35.2's `parsed-derivations.cc`.

use std::fmt::Write;

use serde_json::{Map, Number, Value};

/// `escapeShellArgAlways`.
fn shell_quote(s: &str) -> String {
	format!("'{}'", s.replace('\'', "'\\''"))
}

/// nlohmann's `get<int>()` of a number: integers wrap to 32 bits, floats
/// truncate, and floats outside the range give `INT_MIN` as x86's
/// conversion does.
fn as_c_int(number: &Number) -> i32 {
	if let Some(n) = number.as_i64() {
		n as i32
	} else if let Some(n) = number.as_u64() {
		n as i32
	} else {
		let f = number.as_f64().unwrap_or(0.0);
		if f >= i32::MIN as f64 && f < (i32::MAX as f64) + 1.0 {
			f.trunc() as i32
		} else {
			i32::MIN
		}
	}
}

fn simple_value(value: &Value) -> Option<String> {
	match value {
		Value::String(s) => Some(shell_quote(s)),
		Value::Number(n) => {
			// CppNix checks the value converted to `float`.
			let f = n.as_f64().unwrap_or(0.0) as f32;
			(f.ceil() == f).then(|| as_c_int(n).to_string())
		}
		Value::Null => Some("''".to_owned()),
		Value::Bool(b) => Some(if *b { "1" } else { "" }.to_owned()),
		Value::Array(_) | Value::Object(_) => None,
	}
}

fn is_shell_var_name(name: &str) -> bool {
	let mut chars = name.chars();
	matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
		&& chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn sorted(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
	let mut entries: Vec<_> = map.iter().collect();
	entries.sort_by(|a, b| a.0.cmp(b.0));
	entries
}

/// `StructuredAttrs::writeShell`: `declare` statements for the attributes
/// whose names are shell variables and whose values are simple or arrays
/// and objects of simple values.
pub fn write_shell(json: &Map<String, Value>) -> String {
	let mut out = String::new();
	for (key, value) in sorted(json) {
		if !is_shell_var_name(key) {
			continue;
		}
		if let Some(s) = simple_value(value) {
			let _ = writeln!(out, "declare {key}={s}");
		} else if let Value::Array(items) = value {
			let items: Option<Vec<String>> = items.iter().map(simple_value).collect();
			if let Some(items) = items {
				let body: String = items.iter().map(|s| format!("{s} ")).collect();
				let _ = writeln!(out, "declare -a {key}=({body})");
			}
		} else if let Value::Object(map) = value {
			let items: Option<Vec<String>> = sorted(map)
				.into_iter()
				.map(|(k, v)| simple_value(v).map(|s| format!("[{}]={s} ", shell_quote(k))))
				.collect();
			if let Some(items) = items {
				let _ = writeln!(out, "declare -A {key}=({})", items.concat());
			}
		}
	}
	out
}

/// A float as nlohmann's `dump` writes it: the shortest digits that
/// round-trip, in fixed notation for decimal exponents from -4 to 15 and
/// otherwise as `d.ddde+XX`.
fn format_float(f: f64) -> String {
	if f == 0.0 {
		return if f.is_sign_negative() { "-0.0" } else { "0.0" }.to_owned();
	}
	let sign = if f < 0.0 { "-" } else { "" };
	// `{:e}` gives the shortest round-trip digits: "d.ddde±N".
	let sci = format!("{:e}", f.abs());
	let (mantissa, exponent) = sci.split_once('e').unwrap();
	let exponent: i32 = exponent.parse().unwrap();
	let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
	let k = digits.len() as i32;
	// The position of the decimal point relative to the digits.
	let n = exponent + 1;
	let body = if k <= n && n <= 15 {
		format!("{digits}{}.0", "0".repeat((n - k) as usize))
	} else if 0 < n && n <= 15 {
		format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
	} else if -4 < n && n <= 0 {
		format!("0.{}{digits}", "0".repeat((-n) as usize))
	} else {
		let mantissa = if k == 1 {
			digits.clone()
		} else {
			format!("{}.{}", &digits[..1], &digits[1..])
		};
		let e = n - 1;
		format!("{mantissa}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
	};
	format!("{sign}{body}")
}

/// `nlohmann::json::dump()` without indentation: sorted keys, compact
/// separators, and nlohmann's number formatting.
pub fn dump(value: &Value) -> String {
	let mut out = String::new();
	write_value(&mut out, value);
	out
}

fn write_value(out: &mut String, value: &Value) {
	match value {
		Value::Null => out.push_str("null"),
		Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
		Value::Number(n) => {
			if let Some(i) = n.as_i64() {
				let _ = write!(out, "{i}");
			} else if let Some(u) = n.as_u64() {
				let _ = write!(out, "{u}");
			} else {
				out.push_str(&format_float(n.as_f64().unwrap_or(0.0)));
			}
		}
		// serde_json escapes the same characters as nlohmann with
		// `ensure_ascii` off.
		Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap()),
		Value::Array(items) => {
			out.push('[');
			for (i, item) in items.iter().enumerate() {
				if i > 0 {
					out.push(',');
				}
				write_value(out, item);
			}
			out.push(']');
		}
		Value::Object(map) => {
			out.push('{');
			for (i, (k, v)) in sorted(map).into_iter().enumerate() {
				if i > 0 {
					out.push(',');
				}
				out.push_str(&serde_json::to_string(k).unwrap());
				out.push(':');
				write_value(out, v);
			}
			out.push('}');
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn floats_follow_nlohmann() {
		for (f, s) in [
			(1.5, "1.5"),
			(100.0, "100.0"),
			(0.001, "0.001"),
			(0.0001, "0.0001"),
			(0.00001, "1e-05"),
			(1e14, "100000000000000.0"),
			(1e15, "1e+15"),
			(1.25e100, "1.25e+100"),
			(-2.5, "-2.5"),
			(0.1, "0.1"),
		] {
			assert_eq!(format_float(f), s, "{f}");
		}
	}

	#[test]
	fn shell_values_follow_cppnix() {
		let json: Value = serde_json::from_str(
			r#"{"a": "it's", "b": 2.0, "c": 2.5, "d": [1, "x", null, true, false], "e": {"k": 1},
			"f": [[1]], "g h": 1, "i": 3000000000, "j": 1e40}"#,
		)
		.unwrap();
		assert_eq!(
			write_shell(json.as_object().unwrap()),
			"declare a='it'\\''s'\ndeclare b=2\ndeclare -a d=(1 'x' '' 1  )\ndeclare -A e=(['k']=1 )\ndeclare i=-1294967296\ndeclare j=-2147483648\n"
		);
	}
}
