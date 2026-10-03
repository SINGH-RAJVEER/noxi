//! Remote builder specifications, the `builders` setting and machines files,
//! CppNix's `Machine::parseConfig` (`src/libstore/machines.cc`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::store_reference::{self, StoreReference, Variant};
use super::url::Query;

/// One remote builder.
#[derive(Clone, Debug, PartialEq)]
pub struct Machine {
	pub store_uri: StoreReference,
	pub system_types: BTreeSet<String>,
	pub ssh_key: Option<PathBuf>,
	pub max_jobs: u32,
	/// Never negative; zero is read as one.
	pub speed_factor: f32,
	pub supported_features: BTreeSet<String>,
	pub mandatory_features: BTreeSet<String>,
	/// The host key, base64-encoded, or empty.
	pub ssh_public_host_key: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
	/// CppNix's `FormatError`.
	#[error("{0}")]
	Format(String),
	/// CppNix's `UsageError`.
	#[error("{0}")]
	Usage(String),
	#[error(transparent)]
	StoreReference(#[from] store_reference::Error),
	#[error("reading machines file '{}': {source}", path.display())]
	Io {
		path: PathBuf,
		#[source]
		source: std::io::Error,
	},
}

const WHITESPACE: &[char] = &[' ', '\t', '\n', '\r'];

/// CppNix's `tokenizeString`: the non-empty pieces between separators.
fn tokenize<'a>(s: &'a str, separators: &'a [char]) -> impl Iterator<Item = &'a str> {
	s.split(separators).filter(|token| !token.is_empty())
}

/// Splits lines and `;`-separated entries, drops comments, and reads
/// `@file` references. A missing file is skipped.
fn expand_builder_lines(builders: &str, result: &mut Vec<String>) -> Result<(), Error> {
	for line in tokenize(builders, &['\n']) {
		let line = line.split('#').next().unwrap_or("");
		for entry in tokenize(line, &[';']) {
			let entry = entry.trim_matches(WHITESPACE);
			if entry.is_empty() {
				continue;
			}
			if let Some(path) = entry.strip_prefix('@') {
				let path = path.trim_matches(WHITESPACE);
				match std::fs::read_to_string(path) {
					Ok(text) => expand_builder_lines(&text, result)?,
					Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
					Err(source) => {
						return Err(Error::Io {
							path: path.into(),
							source,
						});
					}
				}
			} else {
				result.push(entry.to_owned());
			}
		}
	}
	Ok(())
}

/// Boost's `lexical_cast<unsigned int>`, which CppNix uses after rejecting
/// a leading `-`.
fn parse_unsigned(s: &str) -> Option<u32> {
	let digits = s.strip_prefix('+').unwrap_or(s);
	if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
		return None;
	}
	digits.parse().ok()
}

/// The store URI a machine line names. A bare host gets `ssh://`.
fn machine_store_uri(uri: &str) -> String {
	let keep = uri.contains("://")
		|| uri.contains('/')
		|| ["auto", "daemon", "local"].contains(&uri)
		|| ["auto?", "daemon?", "local?", "?"]
			.iter()
			.any(|prefix| uri.starts_with(prefix));
	if keep {
		uri.to_owned()
	} else {
		format!("ssh://{uri}")
	}
}

fn parse_builder_line(
	default_systems: &BTreeSet<String>,
	line: &str,
	cwd: &Path,
	warnings: &mut Vec<String>,
) -> Result<Machine, Error> {
	let tokens: Vec<&str> = tokenize(line, WHITESPACE).collect();
	let is_set = |index: usize| tokens.get(index).is_some_and(|token| *token != "-");
	let set = |index: usize| -> BTreeSet<String> {
		tokenize(tokens[index], &[',']).map(str::to_owned).collect()
	};
	let column_error = |index: usize, kind: &str| {
		Error::Format(format!(
			"bad machine specification: failed to convert column #{index} in a row: '{line}' to '{kind}'"
		))
	};

	if !is_set(0) {
		return Err(Error::Format(format!(
			"bad machine specification: store URL was not found at the first column of a row: '{line}'"
		)));
	}

	let max_jobs = if is_set(3) {
		let token = tokens[3];
		let parsed = (!token.starts_with('-'))
			.then(|| parse_unsigned(token))
			.flatten();
		parsed.ok_or_else(|| column_error(3, "unsigned int"))?
	} else {
		1
	};
	let speed_factor = if is_set(4) {
		tokens[4]
			.parse::<f32>()
			.map_err(|_| column_error(4, "float"))?
	} else {
		1.0
	};
	let ssh_public_host_key = if is_set(7) {
		let key = tokens[7];
		// CppNix only checks the characters it decodes: those before the
		// first `=`, skipping newlines.
		let data: String = key.split('=').next().unwrap_or("").replace('\n', "");
		if let Some(c) = data
			.chars()
			.find(|c| !(c.is_ascii_alphanumeric() || *c == '+' || *c == '/'))
		{
			return Err(Error::Format(format!(
				"invalid character in Base64 string: '{c}'"
			)));
		}
		key.to_owned()
	} else {
		String::new()
	};

	let store_uri =
		StoreReference::parse(&machine_store_uri(tokens[0]), &Query::new(), cwd, warnings)?;
	if speed_factor < 0.0 {
		return Err(Error::Usage("speed factor must be >= 0".into()));
	}

	Ok(Machine {
		store_uri,
		system_types: if is_set(1) {
			set(1)
		} else {
			default_systems.clone()
		},
		ssh_key: is_set(2).then(|| PathBuf::from(tokens[2])),
		max_jobs,
		speed_factor: if speed_factor == 0.0 {
			1.0
		} else {
			speed_factor
		},
		supported_features: if is_set(5) { set(5) } else { BTreeSet::new() },
		mandatory_features: if is_set(6) { set(6) } else { BTreeSet::new() },
		ssh_public_host_key,
	})
}

impl Machine {
	/// Parses the `builders` setting or a machines file. `@path` entries
	/// are read from disk; relative store paths resolve against `cwd`.
	pub fn parse_config(
		default_systems: &BTreeSet<String>,
		s: &str,
		cwd: &Path,
		warnings: &mut Vec<String>,
	) -> Result<Vec<Machine>, Error> {
		let mut lines = Vec::new();
		expand_builder_lines(s, &mut lines)?;
		lines
			.iter()
			.map(|line| parse_builder_line(default_systems, line, cwd, warnings))
			.collect()
	}

	/// Whether the machine builds for `system`; every machine builds
	/// `builtin` derivations.
	pub fn system_supported(&self, system: &str) -> bool {
		system == "builtin" || self.system_types.contains(system)
	}

	/// Whether every feature is supported or mandatory.
	pub fn all_supported(&self, features: &BTreeSet<String>) -> bool {
		features
			.iter()
			.all(|f| self.supported_features.contains(f) || self.mandatory_features.contains(f))
	}

	/// Whether `features` includes every mandatory feature.
	pub fn mandatory_met(&self, features: &BTreeSet<String>) -> bool {
		self.mandatory_features.iter().all(|f| features.contains(f))
	}

	/// The store reference used to open the machine, with the parameters
	/// CppNix adds.
	pub fn complete_store_reference(&self) -> StoreReference {
		let mut reference = self.store_uri.clone();
		let scheme = match &reference.variant {
			Variant::Specified { scheme, .. } => Some(scheme.clone()),
			_ => None,
		};
		let params = &mut reference.params;
		if scheme.as_deref() == Some("ssh") {
			params.insert("max-connections".into(), "1".into());
			params.insert("log-fd".into(), "4".into());
		}
		if matches!(scheme.as_deref(), Some("ssh" | "ssh-ng")) {
			if let Some(key) = &self.ssh_key {
				params.insert("ssh-key".into(), key.to_string_lossy().into_owned());
			}
			if !self.ssh_public_host_key.is_empty() {
				params.insert(
					"base64-ssh-public-host-key".into(),
					self.ssh_public_host_key.clone(),
				);
			}
		}
		let features = params.entry("system-features".into()).or_default();
		for feature in self
			.supported_features
			.iter()
			.chain(&self.mandatory_features)
		{
			if !features.is_empty() {
				features.push(' ');
			}
			features.push_str(feature);
		}
		reference
	}
}
