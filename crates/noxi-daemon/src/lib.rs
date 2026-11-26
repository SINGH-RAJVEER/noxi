//! A Nix daemon for the noxi engine: the store owner that CppNix 2.35.2
//! clients talk to over `nix-daemon`'s Unix socket with the worker
//! protocol (`src/libstore/daemon.cc`).
//!
//! The executable supports Unix listeners, systemd socket activation and
//! stdio workers. Protocol and system qualification remain partial; see
//! `docs/verification.md` for the tested operations and remaining gates.

pub mod client;
pub mod connection;
pub mod error;
pub mod legacy_ssh;
pub mod nar_stream;
pub mod nix_daemon;
pub mod peer;
pub mod serve_protocol;
pub mod server;
pub mod settings;
pub mod ssh;
pub mod wire;

pub use error::{Error, Result};
pub use settings::DaemonSettings;
