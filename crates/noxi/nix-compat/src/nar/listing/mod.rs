//! Parser for the Nix archive listing format, aka .ls.
//!
//! LS files are produced by the C++ Nix implementation via `write-nar-listing=1` query parameter
//! passed to a store implementation when transferring store paths.
//!
//! Listing files contains metadata about a file and its offset in the corresponding NAR.
//!
//! NOTE: LS entries does not offer any integrity field to validate the retrieved file at the provided
//! offset. Validating the contents is the caller's responsibility.

use std::{
	collections::{BTreeMap, BTreeSet},
	path::{Component, Path},
};

use serde::{Deserialize, Serialize, ser::SerializeMap};

#[cfg(test)]
mod test;

#[derive(Debug, thiserror::Error)]
pub enum ListingError {
	// TODO: add an enum of what component was problematic
	// reusing `std::path::Component` is not possible as it contains a lifetime.
	/// An unsupported path component can be:
	/// - either a Windows prefix (`C:\\`, `\\share\\`)
	/// - either a parent directory (`..`)
	/// - either a root directory (`/`)
	#[error("unsupported path component")]
	UnsupportedPathComponent,
	#[error("invalid encoding for entry component")]
	InvalidEncoding,
}

/// A deep listing entry, CppNix's `NarListing`.
///
/// CppNix 2.35.2 writes every regular file's `executable` flag, omits an
/// unknown size and an unknown or zero `narOffset`, and sorts object keys.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ListingEntry {
	Regular {
		#[serde(default)]
		size: Option<u64>,
		#[serde(default)]
		executable: bool,
		#[serde(rename = "narOffset", default, deserialize_with = "nar_offset")]
		nar_offset: Option<u64>,
	},
	Directory {
		// It's tempting to think that the key should be a `Vec<u8>`
		// but Nix does not support that and will fail to emit a listing version 1 for any non-UTF8
		// encodeable string.
		entries: BTreeMap<String, ListingEntry>,
	},
	Symlink {
		target: String,
	},
}

/// A listing of one directory level, CppNix's `ShallowNarListing`. A
/// directory's entries are written as empty objects.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ShallowListingEntry {
	Regular {
		#[serde(default)]
		size: Option<u64>,
		#[serde(default)]
		executable: bool,
		#[serde(rename = "narOffset", default, deserialize_with = "nar_offset")]
		nar_offset: Option<u64>,
	},
	Directory {
		#[serde(deserialize_with = "opaque_entries")]
		entries: BTreeSet<String>,
	},
	Symlink {
		target: String,
	},
}

/// CppNix reads a zero `narOffset` as unknown.
fn nar_offset<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
	D: serde::Deserializer<'de>,
{
	Ok(Option::<u64>::deserialize(deserializer)?.filter(|offset| *offset != 0))
}

/// CppNix ignores the value of each shallow directory entry.
fn opaque_entries<'de, D>(deserializer: D) -> Result<BTreeSet<String>, D::Error>
where
	D: serde::Deserializer<'de>,
{
	let entries = BTreeMap::<String, serde::de::IgnoredAny>::deserialize(deserializer)?;
	Ok(entries.into_keys().collect())
}

fn serialize_regular<S>(
	serializer: S,
	size: &Option<u64>,
	executable: bool,
	nar_offset: &Option<u64>,
) -> Result<S::Ok, S::Error>
where
	S: serde::Serializer,
{
	let len = 2 + usize::from(size.is_some()) + usize::from(nar_offset.is_some());
	let mut map = serializer.serialize_map(Some(len))?;
	map.serialize_entry("executable", &executable)?;
	if let Some(nar_offset) = nar_offset {
		map.serialize_entry("narOffset", nar_offset)?;
	}
	if let Some(size) = size {
		map.serialize_entry("size", size)?;
	}
	map.serialize_entry("type", "regular")?;
	map.end()
}

fn serialize_symlink<S>(serializer: S, target: &str) -> Result<S::Ok, S::Error>
where
	S: serde::Serializer,
{
	let mut map = serializer.serialize_map(Some(2))?;
	map.serialize_entry("target", target)?;
	map.serialize_entry("type", "symlink")?;
	map.end()
}

// Custom Serialize impls, as otherwise type would always come first, causing noncanonical JSON.
impl Serialize for ListingEntry {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match self {
			ListingEntry::Regular {
				size,
				executable,
				nar_offset,
			} => serialize_regular(serializer, size, *executable, nar_offset),
			ListingEntry::Directory { entries } => {
				let mut map = serializer.serialize_map(Some(2))?;
				map.serialize_entry("entries", entries)?;
				map.serialize_entry("type", "directory")?;
				map.end()
			}
			ListingEntry::Symlink { target } => serialize_symlink(serializer, target),
		}
	}
}

impl Serialize for ShallowListingEntry {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		struct Opaque<'a>(&'a BTreeSet<String>);

		impl Serialize for Opaque<'_> {
			fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
			where
				S: serde::Serializer,
			{
				let mut map = serializer.serialize_map(Some(self.0.len()))?;
				for name in self.0 {
					map.serialize_entry(name, &BTreeMap::<(), ()>::new())?;
				}
				map.end()
			}
		}

		match self {
			ShallowListingEntry::Regular {
				size,
				executable,
				nar_offset,
			} => serialize_regular(serializer, size, *executable, nar_offset),
			ShallowListingEntry::Directory { entries } => {
				let mut map = serializer.serialize_map(Some(2))?;
				map.serialize_entry("entries", &Opaque(entries))?;
				map.serialize_entry("type", "directory")?;
				map.end()
			}
			ShallowListingEntry::Symlink { target } => serialize_symlink(serializer, target),
		}
	}
}

impl ListingEntry {
	/// Given a relative path without `..` component, this will locate, relative to this entry, a
	/// deeper entry.
	///
	/// If the path is invalid, a listing error [`ListingError`] will be returned.
	/// If the entry cannot be found, `None` will be returned.
	pub fn locate<P: AsRef<Path>>(&self, path: P) -> Result<Option<&ListingEntry>, ListingError> {
		// We perform a simple DFS on the components of the path
		// while rejecting dangerous components, e.g. `..` or `/`
		// Files and symlinks are *leaves*, i.e. we return them
		let mut cur = self;
		for component in path.as_ref().components() {
			match component {
				Component::CurDir => continue,
				Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
					return Err(ListingError::UnsupportedPathComponent);
				}
				Component::Normal(file_or_dir_name) => {
					if let Self::Directory { entries } = cur {
						// As Nix cannot encode non-UTF8 components in the listing (see comment on
						// the `Directory` enum variant), invalid encodings path components are
						// errors.
						let entry_name = file_or_dir_name
							.to_str()
							.ok_or(ListingError::InvalidEncoding)?;

						if let Some(new_entry) = entries.get(entry_name) {
							cur = new_entry;
						} else {
							return Ok(None);
						}
					} else {
						return Ok(None);
					}
				}
			}
		}

		// By construction, we found the node that corresponds to the path traversal.
		Ok(Some(cur))
	}
}

#[derive(Debug)]
pub struct ListingVersion<const V: u8>;

#[derive(Debug, thiserror::Error)]
#[error("Invalid version: {0}")]
struct ListingVersionError(u8);

impl<'de, const V: u8> Deserialize<'de> for ListingVersion<V> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let value = u8::deserialize(deserializer)?;
		if value == V {
			Ok(ListingVersion::<V>)
		} else {
			Err(serde::de::Error::custom(ListingVersionError(value)))
		}
	}
}

impl<const V: u8> Serialize for ListingVersion<V> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		V.serialize(serializer)
	}
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum Listing {
	V1 {
		root: ListingEntry,
		version: ListingVersion<1>,
	},
}
