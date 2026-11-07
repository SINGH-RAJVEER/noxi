//! Flake registries (`src/libfetchers/registry.cc`): version 2 JSON files
//! mapping indirect inputs to direct ones, searched flag, user, system,
//! then global.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result, warn};
use crate::input::{Attr, Attrs, FetchSettings, Input, attrs_to_json, json_to_attrs};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistryType {
	Flag,
	User,
	System,
	Global,
	Custom,
}

#[derive(Clone, Debug)]
pub struct Entry {
	pub from: Input,
	pub to: Input,
	pub extra_attrs: Attrs,
	pub exact: bool,
}

#[derive(Clone, Debug)]
pub struct Registry {
	pub registry_type: RegistryType,
	pub entries: Vec<Entry>,
}

impl Registry {
	pub fn new(registry_type: RegistryType) -> Self {
		Self {
			registry_type,
			entries: Vec::new(),
		}
	}

	/// `Registry::read`: a missing file is empty; unreadable entries warn
	/// and keep the entries read before them.
	pub fn read(settings: &FetchSettings, path: &Path, registry_type: RegistryType) -> Self {
		let path = resolve_symlinks(path);
		let Ok(contents) = std::fs::read_to_string(&path) else {
			return Self::new(registry_type);
		};
		Self::parse(settings, &contents, &path.to_string_lossy(), registry_type)
	}

	pub fn parse(
		settings: &FetchSettings,
		contents: &str,
		display: &str,
		registry_type: RegistryType,
	) -> Self {
		let mut registry = Self::new(registry_type);
		let json: serde_json::Value = match serde_json::from_str(contents) {
			Ok(json) => json,
			Err(error) => {
				warn(&format!("cannot parse flake registry '{display}': {error}"));
				return registry;
			}
		};
		let result = (|| -> Result<()> {
			let version = json.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
			if version != 2 {
				return Err(Error::new(format!(
					"flake registry '{display}' has unsupported version {version}"
				)));
			}
			let empty = serde_json::Value::Null;
			let flakes = json.get("flakes").and_then(|f| f.as_array());
			for entry in flakes.into_iter().flatten() {
				let to_json = entry.get("to").unwrap_or(&empty);
				let from_json = entry.get("from").unwrap_or(&empty);
				let mut to_attrs = attrs_or_empty(to_json)?;
				let mut extra_attrs = Attrs::new();
				if let Some(dir) = to_attrs.remove("dir") {
					extra_attrs.insert("dir".into(), dir);
				}
				let exact = entry
					.get("exact")
					.and_then(|e| e.as_bool())
					.unwrap_or(false);
				let from = Input::from_attrs(settings, attrs_or_empty(from_json)?)?;
				let to = Input::from_attrs(settings, to_attrs)?;
				registry.entries.push(Entry {
					from,
					to,
					extra_attrs,
					exact,
				});
			}
			Ok(())
		})();
		if let Err(error) = result {
			warn(&format!("cannot read flake registry '{display}': {error}"));
		}
		registry
	}

	/// `Registry::write`.
	pub fn to_json_string(&self) -> Result<String> {
		let mut entries = Vec::new();
		for entry in &self.entries {
			let mut object = serde_json::Map::new();
			object.insert(
				"from".into(),
				serde_json::Value::Object(attrs_to_json(&entry.from.attrs)?),
			);
			let mut to = attrs_to_json(&entry.to.attrs)?;
			to.extend(attrs_to_json(&entry.extra_attrs)?);
			object.insert("to".into(), serde_json::Value::Object(to));
			if entry.exact {
				object.insert("exact".into(), true.into());
			}
			entries.push(serde_json::Value::Object(object));
		}
		let mut json = serde_json::Map::new();
		json.insert("version".into(), 2.into());
		json.insert("flakes".into(), serde_json::Value::Array(entries));
		Ok(crate::json::dump(&serde_json::Value::Object(json), Some(2)))
	}

	pub fn add(&mut self, from: Input, to: Input, extra_attrs: Attrs) {
		self.entries.push(Entry {
			from,
			to,
			extra_attrs,
			exact: false,
		});
	}

	pub fn remove(&mut self, input: &Input) {
		self.entries.retain(|entry| entry.from != *input);
	}
}

fn attrs_or_empty(json: &serde_json::Value) -> Result<Attrs> {
	if json.is_null() {
		Ok(Attrs::new())
	} else {
		json_to_attrs(json)
	}
}

fn resolve_symlinks(path: &Path) -> PathBuf {
	std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

/// Which registries a lookup may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UseRegistries {
	No,
	/// Only the flag and global registries, as for flake inputs.
	Limited,
	All,
}

/// `getUserRegistryPath`: `$XDG_CONFIG_HOME/nix/registry.json`.
pub fn user_registry_path() -> PathBuf {
	config_dir().join("registry.json")
}

/// `getSystemRegistryPath`: `$NIX_CONF_DIR/registry.json`.
pub fn system_registry_path() -> PathBuf {
	std::env::var_os("NIX_CONF_DIR")
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from("/etc/nix"))
		.join("registry.json")
}

fn config_dir() -> PathBuf {
	if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
		return PathBuf::from(dir).join("nix");
	}
	let home = std::env::var_os("HOME")
		.map(PathBuf::from)
		.unwrap_or_default();
	home.join(".config/nix")
}

/// `lookupInRegistries`: rewrite an indirect input through the
/// registries in order until it is direct, carrying `ref` and `rev` the
/// matched entry does not fix.
pub fn lookup_in_registries(
	registries: &[Registry],
	input: &Input,
	use_registries: UseRegistries,
) -> Result<(Input, Attrs)> {
	let mut extra_attrs = Attrs::new();
	let mut input = input.clone();
	if use_registries == UseRegistries::No {
		return Ok((input, extra_attrs));
	}
	let mut n = 0;
	'restart: loop {
		n += 1;
		if n > 100 {
			return Err(Error::new(format!(
				"cycle detected in flake registry for '{}'",
				input.display()
			)));
		}
		for registry in registries {
			if use_registries == UseRegistries::Limited
				&& !matches!(
					registry.registry_type,
					RegistryType::Flag | RegistryType::Global
				) {
				continue;
			}
			for entry in &registry.entries {
				if entry.exact {
					if entry.from == input {
						input = entry.to.clone();
						extra_attrs = entry.extra_attrs.clone();
						continue 'restart;
					}
				} else if entry.from.contains(&input) {
					let r#ref = if entry.from.get_ref()?.is_none() {
						input.get_ref()?
					} else {
						None
					};
					let rev = if entry.from.get_rev()?.is_none() {
						input.get_rev()?
					} else {
						None
					};
					input = entry.to.apply_overrides(r#ref.as_deref(), rev.as_ref())?;
					extra_attrs = entry.extra_attrs.clone();
					continue 'restart;
				}
			}
		}
		break;
	}
	if !input.is_direct() {
		return Err(Error::new(format!(
			"cannot find flake '{}' in the flake registries",
			input.display()
		)));
	}
	Ok((input, extra_attrs))
}

/// Build the flag registry from `--override-flake FROM TO` pairs; `dir`
/// in `TO` becomes an extra attribute.
pub fn flag_registry(
	settings: &FetchSettings,
	overrides: &[(String, String)],
	base_dir: &Path,
) -> Result<Registry> {
	use crate::flake_ref::{ParseOptions, parse_flake_ref};
	let mut registry = Registry::new(RegistryType::Flag);
	for (from, to) in overrides {
		let from = parse_flake_ref(
			settings,
			from,
			ParseOptions {
				base_dir: Some(base_dir),
				..ParseOptions::default()
			},
		)?;
		let to = parse_flake_ref(
			settings,
			to,
			ParseOptions {
				base_dir: Some(base_dir),
				..ParseOptions::default()
			},
		)?;
		let mut extra = Attrs::new();
		if !to.subdir.is_empty() {
			extra.insert("dir".into(), Attr::String(to.subdir.clone()));
		}
		registry.add(from.input, to.input, extra);
	}
	Ok(registry)
}
