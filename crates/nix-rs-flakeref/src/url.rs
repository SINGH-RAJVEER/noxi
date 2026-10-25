//! URLs as CppNix 2.35.2's `ParsedURL` parses and renders them
//! (`src/libutil/url.cc`). Locked `url` attributes, flake reference strings
//! and error messages all go through this rendering, so it must not
//! normalise differently from the reference.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// The kind of host an authority names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HostType {
	Name,
	IPv4,
	IPv6,
	IPvFuture,
}

/// `ParsedURL::Authority`: decoded user, password and host.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Authority {
	pub host_type: HostType,
	pub host: String,
	pub user: Option<String>,
	pub password: Option<String>,
	pub port: Option<u16>,
}

impl Default for Authority {
	fn default() -> Self {
		Self {
			host_type: HostType::Name,
			host: String::new(),
			user: None,
			password: None,
			port: None,
		}
	}
}

impl fmt::Display for Authority {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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

/// A URL split into decoded components. `path` holds the decoded segments
/// between slashes; an absolute path starts with an empty segment.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ParsedUrl {
	pub scheme: String,
	pub authority: Option<Authority>,
	pub path: Vec<String>,
	pub query: BTreeMap<String, String>,
	pub fragment: String,
}

impl ParsedUrl {
	/// The non-empty path segments, as `pathSegments(skipEmpty = true)`.
	pub fn path_segments(&self) -> impl Iterator<Item = &str> {
		self.path
			.iter()
			.map(String::as_str)
			.filter(|segment| !segment.is_empty())
	}

	/// The path with each segment percent-encoded.
	pub fn render_path(&self) -> String {
		encode_url_path(&self.path)
	}
}

impl fmt::Display for ParsedUrl {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}:", self.scheme)?;
		if let Some(authority) = &self.authority {
			write!(f, "//{authority}")?;
		}
		f.write_str(&encode_url_path(&self.path))?;
		if !self.query.is_empty() {
			write!(f, "?{}", encode_query(&self.query))?;
		}
		if !self.fragment.is_empty() {
			write!(f, "#{}", percent_encode(&self.fragment, ""))?;
		}
		Ok(())
	}
}

/// The `(application+)?transport` parts of a scheme.
pub fn parse_url_scheme(scheme: &str) -> (Option<&str>, &str) {
	match scheme.split_once('+') {
		Some((application, transport)) => (Some(application), transport),
		None => (None, scheme),
	}
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

/// Percent-encode every byte that is neither unreserved nor in `keep`.
pub fn percent_encode(s: &str, keep: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for &c in s.as_bytes() {
		if is_unreserved(c) || keep.as_bytes().contains(&c) {
			out.push(c as char);
		} else {
			out.push_str(&format!("%{c:02X}"));
		}
	}
	out
}

fn hex_value(c: u8) -> Option<u8> {
	match c {
		b'0'..=b'9' => Some(c - b'0'),
		b'a'..=b'f' => Some(c - b'a' + 10),
		b'A'..=b'F' => Some(c - b'A' + 10),
		_ => None,
	}
}

fn decode_bytes(input: &str) -> Option<Vec<u8>> {
	let bytes = input.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'%' {
			let high = hex_value(*bytes.get(i + 1)?)?;
			let low = hex_value(*bytes.get(i + 2)?)?;
			out.push(high << 4 | low);
			i += 3;
		} else {
			out.push(bytes[i]);
			i += 1;
		}
	}
	Some(out)
}

/// Decode `%XX` escapes.
pub fn percent_decode(input: &str) -> Result<String, Error> {
	decode_bytes(input)
		.map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
		.ok_or_else(|| Error::bad_url(format!("invalid URI parameter '{input}': leftover input")))
}

fn percent_encode_set(s: &str, set: &[u8]) -> String {
	let mut out = String::with_capacity(s.len());
	for c in s.chars() {
		if c.is_ascii() && set.contains(&(c as u8)) {
			out.push_str(&format!("%{:02X}", c as u8));
		} else {
			out.push(c);
		}
	}
	out
}

const EXTRA_ALLOWED_IN_QUERY: &[u8] = b" \"";
const EXTRA_ALLOWED_IN_FRAGMENT: &[u8] = b" \"^";
const ALLOWED_IN_QUERY: &str = ":@/?";
const ALLOWED_IN_PATH: &str = ":@";

pub fn encode_url_path(path: &[String]) -> String {
	path.iter()
		.map(|segment| percent_encode(segment, ALLOWED_IN_PATH))
		.collect::<Vec<_>>()
		.join("/")
}

pub fn encode_query(query: &BTreeMap<String, String>) -> String {
	query
		.iter()
		.map(|(name, value)| {
			format!(
				"{}={}",
				percent_encode(name, ALLOWED_IN_QUERY),
				percent_encode(value, ALLOWED_IN_QUERY)
			)
		})
		.collect::<Vec<_>>()
		.join("&")
}

/// Decode a query string; the first value of a repeated key wins, and keys
/// without `=` are ignored, as in CppNix's `decodeQuery`.
pub fn decode_query(query: &str, lenient: bool) -> Result<BTreeMap<String, String>, Error> {
	let fixed;
	let query = if lenient {
		fixed = percent_encode_set(query, EXTRA_ALLOWED_IN_QUERY);
		fixed.as_str()
	} else {
		query
	};
	let mut result = BTreeMap::new();
	if query.is_empty() {
		return Ok(result);
	}
	for param in query.split('&') {
		let Some((key, value)) = param.split_once('=') else {
			continue;
		};
		let invalid = || Error::bad_url(format!("invalid URI query '{query}'"));
		let key = decode_bytes(key).ok_or_else(invalid)?;
		let value = decode_bytes(value).ok_or_else(invalid)?;
		result
			.entry(String::from_utf8_lossy(&key).into_owned())
			.or_insert_with(|| String::from_utf8_lossy(&value).into_owned());
	}
	Ok(result)
}

fn valid_component(s: &str, extra: &[u8]) -> bool {
	let bytes = s.as_bytes();
	let mut i = 0;
	while i < bytes.len() {
		let c = bytes[i];
		if c == b'%' {
			if bytes.len() < i + 3
				|| hex_value(bytes[i + 1]).is_none()
				|| hex_value(bytes[i + 2]).is_none()
			{
				return false;
			}
			i += 3;
			continue;
		}
		if !(is_pchar(c) || extra.contains(&c)) {
			return false;
		}
		i += 1;
	}
	true
}

fn is_valid_scheme(s: &str) -> bool {
	let mut bytes = s.bytes();
	matches!(bytes.next(), Some(c) if c.is_ascii_alphabetic())
		&& bytes.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
}

fn parse_ipv4(s: &str) -> bool {
	let parts: Vec<_> = s.split('.').collect();
	parts.len() == 4
		&& parts.iter().all(|part| {
			!part.is_empty()
				&& part.len() <= 3
				&& part.bytes().all(|c| c.is_ascii_digit())
				&& (part.len() == 1 || !part.starts_with('0'))
				&& part.parse::<u16>().is_ok_and(|n| n <= 255)
		})
}

fn parse_authority(encoded: &str, whole: &str) -> Result<Authority, Error> {
	let invalid = || Error::bad_url(format!("'{whole}' is not a valid URL: invalid authority"));
	let (userinfo, hostport) = match encoded.split_once('@') {
		Some((userinfo, hostport)) => {
			if hostport.contains('@') || !valid_component(userinfo, b"") || userinfo.contains('@') {
				return Err(invalid());
			}
			(Some(userinfo), hostport)
		}
		None => (None, encoded),
	};
	let (host_type, host, port) = if let Some(rest) = hostport.strip_prefix('[') {
		let (literal, after) = rest.split_once(']').ok_or_else(invalid)?;
		let port = match after {
			"" => None,
			_ => Some(after.strip_prefix(':').ok_or_else(invalid)?),
		};
		let host_type = if literal.starts_with(['v', 'V']) {
			HostType::IPvFuture
		} else {
			HostType::IPv6
		};
		(host_type, literal.to_owned(), port)
	} else {
		let (host, port) = match hostport.split_once(':') {
			Some((host, port)) => (host, Some(port)),
			None => (hostport, None),
		};
		if !host
			.bytes()
			.all(|c| is_unreserved(c) || is_sub_delim(c) || c == b'%')
			|| !valid_component(host, b"")
		{
			return Err(invalid());
		}
		if parse_ipv4(host) {
			(HostType::IPv4, host.to_owned(), port)
		} else {
			(
				HostType::Name,
				percent_decode(host).map_err(|_| invalid())?,
				port,
			)
		}
	};
	let port = match port {
		None | Some("") => None,
		Some(port) => {
			if !port.bytes().all(|c| c.is_ascii_digit()) {
				return Err(invalid());
			}
			match port.parse::<u16>() {
				Ok(n) if n != 0 => Some(n),
				_ => return Err(Error::bad_url(format!("port '{port}' is invalid"))),
			}
		}
	};
	let (user, password) = match userinfo {
		None => (None, None),
		Some(userinfo) => match userinfo.split_once(':') {
			Some((user, password)) => {
				(Some(percent_decode(user)?), Some(percent_decode(password)?))
			}
			None => (Some(percent_decode(userinfo)?), None),
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

/// Parse an absolute URL. `lenient` accepts unescaped spaces and quotes in
/// the query and fragment, as flake references do.
pub fn parse_url(url: &str, lenient: bool) -> Result<ParsedUrl, Error> {
	let fixed;
	let text = if lenient {
		let mut out = String::new();
		let mut view = url;
		if let Some((before, rest)) = view.split_once('?') {
			out.push_str(before);
			out.push('?');
			let end = rest.find('#').unwrap_or(rest.len());
			out.push_str(&percent_encode_set(&rest[..end], EXTRA_ALLOWED_IN_QUERY));
			view = &rest[end..];
		}
		if let Some((before, rest)) = view.split_once('#') {
			out.push_str(before);
			out.push('#');
			out.push_str(&percent_encode_set(rest, EXTRA_ALLOWED_IN_FRAGMENT));
		} else {
			out.push_str(view);
		}
		fixed = out;
		fixed.as_str()
	} else {
		url
	};

	let invalid = |what: &str| Error::bad_url(format!("'{url}' is not a valid URL: {what}"));

	let (before_fragment, fragment) = match text.split_once('#') {
		Some((before, fragment)) => (before, Some(fragment)),
		None => (text, None),
	};
	let (before_query, query) = match before_fragment.split_once('?') {
		Some((before, query)) => (before, Some(query)),
		None => (before_fragment, None),
	};
	let scheme_end = before_query
		.find(|c: char| matches!(c, ':' | '/'))
		.filter(|&i| before_query.as_bytes()[i] == b':');
	let Some(scheme_end) = scheme_end else {
		return Err(Error::bad_url(format!("'{url}' doesn't have a scheme")));
	};
	let scheme = &before_query[..scheme_end];
	if !is_valid_scheme(scheme) {
		return Err(Error::bad_url(format!("'{url}' doesn't have a scheme")));
	}
	let hier = &before_query[scheme_end + 1..];
	let (authority, encoded_path) = if let Some(rest) = hier.strip_prefix("//") {
		let end = rest.find('/').unwrap_or(rest.len());
		(Some(parse_authority(&rest[..end], url)?), &rest[end..])
	} else {
		(None, hier)
	};
	if !valid_component(encoded_path, b"/") {
		return Err(invalid("invalid path"));
	}
	if let Some(query) = query
		&& !valid_component(query, b"/?")
	{
		return Err(invalid("invalid query"));
	}
	if let Some(fragment) = fragment
		&& !valid_component(fragment, b"/?")
	{
		return Err(invalid("invalid fragment"));
	}

	let transport_is_file = parse_url_scheme(scheme).1 == "file";
	if let Some(authority) = &authority
		&& !authority.host.is_empty()
		&& transport_is_file
	{
		return Err(Error::bad_url(format!(
			"file:// URL '{url}' has unexpected authority '{authority}'"
		)));
	}
	let encoded_path = if transport_is_file && encoded_path.is_empty() {
		"/"
	} else {
		encoded_path
	};
	let path = encoded_path
		.split('/')
		.map(percent_decode)
		.collect::<Result<Vec<_>, _>>()?;

	Ok(ParsedUrl {
		scheme: scheme.to_owned(),
		authority,
		path,
		query: decode_query(query.unwrap_or(""), false)?,
		fragment: percent_decode(fragment.unwrap_or(""))?,
	})
}

/// `pathToUrlPath`: an absolute path starts with an empty segment, and a
/// trailing separator leaves an empty last segment.
pub fn path_to_url_path(path: &Path) -> Vec<String> {
	let text = path.to_string_lossy();
	if text.is_empty() {
		return vec![String::new()];
	}
	let mut segments: Vec<String> = Vec::new();
	let absolute = text.starts_with('/');
	if absolute {
		segments.push(String::new());
	}
	let components: Vec<&str> = text.split('/').filter(|c| !c.is_empty()).collect();
	segments.extend(components.iter().map(|c| (*c).to_owned()));
	// `std::filesystem::path::filename()` is empty after a trailing slash.
	if text.ends_with('/') {
		segments.push(String::new());
	}
	segments
}

/// `urlPathToPath`.
pub fn url_path_to_path(path: &[String]) -> Result<PathBuf, Error> {
	for segment in path {
		if segment.contains('/') {
			return Err(Error::bad_url(format!(
				"URL path component '{segment}' contains '/', which is not allowed in file names"
			)));
		}
		if segment.contains('\0') {
			return Err(Error::bad_url(format!(
				"URL path component '{}' contains NUL byte which is not allowed",
				segment.replace('\0', "\u{2400}")
			)));
		}
	}
	let mut result = PathBuf::new();
	let mut segments = path.iter();
	let mut rest: Vec<&str> = Vec::new();
	match segments.next() {
		None => return Ok(result),
		Some(first) if first.is_empty() => result.push("/"),
		Some(first) => rest.push(first),
	}
	rest.extend(segments.map(String::as_str));
	let mut text = result.to_string_lossy().into_owned();
	for segment in rest {
		// `operator/=` inserts a separator unless the left side already ends
		// with one; an empty right side appends just the separator.
		if !text.is_empty() && !text.ends_with('/') {
			text.push('/');
		}
		text.push_str(segment);
	}
	result = PathBuf::from(text);
	Ok(result)
}

/// `baseNameOf`, on a string.
pub fn base_name_of(path: &str) -> &str {
	if path.is_empty() {
		return "";
	}
	let bytes = path.as_bytes();
	let mut last = bytes.len() - 1;
	if bytes[last] == b'/' && last > 0 {
		last -= 1;
	}
	let start = path[..=last].rfind('/').map_or(0, |i| i + 1);
	&path[start..=last]
}

/// Parse an SCP-like Git location (`[user@]host:path`), as CppNix's
/// `tryParseScpStyle`. Relative paths become absolute, with a warning.
fn try_parse_scp_style(url: &str) -> Result<Option<ParsedUrl>, Error> {
	if url.contains("://") {
		return Ok(None);
	}
	let Some(first_colon) = url.find(':') else {
		return Ok(None);
	};
	let mut scheme_or_host = &url[..first_colon];
	if scheme_or_host.contains('/') {
		return Ok(None);
	}
	const GIT_SCHEMES: &[&str] = &["ssh", "http", "https", "file", "ftp", "ftps", "git"];
	if GIT_SCHEMES
		.iter()
		.any(|s| scheme_or_host == *s || scheme_or_host.strip_prefix("git+") == Some(s))
	{
		return Ok(None);
	}
	let bracket_start = if let Some(at) = url.find("@[") {
		Some(at + 1)
	} else if url.starts_with('[') {
		Some(0)
	} else {
		None
	};
	if let Some(start) = bracket_start
		&& let Some(close) = url[start + 1..].find(']').map(|i| i + start + 1)
	{
		if url.as_bytes().get(close + 1) == Some(&b':') {
			scheme_or_host = &url[..close + 1];
		} else {
			return Ok(None);
		}
	}
	let path_view = &url[scheme_or_host.len() + 1..];
	let (user, host) = match scheme_or_host.split_once('@') {
		Some((user, host)) => (Some(user.to_owned()), host),
		None => (None, scheme_or_host),
	};
	let mut authority = if parse_ipv4(host) {
		Authority {
			host_type: HostType::IPv4,
			host: host.to_owned(),
			..Authority::default()
		}
	} else if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
		if inner.parse::<std::net::Ipv6Addr>().is_err() {
			return Err(Error::bad_url(format!(
				"Git SCP bracketed URL is not valid: '{inner}' is not a valid IPv6 address"
			)));
		}
		Authority {
			host_type: HostType::IPv6,
			host: inner.parse::<std::net::Ipv6Addr>().unwrap().to_string(),
			..Authority::default()
		}
	} else {
		Authority {
			host: host.to_owned(),
			..Authority::default()
		}
	};
	authority.user = user;
	if path_view.is_empty() {
		return Err(Error::bad_url(format!(
			"SCP-style Git URL '{url}' has an empty path"
		)));
	}
	let mut res = ParsedUrl {
		scheme: "ssh".into(),
		authority: Some(authority),
		path: path_view.split('/').map(str::to_owned).collect(),
		..ParsedUrl::default()
	};
	if !res.path[0].is_empty() {
		res.path.insert(0, String::new());
		if !res.path[1].starts_with('~') {
			let mut tilde = res.clone();
			tilde.path.insert(1, "~".into());
			crate::warn(&format!(
				"SCP-style URL '{url}' has a relative path which cannot be faithfully converted to an SSH URL: we will convert it to an absolute path when making an SSH URL.\n\nUse an explicit absolute path:\n\n    {host}:/{path_view}\n\nor with an explicit SSH URL:\n\n    {res}\n\ninstead, to explicitly use an absolute path (and silence this warning). Or, to be relative to the remote user's home directory:\n\n    {host}:~/{path_view}\n\nor with an explicit SSH URL:\n\n    {tilde}",
				host = scheme_or_host
			));
		}
	}
	Ok(Some(res))
}

/// `fixGitURL`: local paths become `file://` URLs, SCP-like locations
/// become `ssh://` URLs, and a `git+` application prefix is dropped.
pub fn fix_git_url(url: &str) -> Result<ParsedUrl, Error> {
	if url.starts_with('/') {
		return Ok(ParsedUrl {
			scheme: "file".into(),
			authority: Some(Authority::default()),
			path: path_to_url_path(Path::new(url)),
			..ParsedUrl::default()
		});
	}
	if let Some(parsed) = try_parse_scp_style(url)? {
		return Ok(parsed);
	}
	let mut parsed = parse_url(url, false)?;
	if let (Some("git"), transport) = parse_url_scheme(&parsed.scheme) {
		parsed.scheme = transport.to_owned();
	}
	Ok(parsed)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn roundtrip(url: &str) -> String {
		parse_url(url, true).unwrap().to_string()
	}

	#[test]
	fn renders_like_cppnix() {
		assert_eq!(roundtrip("path:/tmp/x"), "path:/tmp/x");
		assert_eq!(roundtrip("file:///tmp/x"), "file:///tmp/x");
		assert_eq!(roundtrip("file:/tmp/x"), "file:/tmp/x");
		assert_eq!(roundtrip("github:NixOS/nixpkgs"), "github:NixOS/nixpkgs");
		assert_eq!(
			roundtrip("https://example.org/a%20b?z=1&a=2&a=3#frag"),
			"https://example.org/a%20b?a=2&z=1#frag"
		);
		assert_eq!(roundtrip("https://e.org/?q=a b"), "https://e.org/?q=a%20b");
		assert_eq!(
			roundtrip("ssh://user@host:2222/repo"),
			"ssh://user@host:2222/repo"
		);
		assert_eq!(roundtrip("path:./go"), "path:./go");
	}

	#[test]
	fn rejects_what_boost_rejects() {
		assert!(parse_url("nothing", false).is_err());
		assert!(parse_url("a b:c", false).is_err());
		assert!(parse_url("http://e.org/a b", false).is_err());
		assert!(parse_url("file://host/x", false).is_err());
		assert!(parse_url("http://e.org/%zz", false).is_err());
	}

	#[test]
	fn fixes_git_urls() {
		assert_eq!(
			fix_git_url("/tmp/repo").unwrap().to_string(),
			"file:///tmp/repo"
		);
		assert_eq!(
			fix_git_url("git@github.com:/NixOS/nix")
				.unwrap()
				.to_string(),
			"ssh://git@github.com/NixOS/nix"
		);
		assert_eq!(
			fix_git_url("git+https://example.org/x")
				.unwrap()
				.to_string(),
			"https://example.org/x"
		);
	}

	#[test]
	fn converts_paths() {
		assert_eq!(path_to_url_path(Path::new("/a/b")), ["", "a", "b"]);
		assert_eq!(path_to_url_path(Path::new("/")), ["", ""]);
		assert_eq!(path_to_url_path(Path::new("./a")), [".", "a"]);
		assert_eq!(
			url_path_to_path(&["".into(), "a".into(), "b".into()]).unwrap(),
			PathBuf::from("/a/b")
		);
		assert_eq!(base_name_of("https://e.org/a/b.tar.gz"), "b.tar.gz");
		assert_eq!(base_name_of("https://e.org/a/"), "a");
	}
}
