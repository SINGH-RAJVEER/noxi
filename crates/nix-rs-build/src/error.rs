use std::io;

pub type Result<T, E = BuildError> = std::result::Result<T, E>;

/// Why a build failed, CppNix's `BuildResult::Failure::Status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureStatus {
	PermanentFailure,
	InputRejected,
	OutputRejected,
	TransientFailure,
	CachedFailure,
	TimedOut,
	MiscFailure,
	DependencyFailed,
	LogLimitExceeded,
	NotDeterministic,
	NoSubstituters,
	HashMismatch,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
	#[error(transparent)]
	Store(#[from] nix_rs_store::Error),

	#[error(transparent)]
	Io(#[from] io::Error),

	/// The build ran or was attempted and failed.
	#[error("{message}")]
	Failed {
		status: FailureStatus,
		message: String,
	},

	#[error("{0}")]
	Other(String),
}

impl BuildError {
	pub fn failed(status: FailureStatus, message: impl Into<String>) -> Self {
		BuildError::Failed {
			status,
			message: message.into(),
		}
	}

	pub fn status(&self) -> FailureStatus {
		match self {
			BuildError::Failed { status, .. } => *status,
			_ => FailureStatus::MiscFailure,
		}
	}
}
