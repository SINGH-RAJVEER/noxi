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

impl FailureStatus {
	/// CppNix's `ExitStatusFlags::failingExitStatus` for a single failure:
	/// 100 for a failed build, with bits for a timeout (101), a hash
	/// mismatch (102) and nondeterminism (104), and 1 otherwise.
	pub fn exit_status(self) -> u8 {
		match self {
			FailureStatus::PermanentFailure | FailureStatus::InputRejected => 100,
			FailureStatus::TimedOut => 101,
			FailureStatus::HashMismatch => 102,
			FailureStatus::NotDeterministic => 0b1100000 | 0b1000,
			_ => 1,
		}
	}
}

impl From<BuildError> for nix_rs_store::Error {
	fn from(error: BuildError) -> Self {
		match error {
			BuildError::Store(error) => error,
			BuildError::Failed { status, message } => nix_rs_store::Error::Build {
				exit_status: status.exit_status(),
				message,
			},
			error => nix_rs_store::Error::Other(error.to_string()),
		}
	}
}
