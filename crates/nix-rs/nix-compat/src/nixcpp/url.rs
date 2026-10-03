//! CppNix's URL handling (`src/libutil/url.cc`).
//!
//! CppNix parses URLs with Boost.URL, which follows RFC 3986 rather than the
//! WHATWG rules the `url` crate implements. Which strings parse decides how a
//! store reference is read, so this module reproduces RFC 3986 validation
//! with CppNix's lenient fix-ups for unescaped spaces and quotes in queries
//! and fragments.

use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter, Write as _};
use std::net::Ipv4Addr;

/// A query, CppNix's `StringMap`: sorted, and the first value of a repeated
/// key wins.
pub type Query = BTreeMap<String, String>;

/// CppNix's `BadURL`.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct BadUrl(pub String);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum HostType {
	Name,
	IPv4,
	IPv6,
	IPvFuture,
}

/// A URL authority with its components percent-decoded.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Authority {
	pub host_type: HostType,
	/// The host without brackets. An IPv6 zone follows a single `%`.
	pub host: String,
	pub user: Option<String>,
	pub password: Option<String>,
	pub port: Option<u16>,
}

/// A parsed URL, CppNix's `ParsedURL`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedUrl {
	pub scheme: String,
	pub authority: Option<Authority>,
	/// The decoded path segments; an absolute path starts with an empty one.
	pub path: Vec<String>,
	pub query: Query,
	pub fragment: String,
}

fn is_unreserved(c: u8) -> bool {
	c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~')
}

fn is_sub_delim(c: u8) -> bool {
	matches!(
		c,
		b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'='
	)
}

fn is_pchar(c: u8) -> bool {
	is_unreserved(c) || is_sub_delim(c) || matches!(c, b':' | b'@')
}

/// Checks that every byte of `s` is allowed or part of a valid
/// percent-encoding.
fn valid_encoded(s: &str, allowed: impl Fn(u8) -> bool) -> bool {
	let bytes = s.as_bytes();
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'%' {
			if i + 2 >= bytes.len()
				|| !(bytes[i + 1].is_ascii_hexdigit() && bytes[i + 2].is_ascii_hexdigit())
			{
				return false;
			}
			i += 3;
		} else if allowed(bytes[i]) {
			i += 1;
		} else {
			return false;
		}
	}
	true
}

fn decode_unchecked(s: &str) -> String {
	let bytes = s.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'%' {
			let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
			out.push(u8::from_str_radix(hex, 16).unwrap());
			i += 3;
		} else {
			out.push(bytes[i]);
			i += 1;
		}
	}
	String::from_utf8_lossy(&out).into_owned()
}

/// Decodes `%XX` sequences, CppNix's `percentDecode`. A `+` is kept.
pub fn percent_decode(s: &str) -> Result<String, BadUrl> {
	if valid_encoded(s, |_| true) {
		Ok(decode_unchecked(s))
	} else {
		Err(BadUrl(format!("invalid URI parameter '{s}': leftover")))
	}
}

/// Encodes every byte other than unreserved characters and those in `keep`
/// as uppercase `%XX`, CppNix's `percentEncode`.
pub fn percent_encode(s: &str, keep: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for &c in s.as_bytes() {
		if is_unreserved(c) || keep.as_bytes().contains(&c) {
			out.push(c as char);
		} else {
			write!(out, "%{c:02X}").unwrap();
		}
	}
	out
}

/// Encodes the bytes of `s` that `encode` selects, leaving the rest as is.
fn encode_set(s: &str, encode: impl Fn(u8) -> bool) -> String {
	let mut out = String::with_capacity(s.len());
	for c in s.chars() {
		if c.is_ascii() && encode(c as u8) {
			write!(out, "%{:02X}", c as u8).unwrap();
		} else {
			out.push(c);
		}
	}
	out
}

/// Unescaped characters CppNix accepts in queries.
fn lenient_query_char(c: u8) -> bool {
	matches!(c, b' ' | b'"')
}

/// Unescaped characters CppNix accepts in fragments.
fn lenient_fragment_char(c: u8) -> bool {
	matches!(c, b' ' | b'"' | b'^')
}

fn is_query_char(c: u8) -> bool {
	is_pchar(c) || matches!(c, b'/' | b'?')
}

/// Parses a query, CppNix's `decodeQuery`. A parameter without `=` is
/// skipped with the warning CppNix prints, without its `warning: ` prefix.
pub fn decode_query(
	query: &str,
	lenient: bool,
	warnings: &mut Vec<String>,
) -> Result<Query, BadUrl> {
	let fixed;
	let encoded = if lenient {
		fixed = encode_set(query, lenient_query_char);
		fixed.as_str()
	} else {
		query
	};
	if !valid_encoded(encoded, is_query_char) {
		return Err(BadUrl(format!("invalid URI query '{query}': leftover")));
	}

	let mut result = Query::new();
	if encoded.is_empty() {
		return Ok(result);
	}
	for param in encoded.split('&') {
		match param.split_once('=') {
			Some((key, value)) => {
				result
					.entry(decode_unchecked(key))
					.or_insert_with(|| decode_unchecked(value));
			}
			None => warnings.push(format!(
				"dubious URI query '{param}' is missing equal sign '=', ignoring"
			)),
		}
	}
	Ok(result)
}

/// Writes a query, CppNix's `encodeQuery`.
pub fn encode_query(query: &Query) -> String {
	let mut out = String::new();
	for (i, (key, value)) in query.iter().enumerate() {
		if i > 0 {
			out.push('&');
		}
		out += &percent_encode(key, ":@/?");
		out.push('=');
		out += &percent_encode(value, ":@/?");
	}
	out
}

/// Writes path segments, CppNix's `encodeUrlPath`.
pub fn encode_url_path<S: AsRef<str>>(path: &[S]) -> String {
	path.iter()
		.map(|segment| percent_encode(segment.as_ref(), ":@"))
		.collect::<Vec<_>>()
		.join("/")
}

/// Whether `s` is an RFC 3986 `IPv6address`.
fn is_ipv6(s: &str) -> bool {
	// `Ipv6Addr` accepts exactly RFC 4291's text forms, which RFC 3986's
	// grammar reproduces, including a trailing dotted IPv4 address.
	s.parse::<std::net::Ipv6Addr>().is_ok()
}

/// Whether `s` is an RFC 3986 `IPv6address`, as Boost's `parse_ipv6_address`
/// accepts it.
pub fn parse_ipv6_address(s: &str) -> bool {
	is_ipv6(s)
}

fn is_ipv4(s: &str) -> bool {
	// RFC 3986's `dec-octet` has no leading zeros, as `Ipv4Addr` requires.
	s.parse::<Ipv4Addr>().is_ok()
}

/// Parses an IP literal without its brackets.
fn parse_ip_literal(inner: &str) -> Option<(HostType, String)> {
	if let Some(future) = inner.strip_prefix(['v', 'V']) {
		let (version, rest) = future.split_once('.')?;
		let valid = !version.is_empty()
			&& version.bytes().all(|c| c.is_ascii_hexdigit())
			&& !rest.is_empty()
			&& rest
				.bytes()
				.all(|c| is_unreserved(c) || is_sub_delim(c) || c == b':');
		return valid.then(|| (HostType::IPvFuture, inner.to_owned()));
	}
	match inner.split_once("%25") {
		Some((address, zone)) => {
			let valid_zone = !zone.is_empty() && valid_encoded(zone, is_unreserved);
			(is_ipv6(address) && valid_zone).then(|| {
				(
					HostType::IPv6,
					format!("{address}%{}", decode_unchecked(zone)),
				)
			})
		}
		None => is_ipv6(inner).then(|| (HostType::IPv6, inner.to_owned())),
	}
}

impl Authority {
	/// Parses an encoded authority, CppNix's `ParsedURL::Authority::parse`.
	pub fn parse(encoded: &str) -> Result<Authority, BadUrl> {
		let invalid = || BadUrl(format!("invalid URL authority: '{encoded}': leftover"));

		let (userinfo, host_port) = match encoded.split_once('@') {
			Some((userinfo, rest)) => (Some(userinfo), rest),
			None => (None, encoded),
		};
		let (user, password) = match userinfo {
			Some(userinfo) => {
				if !valid_encoded(userinfo, |c| {
					is_unreserved(c) || is_sub_delim(c) || c == b':'
				}) {
					return Err(invalid());
				}
				match userinfo.split_once(':') {
					Some((user, password)) => (
						Some(decode_unchecked(user)),
						Some(decode_unchecked(password)),
					),
					None => (Some(decode_unchecked(userinfo)), None),
				}
			}
			None => (None, None),
		};

		let (host_type, host, port) = if let Some(literal) = host_port.strip_prefix('[') {
			let (inner, rest) = literal.split_once(']').ok_or_else(invalid)?;
			let (host_type, host) = parse_ip_literal(inner).ok_or_else(invalid)?;
			let port = match rest {
				"" => None,
				rest => Some(rest.strip_prefix(':').ok_or_else(invalid)?),
			};
			(host_type, host, port)
		} else {
			let (host, port) = match host_port.split_once(':') {
				Some((host, port)) => (host, Some(port)),
				None => (host_port, None),
			};
			if !valid_encoded(host, |c| is_unreserved(c) || is_sub_delim(c)) {
				return Err(invalid());
			}
			if is_ipv4(host) {
				(HostType::IPv4, host.to_owned(), port)
			} else {
				(HostType::Name, decode_unchecked(host), port)
			}
		};

		let port = match port {
			None | Some("") => None,
			Some(port) if !port.bytes().all(|c| c.is_ascii_digit()) => return Err(invalid()),
			Some(port) => match port.parse::<u16>() {
				Ok(number) if number != 0 => Some(number),
				_ => return Err(BadUrl(format!("port '{port}' is invalid"))),
			},
		};

		Ok(Authority {
			host_type,
			host,
			user,
			password,
			port,
		})
	}
}

impl Display for Authority {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		if let Some(user) = &self.user {
			f.write_str(&percent_encode(user, ""))?;
			if let Some(password) = &self.password {
				write!(f, ":{}", percent_encode(password, ""))?;
			}
			f.write_str("@")?;
		}
		match self.host_type {
			HostType::Name => f.write_str(&percent_encode(&self.host, ""))?,
			HostType::IPv4 => f.write_str(&self.host)?,
			// The zone's `%` is encoded again.
			HostType::IPv6 | HostType::IPvFuture => {
				write!(f, "[{}]", percent_encode(&self.host, ":"))?
			}
		}
		if let Some(port) = self.port {
			write!(f, ":{port}")?;
		}
		Ok(())
	}
}

fn is_scheme(s: &str) -> bool {
	let mut bytes = s.bytes();
	bytes.next().is_some_and(|c| c.is_ascii_alphabetic())
		&& bytes.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
}

/// Parses a URL, CppNix's `parseURL`. With `lenient`, unescaped spaces and
/// quotes are accepted in the query, and those and `^` in the fragment.
pub fn parse_url(url: &str, lenient: bool) -> Result<ParsedUrl, BadUrl> {
	let mut warnings = Vec::new();
	parse_url_with_warnings(url, lenient, &mut warnings)
}

/// [`parse_url`], collecting CppNix's warnings about the query.
pub fn parse_url_with_warnings(
	url: &str,
	lenient: bool,
	warnings: &mut Vec<String>,
) -> Result<ParsedUrl, BadUrl> {
	let invalid = || BadUrl(format!("'{url}' is not a valid URL: leftover"));

	let (rest, fragment) = match url.split_once('#') {
		Some((rest, fragment)) => (rest, Some(fragment)),
		None => (url, None),
	};
	let (rest, query) = match rest.split_once('?') {
		Some((rest, query)) => (rest, Some(query)),
		None => (rest, None),
	};

	let fragment = match fragment {
		Some(fragment) => {
			let fixed = if lenient {
				encode_set(fragment, lenient_fragment_char)
			} else {
				fragment.to_owned()
			};
			if !valid_encoded(&fixed, is_query_char) {
				return Err(invalid());
			}
			decode_unchecked(&fixed)
		}
		None => String::new(),
	};
	if let Some(query) = query {
		let fixed = if lenient {
			encode_set(query, lenient_query_char)
		} else {
			query.to_owned()
		};
		if !valid_encoded(&fixed, is_query_char) {
			return Err(invalid());
		}
	}

	// A scheme ends at the first `:`; a relative reference has none before
	// its first `/`.
	let scheme = rest
		.split_once(':')
		.map(|(scheme, _)| scheme)
		.filter(|scheme| !scheme.contains('/'));
	let Some(scheme) = scheme.filter(|scheme| is_scheme(scheme)) else {
		// A relative reference's first segment cannot contain `:`.
		let first = rest.split('/').next().unwrap_or("");
		if first.contains(':') || !valid_encoded(rest, |c| is_pchar(c) || c == b'/') {
			return Err(invalid());
		}
		return Err(BadUrl(format!("'{url}' doesn't have a scheme")));
	};
	let hier = &rest[scheme.len() + 1..];

	let (authority, encoded_path) = match hier.strip_prefix("//") {
		Some(after) => {
			let end = after.find('/').unwrap_or(after.len());
			// Boost's grammar accepts any port digits; CppNix rejects the
			// unrepresentable ones afterwards with its own message.
			let authority = Authority::parse(&after[..end]).map_err(|error| {
				if error.0.starts_with("port ") {
					error
				} else {
					invalid()
				}
			})?;
			(Some(authority), &after[end..])
		}
		None => (None, hier),
	};
	if !valid_encoded(encoded_path, |c| is_pchar(c) || c == b'/') {
		return Err(invalid());
	}

	let transport = scheme
		.rsplit_once('+')
		.map_or(scheme, |(_, transport)| transport);
	let is_file = transport == "file";
	if let Some(authority) = &authority
		&& is_file
		&& !authority.host.is_empty()
	{
		return Err(BadUrl(format!(
			"file:// URL '{url}' has unexpected authority '{authority}'"
		)));
	}
	let encoded_path = if is_file && encoded_path.is_empty() {
		"/"
	} else {
		encoded_path
	};

	Ok(ParsedUrl {
		scheme: scheme.to_owned(),
		authority,
		path: encoded_path.split('/').map(decode_unchecked).collect(),
		query: decode_query(query.unwrap_or(""), lenient, warnings)?,
		fragment,
	})
}

impl ParsedUrl {
	/// The authority and path, CppNix's `renderAuthorityAndPath`.
	pub fn render_authority_and_path(&self) -> String {
		let mut out = match &self.authority {
			Some(authority) => authority.to_string(),
			None => String::new(),
		};
		out += &encode_url_path(&self.path);
		out
	}
}

impl Display for ParsedUrl {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		write!(f, "{}:", self.scheme)?;
		if self.authority.is_some() {
			f.write_str("//")?;
		}
		f.write_str(&self.render_authority_and_path())?;
		if !self.query.is_empty() {
			write!(f, "?{}", encode_query(&self.query))?;
		}
		if !self.fragment.is_empty() {
			write!(f, "#{}", percent_encode(&self.fragment, ""))?;
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn percent_encoding_matches_cppnix() {
		assert_eq!(percent_encode("/foo bar/baz", ":@/?"), "/foo%20bar/baz");
		assert_eq!(percent_encode("a/b", ":@"), "a%2Fb");
		assert_eq!(percent_encode("é", ""), "%C3%A9");
		assert_eq!(percent_decode("a%2fb+c").unwrap(), "a/b+c");
		assert!(percent_decode("%zz").is_err());
		assert!(percent_decode("%4").is_err());
	}

	#[test]
	fn queries_keep_the_first_value_and_skip_bare_keys() {
		let mut warnings = Vec::new();
		let query = decode_query("b=2&a=1&a=3&c", false, &mut warnings).unwrap();
		assert_eq!(
			query,
			Query::from([("a".into(), "1".into()), ("b".into(), "2".into())])
		);
		assert_eq!(
			warnings,
			["dubious URI query 'c' is missing equal sign '=', ignoring"]
		);
		assert!(decode_query("", false, &mut warnings).unwrap().is_empty());
		assert_eq!(warnings.len(), 1);
		assert!(decode_query("a=b c", false, &mut warnings).is_err());
		assert_eq!(
			decode_query("a=b c\"", true, &mut warnings).unwrap()["a"],
			"b c\""
		);
		assert_eq!(
			decode_query("a=%zz", true, &mut warnings).unwrap_err().0,
			"invalid URI query 'a=%zz': leftover"
		);
	}

	#[test]
	fn authorities_parse_as_boost_does() {
		let authority = Authority::parse("u%40:p@x%41y:22").unwrap();
		assert_eq!(authority.user.as_deref(), Some("u@"));
		assert_eq!(authority.password.as_deref(), Some("p"));
		assert_eq!(authority.host, "xAy");
		assert_eq!(authority.port, Some(22));
		assert_eq!(authority.to_string(), "u%40:p@xAy:22");

		let zoned = Authority::parse("[fe80::1%25eth0]").unwrap();
		assert_eq!(zoned.host_type, HostType::IPv6);
		assert_eq!(zoned.host, "fe80::1%eth0");
		assert_eq!(zoned.to_string(), "[fe80::1%25eth0]");

		assert_eq!(
			Authority::parse("1.2.3.4").unwrap().host_type,
			HostType::IPv4
		);
		assert_eq!(
			Authority::parse("1.2.3.999").unwrap().host_type,
			HostType::Name
		);
		assert_eq!(
			Authority::parse("[v1.x]").unwrap().host_type,
			HostType::IPvFuture
		);
		assert_eq!(Authority::parse("h:").unwrap().port, None);
		for bad in [
			"::1",
			"[::1",
			"[fe80::1%eth0]",
			"h:0",
			"h:99999",
			"h:1x",
			"a b",
			"a@b@c",
		] {
			assert!(Authority::parse(bad).is_err(), "{bad}");
		}
	}

	#[test]
	fn urls_parse_and_render() {
		let url = parse_url("local:///foo/bar%20baz?trusted=true", true).unwrap();
		assert_eq!(url.path, ["", "foo", "bar baz"]);
		assert_eq!(url.render_authority_and_path(), "/foo/bar%20baz");
		assert_eq!(url.to_string(), "local:///foo/bar%20baz?trusted=true");

		let url = parse_url("dummy:///a%2Fb/c", false).unwrap();
		assert_eq!(url.render_authority_and_path(), "/a%2Fb/c");

		let url = parse_url("file://", false).unwrap();
		assert_eq!(url.path, ["", ""]);
		assert!(parse_url("file://host/x", false).is_err());

		assert!(parse_url("local://?root=/foo bar", true).is_ok());
		assert!(parse_url("local://?root=/foo bar", false).is_err());
		assert!(parse_url("x://h#a^b", true).is_ok());
		assert!(parse_url("x://h#a^b", false).is_err());
		for bad in [
			"auto",
			"/foo/bar",
			"daemon?x=1",
			"C:\\foo",
			"ssh://::1",
			"1x://h",
		] {
			assert!(parse_url(bad, true).is_err(), "{bad}");
		}
	}
}
