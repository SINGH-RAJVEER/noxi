//! CppNix's configuration: `nix.conf` files, `NIX_CONFIG` and settings
//! given on the command line, applied in the order and with the messages of
//! CppNix 2.35.2's `loadConfFile` and `Config`.
//!
//! [`Settings::load`] reads `$NIX_CONF_DIR/nix.conf` (default
//! `/etc/nix/nix.conf`), then the user configuration files from last to
//! first, so the first one wins, then `NIX_CONFIG`. A file applies its
//! `experimental-features` lines before its other lines, follows `include`
//! and `!include` relative to its own directory, and appends `extra-NAME`
//! values to list settings. A setting gated on a disabled experimental feature
//! is ignored with a warning. Names CppNix does not know are collected, and
//! [`Settings::finish`] warns about them, as CppNix does once the command
//! line has been read.
//!
//! Values are validated against the setting's type but kept as text; callers
//! read the settings they use through the typed accessors.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use super::settings_table::{EXPERIMENTAL_FEATURES, SETTINGS};

/// The type of a setting's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
	Bool,
	Int,
	String,
	/// Whitespace-separated words; `extra-NAME` appends.
	List,
	/// Whitespace-separated `key=value` words; `extra-NAME` appends.
	Map,
	/// A count or `auto`.
	MaxJobs,
	/// `true`, `false` or `relaxed`.
	Sandbox,
}

impl Kind {
	fn appendable(self) -> bool {
		matches!(self, Kind::List | Kind::Map)
	}
}

/// A setting CppNix knows.
#[derive(Clone, Copy, Debug)]
pub struct SettingInfo {
	pub name: &'static str,
	pub aliases: &'static [&'static str],
	pub kind: Kind,
	/// The experimental feature the setting needs, if any.
	pub experimental_feature: Option<&'static str>,
}

impl SettingInfo {
	/// The setting called `name` or with `name` as an alias.
	pub fn find(name: &str) -> Option<&'static SettingInfo> {
		SETTINGS
			.iter()
			.find(|info| info.name == name || info.aliases.contains(&name))
	}
}

/// An error that stops CppNix while it reads its configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
	/// A line that is not `NAME = VALUE`, `include PATH` or `!include PATH`.
	Syntax {
		line: String,
		path: String,
	},
	InvalidValue {
		name: String,
		value: String,
	},
	InvalidBool {
		name: String,
		value: String,
	},
	MissingInclude {
		file: PathBuf,
		from: String,
	},
}

impl Error {
	/// Whether CppNix reports the error as a usage error, which commands
	/// follow with a pointer to `--help`.
	pub fn is_usage(&self) -> bool {
		!matches!(self, Error::MissingInclude { .. })
	}
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Error::Syntax { line, path } => {
				write!(
					f,
					"syntax error in configuration line '{line}' in \"{path}\""
				)
			}
			Error::InvalidValue { name, value } => {
				write!(f, "setting '{name}' has invalid value '{value}'")
			}
			Error::InvalidBool { name, value } => {
				write!(f, "Boolean setting '{name}' has invalid value '{value}'")
			}
			Error::MissingInclude { file, from } => write!(
				f,
				"file \"{}\" included from \"{from}\" not found",
				file.display()
			),
		}
	}
}

impl std::error::Error for Error {}

/// A value that was set. List and map values are their words joined by
/// single spaces.
#[derive(Clone, Debug)]
struct Entry {
	value: String,
	/// False when only `extra-NAME` was given, which appends to the default.
	replaces_default: bool,
}

/// Settings read so far, with the warnings CppNix printed while reading them.
#[derive(Clone, Debug, Default)]
pub struct Settings {
	/// Values of the settings that were set, by canonical name.
	values: BTreeMap<&'static str, Entry>,
	/// Names CppNix does not know, reported by [`Settings::finish`].
	unknown: BTreeSet<String>,
	warnings: Vec<String>,
}

impl Settings {
	/// Read the configuration files and `NIX_CONFIG` from the environment
	/// `env`, as CppNix's `loadConfFile` does at startup.
	pub fn load(env: impl Fn(&str) -> Option<String>) -> Result<Settings, Error> {
		let mut settings = Settings::default();
		let conf_dir = env("NIX_CONF_DIR").unwrap_or_else(|| "/etc/nix".to_owned());
		settings.apply_file(&Path::new(&conf_dir).join("nix.conf"))?;
		for file in user_config_files(&env).iter().rev() {
			settings.apply_file(Path::new(file))?;
		}
		if let Some(config) = env("NIX_CONFIG") {
			settings.apply_config(&config, "NIX_CONFIG")?;
		}
		Ok(settings)
	}

	/// Apply the configuration file at `path`; a file that cannot be read
	/// is skipped.
	pub fn apply_file(&mut self, path: &Path) -> Result<(), Error> {
		match std::fs::read_to_string(path) {
			Ok(contents) => self.apply_config(&contents, &path.to_string_lossy()),
			Err(_) => Ok(()),
		}
	}

	/// Apply configuration text read from `path`, which names it in errors
	/// and is the base of relative includes.
	pub fn apply_config(&mut self, contents: &str, path: &str) -> Result<(), Error> {
		let mut parsed = Vec::new();
		parse_config(contents, path, &mut parsed)?;
		for (name, value) in &parsed {
			if is_experimental_features(name) {
				self.set(name, value)?;
			}
		}
		for (name, value) in &parsed {
			if !is_experimental_features(name) {
				self.set(name, value)?;
			}
		}
		Ok(())
	}

	/// Set `name`, which may be an alias or `extra-` followed by the name of a
	/// list setting, to `value`. An unknown name is remembered for
	/// [`Settings::finish`]; the result says whether the name was known.
	pub fn set(&mut self, name: &str, value: &str) -> Result<bool, Error> {
		let (info, append) = match SettingInfo::find(name) {
			Some(info) => (info, false),
			None => match name
				.strip_prefix("extra-")
				.and_then(SettingInfo::find)
				.filter(|info| info.kind.appendable())
			{
				Some(info) => (info, true),
				None => {
					self.unknown.insert(name.to_owned());
					return Ok(false);
				}
			},
		};
		let value = parse_value(info, value)?;
		if let Some(feature) = info.experimental_feature
			&& !self.experimental_features().contains(feature)
		{
			self.warnings.push(format!(
				"Ignoring setting '{}' because experimental feature '{feature}' is not enabled",
				info.name
			));
			return Ok(true);
		}
		if info.name == "experimental-features" {
			for word in value.split_whitespace() {
				if !EXPERIMENTAL_FEATURES.contains(&word) {
					self.warnings
						.push(format!("unknown experimental feature '{word}'"));
				}
			}
		}
		let entry = self.values.entry(info.name).or_insert(Entry {
			value: String::new(),
			replaces_default: false,
		});
		if !append {
			*entry = Entry {
				value,
				replaces_default: true,
			};
		} else if !value.is_empty() {
			if !entry.value.is_empty() {
				entry.value.push(' ');
			}
			entry.value.push_str(&value);
		}
		Ok(true)
	}

	/// Handle a command-line flag that sets a setting: `--option NAME
	/// VALUE`, `--NAME VALUE`, `--extra-NAME VALUE` for list settings, and
	/// `--NAME` or `--no-NAME` for Boolean settings. `next` returns the
	/// following argument. `None` means `flag` is not a settings flag.
	pub fn apply_flag(
		&mut self,
		flag: &str,
		mut next: impl FnMut() -> Option<String>,
	) -> Option<Result<(), FlagError>> {
		let mut value = |count| {
			next().ok_or_else(|| FlagError::MissingArguments {
				flag: flag.to_owned(),
				count,
			})
		};
		if flag == "--option" {
			return Some((|| {
				let name = value(2)?;
				let setting = value(2)?;
				self.set(&name, &setting)?;
				Ok(())
			})());
		}
		let name = flag.strip_prefix("--")?;
		if let Some(info) = SettingInfo::find(name).filter(|info| info.name == name) {
			return Some((|| {
				if info.kind == Kind::Bool {
					self.set(name, "true")?;
				} else {
					let setting = value(1)?;
					self.set(name, &setting)?;
				}
				Ok(())
			})());
		}
		if let Some(info) = name
			.strip_prefix("no-")
			.and_then(SettingInfo::find)
			.filter(|info| info.kind == Kind::Bool && Some(info.name) == name.strip_prefix("no-"))
		{
			return Some(
				self.set(info.name, "false")
					.map(|_| ())
					.map_err(FlagError::from),
			);
		}
		if name
			.strip_prefix("extra-")
			.and_then(SettingInfo::find)
			.is_some_and(|info| {
				info.kind.appendable() && Some(info.name) == name.strip_prefix("extra-")
			}) {
			return Some((|| {
				let setting = value(1)?;
				self.set(name, &setting)?;
				Ok(())
			})());
		}
		None
	}

	/// The warnings CppNix prints once the command line has been read:
	/// those printed while settings were applied, then one for each unknown
	/// setting. The warnings are returned without their `warning: ` prefix.
	pub fn finish(&mut self) -> Vec<String> {
		let mut warnings = std::mem::take(&mut self.warnings);
		warnings.extend(
			std::mem::take(&mut self.unknown)
				.into_iter()
				.map(|name| format!("unknown setting '{name}'")),
		);
		warnings
	}

	/// The value of a setting that was set, by canonical name. For a list
	/// setting given only with `extra-NAME`, these are the appended words.
	pub fn get(&self, name: &str) -> Option<&str> {
		self.values.get(name).map(|entry| entry.value.as_str())
	}

	/// The settings that were set, by canonical name, with their values and
	/// whether the value replaces the default rather than extending it.
	pub fn overridden(&self) -> impl Iterator<Item = (&'static str, &str, bool)> {
		self.values
			.iter()
			.map(|(name, entry)| (*name, entry.value.as_str(), entry.replaces_default))
	}

	/// The value of a Boolean setting, or `default`.
	pub fn get_bool(&self, name: &str, default: bool) -> bool {
		self.get(name).map_or(default, |value| value == "true")
	}

	/// The value of an integer setting, or `default`.
	pub fn get_int(&self, name: &str, default: i64) -> i64 {
		self.get(name)
			.and_then(|value| value.parse().ok())
			.unwrap_or(default)
	}

	/// The words of a list setting: `default` unless the setting was set,
	/// followed by any words `extra-NAME` appended.
	pub fn get_list<'a>(&'a self, name: &str, default: &[&'a str]) -> Vec<&'a str> {
		match self.values.get(name) {
			Some(entry) => {
				let words = entry.value.split_whitespace();
				if entry.replaces_default {
					words.collect()
				} else {
					default.iter().copied().chain(words).collect()
				}
			}
			None => default.to_vec(),
		}
	}

	/// The enabled experimental features, including those an enabled
	/// feature implies (`flakes` implies `fetch-tree`).
	pub fn experimental_features(&self) -> BTreeSet<&'static str> {
		let mut features: BTreeSet<&'static str> = self
			.get_list("experimental-features", &[])
			.into_iter()
			.filter_map(|word| EXPERIMENTAL_FEATURES.iter().copied().find(|f| *f == word))
			.collect();
		if features.contains("flakes") {
			features.insert("fetch-tree");
		}
		features
	}
}

/// An error in a settings flag on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagError {
	MissingArguments { flag: String, count: usize },
	Setting(Error),
}

impl From<Error> for FlagError {
	fn from(error: Error) -> Self {
		FlagError::Setting(error)
	}
}

impl fmt::Display for FlagError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			FlagError::MissingArguments { flag, count: 1 } => {
				write!(f, "flag '{flag}' requires 1 argument")
			}
			FlagError::MissingArguments { flag, count } => {
				write!(f, "flag '{flag}' requires {count} arguments")
			}
			FlagError::Setting(error) => error.fmt(f),
		}
	}
}

impl std::error::Error for FlagError {}

fn is_experimental_features(name: &str) -> bool {
	name == "experimental-features" || name == "extra-experimental-features"
}

/// CppNix's `getUserConfigFiles`: `NIX_USER_CONF_FILES`, or `nix/nix.conf`
/// under `XDG_CONFIG_HOME` (default `~/.config`) and each `XDG_CONFIG_DIRS`
/// entry (default `/etc/xdg`).
fn user_config_files(env: &impl Fn(&str) -> Option<String>) -> Vec<String> {
	if let Some(files) = env("NIX_USER_CONF_FILES") {
		return files
			.split(':')
			.filter(|file| !file.is_empty())
			.map(str::to_owned)
			.collect();
	}
	let config_home = env("XDG_CONFIG_HOME")
		.or_else(|| env("HOME").map(|home| format!("{home}/.config")))
		.unwrap_or_else(|| "/.config".to_owned());
	let config_dirs = env("XDG_CONFIG_DIRS").unwrap_or_else(|| "/etc/xdg".to_owned());
	std::iter::once(config_home.as_str())
		.chain(config_dirs.split(':').filter(|dir| !dir.is_empty()))
		.map(|dir| format!("{dir}/nix/nix.conf"))
		.collect()
}

/// Read `NAME = VALUE` lines, following includes, as CppNix's
/// `applyConfigInternal` does.
fn parse_config(contents: &str, path: &str, out: &mut Vec<(String, String)>) -> Result<(), Error> {
	for raw_line in contents.split('\n') {
		let line = raw_line.split('#').next().unwrap_or_default();
		let tokens: Vec<&str> = line
			.split([' ', '\t', '\n', '\r'])
			.filter(|token| !token.is_empty())
			.collect();
		if tokens.is_empty() {
			continue;
		}
		let syntax = || Error::Syntax {
			line: line.to_owned(),
			path: path.to_owned(),
		};
		if tokens.len() < 2 {
			return Err(syntax());
		}
		let ignore_missing = match tokens[0] {
			"include" => Some(false),
			"!include" => Some(true),
			_ => None,
		};
		if let Some(ignore_missing) = ignore_missing {
			if tokens.len() != 2 {
				return Err(syntax());
			}
			let dir = Path::new(path).parent().unwrap_or(Path::new(""));
			let file = dir.join(tokens[1]);
			let file = if file.is_absolute() {
				file
			} else {
				std::env::current_dir().unwrap_or_default().join(file)
			};
			match std::fs::read_to_string(&file) {
				Ok(included) => parse_config(&included, &file.to_string_lossy(), out)?,
				Err(_) if ignore_missing => {}
				Err(_) => {
					return Err(Error::MissingInclude {
						file,
						from: path.to_owned(),
					});
				}
			}
			continue;
		}
		if tokens[1] != "=" {
			return Err(syntax());
		}
		out.push((tokens[0].to_owned(), tokens[2..].join(" ")));
	}
	Ok(())
}

/// Validate `value` for `info`, returning the text to store.
fn parse_value(info: &SettingInfo, value: &str) -> Result<String, Error> {
	let invalid = || Error::InvalidValue {
		name: info.name.to_owned(),
		value: value.to_owned(),
	};
	Ok(match info.kind {
		Kind::Bool => match value {
			"true" | "yes" | "1" => "true".to_owned(),
			"false" | "no" | "0" => "false".to_owned(),
			_ => {
				return Err(Error::InvalidBool {
					name: info.name.to_owned(),
					value: value.to_owned(),
				});
			}
		},
		Kind::Int => {
			if info.name == "cores" {
				value.parse::<u32>().map_err(|_| invalid())?;
			} else {
				value.parse::<i64>().map_err(|_| invalid())?;
			}
			value.to_owned()
		}
		Kind::MaxJobs if value == "auto" => value.to_owned(),
		Kind::MaxJobs => {
			value.parse::<u64>().map_err(|_| invalid())?;
			value.to_owned()
		}
		Kind::Sandbox => match value {
			"true" | "false" | "relaxed" => value.to_owned(),
			_ => return Err(invalid()),
		},
		Kind::String => value.to_owned(),
		Kind::List | Kind::Map => value.split_whitespace().collect::<Vec<_>>().join(" "),
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	fn load(
		files: &[(&str, &str)],
		env: &[(&str, &str)],
	) -> (Result<Settings, Error>, tempfile::TempDir) {
		let dir = tempfile::tempdir().unwrap();
		for (name, contents) in files {
			let path = dir.path().join(name);
			std::fs::create_dir_all(path.parent().unwrap()).unwrap();
			std::fs::write(path, contents).unwrap();
		}
		let root = dir.path().to_str().unwrap().to_owned();
		let env: Vec<(String, String)> = env
			.iter()
			.map(|(k, v)| (k.to_string(), v.replace("$ROOT", &root)))
			.collect();
		let settings = Settings::load(|name| {
			env.iter()
				.find(|(key, _)| key == name)
				.map(|(_, value)| value.clone())
		});
		(settings, dir)
	}

	/// The precedence and messages below were recorded from CppNix 2.35.2.
	#[test]
	fn applies_files_in_cppnix_order() {
		let (settings, _dir) = load(
			&[
				(
					"etc/nix.conf",
					"experimental-features = nix-command\nextra-experimental-features = flakes\nmax-call-depth = 5\ninclude extra.conf\n!include missing.conf\nfoo-unknown = 1\n",
				),
				(
					"etc/extra.conf",
					"extra-experimental-features = pipe-operators\n",
				),
				("user/a.conf", "max-call-depth = 7\n"),
				(
					"user/b.conf",
					"max-call-depth = 8\nextra-experimental-features = fetch-tree\n",
				),
			],
			&[
				("NIX_CONF_DIR", "$ROOT/etc"),
				("NIX_USER_CONF_FILES", "$ROOT/user/a.conf:$ROOT/user/b.conf"),
				("NIX_CONFIG", "extra-experimental-features = ca-derivations"),
			],
		);
		let mut settings = settings.unwrap();
		assert_eq!(settings.get_int("max-call-depth", 10000), 7);
		assert_eq!(
			settings
				.experimental_features()
				.into_iter()
				.collect::<Vec<_>>(),
			[
				"ca-derivations",
				"fetch-tree",
				"flakes",
				"nix-command",
				"pipe-operators"
			]
		);
		assert_eq!(settings.finish(), ["unknown setting 'foo-unknown'"]);
	}

	#[test]
	fn gates_settings_and_reports_errors_like_cppnix() {
		let (settings, _dir) = load(
			&[(
				"etc/nix.conf",
				"flake-registry = /x\nexperimental-features = bogus-feature\nextra-max-call-depth = 3\nbuild-use-sandbox = false\n",
			)],
			&[("NIX_CONF_DIR", "$ROOT/etc"), ("NIX_USER_CONF_FILES", "")],
		);
		let mut settings = settings.unwrap();
		assert_eq!(settings.get("sandbox"), Some("false"));
		assert_eq!(
			settings.get_list("substituters", &["https://cache.nixos.org/"]),
			["https://cache.nixos.org/"]
		);
		settings.set("extra-substituters", "http://x").unwrap();
		assert_eq!(
			settings.get_list("substituters", &["https://cache.nixos.org/"]),
			["https://cache.nixos.org/", "http://x"]
		);
		assert_eq!(
			settings.finish(),
			[
				"unknown experimental feature 'bogus-feature'",
				"Ignoring setting 'flake-registry' because experimental feature 'flakes' is not enabled",
				"unknown setting 'extra-max-call-depth'",
			]
		);

		for (contents, message) in [
			(
				"max-call-depth",
				"syntax error in configuration line 'max-call-depth' in \"$ROOT/etc/nix.conf\"",
			),
			(
				"max-call-depth=3",
				"syntax error in configuration line 'max-call-depth=3' in \"$ROOT/etc/nix.conf\"",
			),
			(
				"max-call-depth = 3 4",
				"setting 'max-call-depth' has invalid value '3 4'",
			),
			(
				"show-trace = maybe",
				"Boolean setting 'show-trace' has invalid value 'maybe'",
			),
			("cores = -1", "setting 'cores' has invalid value '-1'"),
			(
				"cores = 4294967296",
				"setting 'cores' has invalid value '4294967296'",
			),
			(
				"include missing.conf",
				"file \"$ROOT/etc/missing.conf\" included from \"$ROOT/etc/nix.conf\" not found",
			),
		] {
			let (settings, dir) = load(
				&[("etc/nix.conf", contents)],
				&[("NIX_CONF_DIR", "$ROOT/etc")],
			);
			let root = dir.path().to_str().unwrap();
			assert_eq!(
				settings.unwrap_err().to_string(),
				message.replace("$ROOT", root),
				"{contents}"
			);
		}
	}

	#[test]
	fn applies_command_line_flags_in_order() {
		let mut settings = Settings::default();
		let mut flags = [
			"--experimental-features",
			"nix-command",
			"--extra-experimental-features",
			"recursive-nix",
			"--max-call-depth",
			"11",
			"--show-trace",
			"--no-show-trace",
			"--option",
			"baz",
			"1",
		]
		.into_iter()
		.map(str::to_owned);
		while let Some(flag) = flags.next() {
			settings
				.apply_flag(&flag, || flags.next())
				.expect("a settings flag")
				.unwrap();
		}
		assert_eq!(
			settings
				.experimental_features()
				.into_iter()
				.collect::<Vec<_>>(),
			["nix-command", "recursive-nix"]
		);
		assert_eq!(settings.get_int("max-call-depth", 10000), 11);
		assert!(!settings.get_bool("show-trace", true));
		assert!(settings.apply_flag("--eval", || None).is_none());
		assert_eq!(settings.finish(), ["unknown setting 'baz'"]);
	}
}
