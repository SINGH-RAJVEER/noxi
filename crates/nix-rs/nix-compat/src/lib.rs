#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate self as nix_compat;

pub(crate) mod aterm;
#[cfg(feature = "serde")]
pub mod cppnix_json;
#[cfg(all(feature = "serde", feature = "wire"))]
pub mod cppnix_serve;
pub mod derivation;
#[cfg(feature = "serde")]
pub mod fetchers;
pub mod git;
pub mod log;
pub mod nar;
pub mod narinfo;
pub mod nix_http;
pub use nix_rs_hash::nixbase32;
pub mod nixcpp;
pub use nix_rs_hash::format_sha256;
pub use nix_rs_hash::nixhash;
pub mod path_info;
pub mod store_path;
#[cfg(feature = "serde")]
pub mod structured_attrs;

#[cfg(feature = "wire")]
pub mod wire;

pub mod derived_path;
#[cfg(feature = "daemon")]
pub mod nix_daemon;
pub mod realisation;
#[cfg(any(test, feature = "test"))]
pub mod test;
#[cfg(feature = "daemon")]
pub use nix_daemon::worker_protocol;
#[cfg(feature = "flakeref")]
pub use nix_rs_flakeref as flakeref;
