//! `noxi-serde` bridges Rust types and [`noxi_eval::Value`] via the
//! standard [`serde`] traits, making it straightforward to use Nix as a
//! configuration language.
//!
//! # Entry points
//!
//! | Function | Direction |
//! |---|---|
//! | [`from_str`] | Evaluate a Nix expression string → `T` |
//! | [`from_value`] | [`noxi_eval::Value`] → `T` |
//! | [`to_value`] | `T` → [`noxi_eval::Value`] |
//!
//! # Type mapping
//!
//! | Rust / serde | Nix |
//! |---|---|
//! | `bool` | `true` / `false` |
//! | integers (`i8`…`i64`, `u8`…`u32`) | integer |
//! | `u64` | integer (errors if > `i64::MAX`) |
//! | `f32`, `f64` | float |
//! | `char`, `str`, `String` | string |
//! | `Option::None` / unit / unit struct | `null` |
//! | `Option::Some(v)` / newtype struct | inner value |
//! | sequence / tuple | list `[ … ]` |
//! | map / struct | attribute set `{ … }` |
//! | unit enum variant | string of the variant name |
//! | newtype enum variant | `{ VariantName = value; }` |
//! | tuple enum variant | `{ VariantName = [ … ]; }` |
//! | struct enum variant | `{ VariantName = { … }; }` |
//!
//! Bytes are not supported by the Nix value model and will produce an error.
//!
//! # Examples
//!
//! Deserialise a Nix expression:
//!
//! ```rust
//! # use serde::Deserialize;
//! #[derive(Deserialize)]
//! struct Config { host: String, port: u16 }
//!
//! let cfg: Config = noxi_serde::from_str(r#"{ host = "localhost"; port = 8080; }"#).unwrap();
//! assert_eq!(cfg.host, "localhost");
//! assert_eq!(cfg.port, 8080);
//! ```
//!
//! Serialise to a [`noxi_eval::Value`] and back:
//!
//! ```rust
//! # use serde::{Serialize, Deserialize};
//! #[derive(Serialize, Deserialize, PartialEq, Debug)]
//! struct Point { x: i64, y: i64 }
//!
//! let p = Point { x: 1, y: 2 };
//! let value = noxi_serde::to_value(&p).unwrap();
//! let q: Point = noxi_serde::from_value(value).unwrap();
//! assert_eq!(p, q);
//! ```
#![cfg_attr(docsrs, feature(doc_cfg))]

mod de;
mod error;
mod ser;

pub use de::from_str;
pub use de::from_str_with_config;
pub use de::from_value;
pub use error::Error;
pub use ser::to_string;
pub use ser::to_value;

#[cfg(test)]
mod de_tests;
#[cfg(test)]
mod ser_tests;
