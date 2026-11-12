//! Native `noxi` operations with no CppNix counterpart: flake input updates,
//! system generation builds and activation, and the snapshot journal that
//! ties a generation to the source and lock it was built from.
//!
//! Each module exposes `main(args)` for the `noxi` binary, where `args` are
//! the arguments after the subcommand name.

pub mod flake;
pub mod rebuild;
pub mod snapshot;

use std::process::ExitCode;

/// Print a usage error and return the usage exit status.
pub(crate) fn usage(message: &str) -> ExitCode {
	eprintln!("{message}");
	ExitCode::from(2)
}
