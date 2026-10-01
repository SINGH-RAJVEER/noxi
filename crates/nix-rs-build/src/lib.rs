//! Derivation builds for the nix-rs engine, following CppNix 2.35.2's
//! `src/libstore/build` and `src/libstore/unix/build`.
//!
//! The crate is a preview: [`options`] is checked against the reference's
//! fixtures, while the sandbox, settings and reference scanner are not yet
//! connected to a scheduler or qualified against the reference.

pub mod error;
pub mod options;
pub mod refscan;
pub mod sandbox;
pub mod seccomp;
pub mod settings;

pub use error::{BuildError, FailureStatus, Result};
