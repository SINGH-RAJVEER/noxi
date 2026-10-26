//! Interface for injecting I/O-related functionality into nix-rs-eval.
//!
//! The Nix language contains several builtins (e.g. `builtins.readDir`), as
//! well as language feature (e.g. string-"coercion" of paths) that interact
//! with the filesystem.
//!
//! The language evaluator implemented by this crate does not depend on any
//! particular filesystem interaction model. Instead, this module provides a
//! trait that can be implemented by nix-rs-eval callers to provide the
//! functionality they desire.
//!
//! In theory this can be used to implement "mocked" filesystem interactions, or
//! interaction with remote filesystems, etc.
//!
//! In the context of Nix builds, callers also use this interface to determine
//! how store paths are opened and so on.

use bstr::ByteSlice;
use path_clean::PathClean;

use crate::{NixString, Value};

use std::{
	ffi::{OsStr, OsString},
	io,
	path::{Path, PathBuf},
};

#[cfg(all(target_family = "unix", feature = "impure"))]
use std::os::unix::ffi::OsStringExt;

#[cfg(feature = "impure")]
use std::fs::File;

/// Types of files as represented by `builtins.readFileType` and `builtins.readDir` in Nix.
#[derive(Debug)]
pub enum FileType {
	Directory,
	Regular,
	Symlink,
	Unknown,
}

impl std::fmt::Display for FileType {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let type_as_str = match &self {
			FileType::Directory => "directory",
			FileType::Regular => "regular",
			FileType::Symlink => "symlink",
			FileType::Unknown => "unknown",
		};

		write!(f, "{type_as_str}")
	}
}

impl From<std::fs::FileType> for FileType {
	fn from(value: std::fs::FileType) -> Self {
		if value.is_file() {
			Self::Regular
		} else if value.is_dir() {
			Self::Directory
		} else if value.is_symlink() {
			Self::Symlink
		} else {
			Self::Unknown
		}
	}
}

/// Represents all possible filesystem interactions that exist in the Nix
/// language, and that need to be executed somehow.
///
/// This trait is specifically *only* concerned with what is visible on the
/// level of the language. All internal implementation details are not part of
/// this trait.
pub trait EvalIO {
	/// Verify whether the file at the specified path exists.
	///
	/// This is used for the following language evaluation cases:
	///
	/// * checking whether a file added to the `NIX_PATH` actually exists when
	///   it is referenced in `<...>` brackets.
	/// * `builtins.pathExists :: path -> bool`
	fn path_exists(&self, path: &Path) -> io::Result<bool>;

	/// Resolve every symlink in the path, as CppNix's `resolveSymlinks` does
	/// for a search-path result. An error means the path does not exist.
	fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
		if self.path_exists(path)? {
			Ok(path.to_owned())
		} else {
			Err(io::ErrorKind::NotFound.into())
		}
	}

	/// Read a symlink without resolving its target. Virtual filesystems can
	/// override this when importing expressions through symbolic links.
	fn read_link(&self, _path: &Path) -> io::Result<PathBuf> {
		Err(io::ErrorKind::Unsupported.into())
	}

	/// Resolve the final symlink and select `default.nix` for a directory.
	/// Ancestor symlinks remain in the expression's lexical path, matching
	/// CppNix's `resolveExprPath` and its relative path semantics.
	fn resolve_import_path(&self, path: &Path) -> io::Result<PathBuf> {
		let mut resolved = path.to_owned().clean();
		let mut follow_count = 0;
		while let Some(parent) = resolved.parent() {
			follow_count += 1;
			if follow_count >= 1024 {
				return Err(io::Error::other(format!(
					"too many symbolic links encountered while traversing the path '{}'",
					resolved.display()
				)));
			}
			let physical = self
				.canonicalize(parent)?
				.join(resolved.file_name().unwrap());
			if !matches!(self.file_type(&physical)?, FileType::Symlink) {
				break;
			}
			resolved = parent.join(self.read_link(&physical)?).clean();
		}
		// canonicalize also rejects a remaining cycle or an excessive chain.
		let real = self.canonicalize(&resolved)?;
		if matches!(self.file_type(&real)?, FileType::Directory) {
			resolved.push("default.nix");
		}
		Ok(resolved)
	}

	/// Realize the dependencies of a path string before filesystem access.
	/// Store integrations build requested outputs and rewrite placeholders.
	fn realise_path(&self, path: &NixString) -> io::Result<PathBuf> {
		Ok(path.to_path().map_err(io::Error::other)?.to_owned())
	}

	/// Read a registered store derivation as its raw import attributes:
	/// `drvPath`, `name`, `outputs`, and each output's contextual string.
	/// `None` leaves the path to ordinary Nix expression import.
	fn import_derivation(&self, _path: &Path) -> io::Result<Option<Value>> {
		Ok(None)
	}

	/// Open the file at the specified path to a `io::Read`.
	fn open(&self, path: &Path) -> io::Result<Box<dyn io::Read>>;

	/// Read a Nix string, retaining references that occur in a stored file.
	fn read_file(&self, path: &Path) -> io::Result<NixString> {
		let mut bytes = Vec::new();
		self.open(path)?.read_to_end(&mut bytes)?;
		if bytes.contains(&0) {
			return Err(io::Error::other(format!(
				"the contents of the file '{}' cannot be represented as a Nix string",
				path.display()
			)));
		}
		Ok(NixString::from(bytes))
	}

	/// Return the [FileType] of the given path, or an error if it doesn't
	/// exist.
	fn file_type(&self, path: &Path) -> io::Result<FileType>;

	/// Read the directory at the specified path and return the names
	/// of its entries associated with their [`FileType`].
	///
	/// This is used for the following language evaluation cases:
	///
	/// * `builtins.readDir :: path -> attrs<filename, filetype>`
	fn read_dir(&self, path: &Path) -> io::Result<Vec<(bytes::Bytes, FileType)>>;

	/// Import the given path. What this means depends on the implementation,
	/// for example for a `std::io`-based implementation this might be a no-op,
	/// while for a nix-rs store this might be a copy of the given files to the
	/// store.
	///
	/// This is used for the following language evaluation cases:
	///
	/// * string coercion of path literals (e.g. `/foo/bar`), which are expected
	///   to return a path
	/// * `builtins.toJSON` on a path literal, also expected to return a path
	fn import_path(&self, path: &Path) -> io::Result<PathBuf>;

	/// Returns the root of the store directory, if such a thing
	/// exists in the evaluation context.
	///
	/// This is used for the following language evaluation cases:
	///
	/// * `builtins.storeDir :: string`
	fn store_dir(&self) -> Option<String> {
		None
	}

	/// Fetches the environment variable key from the current process.
	fn get_env(&self, key: &OsStr) -> Option<OsString>;
}

/// Implementation of [`EvalIO`] that simply uses the equivalent
/// standard library functions, i.e. does local file-IO.
#[cfg(feature = "impure")]
pub struct StdIO;

// TODO: we might want to make this whole impl to be target_family = "unix".
#[cfg(feature = "impure")]
impl EvalIO for StdIO {
	fn path_exists(&self, path: &Path) -> io::Result<bool> {
		// CppNix resolves ancestor symlinks but uses lstat for the final
		// component. A dangling symlink therefore exists. A trailing slash
		// or `/.` is kept in the syscall path and requires a directory.
		Ok(std::fs::symlink_metadata(path).is_ok())
	}

	fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
		std::fs::canonicalize(path)
	}

	fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
		std::fs::read_link(path)
	}

	fn open(&self, path: &Path) -> io::Result<Box<dyn io::Read>> {
		Ok(Box::new(File::open(path)?))
	}

	fn file_type(&self, path: &Path) -> io::Result<FileType> {
		let file_type = std::fs::symlink_metadata(path)?;

		Ok(if file_type.is_dir() {
			FileType::Directory
		} else if file_type.is_file() {
			FileType::Regular
		} else if file_type.is_symlink() {
			FileType::Symlink
		} else {
			FileType::Unknown
		})
	}

	fn read_dir(&self, path: &Path) -> io::Result<Vec<(bytes::Bytes, FileType)>> {
		let mut result = vec![];

		for entry in path.read_dir()? {
			let entry = entry?;
			// Use entry.file_type() instead of entry.metadata() to avoid stat syscalls.
			// file_type() uses the d_type field from the readdir() syscall (when available),
			// which is cached in the DirEntry and doesn't require additional filesystem access.
			let file_type = entry.file_type()?;

			let val = if file_type.is_dir() {
				FileType::Directory
			} else if file_type.is_file() {
				FileType::Regular
			} else if file_type.is_symlink() {
				FileType::Symlink
			} else {
				FileType::Unknown
			};

			result.push((entry.file_name().into_vec().into(), val))
		}

		Ok(result)
	}

	// this is a no-op for `std::io`, as the user can already refer to
	// the path directly
	fn import_path(&self, path: &Path) -> io::Result<PathBuf> {
		Ok(path.to_path_buf())
	}

	fn get_env(&self, key: &OsStr) -> Option<OsString> {
		std::env::var_os(key)
	}
}

/// Dummy implementation of [`EvalIO`], can be used in contexts where
/// IO is not available but code should "pretend" that it is.
pub struct DummyIO;

impl EvalIO for DummyIO {
	fn path_exists(&self, _: &Path) -> io::Result<bool> {
		Err(io::Error::new(
			io::ErrorKind::Unsupported,
			"I/O methods are not implemented in DummyIO",
		))
	}

	fn open(&self, _: &Path) -> io::Result<Box<dyn io::Read>> {
		Err(io::Error::new(
			io::ErrorKind::Unsupported,
			"I/O methods are not implemented in DummyIO",
		))
	}

	fn file_type(&self, _: &Path) -> io::Result<FileType> {
		Err(io::Error::new(
			io::ErrorKind::Unsupported,
			"I/O methods are not implemented in DummyIO",
		))
	}

	fn read_dir(&self, _: &Path) -> io::Result<Vec<(bytes::Bytes, FileType)>> {
		Err(io::Error::new(
			io::ErrorKind::Unsupported,
			"I/O methods are not implemented in DummyIO",
		))
	}

	fn import_path(&self, _: &Path) -> io::Result<PathBuf> {
		Err(io::Error::new(
			io::ErrorKind::Unsupported,
			"I/O methods are not implemented in DummyIO",
		))
	}

	fn get_env(&self, _: &OsStr) -> Option<OsString> {
		None
	}
}
