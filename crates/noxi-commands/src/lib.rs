//! CppNix-compatible commands built on the noxi evaluator.
//!
//! Each module implements one legacy command's supported subset. Anything
//! outside it fails with an explicit error rather than behaving differently.

#[cfg(feature = "eval-allocation-diagnostics")]
mod allocation_diagnostics;

pub mod build;
pub mod channel;
pub mod cli;
pub mod collect_garbage;
pub mod instantiate;
mod instantiate_lints;
pub mod legacy_environment;
pub mod maintenance;
pub mod nix;
pub mod nix_build;
pub mod nix_env;
pub mod nix_shell;
pub mod nix_store;
mod nix_store_ops;
pub mod paths;
pub mod prefetch_url;
pub mod store;
pub mod user_env;

// Evaluation allocates and frees millions of small values, which mimalloc
// serves faster than the system allocator.
#[cfg(not(feature = "eval-allocation-diagnostics"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "eval-allocation-diagnostics")]
#[global_allocator]
static GLOBAL: allocation_diagnostics::CountingMiMalloc = allocation_diagnostics::CountingMiMalloc;
