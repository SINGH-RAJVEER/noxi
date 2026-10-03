//! In-memory file system objects, CppNix's `MemorySourceAccessor`, with its
//! JSON encoding and NAR listings.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize, ser::SerializeMap};

use crate::nar::listing::{ListingEntry, ShallowListingEntry};

/// A file system object held in memory.
///
/// Its JSON form is CppNix 2.35.2's: a regular file's contents are a JSON
/// string, so they must be valid UTF-8 to be written.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum File {
	Regular {
		executable: bool,
		#[serde(deserialize_with = "contents")]
		contents: Vec<u8>,
	},
	Directory {
		entries: BTreeMap<String, File>,
	},
	Symlink {
		target: String,
	},
}

fn contents<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
	D: serde::Deserializer<'de>,
{
	Ok(String::deserialize(deserializer)?.into_bytes())
}

impl Serialize for File {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let mut map = serializer.serialize_map(Some(3))?;
		match self {
			File::Regular {
				executable,
				contents,
			} => {
				let contents = std::str::from_utf8(contents).map_err(serde::ser::Error::custom)?;
				map.serialize_entry("contents", contents)?;
				map.serialize_entry("executable", executable)?;
				map.serialize_entry("type", "regular")?;
			}
			File::Directory { entries } => {
				map.serialize_entry("entries", entries)?;
				map.serialize_entry("type", "directory")?;
			}
			File::Symlink { target } => {
				map.serialize_entry("target", target)?;
				map.serialize_entry("type", "symlink")?;
			}
		}
		map.end()
	}
}

impl File {
	/// Lists this object and everything below it, as CppNix's `listNarDeep`
	/// does. Sizes are known; NAR offsets are not.
	pub fn list_deep(&self) -> ListingEntry {
		match self {
			File::Regular {
				executable,
				contents,
			} => ListingEntry::Regular {
				size: Some(contents.len() as u64),
				executable: *executable,
				nar_offset: None,
			},
			File::Directory { entries } => ListingEntry::Directory {
				entries: entries
					.iter()
					.map(|(name, file)| (name.clone(), file.list_deep()))
					.collect(),
			},
			File::Symlink { target } => ListingEntry::Symlink {
				target: target.clone(),
			},
		}
	}

	/// Lists this object and only the names of a directory's entries, as
	/// CppNix's `listNarShallow` does.
	pub fn list_shallow(&self) -> ShallowListingEntry {
		match self {
			File::Regular {
				executable,
				contents,
			} => ShallowListingEntry::Regular {
				size: Some(contents.len() as u64),
				executable: *executable,
				nar_offset: None,
			},
			File::Directory { entries } => ShallowListingEntry::Directory {
				entries: entries.keys().cloned().collect(),
			},
			File::Symlink { target } => ShallowListingEntry::Symlink {
				target: target.clone(),
			},
		}
	}
}
