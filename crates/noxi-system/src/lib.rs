//! Native `noxi` operations with no CppNix counterpart: flake input updates,
//! system generation builds and activation, the snapshot journal that ties
//! a generation to the source and lock it was built from, and the native
//! equivalents of the NixOS `nixos-*` tools.
//!
//! Each module exposes `main(args)` for the `noxi` binary, where `args` are
//! the arguments after the subcommand name.

pub mod enter;
pub mod flake;
pub mod init;
pub mod install;
pub mod option;
pub mod rebuild;
pub mod snapshot;
pub mod version;

use std::io::Write;
use std::process::ExitCode;

use noxi_events::{Diagnostic, Styling};

/// Print a usage error and return the usage exit status.
pub(crate) fn usage(message: &str) -> ExitCode {
	eprintln!("{message}");
	ExitCode::from(2)
}

/// Render `message` as a rustc-style `error:` diagnostic with an
/// optional code and return the failure exit status.
pub(crate) fn fail(
	err: &mut dyn Write,
	styling: &Styling,
	code: Option<&'static str>,
	message: &str,
) -> ExitCode {
	let _ = write!(err, "{}", Diagnostic::error(code, message).render(styling));
	ExitCode::FAILURE
}
