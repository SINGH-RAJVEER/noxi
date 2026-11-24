//! CppNix's lookup path, which `<...>` paths, `builtins.findFile` and
//! `nix-instantiate --find-file` search.

use path_clean::PathClean;
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::errors::{AddContext, CatchableErrorKind, ErrorKind};
use crate::{EvalIO, Value};

/// One lookup path element: `prefix=path`, or a bare `path` with an empty
/// prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NixSearchPathEntry {
	pub prefix: String,
	pub path: String,
}

impl NixSearchPathEntry {
	/// `LookupPath::Elem::parse`: the prefix ends at the first `=`.
	pub fn parse(element: &str) -> Self {
		match element.split_once('=') {
			Some((prefix, path)) => Self {
				prefix: prefix.to_owned(),
				path: path.to_owned(),
			},
			None => Self {
				prefix: String::new(),
				path: element.to_owned(),
			},
		}
	}

	/// `LookupPath::Prefix::suffixIfPotentialMatch`: the rest of `path`
	/// after the prefix and its separating `/`, if the prefix matches.
	fn suffix<'a>(&self, path: &'a str) -> Option<&'a str> {
		let n = self.prefix.len();
		let need_separator = n > 0 && n < path.len();
		if need_separator && path.as_bytes()[n] != b'/' {
			return None;
		}
		if !path.starts_with(&self.prefix) {
			return None;
		}
		Some(&path[if need_separator { n + 1 } else { n }..])
	}
}

/// `EvalSettings::isPseudoUrl`: a value CppNix downloads rather than reads.
pub fn is_pseudo_url(s: &str) -> bool {
	if s.starts_with("channel:") {
		return true;
	}
	match s.find("://") {
		Some(pos) => matches!(
			&s[..pos],
			"http" | "https" | "file" | "channel" | "git" | "s3" | "ssh"
		),
		None => false,
	}
}

/// `EvalSettings::parseNixPath`: split `NIX_PATH` at colons, except the one
/// after a URL's scheme or a `flake:` reference.
pub fn parse_nix_path(s: &str) -> Vec<String> {
	let bytes = s.as_bytes();
	let mut elements = Vec::new();
	let mut p = 0;
	while p != bytes.len() {
		let start = p;
		let mut value_start = p;
		while p != bytes.len() && bytes[p] != b':' {
			if bytes[p] == b'=' {
				value_start = p + 1;
			}
			p += 1;
		}
		if p == bytes.len() {
			if p != start {
				elements.push(s[start..p].to_owned());
			}
			break;
		}
		let value = &s[value_start..];
		if is_pseudo_url(value) || value.starts_with("flake:") {
			p += 1;
			while p != bytes.len() && bytes[p] != b':' {
				p += 1;
			}
		}
		elements.push(s[start..p].to_owned());
		if p == bytes.len() {
			break;
		}
		p += 1;
	}
	elements
}

/// Why a lookup found nothing.
#[derive(Debug, Clone, thiserror::Error)]
pub enum LookupError {
	/// No entry has the file, which `builtins.tryEval` catches.
	#[error("{0}")]
	NotFound(Box<str>),
	/// An entry failed to resolve or may not be accessed.
	#[error("{message}")]
	Fatal {
		message: String,
		/// Context of the failure, outermost first.
		traces: Vec<String>,
	},
}

impl LookupError {
	/// The error as CppNix prints it outside an evaluation.
	pub fn report(&self) -> String {
		match self {
			LookupError::Fatal { message, traces } if !traces.is_empty() => {
				let traces: String = traces
					.iter()
					.map(|trace| format!("       … {trace}\n\n"))
					.collect();
				format!("error:\n{traces}       error: {message}")
			}
			error => format!("error: {error}"),
		}
	}
}

impl From<io::Error> for LookupError {
	fn from(error: io::Error) -> Self {
		error
			.downcast::<LookupError>()
			.unwrap_or_else(|error| LookupError::Fatal {
				message: error.to_string(),
				traces: Vec::new(),
			})
	}
}

/// A lookup's value: a failed one is catchable only if nothing was found.
pub(crate) fn lookup_value(result: Result<PathBuf, LookupError>) -> Result<Value, ErrorKind> {
	match result {
		Ok(path) => Ok(path.into()),
		Err(LookupError::NotFound(message)) => {
			Ok(CatchableErrorKind::NixPathResolution(message).into())
		}
		Err(LookupError::Fatal { message, traces }) => {
			let mut kind = ErrorKind::EvalError(message);
			for trace in traces.into_iter().rev() {
				kind = kind.trace(None, trace);
			}
			Err(kind)
		}
	}
}

/// The lookup path of an evaluation, with CppNix's resolution rules.
#[derive(Default, Debug, Clone)]
pub struct NixSearchPath {
	entries: Vec<NixSearchPathEntry>,
	/// Where `<nix/...>` resolves when no entry has the file.
	corepkgs: Option<PathBuf>,
	/// CppNix's `pure-eval`, which changes the error for a missing file.
	pure_eval: bool,
	/// Each entry value's resolved directory, shared with the search paths
	/// `builtins.findFile` makes, so that a missing entry warns once.
	resolved: Rc<RefCell<FxHashMap<String, Option<PathBuf>>>>,
}

impl NixSearchPath {
	pub fn new(entries: Vec<NixSearchPathEntry>, corepkgs: Option<PathBuf>) -> Self {
		Self {
			entries,
			corepkgs,
			pure_eval: false,
			resolved: Rc::default(),
		}
	}

	pub fn with_pure_eval(self, pure_eval: bool) -> Self {
		Self { pure_eval, ..self }
	}

	/// A search path of other entries, sharing this one's resolutions.
	pub fn with_entries(&self, entries: Vec<NixSearchPathEntry>) -> Self {
		Self {
			entries,
			corepkgs: self.corepkgs.clone(),
			pure_eval: self.pure_eval,
			resolved: Rc::clone(&self.resolved),
		}
	}

	pub fn entries(&self) -> &[NixSearchPathEntry] {
		&self.entries
	}

	/// `EvalState::resolveLookupPathPath`: the directory an entry's value
	/// names, or `None` with a warning when it does not exist. A pseudo-URL
	/// that cannot be downloaded is tried as a path. A `flake:` entry that
	/// cannot be resolved is an error, as is one restricted evaluation may
	/// not access; `allowed` says that the evaluation may access `value`.
	fn resolve_entry(
		&self,
		io: &dyn EvalIO,
		value: &str,
		allowed: bool,
	) -> Result<Option<PathBuf>, LookupError> {
		if let Some(cached) = self.resolved.borrow().get(value) {
			return Ok(cached.clone());
		}
		let mut fetched = None;
		if is_pseudo_url(value) {
			fetched = io.resolve_lookup_path(value).unwrap_or_default();
			if fetched.is_none() {
				eprintln!(
					"warning: Nix search path entry '{value}' cannot be downloaded, ignoring"
				);
			}
		} else if value.starts_with("flake:") {
			fetched = io.resolve_lookup_path(value).map_err(LookupError::from)?;
		}
		let resolved = match fetched {
			Some(path) => Some(path),
			None => {
				let path = Path::new(value);
				let path = if path.is_absolute() {
					path.to_owned()
				} else {
					std::env::current_dir().unwrap_or_default().join(path)
				}
				.clean();
				if existing(io, &path).is_some() {
					Some(path)
				} else {
					if !allowed {
						check_access(io, &path)?;
					}
					eprintln!("warning: Nix search path entry '{value}' does not exist, ignoring");
					None
				}
			}
		};
		self.resolved
			.borrow_mut()
			.insert(value.to_owned(), resolved.clone());
		Ok(resolved)
	}

	/// Resolve every entry, as CppNix does before a restricted evaluation.
	pub fn resolve_entries(&self, io: &dyn EvalIO) -> Result<(), LookupError> {
		for entry in &self.entries {
			self.resolve_entry(io, &entry.path, true)?;
		}
		Ok(())
	}

	/// `EvalState::findFile`: the first entry with the file, symlinks
	/// resolved, falling back to the built-in `<nix/...>` files.
	pub fn resolve<IO>(&self, io: IO, path: &str) -> Result<PathBuf, LookupError>
	where
		IO: AsRef<dyn EvalIO>,
	{
		let io = io.as_ref();
		for entry in &self.entries {
			let Some(suffix) = entry.suffix(path) else {
				continue;
			};
			let Some(root) = self.resolve_entry(io, &entry.path, false)? else {
				continue;
			};
			let candidate = root.join(suffix.trim_start_matches('/')).clean();
			if let Some(found) = existing(io, &candidate) {
				return Ok(found);
			}
			check_access(io, &io.canonicalize(&candidate).unwrap_or(candidate))?;
		}
		if let (Some(rest), Some(corepkgs)) = (path.strip_prefix("nix/"), &self.corepkgs) {
			return Ok(corepkgs.join(rest.trim_start_matches('/')).clean());
		}
		let message = if self.pure_eval {
			format!(
				"cannot look up '<{path}>' in pure evaluation mode (use '--impure' to override)"
			)
		} else {
			format!(
				"file '{path}' was not found in the Nix search path (add it using $NIX_PATH or -I)"
			)
		};
		Err(LookupError::NotFound(message.into_boxed_str()))
	}
}

/// `path` with its symlinks resolved, if evaluation can see that it exists.
fn existing(io: &dyn EvalIO, path: &Path) -> Option<PathBuf> {
	let found = io.canonicalize(path).ok()?;
	io.file_type(&found).is_ok().then_some(found)
}

/// `FilteringSourceAccessor::checkAccess`: fail if the evaluation's access
/// rules, rather than the filesystem, refuse `path`.
fn check_access(io: &dyn EvalIO, path: &Path) -> Result<(), LookupError> {
	match io.file_type(path) {
		Err(error)
			if error.kind() == io::ErrorKind::PermissionDenied
				&& error.raw_os_error().is_none() =>
		{
			Err(error.into())
		}
		_ => Ok(()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_nix_path_like_cppnix() {
		assert_eq!(
			parse_nix_path("nixpkgs=/my/nixpkgs:/etc/nixos::"),
			["nixpkgs=/my/nixpkgs", "/etc/nixos", ""]
		);
		assert_eq!(
			parse_nix_path("a=https://example.org/x.tar.gz:b=flake:nixpkgs:/c"),
			["a=https://example.org/x.tar.gz", "b=flake:nixpkgs", "/c"]
		);
		assert_eq!(
			parse_nix_path("channel:nixos-unstable"),
			["channel:nixos-unstable"]
		);
		assert!(parse_nix_path("").is_empty());
	}

	#[test]
	fn matches_prefixes_at_separators() {
		let entry = NixSearchPathEntry::parse("nixpkgs=/x");
		assert_eq!(entry.suffix("nixpkgs"), Some(""));
		assert_eq!(entry.suffix("nixpkgs/lib"), Some("lib"));
		assert_eq!(entry.suffix("nixpkgs-unstable"), None);
		assert_eq!(entry.suffix("nix"), None);
		assert_eq!(NixSearchPathEntry::parse("/x").suffix("a/b"), Some("a/b"));
	}

	// this uses StdIO, which is only available with the impure feature.
	#[cfg(feature = "impure")]
	#[test]
	fn resolves_entries_in_order_through_symlinks() {
		use crate::StdIO;

		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		std::fs::create_dir_all(root.join("a/sub")).unwrap();
		std::fs::write(root.join("a/sub/x.nix"), "1").unwrap();
		std::fs::create_dir_all(root.join("b")).unwrap();
		std::fs::write(root.join("b/y.nix"), "2").unwrap();
		std::os::unix::fs::symlink(root.join("b"), root.join("link")).unwrap();

		let entries = [
			"/nonexistent-search-path-entry".to_owned(),
			format!("p={}", root.join("link").display()),
			root.join("a").display().to_string(),
		];
		let search_path = NixSearchPath::new(
			entries
				.iter()
				.map(|e| NixSearchPathEntry::parse(e))
				.collect(),
			Some("/corepkgs".into()),
		);
		let io = Box::new(StdIO) as Box<dyn EvalIO>;
		assert_eq!(
			search_path.resolve(&io, "sub/x.nix").unwrap(),
			root.join("a/sub/x.nix")
		);
		assert_eq!(
			search_path.resolve(&io, "p/y.nix").unwrap(),
			root.join("b/y.nix")
		);
		assert_eq!(search_path.resolve(&io, "p").unwrap(), root.join("b"));
		assert_eq!(
			search_path.resolve(&io, "nix/fetchurl.nix").unwrap(),
			Path::new("/corepkgs/fetchurl.nix")
		);
		assert!(matches!(
			search_path.resolve(&io, "y.nix"),
			Err(LookupError::NotFound(_))
		));
	}
}
