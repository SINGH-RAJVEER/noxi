//! Contains code parsing some of the Nixcpp config files etc.
//! left by Nix *on the local disk*.
//!
//! This is only for Nix' own state/config.
//!
//! More "standardized" protocols, like parts of the Nix HTTP Binary Cache
//! protocol live elsewhere.

pub mod conf;
pub mod machines;
#[cfg(feature = "serde")]
pub mod narinfo;
pub mod settings;
pub mod store_reference;
pub mod url;
mod settings_table;
