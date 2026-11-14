//! `builtins.fromJSON` and `builtins.fromTOML` values, after CppNix 2.35.2's
//! `json-to-value.cc` and `primops/fromTOML.cc`.

use bstr::ByteSlice;

use crate::errors::ErrorKind;
use crate::value::{NixAttrs, NixList, NixString, Value};

mod json;
pub(crate) use json::parse_json;

/// Translate structured TOML parser failures to the pinned toml11 renderer.
/// Keep the source ranges rather than depending on serde's display wording.
pub(crate) fn toml_error(input: &str, error: toml::de::Error) -> ErrorKind {
	let range = error.span().unwrap_or(0..0);
	let before = &input[..range.start.min(input.len())];
	let line = before.bytes().filter(|&b| b == b'\n').count() + 1;
	let line_start = before.rfind('\n').map_or(0, |i| i + 1);
	let text = input.lines().nth(line - 1).unwrap_or("");
	let column = range.start - line_start;
	let point = |message: &str, column: usize| {
		ErrorKind::FromTomlError(format!(
			"while parsing TOML: [error] {message}\n --> fromTOML\n   |\n {line} | {text}\n   | {}^-- here\n",
			" ".repeat(column)
		))
	};
	let message = error.message();
	if message == "invalid key" {
		return ErrorKind::FromTomlError(format!(
			"while parsing TOML: [error] toml::parse_simple_key: invalid key: key must be \"quoted\", 'quoted-literal', or bare key.\n --> fromTOML\n   |\n {line} | {text}\n   | {}^-- expected alpha [a-zA-Z], digit [0-9], -, or _\nHint: non-ASCII scripts are allowed in toml v1.1.0, but not in v1.0.0.\n",
			" ".repeat(column)
		));
	}

	if message.contains("expected `.`") && message.contains("`=`") && !text.contains('=') {
		return ErrorKind::FromTomlError(format!(
			"while parsing TOML: [error] toml::parse_key_value_pair: invalid key value separator `=`\n --> fromTOML\n   |\n {line} | {text}\n   | {}^-- expected =\n",
			" ".repeat(text.len())
		));
	}
	if matches!(
		message,
		"number too large to fit in target type" | "number too small to fit in target type"
	) {
		let token = text[column..]
			.split(|c: char| !(c.is_ascii_digit() || c == '-' || c == '+' || c == '_'))
			.next()
			.unwrap_or("");
		return ErrorKind::FromTomlError(format!(
			"while parsing TOML: [error] toml::parse_dec_integer: too large integer: current max digits = 2^63\n --> fromTOML\n   |\n {line} | {text}\n   | {}^-- must be < 2^63\n",
			" ".repeat(column + token.len())
		));
	}
	if message.contains("floating-point") || message.contains("float") {
		return point(
			"toml::parse_floating: failed to read floating point value from stream",
			text.len(),
		);
	}
	if let Some(start) = input.find("\\u")
		&& input
			.get(start + 2..start + 6)
			.and_then(|code| u16::from_str_radix(code, 16).ok())
			.is_some_and(|code| (0xd800..=0xdfff).contains(&code))
	{
		return ErrorKind::FromTomlError(format!(
			"while parsing TOML: [error] toml::parse_utf8_codepoint: [0xD800, 0xDFFF] is not a valid UTF-8\n --> internal temporary\n   |\n 1 | {}\n   |  ^^^^^-- here\n",
			&input[start..start + 6]
		));
	}
	if message.starts_with("duplicate key") || message.starts_with("invalid table header") {
		let table = text.trim_start().starts_with('[');
		let key = text.split('=').next().unwrap_or(text).trim();
		let root = key
			.trim_matches(['[', ']'])
			.split('.')
			.next()
			.unwrap_or(key)
			.trim();
		let previous = input.lines().take(line - 1).enumerate().find(|(_, old)| {
			let old = old
				.split('=')
				.next()
				.unwrap_or(old)
				.trim()
				.trim_matches(['[', ']']);
			old.split('.').next().unwrap_or(old).trim() == root
		});
		if let Some((old_index, old)) = previous {
			let (old_column, old_length, old_hint) = if table {
				(0, old.trim().len(), "this table is explicitly defined")
			} else if old.split('=').next().is_some_and(|key| key.contains('.')) {
				(
					0,
					old.split('=').next().unwrap().trim().len(),
					"but value already exists",
				)
			} else {
				let column = old.find('=').map_or(0, |i| {
					i + 1 + old[i + 1..].len() - old[i + 1..].trim_start().len()
				});
				(
					column,
					old[column..].trim_end().len().max(1),
					"but value already exists",
				)
			};
			let reason = if table {
				"failed to insert a table, table already defined"
			} else {
				"failed to insert a value, value already exists"
			};
			return ErrorKind::FromTomlError(format!(
				"while parsing TOML: [error] toml::insert_value: {reason}\n --> fromTOML\n   |\n {line} | {text}\n   | {}{}-- inserting this\n   | ...\n   |\n {} | {old}\n   | {}{}-- {old_hint}\n",
				" ".repeat(column),
				"^".repeat(key.len().max(1)),
				old_index + 1,
				" ".repeat(old_column),
				"^".repeat(old_length)
			));
		}
	}
	if text.trim_end().ends_with('[') && message.contains("array") {
		return ErrorKind::FromTomlError(format!(
			"while parsing TOML: [error] toml::parse_array: missing closing bracket `]`\n --> fromTOML\n   |\n {line} | {text}\n   | {}--\n {} | {text}\n   | {}-- expected `]`, reached EOF\n",
			" ".repeat(text.len()),
			line + 1,
			"^".repeat(text.len() + 1)
		));
	}
	if text.trim_end().ends_with('=') {
		return point("bad format: unknown value appeared", text.len());
	}
	if message.starts_with("expected newline") {
		return point(
			"toml::parse_table: newline (LF / CRLF) or EOF is expected",
			column,
		);
	}
	error.into()
}

/// CppNix's `forceNoNullByte`: Nix strings cannot hold NUL bytes.
pub(crate) fn check_no_nul(s: &[u8]) -> Result<(), ErrorKind> {
	if s.contains(&0) {
		return Err(ErrorKind::EvalError(nul_message(s)));
	}
	Ok(())
}

/// The message of CppNix's `forceNoNullByte`, which shows each NUL byte as ␀.
pub(crate) fn nul_message(s: &[u8]) -> String {
	format!(
		"input string '{}' cannot be represented as Nix string because it contains null bytes",
		s.replace(b"\0", "␀".as_bytes()).as_bstr()
	)
}

/// A parsed JSON document as a Nix value. Integers beyond the signed 64-bit
/// range but within the unsigned one are an error, as in CppNix; larger
/// numbers are already floats.
pub(crate) fn json_to_value(json: serde_json::Value) -> Result<Value, ErrorKind> {
	Ok(match json {
		serde_json::Value::Null => Value::Null,
		serde_json::Value::Bool(b) => Value::Bool(b),
		serde_json::Value::Number(n) => {
			if let Some(i) = n.as_i64() {
				Value::Integer(i)
			} else if let Some(u) = n.as_u64() {
				return Err(ErrorKind::EvalError(format!(
					"unsigned json number {u} outside of Nix integer range"
				)));
			} else {
				Value::Float(n.as_f64().unwrap_or(f64::NAN))
			}
		}
		serde_json::Value::String(s) => {
			check_no_nul(s.as_bytes())?;
			Value::from(s)
		}
		serde_json::Value::Array(items) => {
			let items = items
				.into_iter()
				.map(json_to_value)
				.collect::<Result<Vec<_>, _>>()?;
			Value::List(NixList::from(items))
		}
		serde_json::Value::Object(map) => {
			let mut attrs = Vec::with_capacity(map.len());
			for (key, value) in map {
				check_no_nul(key.as_bytes())?;
				attrs.push((NixString::from(key), json_to_value(value)?));
			}
			Value::attrs(NixAttrs::from_iter(attrs))
		}
	})
}

/// CppNix's `forceNoNullByte` inside `fromTOML`, whose message the TOML
/// error wraps.
fn check_toml_no_nul(s: &[u8]) -> Result<(), ErrorKind> {
	if s.contains(&0) {
		return Err(ErrorKind::EvalError(format!(
			"while parsing TOML: error: {}",
			nul_message(s)
		)));
	}
	Ok(())
}

/// A parsed TOML document as a Nix value. Dates and times need the
/// `parse-toml-timestamps` experimental feature, which turns them into
/// `{ _type = "timestamp"; value = ...; }`.
pub(crate) fn toml_to_value(toml: toml::Value, timestamps: bool) -> Result<Value, ErrorKind> {
	Ok(match toml {
		toml::Value::String(s) => {
			check_toml_no_nul(s.as_bytes())?;
			Value::from(s)
		}
		toml::Value::Integer(i) => Value::Integer(i),
		toml::Value::Float(f) => Value::Float(f),
		toml::Value::Boolean(b) => Value::Bool(b),
		toml::Value::Array(items) => {
			let items = items
				.into_iter()
				.map(|item| toml_to_value(item, timestamps))
				.collect::<Result<Vec<_>, _>>()?;
			Value::List(NixList::from(items))
		}
		toml::Value::Table(table) => {
			let mut attrs = Vec::with_capacity(table.len());
			for (key, value) in table {
				check_toml_no_nul(key.as_bytes())?;
				attrs.push((NixString::from(key), toml_to_value(value, timestamps)?));
			}
			Value::attrs(NixAttrs::from_iter(attrs))
		}
		toml::Value::Datetime(datetime) if timestamps => {
			let text = format_datetime(&datetime);
			Value::attrs(NixAttrs::from_iter([
				("_type", Value::from("timestamp")),
				("value", Value::from(text)),
			]))
		}
		toml::Value::Datetime(_) => {
			return Err(ErrorKind::EvalError(
				"while parsing TOML: Dates and times are not supported".into(),
			));
		}
	})
}

/// A TOML date or time as CppNix prints it: an upper-case `T` between date
/// and time, seconds always, and 3, 6 or 9 fractional digits depending on
/// which of milliseconds, microseconds and nanoseconds are non-zero.
fn format_datetime(datetime: &toml::value::Datetime) -> String {
	let mut out = String::new();
	if let Some(date) = &datetime.date {
		out.push_str(&format!(
			"{:04}-{:02}-{:02}",
			date.year, date.month, date.day
		));
	}
	if let Some(time) = &datetime.time {
		if datetime.date.is_some() {
			out.push('T');
		}
		out.push_str(&format!(
			"{:02}:{:02}:{:02}",
			time.hour, time.minute, time.second
		));
		let nanos = time.nanosecond;
		if nanos % 1_000 != 0 {
			out.push_str(&format!(".{nanos:09}"));
		} else if nanos % 1_000_000 != 0 {
			out.push_str(&format!(".{:06}", nanos / 1_000));
		} else if nanos != 0 {
			out.push_str(&format!(".{:03}", nanos / 1_000_000));
		}
	}
	match &datetime.offset {
		Some(toml::value::Offset::Z)
		| Some(toml::value::Offset::Custom {
			hours: 0,
			minutes: 0,
		}) => out.push('Z'),
		Some(toml::value::Offset::Custom { hours, minutes }) => {
			let sign = if *hours < 0 { '-' } else { '+' };
			out.push_str(&format!("{sign}{:02}:{minutes:02}", hours.unsigned_abs()));
		}
		None => {}
	}
	out
}
