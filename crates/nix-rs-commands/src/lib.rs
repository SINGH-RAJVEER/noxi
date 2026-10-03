//! CppNix-compatible commands built on the nix-rs evaluator.
//!
//! Each module implements one legacy command's supported subset. Anything
//! outside it fails with an explicit error rather than behaving differently.

pub mod build;
pub mod instantiate;
pub mod store;
