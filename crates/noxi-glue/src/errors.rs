//! Errors raised by the builtins in this crate.
use std::path::PathBuf;
use std::sync::Arc;

use nix_compat::derivation::OutputName;
use thiserror::Error;

/// Errors related to derivation construction.
#[derive(Debug, Error)]
pub enum DerivationError {
	#[error("duplicate derivation output '{0}'")]
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

impl From<DerivationError> for noxi_eval::ErrorKind {
	fn from(err: DerivationError) -> Self {
		noxi_eval::ErrorKind::NoxiError(Arc::from(err))
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

impl From<ImportError> for noxi_eval::ErrorKind {
	fn from(err: ImportError) -> Self {
		noxi_eval::ErrorKind::NoxiError(Arc::from(err))
	}
}

/// CppNix's store-name validator explanation, shared by fetching and derivations.
pub(crate) fn invalid_store_name(name: &str) -> Option<String> {
	if name.is_empty() {
		return Some("name is empty".into());
	}
	if name.len() > 211 {
		return Some(format!("name '{name}' is too long"));
	}
	if name.starts_with('.') {
		return Some(format!("name '{name}' starts with a period"));
	}
	for ch in name.chars() {
		if !(ch.is_ascii_alphanumeric() || "+-._?=".contains(ch)) {
			return Some(format!("name '{name}' contains illegal character '{ch}'"));
		}
	}
	None
}
