use std::{fmt, str::FromStr};

use crate::{derived_path::OutputSpec, store_path::StorePath};

use super::DerivedPath;

/// Format a [`DerivedPath`] in the "legacy" format.
///
/// Normally a [`DerivedPath::Built`] it formatted like
/// `/nix/store/00000000000000000000000000000000-test.drv^out`. But in some
/// places (most notably in the Nix daemon protocol) a format like
/// `/nix/store/00000000000000000000000000000000-test.drv!out` is used.
///
/// This formatter implements [`FromStr`] and [`fmt::Display`] that use this format.
///
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct LegacyDerivedPath(DerivedPath);
impl LegacyDerivedPath {
	pub fn from_path(path: DerivedPath) -> Self {
		Self(path)
	}

	pub fn as_path(&self) -> &DerivedPath {
		&self.0
	}

	pub fn into_path(self) -> DerivedPath {
		self.0
	}
}

impl fmt::Display for LegacyDerivedPath {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match &self.0 {
			DerivedPath::Opaque(store_path) => write!(f, "{}", store_path.to_absolute_path()),
			DerivedPath::Built { drv_path, outputs } => {
				write!(f, "{}!{}", drv_path.to_absolute_path(), outputs)
			}
		}
	}
}

impl FromStr for LegacyDerivedPath {
	type Err = super::ParseDerivedPathError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		if let Some((prefix, outputs_s)) = s.rsplit_once('!') {
			let drv_path = StorePath::from_absolute_path(prefix.as_bytes())?;
			let outputs = outputs_s.parse::<OutputSpec>()?;
			Ok(LegacyDerivedPath(DerivedPath::Built { drv_path, outputs }))
		} else {
			Ok(LegacyDerivedPath(DerivedPath::Opaque(
				StorePath::from_absolute_path(s.as_bytes())?,
			)))
		}
	}
}

impl From<DerivedPath> for LegacyDerivedPath {
	fn from(value: DerivedPath) -> Self {
		value.into_legacy_format()
	}
}

impl<'a> From<&'a DerivedPath> for &'a LegacyDerivedPath {
	fn from(value: &'a DerivedPath) -> Self {
		value.as_legacy_format()
	}
}

impl From<LegacyDerivedPath> for DerivedPath {
	fn from(value: LegacyDerivedPath) -> Self {
		value.into_path()
	}
}

impl<'a> From<&'a LegacyDerivedPath> for &'a DerivedPath {
	fn from(value: &'a LegacyDerivedPath) -> Self {
		value.as_path()
	}
}

#[cfg(test)]
mod tests {
	use rstest::rstest;

	use super::*;
	#[rstest]
	#[case("/nix/store/00000000000000000000000000000000-test.drv", DerivedPath::Opaque("00000000000000000000000000000000-test.drv".parse().unwrap()))]
	#[case("/nix/store/00000000000000000000000000000000-test.drv!out", DerivedPath::Built {
        drv_path: "00000000000000000000000000000000-test.drv".parse().unwrap(),
        outputs: "out".parse().unwrap(),
    })]
	#[case("/nix/store/00000000000000000000000000000000-test.drv!*", DerivedPath::Built {
        drv_path: "00000000000000000000000000000000-test.drv".parse().unwrap(),
        outputs: "*".parse().unwrap(),
    })]
	#[case("/nix/store/00000000000000000000000000000000-test.drv!bin,lib", DerivedPath::Built {
        drv_path: "00000000000000000000000000000000-test.drv".parse().unwrap(),
        outputs: "bin,lib".parse().unwrap(),
    })]
	fn parse(#[case] input: &str, #[case] expected: DerivedPath) {
		let actual = input.parse::<LegacyDerivedPath>().unwrap().into_path();
		assert_eq!(actual, expected);
	}

	#[rstest]
	// tries to parse `test.drv!out` as store path name
	#[case("/nix/store/00000000000000000000000000000000-test.drv!out!bin,lib")]
	// tries to parse `test.drv!out!bin` as store path name
	#[case("/nix/store/00000000000000000000000000000000-test.drv!out!bin!lib")]
	// treats it as opaque
	#[case("/nix/store/00000000000000000000000000000000-test.drv^out")]
	// tries to parse `test.drv^out` as store path name
	#[case("/nix/store/00000000000000000000000000000000-test.drv^out!bin")]
	// tries to parse `test.drv!out!bin^out` as store path name
	#[case("/nix/store/00000000000000000000000000000000-test.drv!out!bin^out!lib")]
	#[should_panic(expected = "StorePath(Name)")]
	fn parse_fail(#[case] input: &str) {
		input.parse::<LegacyDerivedPath>().unwrap();
	}

	#[rstest]
	#[case(DerivedPath::Opaque("00000000000000000000000000000000-test.drv".parse().unwrap()), "/nix/store/00000000000000000000000000000000-test.drv")]
	#[case(DerivedPath::Built {
        drv_path: "00000000000000000000000000000000-test.drv".parse().unwrap(),
        outputs: "out".parse().unwrap(),
    }, "/nix/store/00000000000000000000000000000000-test.drv!out")]
	#[case(DerivedPath::Built {
        drv_path: "00000000000000000000000000000000-test.drv".parse().unwrap(),
        outputs: "*".parse().unwrap(),
    }, "/nix/store/00000000000000000000000000000000-test.drv!*")]
	#[case(DerivedPath::Built {
        drv_path: "00000000000000000000000000000000-test.drv".parse().unwrap(),
        outputs: "bin,lib".parse().unwrap(),
    }, "/nix/store/00000000000000000000000000000000-test.drv!bin,lib")]
	fn display(#[case] value: DerivedPath, #[case] expected: &str) {
		assert_eq!(value.as_legacy_format().to_string(), expected);
	}
}
