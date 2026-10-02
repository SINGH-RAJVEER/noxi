//! A Nix daemon for the nix-rs engine: the store owner that CppNix 2.35.2
//! clients talk to over `nix-daemon`'s Unix socket with the worker
//! protocol (`src/libstore/daemon.cc`).
//!
//! This is a preview library: it implements the handshake, option
//! handling and the read-only and root operations, and is started by tests
//! rather than installed as `nix-daemon`.

pub mod connection;
pub mod error;
pub mod server;
pub mod settings;
pub mod wire;

pub use error::{Error, Result};
pub use settings::DaemonSettings;
