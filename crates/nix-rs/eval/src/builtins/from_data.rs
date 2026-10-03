//! `builtins.fromJSON` and `builtins.fromTOML` values, after CppNix 2.35.2's
//! `json-to-value.cc` and `primops/fromTOML.cc`.

use bstr::ByteSlice;

use crate::errors::ErrorKind;
use crate::value::{NixAttrs, NixList, NixString, Value};

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
