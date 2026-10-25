//! CppNix-compatible commands built on the nix-rs evaluator.
//!
//! Each module implements one legacy command's supported subset. Anything
//! outside it fails with an explicit error rather than behaving differently.

pub mod build;
pub mod cli;
pub mod collect_garbage;
pub mod instantiate;
mod instantiate_lints;
pub mod maintenance;
pub mod nix_build;
pub mod nix_env;
pub mod nix_store;
pub mod paths;
pub mod store;
pub mod user_env;
