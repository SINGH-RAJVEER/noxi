use std::io;
use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("{context}: {source}")]
	Io {
		context: String,
		#[source]
		source: io::Error,
	},

	#[error(transparent)]
	Os(#[from] io::Error),

	#[error("SQLite error: {0}")]
	Sqlite(#[from] rusqlite::Error),

	/// The path is not registered in the database.
	#[error("path '{0}' is not valid")]
	InvalidPath(String),

	/// The path still has referrers, so it cannot be removed.
	#[error("cannot delete path '{path}' because it is in use by {referrers}")]
	PathInUse { path: String, referrers: String },

	#[error("{0} is corrupt")]
	CorruptSchema(PathBuf),

	/// A build or substitution failed; commands exit with `exit_status`,
	/// CppNix's `failingExitStatus`.
	#[error("{message}")]
	Build { exit_status: u8, message: String },

	/// Any other failure, with CppNix's message.
	#[error("{0}")]
	Other(String),
}

impl Error {
	pub fn io(context: impl Into<String>, source: io::Error) -> Self {
		Error::Io {
			context: context.into(),
			source,
		}
	}
}

/// Adds a description to I/O errors.
pub(crate) trait IoContext<T> {
	fn context(self, context: impl FnOnce() -> String) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
	fn context(self, context: impl FnOnce() -> String) -> Result<T> {
		self.map_err(|source| Error::io(context(), source))
	}
}
