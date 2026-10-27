//! Store references such as `auto`, `daemon`, `local?root=/x` or
//! `ssh://host`, CppNix's `StoreReference` (`src/libstore/store-reference.cc`).

use std::fmt::{self, Display, Formatter};
use std::path::Path;

use super::url::{self, BadUrl, Query};

/// Which store a reference names.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Variant {
	/// `auto`, or an empty string.
	Auto,
	/// A URL-like reference, `scheme://authority`.
	Specified { scheme: String, authority: String },
	/// `daemon` without parameters. It means `unix://` but compares unequal
	/// to it, as in CppNix.
	Daemon,
	/// `local` without parameters. It means `local://` but compares unequal
	/// to it, as in CppNix.
	Local,
}

/// A parsed store reference with its parameters.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct StoreReference {
	pub variant: Variant,
	pub params: Query,
}

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum Error {
	/// CppNix's `BadURL`, from a malformed query.
	#[error(transparent)]
	BadUrl(#[from] BadUrl),
	/// CppNix's `UsageError`.
	#[error("Cannot parse Nix store '{0}'")]
	Unparsable(String),
}

/// Whether `spec` is a filesystem path rather than a URL or a single word
/// such as `auto`.
fn is_non_uri_path(spec: &str) -> bool {
	!spec.contains("://") && spec.contains('/')
}

/// CppNix's `absPath` without symlink resolution: `path` against `cwd`, with
/// `.`, `..` and repeated slashes removed.
fn abs_path(path: &str, cwd: &Path) -> String {
	let joined = if path.starts_with('/') {
		path.to_owned()
	} else {
		format!("{}/{path}", cwd.to_string_lossy())
	};
	let mut components: Vec<&str> = Vec::new();
	for component in joined.split('/') {
		match component {
			"" | "." => {}
			".." => {
				components.pop();
			}
			component => components.push(component),
		}
	}
	format!("/{}", components.join("/"))
}

/// CppNix's `pathToUrlPath` for an absolute, canonical Unix path.
fn path_to_url_path(path: &str) -> Vec<String> {
	let mut segments = vec![String::new()];
	segments.extend(path.split('/').filter(|s| !s.is_empty()).map(str::to_owned));
	if path == "/" {
		segments.push(String::new());
	}
	segments
}

/// Splits a reference into the part before `?` and its parameters,
/// CppNix's `splitUriAndParams`.
pub fn split_uri_and_params(
	uri: &str,
	warnings: &mut Vec<String>,
) -> Result<(String, Query), BadUrl> {
	match uri.split_once('?') {
		Some((base, query)) => Ok((base.to_owned(), url::decode_query(query, true, warnings)?)),
		None => Ok((uri.to_owned(), Query::new())),
	}
}

impl StoreReference {
	/// Parses a store reference. Parameters in `extra_params` take
	/// precedence over the reference's own; a relative path is resolved
	/// against `cwd`. Warnings CppNix prints while parsing the query are
	/// added to `warnings`.
	pub fn parse(
		uri: &str,
		extra_params: &Query,
		cwd: &Path,
		warnings: &mut Vec<String>,
	) -> Result<StoreReference, Error> {
		let mut params = extra_params.clone();
		let add = |params: &mut Query, more: Query| {
			for (key, value) in more {
				params.entry(key).or_insert(value);
			}
		};

		let mut url_warnings = Vec::new();
		if let Ok(parsed) = url::parse_url_with_warnings(uri, true, &mut url_warnings) {
			warnings.append(&mut url_warnings);
			let authority = parsed.render_authority_and_path();
			add(&mut params, parsed.query);
			return Ok(StoreReference {
				variant: Variant::Specified {
					scheme: parsed.scheme,
					authority,
				},
				params,
			});
		}

		let (base, uri_params) = split_uri_and_params(uri, warnings)?;
		add(&mut params, uri_params);

		let specified = |scheme: &str, authority: String, params: Query| StoreReference {
			variant: Variant::Specified {
				scheme: scheme.to_owned(),
				authority,
			},
			params,
		};

		if base.is_empty() || base == "auto" {
			return Ok(StoreReference {
				variant: Variant::Auto,
				params,
			});
		}
		if base == "daemon" {
			if params.is_empty() {
				return Ok(StoreReference {
					variant: Variant::Daemon,
					params,
				});
			}
			return Ok(specified("unix", String::new(), params));
		}
		if base == "local" {
			if params.is_empty() {
				return Ok(StoreReference {
					variant: Variant::Local,
					params,
				});
			}
			return Ok(specified("local", String::new(), params));
		}
		if is_non_uri_path(&base) {
			let path = path_to_url_path(&abs_path(&base, cwd));
			return Ok(specified("local", url::encode_url_path(&path), params));
		}

		// Old versions accepted an unbracketed IPv6 address after the scheme,
		// and a zone without its `%` encoded.
		if let Some((scheme, rest)) = base.split_once(':') {
			let authority = rest.strip_prefix("//").unwrap_or(rest);
			let (userinfo, host) = match authority.split_once('@') {
				Some((userinfo, host)) => (Some(userinfo), host),
				None => (None, authority),
			};
			let host = host
				.strip_prefix('[')
				.and_then(|host| host.strip_suffix(']'))
				.unwrap_or(host);
			let (address, zone) = match host.split_once('%') {
				Some((address, zone)) => (address, Some(zone)),
				None => (host, None),
			};
			if url::parse_ipv6_address(address) {
				let mut fixed = String::new();
				if let Some(userinfo) = userinfo {
					fixed += userinfo;
					fixed.push('@');
				}
				fixed.push('[');
				fixed += address;
				if let Some(zone) = zone {
					fixed += "%25";
					fixed += zone;
				}
				fixed.push(']');
				return Ok(specified(scheme, fixed, params));
			}
		}

		Err(Error::Unparsable(uri.to_owned()))
	}

	/// Writes the reference, optionally without its parameters, CppNix's
	/// `render`.
	pub fn render(&self, with_params: bool) -> String {
		let mut out = match &self.variant {
			Variant::Auto => "auto".to_owned(),
			Variant::Daemon => "daemon".to_owned(),
			Variant::Local => "local".to_owned(),
			Variant::Specified { scheme, authority } => format!("{scheme}://{authority}"),
		};
		if with_params && !self.params.is_empty() {
			out.push('?');
			out += &url::encode_query(&self.params);
		}
		out
	}
}

impl Display for StoreReference {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		f.write_str(&self.render(true))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parse(uri: &str) -> Result<StoreReference, Error> {
		StoreReference::parse(uri, &Query::new(), Path::new("/work/dir"), &mut Vec::new())
	}

	#[test]
	fn relative_paths_resolve_lexically() {
		let reference = parse("./a/../b c//d/").unwrap();
		assert_eq!(reference.render(true), "local:///work/dir/b%20c/d");
		assert_eq!(parse("/").unwrap().render(true), "local:///");
		assert_eq!(parse("/..").unwrap().render(true), "local:///");
		assert_eq!(parse("x/..").unwrap().render(true), "local:///work/dir");
	}

	#[test]
	fn extra_params_take_precedence() {
		let extra = Query::from([("root".into(), "/extra".into())]);
		for uri in ["local://?root=/uri&x=1", "local?root=/uri&x=1"] {
			let reference =
				StoreReference::parse(uri, &extra, Path::new("/"), &mut Vec::new()).unwrap();
			assert_eq!(reference.params["root"], "/extra", "{uri}");
			assert_eq!(reference.params["x"], "1", "{uri}");
		}
		// Extra parameters turn `daemon` into `unix://`.
		let reference =
			StoreReference::parse("daemon", &extra, Path::new("/"), &mut Vec::new()).unwrap();
		assert_eq!(reference.render(false), "unix://");
	}

	#[test]
	fn malformed_references_fail_as_in_cppnix() {
		for uri in [
			"foo",
			"ssh://[::1",
			"local://x:0",
			"dummy://a!b/p q",
			"C:\\foo",
		] {
			assert_eq!(
				parse(uri).unwrap_err(),
				Error::Unparsable(uri.into()),
				"{uri}"
			);
		}
		assert_eq!(
			parse("dummy://?a=%zz").unwrap_err().to_string(),
			"invalid URI query 'a=%zz': leftover"
		);
		let mut warnings = Vec::new();
		let reference = StoreReference::parse(
			"dummy://?&a=1",
			&Query::new(),
			Path::new("/"),
			&mut warnings,
		)
		.unwrap();
		assert_eq!(reference.render(true), "dummy://?a=1");
		assert_eq!(
			warnings,
			["dubious URI query '' is missing equal sign '=', ignoring"]
		);
	}
}
