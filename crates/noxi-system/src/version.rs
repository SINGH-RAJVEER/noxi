//! `noxi system version`: the native `nixos-version`.
//!
//! Upstream `nixos-version` (nixos/modules/installer/tools/nixos-version.sh)
//! is a script with the values of one system generation substituted in at
//! build time. This command reads the same values from the running system
//! instead, so it reports the generation that is actually running:
//!
//! - the version and code name from `/etc/os-release` (`BUILD_ID` and the
//!   parenthesised part of `VERSION`), falling back to `VERSION_CODENAME`
//!   and to the system's `nixos-version` file;
//! - the label from `/run/current-system/nixos-version`, which is
//!   `system.nixos.label` and can differ from the version;
//! - the specialisations from the system's `specialisation` directory;
//! - the Nixpkgs and configuration revisions and the kernel version from
//!   the `--json` document the system's own `sw/bin/nixos-version`
//!   embeds, because neither file above records them; the kernel version
//!   falls back to the name of the system's kernel module directory.
//!
//! The flags and their output match upstream: no flag prints
//! `VERSION (CODENAME)`, `--hash`/`--revision` the Nixpkgs revision,
//! `--configuration-revision` the configuration revision,
//! `--kernel-version` the kernel version, `--specialisations` one name per
//! line, and `--json` the upstream JSON object, here with the `label`,
//! `distroId` and `codeName` keys added. `--label` is noxi's own. Errors
//! are rustc-style diagnostics instead of upstream's `$0: ...` lines, with
//! the same exit status.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use noxi_events::Styling;
use serde_json::{Map, Value};

use crate::snapshot::CURRENT_SYSTEM;

pub const USAGE: &str = "usage: noxi system version [--hash | --revision | --configuration-revision | --label | --kernel-version | --specialisations | --json]";

/// The default location of the os-release file.
pub const OS_RELEASE: &str = "/etc/os-release";

/// Where the version values are read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sources {
	/// The running system, normally `/run/current-system`.
	pub system: PathBuf,
	/// The os-release file, normally `/etc/os-release`.
	pub os_release: PathBuf,
}

impl Default for Sources {
	fn default() -> Self {
		Sources {
			system: PathBuf::from(CURRENT_SYSTEM),
			os_release: PathBuf::from(OS_RELEASE),
		}
	}
}

/// What one system reports about its version.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Info {
	/// The full version, such as `26.11.20261113.d5cc683`.
	pub version: Option<String>,
	/// The release code name, such as `Zokor`.
	pub code_name: Option<String>,
	/// The `nixos-version` file of the system: `system.nixos.label`.
	pub label: Option<String>,
	/// `ID` from os-release, such as `nixos` or `noxios`.
	pub distro_id: Option<String>,
	/// The full Nixpkgs commit hash.
	pub revision: Option<String>,
	/// `system.configurationRevision`.
	pub configuration_revision: Option<String>,
	/// The kernel version, such as `7.3.4`.
	pub kernel_version: Option<String>,
	/// The names of the system's specialisations, sorted.
	pub specialisations: Vec<String>,
	/// The upstream `--json` document, when the system has one.
	pub upstream_json: Option<Map<String, Value>>,
}

/// Parse an os-release file: `KEY=VALUE` lines whose values may be quoted
/// with shell quoting rules, as os-release(5) specifies.
pub fn parse_os_release(text: &str) -> BTreeMap<String, String> {
	let mut fields = BTreeMap::new();
	for line in text.lines() {
		let line = line.trim();
		if line.is_empty() || line.starts_with('#') {
			continue;
		}
		let Some((key, value)) = line.split_once('=') else {
			continue;
		};
		fields.insert(key.to_owned(), unquote(value));
	}
	fields
}

fn unquote(value: &str) -> String {
	let mut chars = value.chars();
	match chars.next() {
		Some(quote @ ('"' | '\'')) => {
			let mut out = String::new();
			let mut escaped = false;
			for c in chars {
				if escaped {
					out.push(c);
					escaped = false;
				} else if c == '\\' && quote == '"' {
					escaped = true;
				} else if c == quote {
					break;
				} else {
					out.push(c);
				}
			}
			out
		}
		_ => value.to_owned(),
	}
}

/// The JSON document a NixOS `nixos-version` script prints for `--json`,
/// taken from the here-document the script embeds.
pub fn embedded_json(script: &str) -> Option<Map<String, Value>> {
	let start = script
		.find("<<'EOF'\n")
		.map(|i| i + 8)
		.or_else(|| script.find("<<EOF\n").map(|i| i + 6))?;
	let rest = &script[start..];
	let end = rest.find("\nEOF")?;
	match serde_json::from_str(&rest[..end]) {
		Ok(Value::Object(map)) => Some(map),
		_ => None,
	}
}

fn read_trimmed(path: &Path) -> Option<String> {
	fs::read_to_string(path)
		.ok()
		.map(|text| text.trim_end().to_owned())
		.filter(|text| !text.is_empty())
}

/// The entry names of directory `dir`, sorted; none if it is missing.
fn dir_names(dir: &Path) -> Vec<String> {
	let mut names: Vec<String> = fs::read_dir(dir)
		.into_iter()
		.flatten()
		.filter_map(|entry| entry.ok()?.file_name().into_string().ok())
		.collect();
	names.sort();
	names
}

/// The name of the only entry of `dir`: the kernel's module directory
/// is named after its version.
fn only_dir(dir: &Path) -> Option<String> {
	match dir_names(dir).as_slice() {
		[name] => Some(name.clone()),
		_ => None,
	}
}

/// Read the version of the system `sources` names. Missing files leave
/// their fields empty; it is an error only when no version is found.
pub fn read(sources: &Sources) -> Result<Info, String> {
	let os_release = fs::read_to_string(&sources.os_release)
		.map(|text| parse_os_release(&text))
		.unwrap_or_default();
	let label = read_trimmed(&sources.system.join("nixos-version"));
	let upstream_json = fs::read_to_string(sources.system.join("sw/bin/nixos-version"))
		.ok()
		.and_then(|script| embedded_json(&script));
	let json_string = |key: &str| {
		upstream_json
			.as_ref()
			.and_then(|json| json.get(key))
			.and_then(Value::as_str)
			.map(str::to_owned)
	};
	let version = os_release
		.get("BUILD_ID")
		.cloned()
		.or_else(|| json_string("nixosVersion"))
		.or_else(|| label.clone());
	let Some(version) = version else {
		return Err(format!(
			"cannot determine the system version: neither {} nor {} records it",
			sources.os_release.display(),
			sources.system.join("nixos-version").display()
		));
	};
	// `VERSION="26.11 (Zokor)"` keeps the code name's capitalisation;
	// `VERSION_CODENAME` is lower-cased.
	let code_name = os_release
		.get("VERSION")
		.and_then(|v| {
			let open = v.find('(')?;
			let close = v.rfind(')')?;
			(open < close).then(|| v[open + 1..close].to_owned())
		})
		.or_else(|| os_release.get("VERSION_CODENAME").cloned());
	Ok(Info {
		version: Some(version),
		code_name,
		label,
		distro_id: os_release.get("ID").cloned(),
		revision: json_string("nixpkgsRevision"),
		configuration_revision: json_string("configurationRevision"),
		kernel_version: json_string("kernelVersion")
			.or_else(|| only_dir(&sources.system.join("kernel-modules/lib/modules"))),
		specialisations: dir_names(&sources.system.join("specialisation")),
		upstream_json,
	})
}

/// What to print.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Query {
	#[default]
	Version,
	Revision,
	ConfigurationRevision,
	Label,
	KernelVersion,
	Specialisations,
	Json,
}

pub fn parse(args: &[String]) -> Result<Option<Query>, String> {
	let mut query = None;
	for arg in args {
		let next = match arg.as_str() {
			"-h" | "--help" => return Ok(None),
			"--hash" | "--revision" => Query::Revision,
			"--configuration-revision" => Query::ConfigurationRevision,
			"--label" => Query::Label,
			"--kernel-version" => Query::KernelVersion,
			"--specialisations" => Query::Specialisations,
			"--json" => Query::Json,
			other => return Err(format!("unexpected argument '{other}'")),
		};
		if query.replace(next).is_some() {
			return Err("only one of the version flags can be given".to_owned());
		}
	}
	Ok(Some(query.unwrap_or_default()))
}

/// The JSON object `--json` prints.
pub fn json(info: &Info) -> Value {
	let mut object = info.upstream_json.clone().unwrap_or_default();
	let mut set = |key: &str, value: &Option<String>| {
		if let Some(value) = value {
			object
				.entry(key.to_owned())
				.or_insert_with(|| Value::String(value.clone()));
		}
	};
	set("nixosVersion", &info.version);
	set("nixpkgsRevision", &info.revision);
	set("configurationRevision", &info.configuration_revision);
	set("kernelVersion", &info.kernel_version);
	set("codeName", &info.code_name);
	set("label", &info.label);
	set("distroId", &info.distro_id);
	if !info.specialisations.is_empty() || !object.contains_key("specialisations") {
		object.insert(
			"specialisations".to_owned(),
			Value::Array(
				info.specialisations
					.iter()
					.cloned()
					.map(Value::String)
					.collect(),
			),
		);
	}
	Value::Object(object)
}

/// Answer `query` about the system `sources` names.
pub fn run(query: Query, sources: &Sources, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
	let info = match read(sources) {
		Ok(info) => info,
		Err(message) => return crate::fail(err, &Styling::for_stderr(), None, &message),
	};
	let line = match query {
		Query::Version => match &info.code_name {
			Some(code_name) => format!("{} ({code_name})", info.version.unwrap_or_default()),
			None => info.version.unwrap_or_default(),
		},
		Query::Revision => match info.revision {
			Some(revision) => revision,
			None => {
				return crate::fail(
					err,
					&Styling::for_stderr(),
					None,
					"the Nixpkgs commit hash is unknown",
				);
			}
		},
		Query::ConfigurationRevision => match info.configuration_revision {
			Some(revision) => revision,
			None => {
				return crate::fail(
					err,
					&Styling::for_stderr(),
					None,
					"the configuration revision is unknown",
				);
			}
		},
		Query::Label => info.label.or(info.version).unwrap_or_default(),
		Query::KernelVersion => match info.kernel_version {
			Some(version) => version,
			None => {
				return crate::fail(
					err,
					&Styling::for_stderr(),
					None,
					"the kernel version is unknown",
				);
			}
		},
		Query::Specialisations if info.specialisations.is_empty() => {
			return crate::fail(
				err,
				&Styling::for_stderr(),
				None,
				"no specialisations found",
			);
		}
		Query::Specialisations => info.specialisations.join("\n"),
		Query::Json => json(&info).to_string(),
	};
	let _ = writeln!(out, "{line}");
	ExitCode::SUCCESS
}

pub fn main(args: Vec<String>, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
	match parse(&args) {
		Ok(Some(query)) => run(query, &Sources::default(), out, err),
		Ok(None) => {
			let _ = writeln!(out, "{USAGE}");
			ExitCode::SUCCESS
		}
		Err(message) => {
			let _ = writeln!(err, "error: {message}\n{USAGE}");
			ExitCode::from(2)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const OS_RELEASE_TEXT: &str = "ANSI_COLOR=\"0;38;2;126;186;228\"\nBUILD_ID=\"26.11.20261113.d5cc683\"\nID=noxios\nID_LIKE=nixos\nNAME=noxios\nVERSION=\"26.11 (Zokor)\"\nVERSION_CODENAME=zokor\nVERSION_ID=\"26.11\"\n";

	const SCRIPT: &str = "#! /bin/sh\ncase \"$1\" in\n  --json)\n    cat <<'EOF'\n{\"kernelVersion\":\"7.3.4\",\"nixosVersion\":\"26.11.20261113.d5cc683\",\"nixpkgsRevision\":\"d5cc6833999746cffdfe0598f7bc801a57b887a9\",\"configurationRevision\":\"abc\"}\nEOF\n    ;;\nesac\n";

	fn system(with_script: bool) -> (tempfile::TempDir, Sources) {
		let dir = tempfile::tempdir().unwrap();
		let system = dir.path().join("system");
		fs::create_dir_all(system.join("sw/bin")).unwrap();
		fs::write(system.join("nixos-version"), "my-label").unwrap();
		if with_script {
			fs::write(system.join("sw/bin/nixos-version"), SCRIPT).unwrap();
		}
		let os_release = dir.path().join("os-release");
		fs::write(&os_release, OS_RELEASE_TEXT).unwrap();
		(dir, Sources { system, os_release })
	}

	fn answer(query: Query, sources: &Sources) -> (ExitCode, String, String) {
		let (mut out, mut err) = (Vec::new(), Vec::new());
		let code = run(query, sources, &mut out, &mut err);
		(
			code,
			String::from_utf8(out).unwrap(),
			String::from_utf8(err).unwrap(),
		)
	}

	#[test]
	fn os_release_quoting() {
		let fields = parse_os_release("A=\"x \\\"y\\\"\"\nB='z'\n# c\nC=plain\n");
		assert_eq!(fields["A"], "x \"y\"");
		assert_eq!(fields["B"], "z");
		assert_eq!(fields["C"], "plain");
	}

	#[test]
	fn default_query_prints_version_and_code_name() {
		let (_dir, sources) = system(true);
		let (code, out, _) = answer(Query::Version, &sources);
		assert_eq!(code, ExitCode::SUCCESS);
		assert_eq!(out, "26.11.20261113.d5cc683 (Zokor)\n");
	}

	#[test]
	fn revisions_come_from_the_embedded_json() {
		let (_dir, sources) = system(true);
		assert_eq!(
			answer(Query::Revision, &sources).1,
			"d5cc6833999746cffdfe0598f7bc801a57b887a9\n"
		);
		assert_eq!(answer(Query::ConfigurationRevision, &sources).1, "abc\n");
		assert_eq!(answer(Query::Label, &sources).1, "my-label\n");
		let json: Value = serde_json::from_str(&answer(Query::Json, &sources).1).unwrap();
		assert_eq!(json["nixosVersion"], "26.11.20261113.d5cc683");
		assert_eq!(json["kernelVersion"], "7.3.4");
		assert_eq!(json["label"], "my-label");
		assert_eq!(json["distroId"], "noxios");
	}

	#[test]
	fn unknown_revision_fails_like_upstream() {
		let (_dir, sources) = system(false);
		let (code, out, err) = answer(Query::Revision, &sources);
		assert_eq!(code, ExitCode::FAILURE);
		assert!(out.is_empty());
		assert!(err.contains("Nixpkgs commit hash is unknown"), "{err}");
		let json: Value = serde_json::from_str(&answer(Query::Json, &sources).1).unwrap();
		assert!(json.get("nixpkgsRevision").is_none());
	}

	#[test]
	fn missing_version_is_an_error() {
		let dir = tempfile::tempdir().unwrap();
		let sources = Sources {
			system: dir.path().join("none"),
			os_release: dir.path().join("none"),
		};
		assert_eq!(answer(Query::Version, &sources).0, ExitCode::FAILURE);
	}

	#[test]
	fn flags() {
		assert_eq!(parse(&[]).unwrap(), Some(Query::Version));
		assert_eq!(parse(&["--hash".into()]).unwrap(), Some(Query::Revision));
		assert!(parse(&["--json".into(), "--label".into()]).is_err());
		assert_eq!(
			parse(&["--kernel-version".into()]).unwrap(),
			Some(Query::KernelVersion)
		);
		assert!(parse(&["--bogus".into()]).is_err());
	}

	#[test]
	fn kernel_version_and_specialisations() {
		let (_dir, sources) = system(false);
		assert_eq!(answer(Query::KernelVersion, &sources).0, ExitCode::FAILURE);
		let (code, _, err) = answer(Query::Specialisations, &sources);
		assert_eq!(code, ExitCode::FAILURE);
		assert!(err.contains("no specialisations found"), "{err}");
		fs::create_dir_all(sources.system.join("kernel-modules/lib/modules/7.3.4")).unwrap();
		for name in ["work", "gaming"] {
			fs::create_dir_all(sources.system.join("specialisation").join(name)).unwrap();
		}
		assert_eq!(answer(Query::KernelVersion, &sources).1, "7.3.4\n");
		assert_eq!(answer(Query::Specialisations, &sources).1, "gaming\nwork\n");
		let json: Value = serde_json::from_str(&answer(Query::Json, &sources).1).unwrap();
		assert_eq!(json["kernelVersion"], "7.3.4");
		assert_eq!(
			json["specialisations"],
			serde_json::json!(["gaming", "work"])
		);
	}
}
