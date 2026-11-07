//! Fetcher inputs: CppNix 2.35.2's `fetchers::Input`, its attribute sets
//! and the URL syntax of each input scheme (`src/libfetchers`).
//!
//! An [`Input`] is a sorted attribute set whose `type` selects a scheme. The
//! schemes are tried in CppNix's registration order (sorted by name) when a
//! URL is parsed.

use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::rc::Rc;

use noxi_hash::nixhash::{HashAlgo, NixHash};

use crate::error::{Error, Result, warn};
use crate::url::{
	ParsedUrl, fix_git_url, parse_url, parse_url_scheme, path_to_url_path, url_path_to_path,
};

/// A deferred attribute, such as a Git `revCount`, computed when forced.
pub struct LazyAttr {
	value: OnceCell<Result<Attr>>,
	compute: Box<dyn Fn() -> Result<Attr>>,
}

impl LazyAttr {
	pub fn new(compute: impl Fn() -> Result<Attr> + 'static) -> Rc<Self> {
		Rc::new(Self {
			value: OnceCell::new(),
			compute: Box::new(compute),
		})
	}

	pub fn force(&self) -> Result<Attr> {
		self.value.get_or_init(|| (self.compute)()).clone()
	}
}

/// One fetcher attribute value.
#[derive(Clone)]
pub enum Attr {
	String(String),
	Int(u64),
	Bool(bool),
	Lazy(Rc<LazyAttr>),
}

impl Attr {
	/// The value, computing a lazy attribute.
	pub fn force(&self) -> Result<Attr> {
		match self {
			Attr::Lazy(lazy) => lazy.force(),
			other => Ok(other.clone()),
		}
	}
}

impl fmt::Debug for Attr {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Attr::String(s) => write!(f, "{s:?}"),
			Attr::Int(n) => write!(f, "{n}"),
			Attr::Bool(b) => write!(f, "{b}"),
			Attr::Lazy(_) => f.write_str("<lazy>"),
		}
	}
}

impl PartialEq for Attr {
	fn eq(&self, other: &Self) -> bool {
		match (self, other) {
			(Attr::String(a), Attr::String(b)) => a == b,
			(Attr::Int(a), Attr::Int(b)) => a == b,
			(Attr::Bool(a), Attr::Bool(b)) => a == b,
			(Attr::Lazy(a), Attr::Lazy(b)) => Rc::ptr_eq(a, b),
			_ => false,
		}
	}
}

impl Eq for Attr {}

impl From<&str> for Attr {
	fn from(s: &str) -> Self {
		Attr::String(s.to_owned())
	}
}

impl From<String> for Attr {
	fn from(s: String) -> Self {
		Attr::String(s)
	}
}

impl From<u64> for Attr {
	fn from(n: u64) -> Self {
		Attr::Int(n)
	}
}

impl From<bool> for Attr {
	fn from(b: bool) -> Self {
		Attr::Bool(b)
	}
}

pub type Attrs = BTreeMap<String, Attr>;

/// `attrsToJSON`, forcing lazy attributes.
pub fn attrs_to_json(attrs: &Attrs) -> Result<serde_json::Map<String, serde_json::Value>> {
	let mut json = serde_json::Map::new();
	for (name, value) in attrs {
		let value = match value.force()? {
			Attr::String(s) => serde_json::Value::String(s),
			Attr::Int(n) => serde_json::Value::from(n),
			Attr::Bool(b) => serde_json::Value::Bool(b),
			Attr::Lazy(_) => unreachable!("forced"),
		};
		json.insert(name.clone(), value);
	}
	Ok(json)
}

/// `jsonToAttrs`.
pub fn json_to_attrs(json: &serde_json::Value) -> Result<Attrs> {
	let mut attrs = Attrs::new();
	let Some(object) = json.as_object() else {
		return Err(Error::new("unsupported input attribute type in lock file"));
	};
	for (name, value) in object {
		let value = match value {
			serde_json::Value::Number(n) => Attr::Int(
				n.as_u64()
					.or_else(|| n.as_i64().map(|n| n as u64))
					.or_else(|| n.as_f64().map(|n| n as u64))
					.unwrap_or_default(),
			),
			serde_json::Value::String(s) => Attr::String(s.clone()),
			serde_json::Value::Bool(b) => Attr::Bool(*b),
			_ => return Err(Error::new("unsupported input attribute type in lock file")),
		};
		attrs.insert(name.clone(), value);
	}
	Ok(attrs)
}

fn attrs_json_string(attrs: &Attrs) -> String {
	attrs_to_json(attrs)
		.map(|json| crate::json::dump(&serde_json::Value::Object(json), None))
		.unwrap_or_default()
}

pub fn maybe_get_str_attr(attrs: &Attrs, name: &str) -> Result<Option<String>> {
	match attrs.get(name) {
		None => Ok(None),
		Some(value) => match value.force()? {
			Attr::String(s) => Ok(Some(s)),
			_ => Err(Error::new(format!(
				"input attribute '{name}' is not a string {}",
				attrs_json_string(attrs)
			))),
		},
	}
}

pub fn get_str_attr(attrs: &Attrs, name: &str) -> Result<String> {
	maybe_get_str_attr(attrs, name)?
		.ok_or_else(|| Error::new(format!("input attribute '{name}' is missing")))
}

pub fn maybe_get_int_attr(attrs: &Attrs, name: &str) -> Result<Option<u64>> {
	match attrs.get(name) {
		None => Ok(None),
		Some(value) => match value.force()? {
			Attr::Int(n) => Ok(Some(n)),
			_ => Err(Error::new(format!(
				"input attribute '{name}' is not an integer"
			))),
		},
	}
}

pub fn maybe_get_bool_attr(attrs: &Attrs, name: &str) -> Result<Option<bool>> {
	match attrs.get(name) {
		None => Ok(None),
		Some(value) => match value.force()? {
			Attr::Bool(b) => Ok(Some(b)),
			_ => Err(Error::new(format!(
				"input attribute '{name}' is not a Boolean"
			))),
		},
	}
}

/// `attrsToQuery`: integers in decimal, Booleans as `1`/`0`.
pub fn attrs_to_query(attrs: &Attrs) -> Result<BTreeMap<String, String>> {
	let mut query = BTreeMap::new();
	for (name, value) in attrs {
		let value = match value.force()? {
			Attr::String(s) => s,
			Attr::Int(n) => n.to_string(),
			Attr::Bool(b) => if b { "1" } else { "0" }.to_owned(),
			Attr::Lazy(_) => unreachable!("forced"),
		};
		query.insert(name.clone(), value);
	}
	Ok(query)
}

/// Settings that change how inputs are parsed, locked and fetched.
#[derive(Clone, Debug)]
pub struct FetchSettings {
	pub flakes: bool,
	pub verified_fetches: bool,
	pub allow_dirty: bool,
	pub warn_dirty: bool,
	pub allow_dirty_locks: bool,
	pub trust_tarballs_from_git_forges: bool,
	pub use_registries: bool,
	pub flake_registry: String,
	pub tarball_ttl: u64,
	pub access_tokens: BTreeMap<String, String>,
}

impl Default for FetchSettings {
	fn default() -> Self {
		Self {
			flakes: false,
			verified_fetches: false,
			allow_dirty: true,
			warn_dirty: true,
			allow_dirty_locks: false,
			trust_tarballs_from_git_forges: true,
			use_registries: true,
			flake_registry: "https://channels.nixos.org/flake-registry.json".into(),
			tarball_ttl: 3600,
			access_tokens: BTreeMap::new(),
		}
	}
}

fn require_feature(enabled: bool, feature: &str) -> Result<()> {
	if enabled {
		Ok(())
	} else {
		Err(Error::new(format!(
			"experimental Nix feature '{feature}' is disabled; add '--extra-experimental-features {feature}' to enable it"
		)))
	}
}

/// The input schemes, in CppNix's registration (name) order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scheme {
	File,
	Git,
	GitHub,
	GitLab,
	Mercurial,
	Indirect,
	Path,
	SourceHut,
	Tarball,
}

pub const SCHEMES: [Scheme; 9] = [
	Scheme::File,
	Scheme::Git,
	Scheme::GitHub,
	Scheme::GitLab,
	Scheme::Mercurial,
	Scheme::Indirect,
	Scheme::Path,
	Scheme::SourceHut,
	Scheme::Tarball,
];

impl Scheme {
	pub fn name(self) -> &'static str {
		match self {
			Scheme::File => "file",
			Scheme::Git => "git",
			Scheme::GitHub => "github",
			Scheme::GitLab => "gitlab",
			Scheme::Mercurial => "hg",
			Scheme::Indirect => "indirect",
			Scheme::Path => "path",
			Scheme::SourceHut => "sourcehut",
			Scheme::Tarball => "tarball",
		}
	}

	pub fn from_name(name: &str) -> Option<Self> {
		SCHEMES.into_iter().find(|scheme| scheme.name() == name)
	}

	fn needs_flakes(self) -> bool {
		matches!(
			self,
			Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut | Scheme::Indirect | Scheme::Path
		)
	}

	pub fn allowed_attrs(self) -> &'static [&'static str] {
		match self {
			Scheme::File | Scheme::Tarball => &[
				"url",
				"narHash",
				"name",
				"unpack",
				"rev",
				"revCount",
				"lastModified",
			],
			Scheme::Git => &[
				"url",
				"ref",
				"rev",
				"shallow",
				"submodules",
				"lfs",
				"exportIgnore",
				"lastModified",
				"revCount",
				"narHash",
				"allRefs",
				"name",
				"dirtyRev",
				"dirtyShortRev",
				"verifyCommit",
				"keytype",
				"publicKey",
				"publicKeys",
			],
			Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut => &[
				"owner",
				"repo",
				"ref",
				"rev",
				"narHash",
				"lastModified",
				"host",
				"treeHash",
			],
			Scheme::Mercurial => &["url", "ref", "rev", "revCount", "narHash", "name"],
			Scheme::Indirect => &["id", "ref", "rev", "narHash"],
			Scheme::Path => &["path", "rev", "revCount", "lastModified", "narHash"],
		}
	}
}

/// A 40-character hexadecimal SHA-1 revision.
pub fn is_rev(s: &str) -> bool {
	s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

fn is_all_caps_and_underscore(name: &str) -> bool {
	!name.is_empty()
		&& name.bytes().all(|c| c.is_ascii_uppercase() || c == b'_')
		&& !name.starts_with('_')
		&& !name.ends_with('_')
}

/// libgit2's `git_reference__normalize_name` validity check without
/// normalisation, allowing one-level names.
fn is_valid_reference_name(name: &str) -> bool {
	if name.is_empty() {
		return false;
	}
	let segments: Vec<&str> = name.split('/').collect();
	for segment in &segments {
		if segment.is_empty() || segment.starts_with('.') {
			return false;
		}
		let bytes = segment.as_bytes();
		let mut prev = 0u8;
		for &c in bytes {
			if c <= b' ' || matches!(c, b'~' | b'^' | b':' | b'\\' | b'?' | b'[' | b'*') {
				return false;
			}
			if (prev == b'.' && c == b'.') || (prev == b'@' && c == b'{') {
				return false;
			}
			prev = c;
		}
		if segment.ends_with(".lock") {
			return false;
		}
	}
	if name.ends_with('.') {
		return false;
	}
	if segments.len() == 1 {
		return is_all_caps_and_underscore(name);
	}
	!is_all_caps_and_underscore(segments[0])
}

/// CppNix's `isLegalRefName`: a valid reference, branch or tag name.
pub fn is_legal_ref_name(name: &str) -> bool {
	if name == "@" || name.contains('\x7f') {
		return false;
	}
	is_valid_reference_name(name)
		|| (!name.starts_with('-')
			&& name != "HEAD"
			&& is_valid_reference_name(&format!("refs/heads/{name}")))
		|| (!name.starts_with('-') && is_valid_reference_name(&format!("refs/tags/{name}")))
}

/// The regex CppNix's Mercurial scheme uses for branch and tag names.
fn matches_ref_regex(s: &str) -> bool {
	let mut bytes = s.bytes();
	matches!(bytes.next(), Some(c) if c.is_ascii_alphanumeric() || c == b'@')
		&& bytes.all(|c| c.is_ascii_alphanumeric() || b"_./@+-".contains(&c))
}

/// A flake ID: `[a-zA-Z][a-zA-Z0-9_-]*`.
pub fn is_flake_id(s: &str) -> bool {
	let mut bytes = s.bytes();
	matches!(bytes.next(), Some(c) if c.is_ascii_alphabetic())
		&& bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// `Hash::parseAny(s, SHA1)`, after trying an algorithm prefix.
pub fn parse_rev(s: &str) -> Result<NixHash> {
	if let Ok(hash) = NixHash::from_str(s, None) {
		return Ok(hash);
	}
	NixHash::from_str(s, Some(HashAlgo::Sha1)).map_err(|_| {
		if s.len() == 40 || s.len() == 32 || s.len() == 28 || s.len() == 27 {
			Error::bad_url(format!("invalid SHA1 hash '{s}'"))
		} else {
			Error::bad_url(format!(
				"hash '{s}' has wrong length for hash algorithm 'sha1'"
			))
		}
	})
}

/// The lowercase hexadecimal form Git uses.
pub fn git_rev(hash: &NixHash) -> String {
	hash.digest_as_bytes()
		.iter()
		.map(|b| format!("{b:02x}"))
		.collect()
}

pub fn git_short_rev(hash: &NixHash) -> String {
	git_rev(hash)[..7].to_owned()
}

/// The all-zero SHA-1 CppNix uses as the revision of an empty repository.
pub fn null_rev() -> NixHash {
	NixHash::Sha1([0; 20])
}

/// A fetcher input.
#[derive(Clone, Debug, Default)]
pub struct Input {
	pub attrs: Attrs,
	pub scheme: Option<Scheme>,
}

impl PartialEq for Input {
	fn eq(&self, other: &Self) -> bool {
		self.attrs == other.attrs
	}
}

impl Eq for Input {}

impl Input {
	/// `Input::fromURL`.
	pub fn from_url_string(
		settings: &FetchSettings,
		url: &str,
		require_tree: bool,
	) -> Result<Self> {
		Self::from_url(settings, &parse_url(url, false)?, require_tree)
	}

	/// `Input::fromURL`: the first scheme that accepts the URL wins.
	pub fn from_url(settings: &FetchSettings, url: &ParsedUrl, require_tree: bool) -> Result<Self> {
		for scheme in SCHEMES {
			if let Some(mut input) = input_from_url(settings, scheme, url, require_tree)? {
				if scheme.needs_flakes() {
					require_feature(settings.flakes, "flakes")?;
				}
				input.scheme = Some(scheme);
				input.fixup()?;
				return Ok(input);
			}
		}
		let (application, transport) = parse_url_scheme(&url.scheme);
		if application == Some("file") && transport == "git" {
			return Err(Error::new(format!(
				"input '{url}' is unsupported; did you mean 'git+file' instead of 'file+git'?"
			)));
		}
		Err(Error::new(format!("input '{url}' is unsupported")))
	}

	/// `Input::fromAttrs`. Unknown types give an input without a scheme.
	pub fn from_attrs(settings: &FetchSettings, attrs: Attrs) -> Result<Self> {
		let Some(type_) = maybe_get_str_attr(&attrs, "type")? else {
			return Err(Error::new(
				"'type' attribute to specify input scheme is required but not provided",
			));
		};
		let Some(scheme) = Scheme::from_name(&type_) else {
			let input = Input {
				attrs,
				scheme: None,
			};
			input.fixup()?;
			return Ok(input);
		};
		if scheme.needs_flakes() {
			require_feature(settings.flakes, "flakes")?;
		}
		let allowed = scheme.allowed_attrs();
		for name in attrs.keys() {
			if name != "type" && name != "__final" && !allowed.contains(&name.as_str()) {
				return Err(Error::new(format!(
					"input attribute '{name}' not supported by scheme '{type_}'"
				)));
			}
		}
		let mut input = input_from_attrs(settings, scheme, attrs)?;
		input.scheme = Some(scheme);
		input.fixup()?;
		Ok(input)
	}

	fn fixup(&self) -> Result<()> {
		self.get_type()?;
		self.get_ref()?;
		self.get_rev_count()?;
		self.get_last_modified()?;
		Ok(())
	}

	pub fn get_type(&self) -> Result<String> {
		get_str_attr(&self.attrs, "type")
	}

	pub fn get_ref(&self) -> Result<Option<String>> {
		maybe_get_str_attr(&self.attrs, "ref")
	}

	pub fn get_rev(&self) -> Result<Option<NixHash>> {
		maybe_get_str_attr(&self.attrs, "rev")?
			.map(|s| parse_rev(&s))
			.transpose()
	}

	pub fn get_rev_count(&self) -> Result<Option<u64>> {
		maybe_get_int_attr(&self.attrs, "revCount")
	}

	pub fn get_last_modified(&self) -> Result<Option<u64>> {
		maybe_get_int_attr(&self.attrs, "lastModified")
	}

	pub fn get_nar_hash(&self) -> Result<Option<NixHash>> {
		let Some(s) = maybe_get_str_attr(&self.attrs, "narHash")? else {
			return Ok(None);
		};
		let hash = if s.is_empty() {
			NixHash::Sha256([0; 32])
		} else {
			NixHash::from_sri(&s).map_err(|_| Error::new(format!("hash '{s}' is not SRI")))?
		};
		if !matches!(hash, NixHash::Sha256(_)) {
			return Err(Error::new("narHash must use SHA-256"));
		}
		Ok(Some(hash))
	}

	pub fn get_name(&self) -> String {
		maybe_get_str_attr(&self.attrs, "name")
			.ok()
			.flatten()
			.unwrap_or_else(|| "source".into())
	}

	pub fn is_final(&self) -> bool {
		maybe_get_bool_attr(&self.attrs, "__final")
			.ok()
			.flatten()
			.unwrap_or(false)
	}

	pub fn is_direct(&self) -> bool {
		self.scheme != Some(Scheme::Indirect)
	}

	pub fn is_locked(&self, settings: &FetchSettings) -> bool {
		let Some(scheme) = self.scheme else {
			return false;
		};
		match scheme {
			Scheme::Git => self
				.get_rev()
				.ok()
				.flatten()
				.is_some_and(|rev| rev != null_rev()),
			Scheme::Path | Scheme::File | Scheme::Tarball => {
				self.get_nar_hash().ok().flatten().is_some()
			}
			Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut => {
				self.get_rev().ok().flatten().is_some()
					&& (settings.trust_tarballs_from_git_forges
						|| self.get_nar_hash().ok().flatten().is_some())
			}
			Scheme::Mercurial => self.get_rev().ok().flatten().is_some(),
			Scheme::Indirect => false,
		}
	}

	/// A relative `path:` input's path.
	pub fn is_relative(&self) -> Option<PathBuf> {
		if self.scheme != Some(Scheme::Path) {
			return None;
		}
		let path = maybe_get_str_attr(&self.attrs, "path").ok().flatten()?;
		if path.starts_with('/') {
			None
		} else {
			Some(PathBuf::from(path))
		}
	}

	/// The local directory this input reads from, if any.
	pub fn get_source_path(&self) -> Option<PathBuf> {
		match self.scheme? {
			Scheme::Path => {
				let path = maybe_get_str_attr(&self.attrs, "path").ok().flatten()?;
				path.starts_with('/').then(|| canon_path(&path))
			}
			Scheme::Git => {
				let url =
					fix_git_url(&maybe_get_str_attr(&self.attrs, "url").ok().flatten()?).ok()?;
				if url.scheme != "file" {
					return None;
				}
				let path = url_path_to_path(&url.path).ok()?;
				if path.exists() && !path.join(".git").exists() {
					return None;
				}
				Some(absolute(&path))
			}
			Scheme::Mercurial => {
				let url = parse_url(
					&maybe_get_str_attr(&self.attrs, "url").ok().flatten()?,
					false,
				)
				.ok()?;
				(url.scheme == "file"
					&& self.get_ref().ok().flatten().is_none()
					&& self.get_rev().ok().flatten().is_none())
				.then(|| url_path_to_path(&url.path).ok())
				.flatten()
			}
			_ => None,
		}
	}

	/// `Input::contains`: equal, or equal after dropping `ref` and `rev`
	/// from `other`.
	pub fn contains(&self, other: &Input) -> bool {
		if self == other {
			return true;
		}
		let mut other = other.clone();
		other.attrs.remove("ref");
		other.attrs.remove("rev");
		*self == other
	}

	/// `Input::toURL`.
	pub fn to_url(&self) -> Result<ParsedUrl> {
		let Some(scheme) = self.scheme else {
			return Err(Error::new(format!(
				"cannot show unsupported input '{}'",
				attrs_json_string(&self.attrs)
			)));
		};
		to_url(scheme, self)
	}

	/// `Input::toURLString`, adding `extra` to the query.
	pub fn to_url_string(&self, extra: &BTreeMap<String, String>) -> Result<String> {
		let mut url = self.to_url()?;
		for (name, value) in extra {
			url.query
				.entry(name.clone())
				.or_insert_with(|| value.clone());
		}
		Ok(url.to_string())
	}

	/// The URL form for messages; unsupported inputs show as JSON.
	pub fn display(&self) -> String {
		self.to_url()
			.map(|url| url.to_string())
			.unwrap_or_else(|error| error.message)
	}

	/// `Input::applyOverrides`.
	pub fn apply_overrides(&self, r#ref: Option<&str>, rev: Option<&NixHash>) -> Result<Input> {
		let Some(scheme) = self.scheme else {
			return Ok(self.clone());
		};
		let mut res = self.clone();
		match scheme {
			Scheme::Git => {
				if let Some(rev) = rev {
					res.attrs.insert("rev".into(), git_rev(rev).into());
				}
				if let Some(r#ref) = r#ref {
					res.attrs.insert("ref".into(), r#ref.into());
				}
				if res.get_ref()?.is_none() && res.get_rev()?.is_some() {
					return Err(Error::new(format!(
						"Git input '{}' has a commit hash but no branch/tag name",
						res.display()
					)));
				}
			}
			Scheme::Mercurial | Scheme::Indirect => {
				if let Some(rev) = rev {
					res.attrs.insert("rev".into(), git_rev(rev).into());
				}
				if let Some(r#ref) = r#ref {
					res.attrs.insert("ref".into(), r#ref.into());
				}
			}
			Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut => {
				if let (Some(rev), Some(r#ref)) = (rev, r#ref) {
					return Err(Error::bad_url(format!(
						"cannot apply both a commit hash ({}) and a branch/tag name ('{}') to input '{}'",
						git_rev(rev),
						r#ref,
						res.display()
					)));
				}
				if let Some(rev) = rev {
					res.attrs.insert("rev".into(), git_rev(rev).into());
					res.attrs.remove("ref");
				}
				if let Some(r#ref) = r#ref {
					res.attrs.insert("ref".into(), r#ref.into());
					res.attrs.remove("rev");
				}
			}
			Scheme::File | Scheme::Tarball | Scheme::Path => {
				if let Some(r#ref) = r#ref {
					return Err(Error::new(format!(
						"don't know how to set branch/tag name of input '{}' to '{}'",
						res.display(),
						r#ref
					)));
				}
				if let Some(rev) = rev {
					return Err(Error::new(format!(
						"don't know how to set revision of input '{}' to '{}'",
						res.display(),
						git_rev(rev)
					)));
				}
			}
		}
		Ok(res)
	}

	/// The fingerprint CppNix uses to cache source hashes, if any.
	pub fn fingerprint_rev(&self) -> Option<String> {
		self.get_rev().ok().flatten().map(|rev| git_rev(&rev))
	}
}

impl fmt::Display for Input {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&self.display())
	}
}

/// `canonPath` without resolving symlinks: drop `.` and empty components
/// and apply `..` lexically.
pub fn canon_path(path: &str) -> PathBuf {
	let mut parts: Vec<&str> = Vec::new();
	for component in path.split('/') {
		match component {
			"" | "." => {}
			".." => {
				parts.pop();
			}
			other => parts.push(other),
		}
	}
	PathBuf::from(format!("/{}", parts.join("/")))
}

/// `std::filesystem::absolute`: prefix the current directory, without
/// normalisation.
fn absolute(path: &std::path::Path) -> PathBuf {
	if path.is_absolute() {
		path.to_owned()
	} else {
		std::env::current_dir()
			.map(|cwd| cwd.join(path))
			.unwrap_or_else(|_| path.to_owned())
	}
}

fn input_from_url(
	settings: &FetchSettings,
	scheme: Scheme,
	url: &ParsedUrl,
	require_tree: bool,
) -> Result<Option<Input>> {
	let (application, transport) = parse_url_scheme(&url.scheme);
	match scheme {
		Scheme::File | Scheme::Tarball => {
			let has_tarball_extension = url.path.last().is_some_and(|last| {
				[
					".zip", ".tar", ".tgz", ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst",
				]
				.iter()
				.any(|ext| last.ends_with(ext))
			});
			let valid_transport = matches!(transport, "file" | "http" | "https");
			let valid = valid_transport
				&& match application {
					Some(application) => application == scheme.name(),
					None if scheme == Scheme::File => !require_tree && !has_tarball_extension,
					None => require_tree || has_tarball_extension,
				};
			if !valid {
				return Ok(None);
			}
			let mut url = url.clone();
			url.scheme = transport.to_owned();
			let mut attrs = Attrs::new();
			if let Some(nar_hash) = url.query.get("narHash") {
				attrs.insert("narHash".into(), nar_hash.as_str().into());
			}
			if let Some(rev) = url.query.get("rev") {
				attrs.insert("rev".into(), rev.as_str().into());
			}
			for name in ["revCount", "lastModified"] {
				if let Some(n) = url.query.get(name).and_then(|n| parse_u64(n)) {
					attrs.insert(name.into(), n.into());
				}
			}
			for name in scheme.allowed_attrs() {
				url.query.remove(*name);
			}
			attrs.insert("type".into(), scheme.name().into());
			attrs.insert("url".into(), url.to_string().into());
			Ok(Some(Input {
				attrs,
				scheme: None,
			}))
		}
		Scheme::Git => {
			if url.scheme != "git" && application != Some("git") {
				return Ok(None);
			}
			let mut url2 = url.clone();
			url2.query.clear();
			let mut attrs = Attrs::new();
			attrs.insert("type".into(), "git".into());
			for (name, value) in &url.query {
				match name.as_str() {
					"rev" | "ref" | "keytype" | "publicKey" | "publicKeys" => {
						attrs.insert(name.clone(), value.as_str().into());
					}
					"shallow" | "submodules" | "lfs" | "exportIgnore" | "allRefs"
					| "verifyCommit" => {
						attrs.insert(name.clone(), (value == "1").into());
					}
					_ => {
						url2.query.insert(name.clone(), value.clone());
					}
				}
			}
			attrs.insert("url".into(), url2.to_string().into());
			input_from_attrs(settings, scheme, attrs).map(Some)
		}
		Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut => {
			if url.scheme != scheme.name() {
				return Ok(None);
			}
			let path: Vec<&str> = url.path_segments().collect();
			let mut attrs = Attrs::new();
			match path.len() {
				3 => {
					let key = if is_rev(path[2]) { "rev" } else { "ref" };
					attrs.insert(key.into(), path[2].into());
				}
				n if n > 3 => {
					attrs.insert("ref".into(), path[2..].join("/").into());
				}
				n if n < 2 => {
					return Err(Error::bad_url(format!("URL '{url}' is invalid")));
				}
				_ => {}
			}
			for (name, value) in &url.query {
				match name.as_str() {
					"rev" => {
						if attrs.contains_key("rev") {
							return Err(Error::bad_url(format!(
								"URL '{url}' contains multiple commit hashes"
							)));
						}
						attrs.insert("rev".into(), value.as_str().into());
					}
					"ref" => {
						if attrs.contains_key("ref") {
							return Err(Error::bad_url(format!(
								"URL '{url}' contains multiple branch/tag names"
							)));
						}
						attrs.insert("ref".into(), value.as_str().into());
					}
					"host" | "narHash" => {
						attrs.insert(name.clone(), value.as_str().into());
					}
					_ => {
						return Err(Error::bad_url(format!(
							"URL '{url}' contains unknown parameter '{name}'"
						)));
					}
				}
			}
			attrs.insert("type".into(), scheme.name().into());
			attrs.insert("owner".into(), path[0].into());
			attrs.insert("repo".into(), path[1].into());
			input_from_attrs(settings, scheme, attrs).map(Some)
		}
		Scheme::Mercurial => {
			if !matches!(
				url.scheme.as_str(),
				"hg+http" | "hg+https" | "hg+ssh" | "hg+file"
			) {
				return Ok(None);
			}
			let mut url2 = url.clone();
			url2.scheme = url.scheme[3..].to_owned();
			url2.query.clear();
			let mut attrs = Attrs::new();
			attrs.insert("type".into(), "hg".into());
			for (name, value) in &url.query {
				if name == "rev" || name == "ref" {
					attrs.insert(name.clone(), value.as_str().into());
				} else {
					url2.query.insert(name.clone(), value.clone());
				}
			}
			attrs.insert("url".into(), url2.to_string().into());
			input_from_attrs(settings, scheme, attrs).map(Some)
		}
		Scheme::Indirect => {
			if url.scheme != "flake" {
				return Ok(None);
			}
			let path: Vec<&str> = url.path_segments().collect();
			let (mut r#ref, mut rev) = (None, None);
			match path.len() {
				1 => {}
				2 => {
					if is_rev(path[1]) {
						rev = Some(parse_rev(path[1])?);
					} else if is_legal_ref_name(path[1]) {
						r#ref = Some(path[1]);
					} else {
						return Err(Error::bad_url(format!(
							"in flake URL '{url}', '{}' is not a commit hash or branch/tag name",
							path[1]
						)));
					}
				}
				3 => {
					if !is_legal_ref_name(path[1]) {
						return Err(Error::bad_url(format!(
							"in flake URL '{url}', '{}' is not a branch/tag name",
							path[1]
						)));
					}
					r#ref = Some(path[1]);
					if !is_rev(path[2]) {
						return Err(Error::bad_url(format!(
							"in flake URL '{url}', '{}' is not a commit hash",
							path[2]
						)));
					}
					rev = Some(parse_rev(path[2])?);
				}
				_ => return Err(Error::bad_url(format!("GitHub URL '{url}' is invalid"))),
			}
			let id = path.first().copied().unwrap_or_default();
			if !is_flake_id(id) {
				return Err(Error::bad_url(format!("'{id}' is not a valid flake ID")));
			}
			let mut attrs = Attrs::new();
			attrs.insert("type".into(), "indirect".into());
			attrs.insert("id".into(), id.into());
			if let Some(rev) = rev {
				attrs.insert("rev".into(), git_rev(&rev).into());
			}
			if let Some(r#ref) = r#ref {
				attrs.insert("ref".into(), r#ref.into());
			}
			Ok(Some(Input {
				attrs,
				scheme: None,
			}))
		}
		Scheme::Path => {
			if url.scheme != "path" {
				return Ok(None);
			}
			if let Some(authority) = &url.authority
				&& !authority.host.is_empty()
			{
				return Err(Error::new(format!(
					"path URL '{url}' should not have an authority ('{authority}')"
				)));
			}
			let mut attrs = Attrs::new();
			attrs.insert("type".into(), "path".into());
			attrs.insert(
				"path".into(),
				url_path_to_path(&url.path)?
					.to_string_lossy()
					.as_ref()
					.into(),
			);
			for (name, value) in &url.query {
				match name.as_str() {
					"rev" | "narHash" => {
						attrs.insert(name.clone(), value.as_str().into());
					}
					"revCount" | "lastModified" => match parse_u64(value) {
						Some(n) => {
							attrs.insert(name.clone(), n.into());
						}
						None => {
							return Err(Error::new(format!(
								"path URL '{url}' has invalid parameter '{name}'"
							)));
						}
					},
					_ => {
						return Err(Error::new(format!(
							"path URL '{url}' has unsupported parameter '{name}'"
						)));
					}
				}
			}
			Ok(Some(Input {
				attrs,
				scheme: None,
			}))
		}
	}
}

/// `string2Int<uint64_t>`.
fn parse_u64(s: &str) -> Option<u64> {
	if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
		return None;
	}
	s.parse().ok()
}

fn input_from_attrs(settings: &FetchSettings, scheme: Scheme, attrs: Attrs) -> Result<Input> {
	match scheme {
		Scheme::Git => {
			for name in attrs.keys() {
				if matches!(
					name.as_str(),
					"verifyCommit" | "keytype" | "publicKey" | "publicKeys"
				) {
					require_feature(settings.verified_fetches, "verified-fetches")?;
				}
			}
			maybe_get_bool_attr(&attrs, "verifyCommit")?;
			if let Some(r#ref) = maybe_get_str_attr(&attrs, "ref")?
				&& !is_legal_ref_name(&r#ref)
			{
				return Err(Error::bad_url(format!(
					"invalid Git branch/tag name '{ref}'"
				)));
			}
			let mut attrs = attrs;
			let url = fix_git_url(&get_str_attr(&attrs, "url")?)?;
			attrs.insert("url".into(), url.to_string().into());
			for name in ["shallow", "submodules", "allRefs"] {
				maybe_get_bool_attr(&attrs, name)?;
			}
			Ok(Input {
				attrs,
				scheme: None,
			})
		}
		Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut => {
			get_str_attr(&attrs, "owner")?;
			get_str_attr(&attrs, "repo")?;
			let r#ref = maybe_get_str_attr(&attrs, "ref")?;
			let rev = maybe_get_str_attr(&attrs, "rev")?;
			if let (Some(r#ref), Some(rev)) = (&r#ref, &rev) {
				return Err(Error::bad_url(format!(
					"input {} contains both a commit hash ('{rev}') and a branch/tag name ('{ref}')",
					attrs_json_string(&attrs)
				)));
			}
			if let Some(rev) = &rev {
				NixHash::from_str(rev, Some(HashAlgo::Sha1)).map_err(|_| {
					Error::bad_url(format!(
						"hash '{rev}' has wrong length for hash algorithm 'sha1'"
					))
				})?;
			}
			if let Some(r#ref) = &r#ref
				&& !is_legal_ref_name(r#ref)
			{
				return Err(Error::bad_url(format!(
					"input {} contains an invalid branch/tag name",
					attrs_json_string(&attrs)
				)));
			}
			if let Some(host) = maybe_get_str_attr(&attrs, "host")?
				&& !host
					.bytes()
					.all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
			{
				return Err(Error::bad_url(format!(
					"input {} contains an invalid instance host",
					attrs_json_string(&attrs)
				)));
			}
			Ok(Input {
				attrs,
				scheme: None,
			})
		}
		Scheme::Mercurial => {
			parse_url(&get_str_attr(&attrs, "url")?, false)?;
			if let Some(r#ref) = maybe_get_str_attr(&attrs, "ref")?
				&& !matches_ref_regex(&r#ref)
			{
				return Err(Error::bad_url(format!(
					"invalid Mercurial branch/tag name '{ref}'"
				)));
			}
			Ok(Input {
				attrs,
				scheme: None,
			})
		}
		Scheme::Indirect => {
			let id = get_str_attr(&attrs, "id")?;
			if !is_flake_id(&id) {
				return Err(Error::bad_url(format!("'{id}' is not a valid flake ID")));
			}
			Ok(Input {
				attrs,
				scheme: None,
			})
		}
		Scheme::Path => {
			get_str_attr(&attrs, "path")?;
			Ok(Input {
				attrs,
				scheme: None,
			})
		}
		Scheme::File | Scheme::Tarball => Ok(Input {
			attrs,
			scheme: None,
		}),
	}
}

fn to_url(scheme: Scheme, input: &Input) -> Result<ParsedUrl> {
	let attrs = &input.attrs;
	match scheme {
		Scheme::File | Scheme::Tarball => {
			let mut url = parse_url(&get_str_attr(attrs, "url")?, false)?;
			if let Some(nar_hash) = input.get_nar_hash()? {
				url.query.insert("narHash".into(), nar_hash.to_sri_string());
			}
			Ok(url)
		}
		Scheme::Git => {
			let mut url = parse_url(&get_str_attr(attrs, "url")?, false)?;
			if url.scheme != "git" {
				url.scheme = format!("git+{}", url.scheme);
			}
			if let Some(rev) = input.get_rev()? {
				url.query.insert("rev".into(), git_rev(&rev));
			}
			if let Some(r#ref) = input.get_ref()? {
				url.query.insert("ref".into(), r#ref);
			}
			for (name, query) in [
				("shallow", "shallow"),
				("lfs", "lfs"),
				("submodules", "submodules"),
				("exportIgnore", "exportIgnore"),
				("verifyCommit", "verifyCommit"),
			] {
				if maybe_get_bool_attr(attrs, name)?.unwrap_or(false) {
					url.query.insert(query.into(), "1".into());
				}
			}
			let public_keys = get_public_keys(attrs)?;
			if public_keys.len() == 1 {
				url.query.insert("keytype".into(), public_keys[0].0.clone());
				url.query
					.insert("publicKey".into(), public_keys[0].1.clone());
			} else if public_keys.len() > 1 {
				let json: Vec<serde_json::Value> = public_keys
					.iter()
					.map(|(t, k)| {
						let mut map = serde_json::Map::new();
						map.insert("key".into(), k.clone().into());
						map.insert("type".into(), t.clone().into());
						serde_json::Value::Object(map)
					})
					.collect();
				url.query.insert(
					"publicKeys".into(),
					crate::json::dump(&serde_json::Value::Array(json), None),
				);
			}
			Ok(url)
		}
		Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut => {
			let mut path = vec![get_str_attr(attrs, "owner")?, get_str_attr(attrs, "repo")?];
			if let Some(r#ref) = input.get_ref()? {
				path.push(r#ref);
			}
			if let Some(rev) = input.get_rev()? {
				path.push(git_rev(&rev));
			}
			let mut url = ParsedUrl {
				scheme: scheme.name().into(),
				path,
				..ParsedUrl::default()
			};
			if let Some(nar_hash) = input.get_nar_hash()? {
				url.query.insert("narHash".into(), nar_hash.to_sri_string());
			}
			if let Some(host) = maybe_get_str_attr(attrs, "host")? {
				url.query.insert("host".into(), host);
			}
			Ok(url)
		}
		Scheme::Mercurial => {
			let mut url = parse_url(&get_str_attr(attrs, "url")?, false)?;
			url.scheme = format!("hg+{}", url.scheme);
			if let Some(rev) = input.get_rev()? {
				url.query.insert("rev".into(), git_rev(&rev));
			}
			if let Some(r#ref) = input.get_ref()? {
				url.query.insert("ref".into(), r#ref);
			}
			Ok(url)
		}
		Scheme::Indirect => {
			let mut path = vec![get_str_attr(attrs, "id")?];
			if let Some(r#ref) = input.get_ref()? {
				path.push(r#ref);
			}
			if let Some(rev) = input.get_rev()? {
				path.push(git_rev(&rev));
			}
			Ok(ParsedUrl {
				scheme: "flake".into(),
				path,
				..ParsedUrl::default()
			})
		}
		Scheme::Path => {
			let mut query = attrs_to_query(attrs)?;
			query.remove("path");
			query.remove("type");
			query.remove("__final");
			Ok(ParsedUrl {
				scheme: "path".into(),
				path: path_to_url_path(std::path::Path::new(&get_str_attr(attrs, "path")?)),
				query,
				..ParsedUrl::default()
			})
		}
	}
}

/// `getPublicKeys`: `publicKeys` JSON, then `publicKey` with `keytype`.
pub fn get_public_keys(attrs: &Attrs) -> Result<Vec<(String, String)>> {
	let mut keys = Vec::new();
	if let Some(json) = maybe_get_str_attr(attrs, "publicKeys")? {
		let parsed: serde_json::Value = serde_json::from_str(&json)
			.map_err(|e| Error::new(format!("parsing publicKeys: {e}")))?;
		for key in parsed.as_array().into_iter().flatten() {
			let r#type = key
				.get("type")
				.and_then(|t| t.as_str())
				.unwrap_or("ssh-ed25519");
			let value = key
				.get("key")
				.and_then(|k| k.as_str())
				.ok_or_else(|| Error::new("public key lacks 'key'"))?;
			keys.push((r#type.to_owned(), value.to_owned()));
		}
	}
	if let Some(key) = maybe_get_str_attr(attrs, "publicKey")? {
		keys.push((
			maybe_get_str_attr(attrs, "keytype")?.unwrap_or_else(|| "ssh-ed25519".into()),
			key,
		));
	}
	Ok(keys)
}

/// Report an unlocked input that is only checked by NAR hash.
pub fn warn_unlocked_with_nar_hash(what: &str, input: &str) {
	warn(&format!(
		"{what} '{input}' is unlocked (e.g. lacks a Git revision) but is checked by NAR hash. This is not reproducible and will break after garbage collection or when shared."
	));
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

	fn roundtrip(url: &str) -> String {
		Input::from_url_string(&settings(), url, true)
			.unwrap()
			.display()
	}

	#[test]
	fn ref_names_follow_libgit2() {
		for name in [
			"main",
			"HEAD",
			"refs/heads/main",
			"v1.2.3",
			"feature/x",
			"a@b",
		] {
			assert!(is_legal_ref_name(name), "{name}");
		}
		for name in [
			"@", "a..b", "a.lock", ".a", "a b", "a~1", "-a", "a/", "a@{1}", "a.",
		] {
			assert!(!is_legal_ref_name(name), "{name}");
		}
	}

	#[test]
	fn formats_inputs_like_cppnix() {
		assert_eq!(
			roundtrip("github:foo/bar/branch%23"),
			"github:foo/bar/branch%23"
		);
		assert_eq!(
			roundtrip("github://////owner%42/////repo%41///branch%43////"),
			"github:ownerB/repoA/branchC"
		);
		assert_eq!(
			roundtrip("git+file:///tmp/repo?ref=main"),
			"git+file:///tmp/repo?ref=main"
		);
		assert_eq!(
			roundtrip("git://somewhere/repo?ref=branch"),
			"git://somewhere/repo?ref=branch"
		);
		assert_eq!(
			roundtrip("path:/tmp/x?lastModified=3"),
			"path:/tmp/x?lastModified=3"
		);
		assert_eq!(roundtrip("flake:nixpkgs/main"), "flake:nixpkgs/main");
		assert_eq!(
			roundtrip(
				"https://example.org/a.tar.gz?narHash=sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA%3D"
			),
			"https://example.org/a.tar.gz?narHash=sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA%3D"
		);
	}

	#[test]
	fn selects_file_or_tarball() {
		let s = settings();
		let ty = |url: &str, tree| {
			Input::from_url_string(&s, url, tree)
				.unwrap()
				.get_type()
				.unwrap()
		};
		assert_eq!(ty("https://e.org/x", false), "file");
		assert_eq!(ty("https://e.org/x", true), "tarball");
		assert_eq!(ty("https://e.org/x.tar.gz", false), "tarball");
		assert_eq!(ty("file+https://e.org/x.tar.gz", true), "file");
	}
}
