//! Fetched source trees served at their store paths without copying, as
//! CppNix 2.35.2 mounts lazily fetched inputs in its store file system.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// The files of a Git working tree that a fetch may see: tracked files,
/// their ancestors, and nothing hidden by `export-ignore`.
#[derive(Debug, Default)]
pub struct TreeFilter {
	/// Tracked files and submodule mount points, relative to the root.
	pub tracked: BTreeSet<PathBuf>,
	/// Paths hidden by `export-ignore`, relative to the root.
	pub ignored: BTreeSet<PathBuf>,
	/// The repository named in errors about untracked files.
	pub repo: PathBuf,
}

impl TreeFilter {
	/// CppNix's `AllowListSourceAccessor` combined with its export-ignore
	/// filter: the root, tracked paths, their ancestors and descendants.
	pub fn allows(&self, rel: &Path) -> bool {
		if rel.as_os_str().is_empty() {
			return true;
		}
		if rel.ancestors().any(|a| self.ignored.contains(a)) {
			return false;
		}
		if self
			.tracked
			.range(rel.to_path_buf()..)
			.next()
			.is_some_and(|p| p.starts_with(rel))
		{
			return true;
		}
		rel.ancestors().skip(1).any(|a| self.tracked.contains(a))
	}

	/// The error for reading `rel`, worded as CppNix's
	/// `makeNotAllowedError`.
	pub fn not_allowed(&self, rel: &Path) -> io::Error {
		let message = if self.ignored.iter().any(|i| rel.starts_with(i)) {
			format!(
				"'/{}' does not exist because it was fetched with exportIgnore enabled",
				rel.display()
			)
		} else if self.repo.join(rel).exists() {
			format!(
				"Path '{rel}' in the repository \"{repo}\" is not tracked by Git.\n\nTo make it visible to Nix, run:\n\ngit -C \"{repo}\" add -N \"{rel}\"",
				rel = rel.display(),
				repo = self.repo.display()
			)
		} else {
			format!(
				"Path '{}' does not exist in Git repository \"{}\".",
				rel.display(),
				self.repo.display()
			)
		};
		io::Error::new(io::ErrorKind::NotFound, message)
	}
}

/// A fetched tree: a directory or file, optionally filtered.
#[derive(Clone, Debug)]
pub struct Mount {
	/// The physical root, with symlinks in its ancestors resolved.
	pub root: PathBuf,
	pub filter: Option<Rc<TreeFilter>>,
	/// Keeps a materialised tree alive for the evaluation.
	#[allow(dead_code)]
	// Ownership keeps the fetched snapshot alive until its last mount is dropped.
	pub keep: Option<Rc<tempfile::TempDir>>,
}

impl Mount {
	pub fn new(root: PathBuf) -> Self {
		Self {
			root,
			filter: None,
			keep: None,
		}
	}

	/// Whether `physical`, below the root, is visible.
	pub fn allows_physical(&self, physical: &Path) -> bool {
		match (&self.filter, physical.strip_prefix(&self.root)) {
			(Some(filter), Ok(rel)) => filter.allows(rel),
			_ => true,
		}
	}
}
