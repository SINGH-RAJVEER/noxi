//! Flake references: an input plus a subdirectory, parsed and printed as
//! CppNix 2.35.2's `src/libflake/flakeref.cc` does.

use std::collections::BTreeMap;
use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result, warn};
use crate::input::{Attr, Attrs, FetchSettings, Input, is_flake_id, is_rev, maybe_get_str_attr};
use crate::url::{
	Authority, ParsedUrl, decode_query, parse_url, path_to_url_path, percent_decode,
	url_path_to_path,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlakeRef {
	pub input: Input,
	pub subdir: String,
}

impl FlakeRef {
	pub fn new(input: Input, subdir: impl Into<String>) -> Self {
		Self {
			input,
			subdir: subdir.into(),
		}
	}

	/// `FlakeRef::fromAttrs`: `dir` selects the subdirectory.
	pub fn from_attrs(settings: &FetchSettings, attrs: &Attrs) -> Result<Self> {
		let mut attrs2 = attrs.clone();
		attrs2.remove("dir");
		Ok(Self {
			input: Input::from_attrs(settings, attrs2)?,
			subdir: maybe_get_str_attr(attrs, "dir")?.unwrap_or_default(),
		})
	}

	/// `FlakeRef::toAttrs`.
	pub fn to_attrs(&self) -> Attrs {
		let mut attrs = self.input.attrs.clone();
		if !self.subdir.is_empty() {
			attrs
				.entry("dir".into())
				.or_insert_with(|| Attr::String(self.subdir.clone()));
		}
		attrs
	}

	/// `FlakeRef::to_string`.
	pub fn try_to_string(&self) -> Result<String> {
		let mut extra = BTreeMap::new();
		if !self.subdir.is_empty() {
			extra.insert("dir".to_owned(), self.subdir.clone());
		}
		self.input.to_url_string(&extra)
	}

	/// `FlakeRef::canonicalize`: drop a `dir` query from a `url` attribute
	/// when it repeats the subdirectory, and re-render the URL.
	pub fn canonicalize(&self) -> FlakeRef {
		let mut flake_ref = self.clone();
		if let Ok(Some(url)) = maybe_get_str_attr(&flake_ref.input.attrs, "url")
			&& let Ok(mut parsed) = parse_url(&url, true)
		{
			if let Some(dir) = parsed.query.get("dir")
				&& !flake_ref.subdir.is_empty()
				&& flake_ref.subdir == *dir
			{
				parsed.query.remove("dir");
			}
			flake_ref
				.input
				.attrs
				.insert("url".into(), Attr::String(parsed.to_string()));
		}
		flake_ref
	}
}

impl fmt::Display for FlakeRef {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self.try_to_string() {
			Ok(s) => f.write_str(&s),
			Err(error) => f.write_str(&error.message),
		}
	}
}

/// How a flake reference string is interpreted.
#[derive(Clone, Copy, Debug)]
pub struct ParseOptions<'a> {
	/// Directory relative paths are resolved against; without one, a path
	/// is never searched for `flake.nix` or a Git repository.
	pub base_dir: Option<&'a Path>,
	pub allow_missing: bool,
	pub is_flake: bool,
	pub preserve_relative_paths: bool,
}

impl Default for ParseOptions<'_> {
	fn default() -> Self {
		Self {
			base_dir: None,
			allow_missing: false,
			is_flake: true,
			preserve_relative_paths: false,
		}
	}
}

/// `parseFlakeRef`: a fragment is an error.
pub fn parse_flake_ref(
	settings: &FetchSettings,
	url: &str,
	options: ParseOptions<'_>,
) -> Result<FlakeRef> {
	let (flake_ref, fragment) = parse_flake_ref_with_fragment(settings, url, options)?;
	if !fragment.is_empty() {
		return Err(Error::new(format!(
			"unexpected fragment '{fragment}' in flake reference '{url}'"
		)));
	}
	Ok(flake_ref)
}

/// `parseFlakeRefWithFragment`.
pub fn parse_flake_ref_with_fragment(
	settings: &FetchSettings,
	url: &str,
	options: ParseOptions<'_>,
) -> Result<(FlakeRef, String)> {
	if let Some(result) = parse_flake_id_ref(settings, url, options.is_flake)? {
		return Ok(result);
	}
	if let Some(result) = parse_url_flake_ref(settings, url, options)? {
		return Ok(result);
	}
	parse_path_flake_ref_with_fragment(settings, url, options)
}

fn from_parsed_url(
	settings: &FetchSettings,
	mut parsed: ParsedUrl,
	is_flake: bool,
) -> Result<(FlakeRef, String)> {
	let dir = parsed.query.remove("dir").unwrap_or_default();
	let fragment = std::mem::take(&mut parsed.fragment);
	Ok((
		FlakeRef::new(Input::from_url(settings, &parsed, is_flake)?, dir),
		fragment,
	))
}

/// Match `flakeId(/(rev|ref(/rev)?))?(#fragment)?` as CppNix's
/// `parseFlakeIdRef` regex does.
fn parse_flake_id_ref(
	settings: &FetchSettings,
	url: &str,
	is_flake: bool,
) -> Result<Option<(FlakeRef, String)>> {
	let (main, fragment) = match url.split_once('#') {
		Some((main, fragment)) => (main, Some(fragment)),
		None => (url, None),
	};
	if let Some(fragment) = fragment
		&& !fragment.bytes().all(is_fragment_char)
	{
		return Ok(None);
	}
	let (id, rest) = match main.split_once('/') {
		Some((id, rest)) => (id, Some(rest)),
		None => (main, None),
	};
	if !is_flake_id(id) {
		return Ok(None);
	}
	if let Some(rest) = rest
		&& !matches_ref_and_or_rev(rest)
	{
		return Ok(None);
	}
	let parsed = ParsedUrl {
		scheme: "flake".into(),
		path: main.split('/').map(str::to_owned).collect(),
		..ParsedUrl::default()
	};
	let input = Input::from_url(settings, &parsed, is_flake)?;
	Ok(Some((
		FlakeRef::new(input, ""),
		percent_decode(fragment.unwrap_or(""))?,
	)))
}

fn is_fragment_char(c: u8) -> bool {
	c.is_ascii_alphanumeric() || b"-._~!$&'\"()*+,;=:@/? ^%".contains(&c)
}

fn matches_ref_regex(s: &str) -> bool {
	let mut bytes = s.bytes();
	matches!(bytes.next(), Some(c) if c.is_ascii_alphanumeric() || c == b'@')
		&& bytes.all(|c| c.is_ascii_alphanumeric() || b"_./@+-".contains(&c))
}

/// `(rev)|(ref(/rev)?)`, matched in full.
fn matches_ref_and_or_rev(s: &str) -> bool {
	if is_rev(s) || matches_ref_regex(s) {
		return true;
	}
	match s.rsplit_once('/') {
		Some((r#ref, rev)) => is_rev(rev) && matches_ref_regex(r#ref),
		None => false,
	}
}

fn parse_url_flake_ref(
	settings: &FetchSettings,
	url: &str,
	options: ParseOptions<'_>,
) -> Result<Option<(FlakeRef, String)>> {
	let attempt = || -> Result<(FlakeRef, String)> {
		let mut parsed = parse_url(url, true)?;
		if let Some(base_dir) = options.base_dir
			&& (parsed.scheme == "path" || parsed.scheme == "git+file")
		{
			let path = url_path_to_path(&parsed.path)?;
			if !path.is_absolute() {
				parsed.path = path_to_url_path(&abs_path(&path, base_dir, false));
			}
		}
		from_parsed_url(settings, parsed, options.is_flake)
	};
	match attempt() {
		Ok(result) => Ok(Some(result)),
		Err(error) if error.is_bad_url() => Ok(None),
		Err(error) => Err(error),
	}
}

fn parse_path_flake_ref_with_fragment(
	settings: &FetchSettings,
	url: &str,
	options: ParseOptions<'_>,
) -> Result<(FlakeRef, String)> {
	let (before_fragment, fragment) = match url.split_once('#') {
		Some((before, fragment)) => (before, fragment),
		None => (url, ""),
	};
	let (path, query) = match before_fragment.split_once('?') {
		Some((path, query)) => (path, query),
		None => (before_fragment, ""),
	};
	let mut path = PathBuf::from(path);
	let query = decode_query(query, true)?;
	let fragment = percent_decode(fragment)?;

	if let Some(base_dir) = options.base_dir {
		path = abs_path(&path, base_dir, true);
		if options.is_flake {
			let is_dir = std::fs::symlink_metadata(&path)
				.map_err(|e| Error::new(format!("getting status of '{}': {e}", path.display())))?
				.is_dir();
			if !is_dir {
				if path.file_name().is_some_and(|name| name == "flake.nix") {
					let parent = path.parent().unwrap_or(Path::new("/")).to_owned();
					warn(&format!(
						"Path \"{}\" should point at the directory containing the 'flake.nix' file, not the file itself. Pretending that you meant \"{}\"",
						path.display(),
						parent.display()
					));
					path = parent;
				} else {
					return Err(Error::bad_url(format!(
						"path \"{}\" is not a flake (because it's not a directory)",
						path.display()
					)));
				}
			}
			if !options.allow_missing && !path.join("flake.nix").exists() {
				crate::notice(&format!(
					"path \"{}\" does not contain a 'flake.nix', searching up",
					path.display()
				));
				let device = std::fs::symlink_metadata(&path)?.dev();
				let mut found = false;
				while let Some(parent) = path.parent() {
					if path.join("flake.nix").exists() {
						found = true;
						break;
					} else if path.join(".git").exists() {
						return Err(Error::new(format!(
							"path \"{}\" is not part of a flake (neither it nor its parent directories contain a 'flake.nix' file)",
							path.display()
						)));
					} else if std::fs::symlink_metadata(&path)?.dev() != device {
						return Err(Error::new(format!(
							"unable to find a flake before encountering filesystem boundary at \"{}\"",
							path.display()
						)));
					}
					path = parent.to_owned();
				}
				if !found {
					return Err(Error::bad_url("could not find a flake.nix file"));
				}
			}
			if !options.allow_missing && !path.join("flake.nix").exists() {
				return Err(Error::bad_url(format!(
					"path \"{}\" is not a flake (because it doesn't contain a 'flake.nix' file)",
					path.display()
				)));
			}
			let mut flake_root = path.clone();
			let mut subdir = String::new();
			while let Some(parent) = flake_root.parent() {
				if flake_root.join(".git").exists() {
					let mut parsed = ParsedUrl {
						scheme: "git+file".into(),
						authority: Some(Authority::default()),
						path: path_to_url_path(&flake_root),
						query: query.clone(),
						fragment: fragment.clone(),
					};
					if !subdir.is_empty() {
						if parsed.query.contains_key("dir") {
							return Err(Error::new(format!(
								"flake URL '{url}' has an inconsistent 'dir' parameter"
							)));
						}
						parsed.query.insert("dir".into(), subdir);
					}
					if flake_root.join(".git/shallow").exists() {
						parsed.query.insert("shallow".into(), "1".into());
					}
					return from_parsed_url(settings, parsed, options.is_flake);
				}
				let name = flake_root
					.file_name()
					.map(|n| n.to_string_lossy().into_owned())
					.unwrap_or_default();
				subdir = if subdir.is_empty() {
					name
				} else {
					format!("{name}/{subdir}")
				};
				flake_root = parent.to_owned();
			}
		}
	} else if !options.preserve_relative_paths && !path.is_absolute() {
		return Err(Error::bad_url(format!(
			"flake reference '{url}' is not an absolute path"
		)));
	}

	let absolute = path.is_absolute();
	from_parsed_url(
		settings,
		ParsedUrl {
			scheme: "path".into(),
			authority: absolute.then(Authority::default),
			path: path_to_url_path(&path),
			query,
			fragment,
		},
		options.is_flake,
	)
}

/// `absPath(path, dir, resolveSymlinks)`: make `path` absolute against
/// `dir`, then canonicalise it.
pub fn abs_path(path: &Path, dir: &Path, resolve_symlinks: bool) -> PathBuf {
	let joined = if path.is_absolute() {
		path.to_owned()
	} else {
		dir.join(path)
	};
	canon_path(&joined, resolve_symlinks)
}

/// `canonPath`: remove `.`, `..` and repeated slashes, optionally
/// resolving symlinks component by component.
pub fn canon_path(path: &Path, resolve_symlinks: bool) -> PathBuf {
	let mut pending: Vec<String> = path
		.to_string_lossy()
		.split('/')
		.rev()
		.map(str::to_owned)
		.collect();
	let mut result: Vec<String> = Vec::new();
	let mut follows = 0;
	while let Some(component) = pending.pop() {
		match component.as_str() {
			"" | "." => continue,
			".." => {
				result.pop();
				continue;
			}
			_ => {}
		}
		result.push(component);
		if resolve_symlinks && follows < 1024 {
			let current = PathBuf::from(format!("/{}", result.join("/")));
			if let Ok(target) = std::fs::read_link(&current) {
				follows += 1;
				result.pop();
				let target = target.to_string_lossy().into_owned();
				if target.starts_with('/') {
					result.clear();
				}
				pending.extend(target.split('/').rev().map(str::to_owned));
			}
		}
	}
	PathBuf::from(format!("/{}", result.join("/")))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn settings() -> FetchSettings {
		FetchSettings {
			flakes: true,
			..FetchSettings::default()
		}
	}

	fn parse(url: &str) -> (String, String) {
		let (flake_ref, fragment) =
			parse_flake_ref_with_fragment(&settings(), url, ParseOptions::default()).unwrap();
		(flake_ref.to_string(), fragment)
	}

	#[test]
	fn parses_ids_urls_and_paths() {
		assert_eq!(parse("nixpkgs"), ("flake:nixpkgs".into(), "".into()));
		assert_eq!(
			parse("nixpkgs/nixos-24.05#hello"),
			("flake:nixpkgs/nixos-24.05".into(), "hello".into())
		);
		assert_eq!(
			parse("github:NixOS/nixpkgs?dir=lib#x"),
			("github:NixOS/nixpkgs?dir=lib".into(), "x".into())
		);
		assert_eq!(parse("/tmp/flake"), ("path:/tmp/flake".into(), "".into()));
		assert!(
			parse_flake_ref_with_fragment(&settings(), "./x", ParseOptions::default()).is_err()
		);
		let relative = ParseOptions {
			preserve_relative_paths: true,
			allow_missing: true,
			..ParseOptions::default()
		};
		assert_eq!(
			parse_flake_ref(&settings(), "./sub", relative)
				.unwrap()
				.to_string(),
			"path:./sub"
		);
	}

	#[test]
	fn canonicalises_paths() {
		assert_eq!(
			canon_path(Path::new("/a/./b//../c"), false),
			PathBuf::from("/a/c")
		);
	}
}
