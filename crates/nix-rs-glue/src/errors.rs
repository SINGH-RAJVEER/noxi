//! Errors raised by the builtins in this crate.
use std::path::PathBuf;
use std::sync::Arc;

use nix_compat::derivation::OutputName;
use thiserror::Error;

/// Errors related to derivation construction.
#[derive(Debug, Error)]
pub enum DerivationError {
	#[error("an output with the name '{0}' is already defined")]
	DuplicateOutput(OutputName),
	#[error("fixed-output derivations can only have the default `out`-output")]
	ConflictingOutputTypes,
	#[error("the environment variable '{0}' has already been set in this derivation")]
	DuplicateEnvVar(String),
	#[error(
		"Cannot have an environment variable named '__json'. This key is reserved for encoding structured attrs"
	)]
	StructuredAttrsJsonKeyPresent,
	#[error("derivation name '{0}' is not a valid store path name")]
	InvalidName(String),
	#[error("string context element '{0}' is not a store path")]
	InvalidContext(String),
	#[error("path '{0}' is not valid")]
	InvalidPath(String),
	#[error("derivation '{0}' is not known to this evaluation")]
	UnknownDerivation(String),
	#[error("invalid derivation parameters: {0}")]
	InvalidDerivation(#[from] nix_compat::derivation::DerivationError),
}

impl From<DerivationError> for nix_rs_eval::ErrorKind {
	fn from(err: DerivationError) -> Self {
		nix_rs_eval::ErrorKind::NixRsError(Arc::from(err))
	}
}

/// Errors from copying paths into the store, which this crate only computes.
#[derive(Debug, Error)]
pub enum ImportError {
	#[error("file names are not allowed to end in '.drv': '{}'", .0.display())]
	DerivationFileName(PathBuf),
	#[error("path '{}' has no valid store path name", .0.display())]
	InvalidName(PathBuf),
	#[error("file '{}' has an unsupported type", .0.display())]
	UnsupportedFileType(PathBuf),
	#[error(
		"in 'toFile': the file named '{0}' must not contain a reference to a derivation output"
	)]
	ToFileOutputReference(String),
}

impl From<ImportError> for nix_rs_eval::ErrorKind {
	fn from(err: ImportError) -> Self {
		nix_rs_eval::ErrorKind::NixRsError(Arc::from(err))
	}
}
