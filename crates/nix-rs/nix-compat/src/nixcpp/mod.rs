//! Contains code parsing some of the Nixcpp config files etc.
//! left by Nix *on the local disk*.
//!
//! This is only for Nix' own state/config.
//!
//! More "standardized" protocols, like parts of the Nix HTTP Binary Cache
//! protocol live elsewhere.

pub mod conf;
pub mod hash;
pub mod machines;
#[cfg(feature = "serde")]
pub mod narinfo;
pub mod settings;
mod settings_table;
pub mod store_reference;
pub mod url;
