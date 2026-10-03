//! Derivation builds for the nix-rs engine, following CppNix 2.35.2's
//! `src/libstore/build` and `src/libstore/unix/build`.
//!
//! The crate is a preview. [`options`] is checked against the reference's
//! fixtures, and [`local::LocalBuilder`] builds derivations in the
//! [`sandbox`] with results compared against the reference, and is connected to the experimental diverted-store commands. It is not
//! qualified as a system build engine.

pub mod error;
pub mod local;
pub mod missing;
pub mod options;
pub mod refscan;
pub mod sandbox;
pub mod seccomp;
pub mod settings;
pub mod structured;

pub use error::{BuildError, FailureStatus, Result};
