use std::{fmt, str::FromStr};

use bstr::BString;

use crate::store_path;

/// A derivation output name. Public constructors validate names, while ATerm
/// parsing retains arbitrary bytes as CppNix does.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputName(BString);

impl OutputName {
	/// Return the exact bytes of this output name.
	pub fn as_bytes(&self) -> &[u8] {
		self.0.as_slice()
	}

	/// Return this output name as UTF-8, if it is valid UTF-8.
	pub fn as_str(&self) -> Option<&str> {
		std::str::from_utf8(self.as_bytes()).ok()
	}

	/// Construct a validated name from a static string.
	pub fn from_static(s: &'static str) -> Result<Self, ParseOutputNameError> {
		validate(s.as_bytes())?;
		Ok(Self(s.into()))
	}

	pub(super) fn from_aterm_unchecked(s: BString) -> Self {
		Self(s)
	}

	pub(super) fn validate(&self) -> Result<(), ParseOutputNameError> {
		validate(self.as_bytes())
	}

	/// Return the default output name `out`.
	pub fn out() -> Self {
		Self("out".into())
	}
}

fn validate(name: &[u8]) -> Result<(), ParseOutputNameError> {
	store_path::validate_name(name)?;
	if name == b"drv" {
		return Err(ParseOutputNameError::ReservedNameDrv);
	}
	Ok(())
}

impl PartialEq<&str> for OutputName {
	fn eq(&self, other: &&str) -> bool {
		self.as_bytes() == other.as_bytes()
	}
}
impl PartialEq<str> for OutputName {
	fn eq(&self, other: &str) -> bool {
		self.as_bytes() == other.as_bytes()
	}
}
impl PartialEq<OutputName> for &str {
	fn eq(&self, other: &OutputName) -> bool {
		self.as_bytes() == other.as_bytes()
	}
}
impl PartialEq<OutputName> for str {
	fn eq(&self, other: &OutputName) -> bool {
		self.as_bytes() == other.as_bytes()
	}
}
impl PartialEq<String> for OutputName {
	fn eq(&self, other: &String) -> bool {
		self.as_bytes() == other.as_bytes()
	}
}
impl PartialEq<OutputName> for String {
	fn eq(&self, other: &OutputName) -> bool {
		self.as_bytes() == other.as_bytes()
	}
}

impl fmt::Display for OutputName {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(&String::from_utf8_lossy(self.as_bytes()))
	}
}

impl AsRef<[u8]> for OutputName {
	fn as_ref(&self) -> &[u8] {
		self.as_bytes()
	}
}

impl Default for OutputName {
	fn default() -> Self {
		Self::out()
	}
}

impl FromStr for OutputName {
	type Err = ParseOutputNameError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		validate(s.as_bytes())?;
		Ok(Self(s.into()))
	}
}

impl TryFrom<String> for OutputName {
	type Error = ParseOutputNameError;

	fn try_from(value: String) -> Result<Self, Self::Error> {
		validate(value.as_bytes())?;
		Ok(Self(value.into()))
	}
}

#[cfg(feature = "serde")]
impl serde::Serialize for OutputName {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		use serde::ser::Error;
		self.as_str()
			.ok_or_else(|| S::Error::custom("output name is not UTF-8"))?
			.serialize(serializer)
	}
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for OutputName {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		use serde::de::Error;
		let name = <String as serde::Deserialize>::deserialize(deserializer)?;
		name.try_into().map_err(D::Error::custom)
	}
}

/// The error type for when parsing an [`OutputName`] fails.
#[derive(thiserror::Error, Debug, PartialEq)]
#[allow(missing_docs)]
pub enum ParseOutputNameError {
	#[error("Invalid length")]
	InvalidLength,
	#[error("Invalid name")]
	InvalidName,
	#[error("Invalid reserved name 'drv'")]
	ReservedNameDrv,
}

impl From<store_path::ParseStorePathNameError> for ParseOutputNameError {
	fn from(value: store_path::ParseStorePathNameError) -> Self {
		match value {
			store_path::ParseStorePathNameError::Length => ParseOutputNameError::InvalidLength,
			store_path::ParseStorePathNameError::Name => ParseOutputNameError::InvalidName,
		}
	}
}

#[cfg(test)]
mod tests {
	use rstest::rstest;

	use super::OutputName;

	#[rstest]
	#[should_panic(expected = "InvalidName")]
	#[case("bin{n")]
	#[should_panic(expected = "InvalidName")]
	#[case("bin{n")]
	#[should_panic(expected = "InvalidName")]
	#[case(" bin{n")]
	#[should_panic(expected = "InvalidName")]
	#[case("invalid name")]
	#[should_panic(expected = "InvalidName")]
	#[case("invalid/name")]
	#[should_panic(expected = "ReservedNameDrv")]
	#[case("drv")]
	#[should_panic(expected = "InvalidLength")]
	#[case("")]
	fn parse_fail(#[case] value: &str) {
		value.parse::<OutputName>().unwrap();
	}

	#[rstest]
	#[case("out")]
	#[case("dev")]
	#[case("lib")]
	#[case("bin")]
	#[case("debug")]
	fn parse(#[case] value: &str) {
		value.parse::<OutputName>().unwrap();
	}

	#[test]
	fn size() {
		assert_eq!(size_of::<OutputName>(), size_of::<String>());
	}
}
