//! A local Nix store that shares CppNix 2.35.2's on-disk format.
//!
//! [`LocalStore`] opens or creates the same directories, SQLite database,
//! schema file and lock files as CppNix's `local` store, so either
//! implementation can use a store the other one wrote. The behavior follows
//! `src/libstore/local-store.cc` and its helpers in the pinned reference.

pub mod binary_cache;
pub mod canonicalise;
pub mod config;
mod db;
pub mod error;
pub mod gc;
mod local;
pub mod lock;
pub mod nar;
pub mod path_info;

pub use config::LocalStoreConfig;
pub use error::{Error, Result};
pub use local::{FileIngestionMethod, LocalStore, ca_hash_of, delete_path};
pub use path_info::ValidPathInfo;
